//! Files the daemon rereads on SIGHUP.

use std::sync::{Arc, RwLock};

use mercurio_server::acl::{Access, Authorizer, ClientIdentity};
use mercurio_server::auth::CredentialValidator;

type Loader<T> = fn(&str) -> Result<T, String>;

/// A value loaded from a file that can be reloaded while the broker runs.
/// A failed reload keeps the previous value, so a typo in the file does not
/// lock everyone out.
pub struct Reloadable<T> {
    path: String,
    load: Loader<T>,
    current: RwLock<Arc<T>>,
}

impl<T> Reloadable<T> {
    pub fn load(path: &str, load: Loader<T>) -> Result<Self, String> {
        Ok(Self {
            path: path.to_string(),
            load,
            current: RwLock::new(Arc::new(load(path)?)),
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

impl<T: Authorizer> Authorizer for Reloadable<T> {
    fn authorize(&self, client: &ClientIdentity, topic: &str, access: Access) -> bool {
        self.get().authorize(client, topic, access)
    }
}

impl<T: CredentialValidator> CredentialValidator for Reloadable<T> {
    fn validate(&self, username: &str, password: &[u8]) -> bool {
        self.get().validate(username, password)
    }
}

#[cfg(test)]
mod tests {
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
}
