//! Reading and writing the `username:hash` password file.

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Load a password file with format: `username:hash` per line.
/// Lines starting with `#` and empty lines are ignored.
pub fn load(path: &str) -> Result<HashMap<String, String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read password file `{}`: {}", path, e))?;
    parse(path, &content, true)
}

/// The file's users; with `check_hashes`, a hash no password can match,
/// such as a plain-text password, is an error naming its line.
fn parse(path: &str, content: &str, check_hashes: bool) -> Result<HashMap<String, String>, String> {
    let mut credentials = HashMap::new();
    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((username, password_hash)) = line.split_once(':') else {
            return Err(format!(
                "Invalid format in password file `{}` at line {}: expected `username:hash`",
                path,
                line_num + 1
            ));
        };
        if check_hashes {
            crate::credentials::check_hash(password_hash).map_err(|e| {
                format!(
                    "Invalid hash in password file `{}` at line {}: {}",
                    path,
                    line_num + 1,
                    e
                )
            })?;
        }
        credentials.insert(username.to_string(), password_hash.to_string());
    }
    Ok(credentials)
}

/// The users of the password file at `path`, for `mercuriod passwd` to
/// change; none when the file doesn't exist. Any other failure is an error,
/// so a file that can't be read is never rewritten with one user.
pub fn read_users(path: &str) -> Result<BTreeMap<String, String>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("Failed to read password file `{path}`: {e}")),
    };
    // A bad entry may be the one `passwd` is about to fix
    Ok(parse(path, &content, false)?.into_iter().collect())
}

/// Refuses user names the password file cannot hold as they are.
pub fn check_username(username: &str) -> Result<(), String> {
    if username.is_empty() {
        return Err("User name cannot be empty".to_string());
    }
    if username.chars().any(char::is_control) {
        return Err(format!(
            "User name {username:?} cannot contain control characters, such as a newline"
        ));
    }
    if username.contains(':') {
        return Err(format!(
            "User name `{username}` cannot contain `:`, which ends the name in the password file"
        ));
    }
    if username.starts_with('#') {
        return Err(format!(
            "User name `{username}` cannot start with `#`, which marks a comment in the password file"
        ));
    }
    if username.starts_with(char::is_whitespace) || username.ends_with(char::is_whitespace) {
        return Err(format!(
            "User name {username:?} cannot start or end with a space, which the password file drops"
        ));
    }
    Ok(())
}

/// Replaces the password file at `path` with `users`. The new file is
/// written beside it and renamed over it, so the broker never reads half a
/// file; it keeps the old file's owner and mode, at most 0640, or gets 0600.
pub fn write_users(path: &str, users: &BTreeMap<String, String>) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("Failed to write password file `{path}`: {e}");
    let existing = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(failed(e)),
    };
    // Through a symbolic link, replace the file it points to
    let target = match existing {
        Some(_) => std::fs::canonicalize(path).map_err(failed)?,
        None => PathBuf::from(path),
    };
    let name = target
        .file_name()
        .ok_or_else(|| format!("`{path}` does not name a file"))?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));

    let mut content = String::from(
        "# Mercurio password file\n# Managed by `mercuriod passwd`. Do not edit manually.\n",
    );
    for (username, hash) in users {
        content.push_str(&format!("{username}:{hash}\n"));
    }
    let result = replace(&target, dir, &temp, existing.as_ref(), content.as_bytes());
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(failed)
}

fn replace(
    target: &Path,
    dir: &Path,
    temp: &Path,
    existing: Option<&std::fs::Metadata>,
    content: &[u8],
) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(temp)?;
    #[cfg(unix)]
    if let Some(existing) = existing {
        keep_owner_and_mode(&file, existing)?;
    }
    #[cfg(not(unix))]
    let _ = existing;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temp, target)?;
    // Makes the rename itself survive a crash
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Gives `file` the owner, group and mode of `existing`, the mode without
/// write access for the group or any access for others. A user who may not
/// give the file away keeps what they can: root rewriting a `root:mercurio`
/// file keeps the group the broker reads it through, and anyone else keeps
/// the group if they belong to it.
#[cfg(unix)]
fn keep_owner_and_mode(file: &std::fs::File, existing: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::{fchown, MetadataExt, PermissionsExt};

    let permitted = |result: std::io::Result<()>| match result {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Ok(false),
        result => result.map(|()| true),
    };
    if !permitted(fchown(file, Some(existing.uid()), Some(existing.gid())))? {
        permitted(fchown(file, None, Some(existing.gid())))?;
    }
    file.set_permissions(std::fs::Permissions::from_mode(existing.mode() & 0o640))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("mercuriod-passwd-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<_> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn users(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(u, h)| (u.to_string(), h.to_string()))
            .collect()
    }

    #[test]
    fn test_hashes_load_and_anything_else_names_its_line() {
        let dir = TempDir::new("hashes");
        let good = dir.file("good");
        std::fs::write(
            &good,
            "# comment\n\
             argon:$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$NyLLmsFKZ9oYOtcaEe8gdiULhOKcU8bYpD4UrZ6/NXo\n\
             pbkdf2:$7$101$lT8RRynk8Dr5uL1b$1FhG/H49+FnZ9qx8YFDOUZOiE+Yd+yOKjQ9AzfsTHwepsiXOTbzCSojC7kA3U/VicSJdksyipSowQpvFhpo0iQ==\n\
             sha512:$6$pZufTypp2a6RmJv0$0E3Uw01KVS5L6/s+8dWtsoMjdF9mnjUcloZJLTWqsIQbvp1b3crPcunceFO+VmiVpj+Wde8NRnyJs4ZAFgaQUA==\n",
        )
        .unwrap();
        assert_eq!(load(&good).unwrap().len(), 3);

        for (line, hash) in [
            (2, ""),
            (2, "secret"),
            (2, "$7$many$c2FsdA==$aGFzaA=="),
            (2, "$6$not base64!$aGFzaA=="),
            (2, "$6$c2FsdA=="),
            (2, "$argon2id$garbage"),
            (2, "$pbkdf2-sha256$i=1000$c2FsdA$aGFzaA"),
        ] {
            let path = dir.file("bad");
            std::fs::write(&path, format!("ok:$6$c2FsdA==$aGFzaA==\nalice:{hash}\n")).unwrap();
            let error = load(&path).unwrap_err();
            assert!(error.contains(&format!("line {line}")), "{hash:?}: {error}");
            assert!(
                !error.contains(hash) || hash.is_empty(),
                "{hash:?} shown: {error}"
            );
        }
    }

    #[test]
    fn test_passwd_still_reads_a_file_with_a_bad_hash() {
        let dir = TempDir::new("passwd-bad");
        let path = dir.file("passwd");
        std::fs::write(&path, "alice:\nbob:$6$c2FsdA==$aGFzaA==\n").unwrap();
        assert_eq!(read_users(&path).unwrap().len(), 2);
    }

    #[test]
    fn test_a_missing_file_has_no_users() {
        let dir = TempDir::new("missing");
        assert!(read_users(&dir.file("passwd")).unwrap().is_empty());
    }

    #[test]
    fn test_an_unreadable_file_is_an_error_and_left_alone() {
        let dir = TempDir::new("unreadable");
        let malformed = dir.file("malformed");
        std::fs::write(&malformed, "alice:$argon2id$x\nbob\n").unwrap();
        let error = read_users(&malformed).unwrap_err();
        assert!(error.contains("line 2"), "{error}");

        let binary = dir.file("binary");
        std::fs::write(&binary, b"alice:\xff\xfe\n").unwrap();
        assert!(read_users(&binary).is_err());

        // A directory fails to read with an error other than NotFound
        let directory = dir.file("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(read_users(&directory).is_err());

        assert_eq!(
            std::fs::read(&malformed).unwrap(),
            b"alice:$argon2id$x\nbob\n"
        );
    }

    #[test]
    fn test_users_survive_a_rewrite() {
        let dir = TempDir::new("rewrite");
        let path = dir.file("passwd");
        write_users(&path, &users(&[("alice", "$a"), ("bob", "$b")])).unwrap();
        let mut read = read_users(&path).unwrap();
        assert_eq!(read, users(&[("alice", "$a"), ("bob", "$b")]));
        read.remove("alice");
        write_users(&path, &read).unwrap();
        assert_eq!(read_users(&path).unwrap(), users(&[("bob", "$b")]));
        assert_eq!(dir.entries(), ["passwd"]);
    }

    #[test]
    fn test_a_new_file_is_private() {
        let dir = TempDir::new("new");
        let path = dir.file("passwd");
        write_users(&path, &users(&[("alice", "$a")])).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_a_rewrite_keeps_the_mode_up_to_group_read_and_the_owner() {
        for (before, after) in [
            (0o600, 0o600),
            (0o640, 0o640),
            (0o644, 0o640),
            (0o666, 0o640),
        ] {
            let dir = TempDir::new(&format!("mode-{before:o}"));
            let path = dir.file("passwd");
            std::fs::write(&path, "alice:$a\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(before)).unwrap();
            let owner = std::fs::metadata(&path).unwrap();

            write_users(&path, &users(&[("alice", "$a"), ("bob", "$b")])).unwrap();
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, after, "{before:o}");
            assert_eq!((metadata.uid(), metadata.gid()), (owner.uid(), owner.gid()));
            assert_ne!(metadata.ino(), owner.ino(), "replaced by a rename");
        }
    }

    #[test]
    fn test_user_names_the_file_cannot_hold_are_refused() {
        for bad in [
            "", "a:b", "a\nb", "a\rb", "a\tb", "a\0b", "a\u{7f}", "#a", " a", "a ",
        ] {
            let error = check_username(bad).unwrap_err();
            assert!(!error.is_empty(), "{bad:?}");
        }
        for good in ["alice", "a.b-c_d@example.com", "ünïcode", "a b"] {
            assert!(check_username(good).is_ok(), "{good:?}");
        }
    }
}
