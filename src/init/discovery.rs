//! Touchpad discovery by direct evdev capability inspection.
//!
//! This walks `/dev/input/event*` and checks each device for the
//! capabilities that define a multitouch touchpad -- the same criteria
//! libinput itself uses to classify one:
//!
//! * `INPUT_PROP_POINTER` (it moves a cursor, i.e. not a touchscreen)
//! * multitouch slots + positions (`ABS_MT_SLOT`, `ABS_MT_POSITION_X/Y`)
//! * `BTN_TOOL_FINGER` (finger-count reporting)
//!
//! Doing this directly (instead of going through libinput's udev seat
//! enumeration, as earlier versions did) needs no C library, no seat
//! assignment, and -- crucially -- lets us skip our *own* synthetic
//! clone, which impersonates the real touchpad's identity exactly and
//! is only distinguishable by the `phys` marker we stamp on it. That
//! matters when re-discovering after the real device re-enumerates.

use std::fs::OpenOptions;
use std::io::{Error, ErrorKind};
use std::os::unix::fs::OpenOptionsExt;

use input_linux::{AbsoluteAxis, EvdevHandle, InputProperty, Key};
use libc::O_NONBLOCK;
use tracing::{debug, error, info};

use crate::runtime::mt_proxy::CLONE_PHYS_MARKER;

/// Finds every real multitouch touchpad, returning `/dev/input/eventN`
/// paths. The caller opens these directly so it can exclusively grab
/// and proxy the raw event stream.
pub fn find_real_trackpads() -> Result<Vec<String>, Error> {
    let mut found = Vec::new();
    let mut denied = 0usize;
    let mut inspected = 0usize;

    let mut entries: Vec<_> = std::fs::read_dir("/dev/input")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .map(|e| e.path())
        .collect();
    entries.sort();

    for path in entries {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                denied += 1;
                continue;
            }
            Err(_) => continue,
        };
        inspected += 1;
        let dev = EvdevHandle::new(file);

        let Ok(props) = dev.device_properties() else {
            continue;
        };
        if !props.get(InputProperty::Pointer) {
            continue; // touchscreens, keyboards, mice without the prop
        }
        let Ok(abs) = dev.absolute_bits() else {
            continue;
        };
        if !(abs.get(AbsoluteAxis::MultitouchSlot)
            && abs.get(AbsoluteAxis::MultitouchPositionX)
            && abs.get(AbsoluteAxis::MultitouchPositionY))
        {
            continue; // pointer device without multitouch (plain mouse)
        }
        let Ok(keys) = dev.key_bits() else { continue };
        if !keys.get(Key::ButtonToolFinger) {
            continue;
        }

        // Never proxy our own synthetic clone: it passes every check
        // above by design (it impersonates the real device), and is
        // recognizable only by the phys marker stamped on it.
        if let Ok(phys) = dev.physical_location() {
            if phys.starts_with(CLONE_PHYS_MARKER.as_bytes()) {
                debug!("Skipping our own synthetic clone at {}.", path.display());
                continue;
            }
        }

        let name = dev
            .device_name()
            .map(|n| {
                String::from_utf8_lossy(&n)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_else(|_| "<unknown>".to_string());
        info!("Touchpad found: \"{}\" at {}.", name, path.display());
        found.push(path.to_string_lossy().into_owned());
    }

    if !found.is_empty() {
        return Ok(found);
    }

    // Nothing found: produce the most useful error we can.
    if inspected == 0 && denied > 0 {
        error!(
            "This program does not have permission to read /dev/input \
            devices, most likely because you are not in the user group \
            'input'. Make sure you've followed the instructions in Step 3 \
            of the Manual Install section of the README. If you have, try \
            logging out and in again, or rebooting (group changes only \
            apply to new logins). If all of these fail, please open an \
            issue at https://github.com/lmr97/linux-3-finger-drag/issues."
        );
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "not permitted to read /dev/input (user not in group 'input'?)",
        ));
    }

    error!(
        "No multitouch touchpad was found among {} readable input \
        devices ({} unreadable). If this machine really has a touchpad, \
        please open an issue at \
        https://github.com/lmr97/linux-3-finger-drag/issues and include \
        the output of `libinput list-devices`.",
        inspected, denied
    );
    Err(Error::new(
        ErrorKind::NotFound,
        "no multitouch touchpad discoverable",
    ))
}

/// An opened keyboard, deliberately read-only and never exclusively grabbed.
/// Remappers may grab the physical keyboard and re-emit through a virtual
/// one, so bus type and device name are not reliable discovery criteria.
pub struct Keyboard {
    pub path: std::path::PathBuf,
    pub device: EvdevHandle<std::fs::File>,
}

/// Sysfs prints unsigned-long words most-significant first, omitting
/// leading zeroes within each word. Keep word boundaries: concatenating
/// the hex strings would shift bits when an interior word is short.
struct Bits(Vec<u64>);

impl Bits {
    fn parse(text: &str) -> Option<Self> {
        let words: Option<Vec<_>> = text
            .split_whitespace()
            .rev()
            .map(|word| {
                if word.len() > 16 || !word.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                u64::from_str_radix(word, 16).ok()
            })
            .collect();
        let words = words?;
        if words.is_empty() {
            return None;
        }
        Some(Self(words))
    }

    fn get(&self, code: u16) -> bool {
        self.0
            .get(usize::from(code) / 64)
            .is_some_and(|word| word & (1u64 << (code % 64)) != 0)
    }
}

/// Remappers can combine keyboard and pointer capabilities on one device.
/// Letters identify typing sources; only absolute (touch/tablet) axes and
/// our own clone's phys marker disqualify an otherwise valid keyboard.
/// Touch *key* bits (BTN_TOUCH/BTN_TOOL_FINGER) are deliberately not a
/// criterion: uinput remappers such as xwaykeyz advertise nearly every
/// key code, touch buttons included, while having no absolute axes.
fn is_typing_keyboard(ev: &Bits, key: &Bits, abs: &Bits, phys: &str) -> bool {
    ev.get(input_linux::EventKind::Key as u16)
        && [Key::A, Key::Z, Key::Space]
            .iter()
            .all(|&code| key.get(code as u16))
        && ![
            AbsoluteAxis::X,
            AbsoluteAxis::MultitouchSlot,
            AbsoluteAxis::MultitouchPositionX,
        ]
        .iter()
        .any(|&code| abs.get(code as u16))
        && !phys.starts_with(CLONE_PHYS_MARKER)
}

fn sysfs_typing_keyboard(device: &std::path::Path) -> Option<bool> {
    let read_bits =
        |name| Bits::parse(&std::fs::read_to_string(device.join("capabilities").join(name)).ok()?);
    let ev = read_bits("ev")?;
    let key = read_bits("key")?;
    let abs = read_bits("abs")?;
    let phys = std::fs::read_to_string(device.join("phys")).ok()?;
    Some(is_typing_keyboard(&ev, &key, &abs, &phys))
}

pub fn find_keyboards(
    monitored: &std::collections::HashSet<std::path::PathBuf>,
) -> Result<Vec<Keyboard>, Error> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/dev/input")?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("event") || monitored.contains(&path) {
            continue;
        }
        // Opening an evdev node can wake/power-cycle its driver. Classify
        // via sysfs first, so housekeeping never opens unrelated devices.
        // Missing or malformed metadata is not evidence of a keyboard.
        let device = std::path::Path::new("/sys/class/input")
            .join(&name)
            .join("device");
        if sysfs_typing_keyboard(&device) != Some(true) {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)
        else {
            continue; // disappearing or unreadable devices must not stop the touchpad
        };
        let device = EvdevHandle::new(file);
        found.push(Keyboard { path, device });
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(codes: &[u16]) -> Bits {
        let mut words = vec![0; 12];
        for &code in codes {
            words[usize::from(code) / 64] |= 1u64 << (code % 64);
        }
        Bits(words)
    }

    #[test]
    fn classifies_typing_sources_including_combined_remapper() {
        let letters = [Key::A as u16, Key::Z as u16, Key::Space as u16];
        let key_ev = bits(&[1]);
        let combined_ev = bits(&[1, 2]); // EV_KEY + EV_REL
        let no_abs = bits(&[]);
        assert!(is_typing_keyboard(
            &key_ev,
            &bits(&letters),
            &no_abs,
            "isa0060/serio0"
        ));
        let mut combined_keys = letters.to_vec();
        combined_keys.push(Key::ButtonLeft as u16);
        assert!(is_typing_keyboard(
            &combined_ev,
            &bits(&combined_keys),
            &no_abs,
            "py-evdev-uinput"
        ));
        assert!(!is_typing_keyboard(
            &combined_ev,
            &bits(&[Key::ButtonLeft as u16]),
            &no_abs,
            "mouse"
        ));
        assert!(!is_typing_keyboard(
            &key_ev,
            &bits(&[Key::VolumeUp as u16]),
            &no_abs,
            "consumer"
        ));
        assert!(!is_typing_keyboard(
            &key_ev,
            &bits(&letters),
            &no_abs,
            &format!("{CLONE_PHYS_MARKER}/keyboard")
        ));
        assert!(!is_typing_keyboard(
            &bits(&[2]),
            &bits(&letters),
            &no_abs,
            "no EV_KEY"
        ));

        // Touch key bits alone don't disqualify: remappers advertise them.
        let mut touch_keys = letters.to_vec();
        touch_keys.extend([Key::ButtonToolFinger as u16, Key::ButtonTouch as u16]);
        assert!(is_typing_keyboard(
            &key_ev,
            &bits(&touch_keys),
            &no_abs,
            "py-evdev-uinput"
        ));

        // Each absolute-axis discriminator must reject even a device
        // advertising letters; test them independently so none masks another.
        for axis in [
            AbsoluteAxis::X,
            AbsoluteAxis::MultitouchSlot,
            AbsoluteAxis::MultitouchPositionX,
        ] {
            assert!(!is_typing_keyboard(
                &key_ev,
                &bits(&letters),
                &bits(&[axis as u16]),
                "touchpad"
            ));
        }
    }

    #[test]
    fn parses_real_xwaykeyz_sysfs_bitmaps() {
        let key = Bits::parse("3f3f3fffffff03ff ffffffff000fffff 700007fffffff 7fe003fffff000f f7ffffffffffffff ffffffff000fe420 1f03ff 1ffffffffffff07 ffffffffffffffff ffffffffffefffff fffffffffffffffe\n").unwrap();
        for code in [Key::A, Key::Z, Key::Space, Key::ButtonLeft] {
            assert!(key.get(code as u16));
        }
        // The real remapper bitmap advertises touch buttons too (word 5,
        // ffffffff000fe420, bits 5 and 10) -- which is exactly why touch
        // key bits can't be a rejection criterion. It has no EV_ABS.
        assert!(key.get(Key::ButtonToolFinger as u16));
        assert!(key.get(Key::ButtonTouch as u16));
        let ev = Bits::parse("200007\n").unwrap();
        assert!(ev.get(1)); // EV_KEY
        assert!(ev.get(2)); // EV_REL
        assert!(!ev.get(3)); // no EV_ABS
        assert!(is_typing_keyboard(&ev, &key, &bits(&[]), "py-evdev-uinput"));
    }

    #[test]
    fn parses_short_words_at_unsigned_long_boundaries() {
        let bitmap = Bits::parse("1 2 3").unwrap();
        for code in [0, 1, 65, 128] {
            assert!(bitmap.get(code));
        }
        for code in [2, 64, 127, 129, 192] {
            assert!(!bitmap.get(code));
        }
        assert!(!Bits::parse("0").unwrap().get(0));
        for invalid in ["", " ", "xyz", "1 invalid", "10000000000000000", "+1"] {
            assert!(Bits::parse(invalid).is_none());
        }
    }
}
