//! Read-only typing activity monitoring. Only a timestamp crosses into the
//! gesture machine: key identities and text are never retained or logged.
//! Each evdev reader has its own queue; a remapper's exclusive grab silences
//! its physical source, so we also discover the remapper's virtual keyboard.

use std::collections::HashSet;
use std::future::poll_fn;
use std::io;
use std::task::Poll;
use std::time::Instant;

use input_linux::Key;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

use crate::init::discovery::{self, Keyboard};
use crate::runtime::gesture::{GestureMachine, EV_KEY};

#[derive(Default)]
pub struct KeyboardMonitor {
    devices: Vec<(
        std::path::PathBuf,
        AsyncFd<input_linux::EvdevHandle<std::fs::File>>,
    )>,
}

impl KeyboardMonitor {
    /// Called on startup and housekeeping, including after hot reload.
    /// Disabled means no open keyboard fds, not merely ignoring their input.
    pub fn refresh(&mut self, enabled: bool) {
        if !enabled {
            self.devices.clear();
            return;
        }
        let monitored: HashSet<_> = self.devices.iter().map(|(path, _)| path.clone()).collect();
        match discovery::find_keyboards(&monitored) {
            Ok(devices) => {
                for Keyboard { path, device } in devices {
                    let name = device.device_name().unwrap_or_default();
                    match AsyncFd::new(device) {
                        Ok(fd) => {
                            info!(
                                "Monitoring keyboard for typing activity: {:?} at {}",
                                String::from_utf8_lossy(&name).trim_end_matches('\0'),
                                path.display()
                            );
                            self.devices.push((path, fd));
                        }
                        Err(e) => warn!("Unable to monitor keyboard at {}: {e}", path.display()),
                    }
                }
            }
            Err(e) => warn!("Unable to discover keyboards: {e}"),
        }
    }

    /// Register every fd with the same task waker. An empty monitor sleeps
    /// indefinitely; no polling timer is needed to detect individual keys.
    pub async fn ready(&self) {
        poll_fn(|cx| {
            for (_, fd) in &self.devices {
                if fd.poll_read_ready(cx).is_ready() {
                    return Poll::Ready(());
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Drain before touch frames AND timed decisions, even if select! chose
    /// the touchpad first: simultaneous readiness must not commit a drag
    /// while a qualifying key is still sitting in a keyboard's queue.
    /// All failures are local to this reader; hotplug/remapper restarts are
    /// recovered by the next rescan, including reuse of an old event path.
    pub fn drain(&mut self, machine: &mut GestureMachine) {
        // SAFETY: input_event contains only integer fields (including
        // its timeval); all-zero is a valid empty read buffer. Reuse the
        // buffer across reads and devices; only the returned prefix is used.
        let mut events = unsafe { std::mem::zeroed::<[input_linux::sys::input_event; 64]>() };
        self.devices.retain(|(path, fd)| {
            loop {
                match fd.get_ref().read(&mut events) {
                    Ok(0) => return false,
                    Ok(n) => {
                        for event in &events[..n] {
                            if qualifying_press(event.type_, event.code, event.value) {
                                machine.note_key_press(Instant::now());
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // We read directly even before Tokio has observed readiness.
                        // Now that the queue is empty, clear any cached readiness
                        // using try_io's normal WouldBlock handling.
                        let _ = fd.try_io(tokio::io::Interest::READABLE, |_| {
                            Err::<(), _>(io::Error::from(io::ErrorKind::WouldBlock))
                        });
                        return true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        debug!("Dropping keyboard monitor at {}: {e}", path.display());
                        return false;
                    }
                }
            }
        });
    }
}

fn qualifying_press(type_: u16, code: u16, value: i32) -> bool {
    type_ == EV_KEY
        && (1..0x100).contains(&code)
        && matches!(value, 1 | 2)
        && ![
            Key::LeftShift,
            Key::RightShift,
            Key::LeftCtrl,
            Key::RightCtrl,
            Key::LeftAlt,
            Key::RightAlt,
            Key::LeftMeta,
            Key::RightMeta,
            Key::CapsLock,
        ]
        .iter()
        .any(|&modifier| modifier as u16 == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket pair supplies evdev-shaped bytes without creating live input
    /// devices. Exercise readiness, draining before readiness is polled, EOF
    /// cleanup and disabling, all without /dev/input or /dev/uinput access.
    #[tokio::test(flavor = "current_thread")]
    async fn monitoring_drains_and_closes_readers_without_live_devices() {
        use std::fs::File;
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let reader: OwnedFd = reader.into();
        let writer: OwnedFd = writer.into();
        let writer = input_linux::EvdevHandle::new(File::from(writer));
        let fd = AsyncFd::new(input_linux::EvdevHandle::new(File::from(reader))).unwrap();
        let mut monitor = KeyboardMonitor {
            devices: vec![("test-keyboard".into(), fd)],
        };
        let mut timing = crate::init::config::Configuration::default().timing();
        timing.typing_timeout = Duration::from_millis(200);
        let mut machine = GestureMachine::new(timing, 10.0, 10.0, 16);
        // SAFETY: input_event is a C struct with only integer fields.
        let mut event: input_linux::sys::input_event = unsafe { std::mem::zeroed() };
        event.type_ = EV_KEY;
        event.code = Key::A as u16;
        event.value = 1;
        assert_eq!(writer.write(&[event]).unwrap(), 1);
        tokio::time::timeout(Duration::from_secs(1), monitor.ready())
            .await
            .unwrap();
        monitor.drain(&mut machine);
        assert_eq!(monitor.devices.len(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), monitor.ready())
                .await
                .is_err()
        );
        // Touchpad readiness can win select! before keyboard readiness is
        // polled. Direct draining must still consume every queued event.
        assert_eq!(writer.write(&[event]).unwrap(), 1);
        monitor.drain(&mut machine);
        let mut remaining = [event; 1];
        assert_eq!(
            monitor.devices[0]
                .1
                .get_ref()
                .read(&mut remaining)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        monitor.refresh(false);
        assert!(monitor.devices.is_empty());

        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let reader: OwnedFd = reader.into();
        monitor.devices.push((
            "disconnected-keyboard".into(),
            AsyncFd::new(input_linux::EvdevHandle::new(File::from(reader))).unwrap(),
        ));
        drop(writer);
        monitor.drain(&mut machine);
        assert!(monitor.devices.is_empty());
    }

    #[test]
    fn button_and_reserved_codes_do_not_count_as_typing() {
        for code in [0, 0x100, 0x110, 0x111, 0x14a, 0x145] {
            for value in [1, 2] {
                assert!(!qualifying_press(EV_KEY, code, value));
            }
        }
    }

    #[test]
    fn only_non_modifier_presses_and_repeats_count() {
        for key in [
            Key::LeftShift,
            Key::RightShift,
            Key::LeftCtrl,
            Key::RightCtrl,
            Key::LeftAlt,
            Key::RightAlt,
            Key::LeftMeta,
            Key::RightMeta,
            Key::CapsLock,
        ] {
            for value in [0, 1, 2] {
                assert!(!qualifying_press(EV_KEY, key as u16, value));
            }
        }
        for key in [Key::A, Key::Space, Key::Enter] {
            assert!(qualifying_press(EV_KEY, key as u16, 1));
            assert!(qualifying_press(EV_KEY, key as u16, 2));
            assert!(!qualifying_press(EV_KEY, key as u16, 0));
        }
        assert!(!qualifying_press(0, Key::A as u16, 1));
    }
}
