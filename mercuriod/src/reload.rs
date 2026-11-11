//! Files the daemon rereads on SIGHUP and on the `reload` command.

use std::sync::{Arc, OnceLock, RwLock};

use mercurio_server::plugin::{
    Access, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginStatus,
};
use serde::Serialize;
use tracing::{error, info};

type Loader<T> = fn(&str) -> Result<T, String>;

/// A value loaded from a file that can be reloaded while the broker runs.
/// A failed reload keeps the previous value, so a typo in the file does not
/// lock everyone out.
pub struct Reloadable<T> {
    path: String,
    load: Loader<T>,
    current: RwLock<Arc<T>>,
    /// The plugin name of the value, which is the same for every version
    name: OnceLock<String>,
}

impl<T> Reloadable<T> {
    pub fn load(path: &str, load: Loader<T>) -> Result<Self, String> {
        Ok(Self {
            path: path.to_string(),
            load,
            current: RwLock::new(Arc::new(load(path)?)),
            name: OnceLock::new(),
        })
    }

    pub fn reload(&self) -> Result<(), String> {
        let value = (self.load)(&self.path)?;
        *self.current.write().unwrap() = Arc::new(value);
        Ok(())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    // Clone the Arc so slow checks, like Argon2 hashing, do not hold the lock
    fn get(&self) -> Arc<T> {
        Arc::clone(&self.current.read().unwrap())
    }
}

/// Any reloadable file, whatever it holds.
pub trait Reload: Send + Sync {
    fn reload(&self) -> Result<(), String>;
    fn path(&self) -> &str;
    /// The name of the plugin the file configures
    fn item(&self) -> &str;
}

impl<T: Plugin> Reload for Reloadable<T> {
    fn reload(&self) -> Result<(), String> {
        Reloadable::reload(self)
    }

    fn path(&self) -> &str {
        Reloadable::path(self)
    }

    fn item(&self) -> &str {
        Plugin::name(self)
    }
}

/// How reloading one file went: an entry of the `reload` command's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reloaded {
    pub item: String,
    pub path: String,
    pub ok: bool,
    /// Why the previous contents were kept
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Reloads each file in turn and logs how it went: what SIGHUP and the
/// `reload` command do.
pub fn reload_all(files: &[Arc<dyn Reload>]) -> Vec<Reloaded> {
    // Runs overlap when SIGHUP arrives during a `reload` command
    static RELOADING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _running = RELOADING.lock().unwrap_or_else(|e| e.into_inner());
    files
        .iter()
        .map(|file| {
            let failure = file.reload().err();
            match &failure {
                None => info!("Reloaded {}", file.path()),
                Some(e) => error!("Keeping the previous {}: {}", file.path(), e),
            }
            Reloaded {
                item: file.item().to_string(),
                path: file.path().to_string(),
                ok: failure.is_none(),
                error: failure,
            }
        })
        .collect()
}

impl<T: Plugin> Plugin for Reloadable<T> {
    fn name(&self) -> &str {
        self.name.get_or_init(|| self.get().name().to_string())
    }

    fn status(&self) -> PluginStatus {
        self.get().status()
    }

    fn authenticate<'a>(
        &'a self,
        client: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let current = self.get();
        Box::pin(async move { current.authenticate(client, credentials).await })
    }

    fn authorize<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        let current = self.get();
        Box::pin(async move { current.authorize(client, topic, access).await })
    }
}

#[cfg(test)]
mod tests {
    use mercurio_server::{acl::AclFile, plugin::Plugin};

    use super::Reloadable;

    fn load(path: &str) -> Result<String, String> {
        let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        if contents.contains("bad") {
            return Err("bad contents".to_string());
        }
        Ok(contents)
    }

    #[test]
    fn test_reload_swaps_value_and_keeps_it_on_error() {
        let path = std::env::temp_dir().join(format!("mercuriod-reload-{}", std::process::id()));
        let path_str = path.to_str().unwrap();

        std::fs::write(&path, "one").unwrap();
        let value = Reloadable::load(path_str, load).unwrap();
        assert_eq!(*value.get(), "one");

        std::fs::write(&path, "two").unwrap();
        value.reload().unwrap();
        assert_eq!(*value.get(), "two");

        std::fs::write(&path, "bad").unwrap();
        assert!(value.reload().is_err());
        assert_eq!(*value.get(), "two");

        std::fs::remove_file(&path).unwrap();
        assert!(value.reload().is_err());
        assert_eq!(*value.get(), "two");
    }

    #[test]
    fn test_plugins_keep_their_own_name() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-reload-acl-{}", std::process::id()));
        std::fs::write(&path, "topic read public/#").unwrap();
        let acl = Reloadable::load(path.to_str().unwrap(), |path| {
            AclFile::parse(&std::fs::read_to_string(path).unwrap())
        })
        .unwrap();
        assert_eq!(Plugin::name(&acl), "acl-file");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_plugins_report_the_status_of_what_they_hold() {
        use mercurio_server::plugin::{PluginKind, PluginStatus};

        let path =
            std::env::temp_dir().join(format!("mercuriod-reload-status-{}", std::process::id()));
        std::fs::write(&path, "topic read public/#").unwrap();
        let acl = Reloadable::load(path.to_str().unwrap(), |path| {
            AclFile::parse(&std::fs::read_to_string(path).unwrap())
        })
        .unwrap();
        assert_eq!(
            Plugin::status(&acl),
            PluginStatus::new("acl-file", PluginKind::Builtin).with_hooks(&["authorize"])
        );
        std::fs::remove_file(&path).unwrap();
    }
}
