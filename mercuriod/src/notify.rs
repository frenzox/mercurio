//! Tells the service manager how the daemon is doing, as sd_notify(3)
//! does, when systemd started it as a `Type=notify` service.

/// Sends `state`, such as `READY=1`, to `$NOTIFY_SOCKET` if it is set. A
/// failure is only logged: the service manager then sees the daemon as it
/// would without notifications.
#[cfg(unix)]
pub(crate) fn notify(state: &str) {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if let Err(e) = send(&socket, state) {
        tracing::warn!(
            "Can't notify the service manager at {}: {}",
            socket.to_string_lossy(),
            e
        );
    }
}

#[cfg(not(unix))]
pub(crate) fn notify(_state: &str) {}

/// Sends one datagram to `socket`, a path or, starting with `@`, a name in
/// Linux's abstract namespace.
#[cfg(unix)]
fn send(socket: &std::ffi::OsStr, state: &str) -> std::io::Result<()> {
    use std::os::unix::{ffi::OsStrExt, net::UnixDatagram};

    let sender = UnixDatagram::unbound()?;
    match socket.as_bytes().strip_prefix(b"@") {
        #[cfg(target_os = "linux")]
        Some(name) => {
            use std::os::{linux::net::SocketAddrExt, unix::net::SocketAddr};
            sender.send_to_addr(state.as_bytes(), &SocketAddr::from_abstract_name(name)?)?;
        }
        #[cfg(not(target_os = "linux"))]
        Some(_) => return Err(std::io::ErrorKind::Unsupported.into()),
        None => {
            sender.send_to(state.as_bytes(), socket)?;
        }
    }
    Ok(())
}
