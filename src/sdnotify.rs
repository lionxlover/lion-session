//! Minimal `sd_notify(3)` client, hand-rolled so the project does not grow a
//! dependency for ~80 lines of datagram writes. When `NOTIFY_SOCKET` is unset
//! (plain greeter-started session, container, test) every call is a silent
//! no-op: running outside systemd must never change behaviour.
//!
//! Supported: READY=1 / STOPPING=1 / STATUS= / WATCHDOG=1, on both regular
//! and abstract (`@name`, kernel namespace) sockets.

use std::{os::unix::net::UnixDatagram, time::Duration};

/// True when a service manager handed us a notify socket.
#[cfg(test)]
pub fn supported() -> bool {
    std::env::var_os("NOTIFY_SOCKET").is_some()
}

/// Send one line to the manager. Returns false if there is no socket or the
/// write failed -- always harmless, never logged above debug.
pub fn notify(line: &str) -> bool {
    let Some(sock) = std::env::var_os("NOTIFY_SOCKET") else {
        return false;
    };
    let s = sock.to_string_lossy().into_owned();
    if let Some(abstract_name) = s.strip_prefix('@') {
        send_abstract(abstract_name, line.as_bytes())
    } else {
        match UnixDatagram::unbound().and_then(|d| d.send_to(line.as_bytes(), &s).map(|_| ())) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, "sd_notify write failed");
                false
            }
        }
    }
}

/// Abstract-namespace notify socket: sun_path[0] = NUL, name follows
/// (the '@' in NOTIFY_SOCKET is the user-space spelling of that NUL).
fn send_abstract(name: &str, bytes: &[u8]) -> bool {
    let name = name.as_bytes();
    if name.len() >= 108 {
        return false; // sockaddr_un::sun_path would overflow
    }
    // SAFETY: sockaddr_un is zeroed; we only write within sun_path bounds
    // (checked above) and pass a consistent sockaddr length.
    unsafe {
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        std::ptr::copy_nonoverlapping(
            name.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr().add(1),
            name.len(),
        );
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return false;
        }
        let sent = libc::sendto(
            fd,
            bytes.as_ptr().cast(),
            bytes.len(),
            0,
            std::ptr::from_ref(&addr).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        libc::close(fd);
        sent == bytes.len() as isize
    }
}

pub fn ready() -> bool {
    notify("READY=1")
}

pub fn stopping() -> bool {
    notify("STOPPING=1")
}

pub fn status(msg: &str) -> bool {
    notify(&format!("STATUS={msg}"))
}

/// Arm the watchdog pinger when the manager set WATCHDOG_USEC (microseconds).
/// Half the interval, floor 250 ms, exactly as sd_notify(3) prescribes.
/// Returns the task handle so callers can shut it down with the session.
pub fn spawn_watchdog() -> Option<tokio::task::JoinHandle<()>> {
    let usec = parse_watchdog_usec(std::env::var("WATCHDOG_USEC").ok().as_deref())?;
    let interval = Duration::from_micros(usec / 2).max(Duration::from_millis(250));
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if !notify("WATCHDOG=1") {
                return; // manager went away; stop pinging into the void
            }
        }
    }))
}

fn parse_watchdog_usec(raw: Option<&str>) -> Option<u64> {
    let raw = raw?;
    let usec: u64 = raw.trim().parse().ok()?;
    (usec > 0).then_some(usec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_interval_parsing() {
        assert_eq!(parse_watchdog_usec(None), None);
        assert_eq!(parse_watchdog_usec(Some("")), None);
        assert_eq!(parse_watchdog_usec(Some("abc")), None);
        assert_eq!(parse_watchdog_usec(Some("0")), None);
        assert_eq!(parse_watchdog_usec(Some("10000000")), Some(10_000_000));
        assert_eq!(parse_watchdog_usec(Some(" 4000000 ")), Some(4_000_000));
    }

    #[test]
    fn notify_without_socket_is_a_noop_false() {
        // The default test environment has no NOTIFY_SOCKET.
        if !supported() {
            assert!(!notify("READY=1"));
            assert!(!ready());
        }
    }

    #[test]
    fn abstract_name_length_guard() {
        // Delivering to an abstract socket nobody bound fails at the OS
        // level (ECONNREFUSED) -- which is fine. What must hold: no panic,
        // and the oversize guard rejects *before* any syscall.
        assert!(!send_abstract(&"x".repeat(108), b"READY=1"));
        let _ = send_abstract(&"x".repeat(107), b"READY=1"); // must not panic
    }
}
