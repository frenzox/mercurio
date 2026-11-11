//! The admin socket: a Unix socket through which root, the broker's own
//! user and the members of one group administer the broker without
//! credentials.

use std::{
    ffi::{CStr, CString},
    fs, io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use mercurio_server::server::{ListenerConfig, Transport};
use tokio::net::{unix::UCred, UnixListener};
use tracing::{info, warn};

use crate::config::AdminConfig;

/// Largest buffer offered to `getgrnam_r` and `getpwnam_r`, for groups with
/// very many members.
const MAX_LOOKUP_BUFFER: usize = 1 << 20;

/// The admin socket's file, removed when this is dropped unless another
/// socket took its place meanwhile.
pub struct AdminSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for AdminSocket {
    fn drop(&mut self) {
        let ours = fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| meta.dev() == self.device && meta.ino() == self.inode);
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A group, with the user ids of the members it lists. Users whose primary
/// group it is are matched by the client's group id instead.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    gid: u32,
    members: Vec<u32>,
}

/// Who may use the socket: root, the broker's own user and the group's
/// members, as the group was when the broker started.
struct Admitted {
    own_uid: u32,
    group: Option<Group>,
}

impl Admitted {
    fn admits(&self, uid: u32, gid: u32) -> bool {
        uid == 0
            || uid == self.own_uid
            || self
                .group
                .as_ref()
                .is_some_and(|group| group.gid == gid || group.members.contains(&uid))
    }
}

/// Binds the admin socket `config` asks for, as a trusted listener. None
/// when `socket` is empty, or when the socket can't be set up safely, which
/// is logged: the broker then runs without it. An error when another
/// process serves the socket, likely another broker, which a new one must
/// not take the socket from.
pub fn open(config: &AdminConfig) -> Result<Option<(ListenerConfig, AdminSocket)>, String> {
    if config.socket.is_empty() {
        return Ok(None);
    }
    let path = Path::new(&config.socket);
    if in_use(path) {
        return Err(format!(
            "Another process serves the admin socket {}; stop it, or set another `socket` in [admin]",
            path.display()
        ));
    }
    match bind(path, &config.group) {
        Ok(opened) => {
            info!("Serving the admin socket on {}", path.display());
            Ok(Some(opened))
        }
        Err(e) => {
            warn!("Running without the admin socket {}: {}", path.display(), e);
            Ok(None)
        }
    }
}

/// Whether a process accepts connections on the Unix socket at `path`, as
/// opposed to a socket file left behind by one that stopped.
pub fn in_use(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn bind(path: &Path, group_name: &str) -> Result<(ListenerConfig, AdminSocket), String> {
    // SAFETY: geteuid has no preconditions and can't fail
    let own_uid = unsafe { libc::geteuid() };
    check_parent(path, own_uid)?;
    match fs::symlink_metadata(path) {
        // Left behind by a broker that did not stop cleanly
        Ok(meta) if meta.file_type().is_socket() => {
            fs::remove_file(path).map_err(|e| format!("can't remove the socket left there: {e}"))?
        }
        Ok(_) => return Err("something other than a socket is there".to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let group = admin_group(group_name);

    let listener = bind_owner_only(path).map_err(|e| e.to_string())?;
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    // From here on, an early return removes the socket again
    let socket = AdminSocket {
        path: path.to_path_buf(),
        device: meta.dev(),
        inode: meta.ino(),
    };
    if meta.mode() & 0o077 != 0 {
        return Err("the socket was not created owner-only".to_string());
    }
    // Group access is only opened once the socket belongs to the group
    if let Some(group) = &group {
        match std::os::unix::fs::lchown(path, None, Some(group.gid)) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o660))
                .map_err(|e| e.to_string())?,
            Err(e) => warn!(
                "The admin socket stays owner-only, so `{}` can't open it: {}",
                group_name, e
            ),
        }
    }

    let admitted = Admitted { own_uid, group };
    let mut config = ListenerConfig::new(listener, Transport::Mqtt);
    config.trusted = true;
    config.peer_filter = Some(Arc::new(move |peer: &UCred| {
        admitted.admits(peer.uid(), peer.gid())
    }));
    Ok((config, socket))
}

/// Binds the Unix socket of a `[[listeners]]` entry at `path`, born
/// owner-only and then given `group` and `mode`; `mode` defaults to 0660
/// with a group and 0600 without. A socket left there by a broker that
/// stopped is replaced, one another process serves is refused.
pub fn bind_listener(
    path: &Path,
    mode: Option<u32>,
    group: Option<&str>,
) -> Result<UnixListener, String> {
    let failed = |e: &dyn std::fmt::Display| format!("Failed to listen on {}: {e}", path.display());
    if in_use(path) {
        return Err(failed(&"another process serves this socket"));
    }
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket()) {
        fs::remove_file(path).map_err(|e| failed(&e))?;
    }
    let gid = match group {
        Some(name) => Some(
            lookup_group(name)
                .map_err(|e| failed(&format!("can't read group `{name}`: {e}")))?
                .ok_or_else(|| failed(&format!("group `{name}` doesn't exist")))?
                .gid,
        ),
        None => None,
    };
    let listener = bind_owner_only(path).map_err(|e| failed(&e))?;
    let mode = mode.unwrap_or(if gid.is_some() { 0o660 } else { 0o600 });
    let opened = std::os::unix::fs::lchown(path, None, gid)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(mode)));
    if let Err(e) = opened {
        let _ = fs::remove_file(path);
        return Err(failed(&e));
    }
    Ok(listener)
}

/// Binds a Unix socket at `path` whose file is born owner-only, so no one
/// else can connect before its group and mode are set. Linux creates the
/// file with the socket's own mode minus the umask, so the mode is set on
/// the socket before binding; changing the umask instead would also strip
/// permissions from whatever other threads create meanwhile.
fn bind_owner_only(path: &Path) -> io::Result<UnixListener> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    // SAFETY: the descriptor is open, as `socket` owns it
    if unsafe { libc::fchmod(socket.as_raw_fd(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    socket.bind(&socket2::SockAddr::unix(path)?)?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    UnixListener::from_std(socket.into())
}

/// Refuses a directory where another user could swap the socket between
/// binding it and setting its mode: it must belong to root or the broker's
/// user, and only its owner may write to it.
fn check_parent(path: &Path, own_uid: u32) -> Result<(), String> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let meta = fs::metadata(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    if meta.uid() != 0 && meta.uid() != own_uid {
        return Err(format!("{} belongs to another user", parent.display()));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!("{} is writable by other users", parent.display()));
    }
    Ok(())
}

/// The group named `name`; None when the name is empty, and, with a
/// warning, when the group is unknown or can't be read.
fn admin_group(name: &str) -> Option<Group> {
    if name.is_empty() {
        return None;
    }
    match lookup_group(name) {
        Ok(Some(group)) => Some(group),
        Ok(None) => {
            warn!(
                "Group `{}` doesn't exist; only root and the broker's user may use the admin socket",
                name
            );
            None
        }
        Err(e) => {
            warn!(
                "Can't read group `{}`, so only root and the broker's user may use the admin socket: {}",
                name, e
            );
            None
        }
    }
}

/// Looks a group up by name. Members whose user can't be found are left
/// out.
fn lookup_group(name: &str) -> io::Result<Option<Group>> {
    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut buffer: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: an all-zero `group` is valid; getgrnam_r fills it with
        // pointers into `buffer`, which outlives their use below
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        let code = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if code == libc::ERANGE && buffer.len() < MAX_LOOKUP_BUFFER {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        if found.is_null() {
            return Ok(None);
        }
        let mut names = Vec::new();
        let mut member = entry.gr_mem;
        // SAFETY: gr_mem is null or a null-terminated array of C strings
        while !member.is_null() && unsafe { !(*member).is_null() } {
            names.push(unsafe { CStr::from_ptr(*member) }.to_owned());
            member = unsafe { member.add(1) };
        }
        let members = names
            .iter()
            .filter_map(|name| lookup_user(name).ok().flatten())
            .collect();
        return Ok(Some(Group {
            gid: entry.gr_gid,
            members,
        }));
    }
}

/// The user id of the user named `name`.
fn lookup_user(name: &CStr) -> io::Result<Option<u32>> {
    let mut buffer: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: as in lookup_group
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let code = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if code == libc::ERANGE && buffer.len() < MAX_LOOKUP_BUFFER {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        return Ok((!found.is_null()).then_some(entry.pw_uid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    /// A fresh directory only this user may write to, for one test.
    fn private_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mercuriod-admin-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn root_the_brokers_user_and_the_group_are_admitted() {
        let admitted = Admitted {
            own_uid: 1000,
            group: Some(Group {
                gid: 500,
                members: vec![1001],
            }),
        };
        assert!(admitted.admits(0, 0));
        assert!(admitted.admits(1000, 1000));
        assert!(admitted.admits(1001, 1001), "a listed member");
        assert!(admitted.admits(1002, 500), "the group is its primary group");
        assert!(!admitted.admits(1002, 1002));
        let alone = Admitted {
            own_uid: 1000,
            group: None,
        };
        assert!(alone.admits(0, 0));
        assert!(!alone.admits(1001, 500));
    }

    #[test]
    fn groups_and_users_are_looked_up_by_name() {
        assert_eq!(lookup_group("root").unwrap().map(|g| g.gid), Some(0));
        assert_eq!(lookup_group("mercurio-no-such-group").unwrap(), None);
        assert_eq!(lookup_user(c"root").unwrap(), Some(0));
        assert_eq!(lookup_user(c"mercurio-no-such-user").unwrap(), None);
    }

    #[tokio::test]
    async fn the_socket_is_private_and_admits_the_brokers_user() {
        let dir = private_dir("mode");
        let path = dir.join("admin.sock");
        let (config, socket) = bind(&path, "").unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert!(config.trusted);

        // Both ends belong to this process, so the client sees the same
        // credentials the broker would
        let client = UnixStream::connect(&path).await.unwrap();
        let peer = client.peer_cred().unwrap();
        let filter = config.peer_filter.as_ref().expect("a peer filter");
        assert!(filter(&peer));

        drop(socket);
        assert!(!path.exists(), "removed when the broker stops");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn own_group_name() -> Option<String> {
        let mut buffer: Vec<libc::c_char> = vec![0; 4096];
        // SAFETY: an all-zero `group` is valid; the name is copied out
        // before `buffer` goes away
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        let code = unsafe {
            libc::getgrgid_r(
                libc::getgid(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if code != 0 || found.is_null() {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(entry.gr_name) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    #[tokio::test]
    async fn the_socket_opens_to_the_group_once_it_belongs_to_it() {
        let Some(name) = own_group_name() else {
            return;
        };
        let dir = private_dir("group");
        let path = dir.join("admin.sock");
        let (_config, _socket) = bind(&path, &name).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o660);
        assert_eq!(meta.gid(), unsafe { libc::getgid() });
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn the_socket_is_born_owner_only() {
        let dir = private_dir("born");
        let path = dir.join("admin.sock");
        let _listener = bind_owner_only(&path).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o077, 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_and_a_newer_one_kept() {
        let dir = private_dir("stale");
        let path = dir.join("admin.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let (_config, first) = bind(&path, "").unwrap();

        // Another broker's socket now in its place outlives this one
        fs::remove_file(&path).unwrap();
        let _newer = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(first);
        assert!(path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_socket_another_process_serves_is_left_alone() {
        let dir = private_dir("live");
        let path = dir.join("admin.sock");
        let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let config = AdminConfig {
            socket: path.to_str().unwrap().to_string(),
            group: String::new(),
        };
        let Err(error) = open(&config) else {
            panic!("took over a live socket");
        };
        assert!(error.contains(&config.socket), "{error}");
        let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        live.set_nonblocking(true).unwrap();
        assert!(live.accept().is_ok(), "the first process still serves");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn listener_sockets_get_their_mode_and_group() {
        let dir = private_dir("listener");
        let path = dir.join("mqtt.sock");
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().mode() & 0o777;

        let listener = bind_listener(&path, None, None).unwrap();
        assert_eq!(mode(&path), 0o600);
        // A second broker would take the socket from the first
        assert!(bind_listener(&path, None, None).is_err());
        drop(listener);
        let _listener = bind_listener(&path, Some(0o666), None).unwrap();
        assert_eq!(mode(&path), 0o666);
        let _client = UnixStream::connect(&path).await.unwrap();

        if let Some(name) = own_group_name() {
            let grouped = dir.join("grouped.sock");
            let _listener = bind_listener(&grouped, None, Some(&name)).unwrap();
            let meta = fs::symlink_metadata(&grouped).unwrap();
            assert_eq!(meta.mode() & 0o777, 0o660);
            assert_eq!(meta.gid(), unsafe { libc::getgid() });
        }
        let unknown = dir.join("unknown.sock");
        let error = bind_listener(&unknown, None, Some("mercurio-no-such-group")).unwrap_err();
        assert!(error.contains("mercurio-no-such-group"), "{error}");
        assert!(!unknown.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn unsafe_or_missing_places_are_refused() {
        let dir = private_dir("refused");
        let file = dir.join("file");
        fs::write(&file, b"keep").unwrap();
        assert!(bind(&file, "").is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep");

        let shared = dir.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(bind(&shared.join("admin.sock"), "").is_err());
        assert!(!shared.join("admin.sock").exists());

        assert!(bind(&dir.join("missing/admin.sock"), "").is_err());
        let off = AdminConfig {
            socket: String::new(),
            group: String::new(),
        };
        assert!(open(&off).unwrap().is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
