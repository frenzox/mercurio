//! Bans: the clients the broker refuses, by client id, user name or
//! address. The bans in force are kept in memory, so checking a CONNECT
//! never waits for storage; a change reaches storage before it takes effect.

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use mercurio_core::message::unix_millis;
use mercurio_storage::{BanStore, StoredBan};
use tokio::{sync::Mutex, time::Instant};
use tracing::{info, warn};

use crate::{
    admin::{storage_failed, AdminError, Ban, BanKey, BanKind, NewBan},
    server::{address_key, AddressRange},
};

pub(crate) fn unix_secs() -> u64 {
    unix_millis() / 1000
}

/// A client as bans see it.
pub(crate) struct Candidate<'a> {
    pub client_id: &'a str,
    /// Only a user the broker verified
    pub username: Option<&'a str>,
    pub ip: Option<IpAddr>,
}

/// A ban in force.
#[derive(Clone)]
pub(crate) struct ActiveBan {
    pub ban: Ban,
    /// What an address ban's value names
    range: Option<AddressRange>,
}

impl ActiveBan {
    pub(crate) fn matches(&self, client: &Candidate<'_>) -> bool {
        let name = match self.ban.kind {
            BanKind::ClientId => client.client_id,
            BanKind::Username => match client.username {
                Some(username) => username,
                None => return false,
            },
            BanKind::Address => {
                return self
                    .range
                    .is_some_and(|range| client.ip.is_some_and(|ip| range.contains(ip)))
            }
        };
        if self.ban.prefix {
            name.starts_with(self.ban.value.as_str())
        } else {
            name == self.ban.value
        }
    }

    /// Whether the ban is over at `now`, in Unix seconds. It holds through
    /// the second it expires in, so it never lasts less than its duration.
    fn expired(&self, now: u64) -> bool {
        self.ban.expires_at.is_some_and(|at| at < now)
    }

    fn has_key(&self, kind: BanKind, value: &str, prefix: bool) -> bool {
        self.ban.kind == kind && self.ban.value == value && self.ban.prefix == prefix
    }
}

/// Checks `key`, and spells its value as bans are kept: an address range
/// by its first address and length.
fn checked(key: &BanKey) -> Result<(String, Option<AddressRange>), AdminError> {
    if key.value.is_empty() {
        return Err(AdminError::invalid_argument("`value` can't be empty"));
    }
    if key.kind != BanKind::Address {
        return Ok((key.value.clone(), None));
    }
    if key.prefix {
        return Err(AdminError::invalid_argument(
            "`prefix` is for client ids and user names; ban a CIDR block instead",
        ));
    }
    let range: AddressRange = key.value.parse().map_err(AdminError::invalid_argument)?;
    Ok((range.to_string(), Some(range)))
}

fn from_stored(stored: &StoredBan) -> Option<ActiveBan> {
    let kind = BanKind::from_name(&stored.kind)?;
    let mut key = BanKey::new(kind, stored.value.clone());
    key.prefix = stored.prefix;
    let (value, range) = checked(&key).ok()?;
    Some(ActiveBan {
        ban: Ban {
            kind,
            value,
            prefix: stored.prefix,
            reason: stored.reason.clone(),
            created_at: stored.created_at,
            expires_at: stored.expires_at,
        },
        range,
    })
}

fn to_stored(ban: &Ban) -> StoredBan {
    StoredBan {
        kind: ban.kind.as_str().to_string(),
        value: ban.value.clone(),
        prefix: ban.prefix,
        reason: ban.reason.clone(),
        created_at: ban.created_at,
        expires_at: ban.expires_at,
    }
}

pub(crate) struct Bans {
    active: RwLock<Vec<ActiveBan>>,
    /// Held while a change goes to storage and then to `active`, so both
    /// take changes in the same order
    changes: Mutex<()>,
    /// None when the broker's storage keeps no bans
    store: Option<Arc<dyn BanStore>>,
}

impl Bans {
    /// The bans `store` holds. Those that expired while the broker was down
    /// are deleted; those this broker can't read are left alone.
    pub(crate) async fn load(store: Option<Arc<dyn BanStore>>) -> mercurio_storage::Result<Bans> {
        let mut active = Vec::new();
        if store.is_none() {
            warn!("This storage keeps no bans: they won't survive a restart");
        }
        if let Some(store) = &store {
            let now = unix_secs();
            for stored in store.load_bans().await? {
                let Some(ban) = from_stored(&stored) else {
                    warn!(
                        "Ignoring the stored {} ban on `{}`, which this broker can't read",
                        stored.kind, stored.value
                    );
                    continue;
                };
                if !ban.expired(now) {
                    // Bans are kept in their canonical form, so a row in
                    // another one could never be removed
                    if ban.ban.value != stored.value {
                        let rewritten = to_stored(&ban.ban);
                        if let Err(e) = store.store_ban(&rewritten).await {
                            warn!("Failed to rewrite a ban in its canonical form: {}", e);
                        } else if let Err(e) = store
                            .delete_ban(&stored.kind, &stored.value, stored.prefix)
                            .await
                        {
                            warn!("Failed to delete a ban's old form: {}", e);
                        }
                    }
                    active.push(ban);
                } else if let Err(e) = store
                    .delete_ban(&stored.kind, &stored.value, stored.prefix)
                    .await
                {
                    warn!("Failed to delete an expired ban: {}", e);
                }
            }
        }
        Ok(Bans {
            active: RwLock::new(active),
            changes: Mutex::new(()),
            store,
        })
    }

    /// Whether a ban in force matches `client`.
    pub(crate) fn refuses(&self, client: &Candidate<'_>) -> bool {
        let now = unix_secs();
        self.active
            .read()
            .unwrap()
            .iter()
            .any(|ban| !ban.expired(now) && ban.matches(client))
    }

    /// Whether an address ban in force matches `ip`, for refusing a
    /// connection before anything else is known of it.
    pub(crate) fn refuses_address(&self, ip: IpAddr) -> bool {
        let now = unix_secs();
        self.active
            .read()
            .unwrap()
            .iter()
            .any(|ban| !ban.expired(now) && ban.range.is_some_and(|range| range.contains(ip)))
    }

    /// The bans in force, by type, then value, then prefix.
    pub(crate) fn list(&self) -> Vec<Ban> {
        let now = unix_secs();
        let mut bans: Vec<Ban> = self
            .active
            .read()
            .unwrap()
            .iter()
            .filter(|ban| !ban.expired(now))
            .map(|ban| ban.ban.clone())
            .collect();
        bans.sort_by(|a, b| (a.kind, &a.value, a.prefix).cmp(&(b.kind, &b.value, b.prefix)));
        bans
    }

    fn created_at(&self, kind: BanKind, value: &str, prefix: bool, now: u64) -> Option<u64> {
        self.active
            .read()
            .unwrap()
            .iter()
            .find(|ban| !ban.expired(now) && ban.has_key(kind, value, prefix))
            .map(|ban| ban.ban.created_at)
    }

    /// Puts `new` in force, replacing the ban with the same type, value and
    /// prefix, whose creation time it keeps. Its duration counts from now.
    ///
    /// If storage commits the ban but reports an error, such as a
    /// PostgreSQL connection lost after the commit, the ban isn't in force
    /// until the next restart loads it.
    pub(crate) async fn add(&self, new: NewBan) -> Result<ActiveBan, AdminError> {
        let (value, range) = checked(&new.key)?;
        if new.duration == Some(0) {
            return Err(AdminError::invalid_argument(
                "`duration` is at least 1 second",
            ));
        }
        let NewBan {
            key,
            duration,
            reason,
        } = new;
        let _changes = self.changes.lock().await;
        let now = unix_secs();
        let ban = ActiveBan {
            ban: Ban {
                kind: key.kind,
                created_at: self
                    .created_at(key.kind, &value, key.prefix, now)
                    .unwrap_or(now),
                value,
                prefix: key.prefix,
                reason,
                expires_at: duration.map(|d| now + u64::from(d)),
            },
            range,
        };
        if let Some(store) = &self.store {
            store
                .store_ban(&to_stored(&ban.ban))
                .await
                .map_err(storage_failed)?;
        }
        let mut active = self.active.write().unwrap();
        active.retain(|b| !b.has_key(ban.ban.kind, &ban.ban.value, ban.ban.prefix));
        active.push(ban.clone());
        Ok(ban)
    }

    /// Lifts the ban `key` names; `notFound` if none is in force.
    pub(crate) async fn remove(&self, key: &BanKey) -> Result<(), AdminError> {
        let (value, _) = checked(key)?;
        let _changes = self.changes.lock().await;
        if self
            .created_at(key.kind, &value, key.prefix, unix_secs())
            .is_none()
        {
            return Err(AdminError::not_found(format!(
                "no {} ban on `{}`{}",
                key.kind.as_str(),
                value,
                if key.prefix { " as a prefix" } else { "" }
            )));
        }
        if let Some(store) = &self.store {
            store
                .delete_ban(key.kind.as_str(), &value, key.prefix)
                .await
                .map_err(storage_failed)?;
        }
        self.active
            .write()
            .unwrap()
            .retain(|ban| !ban.has_key(key.kind, &value, key.prefix));
        Ok(())
    }

    /// Drops the bans that expired, from storage too. Matching already
    /// ignores them; this keeps them from piling up. One whose deletion
    /// fails stays until a later sweep deletes it.
    pub(crate) async fn sweep(&self) {
        let expired = |now| -> Vec<Ban> {
            self.active
                .read()
                .unwrap()
                .iter()
                .filter(|ban| ban.expired(now))
                .map(|ban| ban.ban.clone())
                .collect()
        };
        if expired(unix_secs()).is_empty() {
            return;
        }
        // The snapshot is taken under the lock: an `add` that replaced a ban
        // in between would otherwise have its new row deleted
        let _changes = self.changes.lock().await;
        let now = unix_secs();
        let expired = expired(now);
        let mut gone = Vec::with_capacity(expired.len());
        for ban in expired {
            if let Some(store) = &self.store {
                if let Err(e) = store
                    .delete_ban(ban.kind.as_str(), &ban.value, ban.prefix)
                    .await
                {
                    warn!(
                        "Failed to delete the expired {} ban on `{}`{}: {}",
                        ban.kind.as_str(),
                        ban.value,
                        if ban.prefix { " as a prefix" } else { "" },
                        e
                    );
                    continue;
                }
            }
            info!(
                "The ban on the {} `{}` expired",
                ban.kind.as_str(),
                ban.value
            );
            gone.push(ban);
        }
        self.active.write().unwrap().retain(|active| {
            !gone
                .iter()
                .any(|ban| active.has_key(ban.kind, &ban.value, ban.prefix) && active.expired(now))
        });
    }
}

/// When failed logins get an address banned for a while.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoginFailures {
    /// Failed logins from one address within `window` that get it banned
    pub attempts: u32,
    pub window: Duration,
    /// How long the ban lasts; whole seconds, at least one
    pub ban: Duration,
}

impl LoginFailures {
    pub fn new(attempts: u32, window: Duration, ban: Duration) -> Self {
        LoginFailures {
            attempts,
            window,
            ban,
        }
    }
}

/// Addresses whose failed logins are tracked at once. An attacker cycling
/// through addresses can't grow the table past it; while it is full of
/// recent failures, new addresses go untracked.
const MAX_TRACKED: usize = 16_384;

/// The recent failed logins of each client address, counted as the
/// per-address connection limit counts them: IPv6 by /64, IPv4-mapped
/// addresses as IPv4.
pub(crate) struct FailedLogins {
    limits: LoginFailures,
    failures: std::sync::Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl FailedLogins {
    pub(crate) fn new(limits: LoginFailures) -> Self {
        FailedLogins {
            limits,
            failures: Default::default(),
        }
    }

    /// Counts a failed login from `ip`. Once its address has failed
    /// `attempts` times within the window, bans it through `bans` for the
    /// ban's duration and starts counting again. Loopback addresses are
    /// never banned.
    pub(crate) async fn failed(&self, ip: IpAddr, bans: &Bans) {
        let key = address_key(ip);
        if ip.to_canonical().is_loopback() || self.limits.attempts == 0 {
            return;
        }
        if !self.count(key) {
            return;
        }
        let range = match key {
            IpAddr::V4(_) => key.to_string(),
            IpAddr::V6(_) => format!("{key}/64"),
        };
        let seconds = self.limits.ban.as_secs().clamp(1, u64::from(u32::MAX));
        let reason = format!(
            "automatic: {} failed logins within {} s",
            self.limits.attempts,
            self.limits.window.as_secs()
        );
        let mut ban = NewBan::new(BanKey::new(BanKind::Address, range.clone()));
        ban.duration = Some(seconds as u32);
        ban.reason = Some(reason);
        match bans.add(ban).await {
            Ok(_) => warn!(
                "Banned `{}` for {} s after {} failed logins",
                range, seconds, self.limits.attempts
            ),
            Err(e) => warn!("Failed to ban `{}` after failed logins: {}", range, e),
        }
    }

    /// Records a failure of `key` now; true when it reaches the limit.
    fn count(&self, key: IpAddr) -> bool {
        let now = Instant::now();
        let window = self.limits.window;
        let recent = |at: &Instant| now.saturating_duration_since(*at) < window;
        let mut failures = self.failures.lock().unwrap();
        if !failures.contains_key(&key) && failures.len() >= MAX_TRACKED {
            failures.retain(|_, times| times.back().is_some_and(recent));
            if failures.len() >= MAX_TRACKED {
                return false;
            }
        }
        let times = failures.entry(key).or_default();
        while times.front().is_some_and(|at| !recent(at)) {
            times.pop_front();
        }
        times.push_back(now);
        if times.len() < self.limits.attempts as usize {
            return false;
        }
        failures.remove(&key);
        true
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.failures.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use mercurio_storage::memory::MemoryStore;

    use super::*;
    use crate::admin::ErrorCode;

    fn active(kind: BanKind, value: &str, prefix: bool) -> ActiveBan {
        let mut key = BanKey::new(kind, value);
        key.prefix = prefix;
        let (value, range) = checked(&key).unwrap();
        ActiveBan {
            ban: Ban {
                kind,
                value,
                prefix,
                reason: None,
                created_at: 0,
                expires_at: None,
            },
            range,
        }
    }

    fn throttle(attempts: u32) -> FailedLogins {
        FailedLogins::new(LoginFailures::new(
            attempts,
            Duration::from_secs(60),
            Duration::from_secs(300),
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn test_failed_logins_within_the_window_ban_the_address() {
        let bans = Bans::load(None).await.unwrap();
        let logins = throttle(3);
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        logins.failed(ip, &bans).await;
        logins.failed(ip, &bans).await;
        // The first failure leaves the window before the third comes
        tokio::time::advance(Duration::from_secs(61)).await;
        logins.failed(ip, &bans).await;
        assert!(!bans.refuses_address(ip));
        logins.failed(ip, &bans).await;
        assert!(!bans.refuses_address(ip));
        logins.failed(ip, &bans).await;
        assert!(bans.refuses_address(ip));

        let listed = bans.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].value, "192.0.2.7");
        assert!(listed[0].reason.as_deref().unwrap().starts_with("automatic"));
        let expires_in = listed[0].expires_at.unwrap() - unix_secs();
        assert!((299..=300).contains(&expires_in), "{expires_in}");
        assert_eq!(logins.tracked(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_failed_logins_count_by_block_and_spare_loopback() {
        let bans = Bans::load(None).await.unwrap();
        let logins = throttle(2);
        logins.failed("2001:db8:0:1::1".parse().unwrap(), &bans).await;
        logins.failed("2001:db8:0:1::2".parse().unwrap(), &bans).await;
        assert!(bans.refuses_address("2001:db8:0:1:ffff::9".parse().unwrap()));
        assert!(!bans.refuses_address("2001:db8:0:2::1".parse().unwrap()));
        assert_eq!(bans.list()[0].value, "2001:db8:0:1::/64");

        for ip in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            let ip: IpAddr = ip.parse().unwrap();
            logins.failed(ip, &bans).await;
            logins.failed(ip, &bans).await;
            assert!(!bans.refuses_address(ip));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_failed_logins_track_a_bounded_number_of_addresses() {
        let bans = Bans::load(None).await.unwrap();
        let logins = throttle(2);
        let nth = |n: usize| IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + n as u32));
        for n in 0..MAX_TRACKED + 10 {
            logins.failed(nth(n), &bans).await;
        }
        assert_eq!(logins.tracked(), MAX_TRACKED);
        // Once their failures leave the window, new addresses take their place
        tokio::time::advance(Duration::from_secs(61)).await;
        logins.failed(nth(MAX_TRACKED + 20), &bans).await;
        assert_eq!(logins.tracked(), 1);
        logins.failed(nth(MAX_TRACKED + 20), &bans).await;
        assert!(bans.refuses_address(nth(MAX_TRACKED + 20)));
    }

    fn client<'a>(
        client_id: &'a str,
        username: Option<&'a str>,
        ip: Option<&str>,
    ) -> Candidate<'a> {
        Candidate {
            client_id,
            username,
            ip: ip.map(|ip| ip.parse().unwrap()),
        }
    }

    #[test]
    fn test_client_ids_and_user_names_match_exactly_or_by_prefix() {
        let exact = active(BanKind::ClientId, "dev", false);
        assert!(exact.matches(&client("dev", None, None)));
        assert!(!exact.matches(&client("device", None, None)));
        let prefix = active(BanKind::ClientId, "dev", true);
        assert!(prefix.matches(&client("device", None, None)));
        assert!(!prefix.matches(&client("de", None, None)));

        let user = active(BanKind::Username, "guest", true);
        assert!(user.matches(&client("c", Some("guest-1"), None)));
        // Client ids don't stand in for user names
        assert!(!user.matches(&client("guest-1", None, None)));
    }

    #[test]
    fn test_addresses_match_by_range() {
        let range = active(BanKind::Address, "10.1.2.3/8", false);
        assert_eq!(range.ban.value, "10.0.0.0/8");
        assert!(range.matches(&client("c", None, Some("10.200.0.1"))));
        assert!(range.matches(&client("c", None, Some("::ffff:10.0.0.1"))));
        assert!(!range.matches(&client("c", None, Some("11.0.0.1"))));
        // In-process and Unix socket clients have no address
        assert!(!range.matches(&client("c", None, None)));

        let single = active(BanKind::Address, "2001:db8::1", false);
        assert!(single.matches(&client("c", None, Some("2001:db8::1"))));
        assert!(!single.matches(&client("c", None, Some("2001:db8::2"))));
    }

    #[test]
    fn test_bad_keys_are_invalid_arguments() {
        let mut prefixed = BanKey::new(BanKind::Address, "10.0.0.0/8");
        prefixed.prefix = true;
        for key in [
            prefixed,
            BanKey::new(BanKind::Address, "10.0.0.0/33"),
            BanKey::new(BanKind::ClientId, ""),
        ] {
            assert_eq!(
                checked(&key).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{key:?}"
            );
        }
    }

    #[test]
    fn test_a_ban_holds_through_the_second_it_expires_in() {
        let mut ban = active(BanKind::ClientId, "c", false);
        ban.ban.expires_at = Some(100);
        assert!(!ban.expired(100));
        assert!(ban.expired(101));
    }

    #[tokio::test]
    async fn test_loading_drops_expired_bans_and_skips_unreadable_ones() {
        let store = Arc::new(MemoryStore::new());
        let stored = |kind: &str, value: &str, expires_at| StoredBan {
            kind: kind.into(),
            value: value.into(),
            prefix: false,
            reason: None,
            created_at: 1,
            expires_at,
        };
        store
            .store_ban(&stored("clientid", "kept", None))
            .await
            .unwrap();
        store
            .store_ban(&stored("clientid", "expired", Some(2)))
            .await
            .unwrap();
        store
            .store_ban(&stored("country", "nl", None))
            .await
            .unwrap();

        let bans = Bans::load(Some(store.clone())).await.unwrap();
        let listed: Vec<String> = bans.list().into_iter().map(|ban| ban.value).collect();
        assert_eq!(listed, ["kept"]);
        let mut left: Vec<String> = store
            .load_bans()
            .await
            .unwrap()
            .into_iter()
            .map(|ban| ban.value)
            .collect();
        left.sort();
        assert_eq!(left, ["kept", "nl"]);
    }

    #[tokio::test]
    async fn test_loading_rewrites_addresses_in_their_canonical_form() {
        let store = Arc::new(MemoryStore::new());
        store
            .store_ban(&StoredBan {
                kind: "address".into(),
                value: "10.1.2.3/8".into(),
                prefix: false,
                reason: None,
                created_at: 1,
                expires_at: None,
            })
            .await
            .unwrap();

        let bans = Bans::load(Some(store.clone())).await.unwrap();
        let key = BanKey::new(BanKind::Address, "10.1.2.3/8");
        bans.remove(&key).await.unwrap();
        assert!(store.load_bans().await.unwrap().is_empty());
    }

    struct FlakyStore {
        inner: MemoryStore,
        failures: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl BanStore for FlakyStore {
        async fn load_bans(&self) -> mercurio_storage::Result<Vec<StoredBan>> {
            self.inner.load_bans().await
        }

        async fn store_ban(&self, ban: &StoredBan) -> mercurio_storage::Result<()> {
            self.inner.store_ban(ban).await
        }

        async fn delete_ban(
            &self,
            kind: &str,
            value: &str,
            prefix: bool,
        ) -> mercurio_storage::Result<()> {
            use std::sync::atomic::Ordering::SeqCst;
            if self.failures.load(SeqCst) > 0 {
                self.failures.fetch_sub(1, SeqCst);
                return Err(mercurio_storage::StorageError::OperationFailed(
                    "connection lost".into(),
                ));
            }
            self.inner.delete_ban(kind, value, prefix).await
        }
    }

    #[tokio::test]
    async fn test_sweep_retries_a_failed_deletion() {
        let store = Arc::new(FlakyStore {
            inner: MemoryStore::new(),
            failures: std::sync::atomic::AtomicUsize::new(1),
        });
        let bans = Bans::load(Some(store.clone())).await.unwrap();
        let mut new = NewBan::new(BanKey::new(BanKind::ClientId, "gone"));
        new.duration = Some(1);
        bans.add(new).await.unwrap();
        // Expired, as far as the sweep can tell
        bans.active.write().unwrap()[0].ban.expires_at = Some(1);

        bans.sweep().await;
        assert_eq!(store.load_bans().await.unwrap().len(), 1);
        assert_eq!(bans.active.read().unwrap().len(), 1);

        bans.sweep().await;
        assert!(store.load_bans().await.unwrap().is_empty());
        assert!(bans.active.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_sweep_keeps_a_ban_added_again_while_it_waits() {
        let store = Arc::new(MemoryStore::new());
        let bans = Arc::new(Bans::load(Some(store.clone())).await.unwrap());
        let key = BanKey::new(BanKind::ClientId, "again");
        let mut new = NewBan::new(key.clone());
        new.duration = Some(1);
        bans.add(new).await.unwrap();
        bans.active.write().unwrap()[0].ban.expires_at = Some(1);

        // Hold the lock as an `add` of the same ban would, so the sweep
        // sees the expired ban and then waits
        let changes = bans.changes.lock().await;
        let sweep = tokio::spawn({
            let bans = Arc::clone(&bans);
            async move { bans.sweep().await }
        });
        tokio::task::yield_now().await;
        let (value, range) = checked(&key).unwrap();
        let fresh = ActiveBan {
            ban: Ban {
                kind: key.kind,
                value,
                prefix: false,
                reason: None,
                created_at: unix_secs(),
                expires_at: None,
            },
            range,
        };
        store.store_ban(&to_stored(&fresh.ban)).await.unwrap();
        *bans.active.write().unwrap() = vec![fresh];
        drop(changes);
        sweep.await.unwrap();

        assert_eq!(store.load_bans().await.unwrap().len(), 1);
        assert_eq!(bans.list().len(), 1);
    }
}
