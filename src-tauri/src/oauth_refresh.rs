//! Generation-safe OAuth refresh coordination.
//!
//! Network I/O runs while holding only a per-account refresh lease. GUI vault
//! reads and compare-and-store writes happen in short critical sections owned
//! by the [`GenerationStore`] adapter.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::oauth::{
    self, is_oauth_token_expired_at, OAuthNetwork, RefreshError, UsageError, UsageFetchError,
    UsageResult,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    pub number: String,
    pub email: String,
    pub stable_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGeneration {
    pub credentials: String,
    pub generation: String,
}

impl StoredGeneration {
    pub fn new(credentials: String) -> Self {
        let generation = credential_generation(&credentials);
        Self {
            credentials,
            generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCredential {
    pub identity: AccountIdentity,
    pub credentials: String,
    pub generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareAndStore {
    Persisted(StoredGeneration),
    AlreadyCurrent(StoredGeneration),
    Superseded(StoredGeneration),
    Missing,
}

pub fn credential_generation(credentials: &str) -> String {
    format!("sha256-full:{:x}", Sha256::digest(credentials.as_bytes()))
}

pub trait GenerationStore: Send + Sync {
    fn read(&self, identity: &AccountIdentity) -> Result<Option<StoredGeneration>, String>;

    fn compare_and_store(
        &self,
        identity: &AccountIdentity,
        expected_generation: &str,
        successor: &str,
    ) -> Result<CompareAndStore, String>;

    fn is_rejected(&self, identity: &AccountIdentity, credentials: &str) -> Result<bool, String>;

    /// Atomically re-read and quarantine only when `expected_generation` is
    /// still current. Returns false when another writer already won.
    fn reject_if_current(
        &self,
        identity: &AccountIdentity,
        expected_generation: &str,
        credentials: &str,
    ) -> Result<bool, String>;
}

pub trait LeaseGuard: Send {}
impl<T: Send> LeaseGuard for T {}

pub trait RefreshLeaseProvider: Send + Sync {
    fn acquire<'a>(
        &'a self,
        stable_key: &'a str,
    ) -> oauth::OAuthFuture<'a, Result<Box<dyn LeaseGuard>, String>>;
}

#[derive(Debug, Clone)]
pub struct FileRefreshLeases {
    root: PathBuf,
    timeout: Duration,
}

impl FileRefreshLeases {
    pub fn new(backup_root: PathBuf, timeout: Duration) -> Self {
        Self {
            root: backup_root.join("oauth-refresh-locks"),
            timeout,
        }
    }
}

impl RefreshLeaseProvider for FileRefreshLeases {
    fn acquire<'a>(
        &'a self,
        stable_key: &'a str,
    ) -> oauth::OAuthFuture<'a, Result<Box<dyn LeaseGuard>, String>> {
        let lock_name = format!("{:x}.lock", Sha256::digest(stable_key.as_bytes()));
        let path = self.root.join(lock_name);
        let timeout = self.timeout;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::locking::acquire_or_err(path, timeout)
                    .map(|guard| Box::new(guard) as Box<dyn LeaseGuard>)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| format!("refresh lease task failed: {error}"))?
        })
    }
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> f64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> f64 {
        chrono::Utc::now().timestamp_millis() as f64
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RefreshCoordinatorError {
    #[error("account has no stored credential")]
    Missing,
    #[error("account requires re-login")]
    ReloginRequired,
    #[error("OAuth refresh could not be completed: {0:?}")]
    RefreshFailed(Option<RefreshError>),
    #[error("refreshed credentials could not be persisted: {0}")]
    PersistenceFailed(String),
    #[error("refresh lease could not be acquired: {0}")]
    Lease(String),
    #[error("usage fetch failed: {0:?}")]
    Usage(UsageError),
    #[error("stored OAuth credential is malformed")]
    InvalidCredential,
}

pub struct RefreshCoordinator {
    network: Arc<dyn OAuthNetwork>,
    store: Arc<dyn GenerationStore>,
    leases: Arc<dyn RefreshLeaseProvider>,
    clock: Arc<dyn Clock>,
}

impl RefreshCoordinator {
    pub fn new(
        network: Arc<dyn OAuthNetwork>,
        store: Arc<dyn GenerationStore>,
        leases: Arc<dyn RefreshLeaseProvider>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            network,
            store,
            leases,
            clock,
        }
    }

    pub async fn freshen_for_activation(
        &self,
        identity: &AccountIdentity,
    ) -> Result<ValidatedCredential, RefreshCoordinatorError> {
        let stored = self.freshen(identity, false).await?;
        Ok(ValidatedCredential {
            identity: identity.clone(),
            credentials: stored.credentials,
            generation: stored.generation,
        })
    }

    pub async fn fetch_inactive_usage(
        &self,
        identity: &AccountIdentity,
    ) -> Result<UsageResult, RefreshCoordinatorError> {
        let first = self.freshen(identity, false).await?;
        let token = access_token(&first.credentials)?;
        match self.network.fetch_usage(&token).await {
            Ok(data) => normalize_usage(data),
            Err(UsageFetchError::Http { status: 401, .. }) => {
                let refreshed = self.freshen(identity, true).await?;
                let token = access_token(&refreshed.credentials)?;
                let data = self
                    .network
                    .fetch_usage(&token)
                    .await
                    .map_err(|error| RefreshCoordinatorError::Usage(classify_usage(error)))?;
                normalize_usage(data)
            }
            Err(error) => Err(RefreshCoordinatorError::Usage(classify_usage(error))),
        }
    }

    async fn freshen(
        &self,
        identity: &AccountIdentity,
        force_refresh: bool,
    ) -> Result<StoredGeneration, RefreshCoordinatorError> {
        let _lease = self
            .leases
            .acquire(&identity.stable_key)
            .await
            .map_err(RefreshCoordinatorError::Lease)?;

        for _ in 0..3 {
            let current = self
                .store
                .read(identity)
                .map_err(RefreshCoordinatorError::PersistenceFailed)?
                .ok_or(RefreshCoordinatorError::Missing)?;
            if self
                .store
                .is_rejected(identity, &current.credentials)
                .map_err(RefreshCoordinatorError::PersistenceFailed)?
            {
                return Err(RefreshCoordinatorError::ReloginRequired);
            }
            if !force_refresh && !credential_is_expired(&current.credentials, self.clock.now_ms())?
            {
                return Ok(current);
            }

            let outcome = self.network.refresh(&current.credentials).await;
            let Some(successor) = outcome.credentials else {
                if outcome.error == Some(RefreshError::InvalidGrant) {
                    let rejected = self
                        .store
                        .reject_if_current(identity, &current.generation, &current.credentials)
                        .map_err(RefreshCoordinatorError::PersistenceFailed)?;
                    if rejected {
                        return Err(RefreshCoordinatorError::ReloginRequired);
                    }
                    continue;
                }
                return Err(RefreshCoordinatorError::RefreshFailed(outcome.error));
            };

            match self
                .store
                .compare_and_store(identity, &current.generation, &successor)
                .map_err(RefreshCoordinatorError::PersistenceFailed)?
            {
                CompareAndStore::Persisted(stored)
                | CompareAndStore::AlreadyCurrent(stored)
                | CompareAndStore::Superseded(stored) => return Ok(stored),
                CompareAndStore::Missing => return Err(RefreshCoordinatorError::Missing),
            }
        }
        Err(RefreshCoordinatorError::PersistenceFailed(
            "credential generation changed repeatedly".to_string(),
        ))
    }
}

fn credential_is_expired(credentials: &str, now_ms: f64) -> Result<bool, RefreshCoordinatorError> {
    let oauth =
        oauth::extract_oauth_data(credentials).ok_or(RefreshCoordinatorError::InvalidCredential)?;
    let expires_at = oauth.get("expiresAt").and_then(serde_json::Value::as_f64);
    Ok(is_oauth_token_expired_at(expires_at, now_ms))
}

fn access_token(credentials: &str) -> Result<String, RefreshCoordinatorError> {
    oauth::extract_access_token(credentials).ok_or(RefreshCoordinatorError::InvalidCredential)
}

fn normalize_usage(data: serde_json::Value) -> Result<UsageResult, RefreshCoordinatorError> {
    oauth::normalize_usage_response(&data)
        .map_err(|error| RefreshCoordinatorError::Usage(UsageError::Other(error.to_string())))?
        .ok_or(RefreshCoordinatorError::Usage(UsageError::BadResponse))
}

fn classify_usage(error: UsageFetchError) -> UsageError {
    match error {
        UsageFetchError::Http { status, .. } => UsageError::Http(status),
        UsageFetchError::Timeout => UsageError::Timeout,
        UsageFetchError::Network => UsageError::Network,
        UsageFetchError::BadResponse => UsageError::BadResponse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{OAuthFuture, RefreshOutcome};
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn credentials(access: &str, refresh: &str, expires_at: f64) -> String {
        json!({"claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at
        }})
        .to_string()
    }

    fn identity() -> AccountIdentity {
        AccountIdentity {
            number: "2".to_string(),
            email: "two@example.com".to_string(),
            stable_key: "org:two".to_string(),
        }
    }

    #[derive(Default)]
    struct FakeStore {
        current: Mutex<Option<StoredGeneration>>,
        rejected_generation: Mutex<Option<String>>,
        fail_write: Mutex<bool>,
        supersede_before_cas: Mutex<Option<String>>,
    }

    impl FakeStore {
        fn with(credentials: String) -> Self {
            Self {
                current: Mutex::new(Some(StoredGeneration::new(credentials))),
                ..Self::default()
            }
        }
    }

    impl GenerationStore for FakeStore {
        fn read(&self, _: &AccountIdentity) -> Result<Option<StoredGeneration>, String> {
            Ok(self.current.lock().unwrap().clone())
        }

        fn compare_and_store(
            &self,
            _: &AccountIdentity,
            expected_generation: &str,
            successor: &str,
        ) -> Result<CompareAndStore, String> {
            if *self.fail_write.lock().unwrap() {
                return Err("disk full".to_string());
            }
            let mut current = self.current.lock().unwrap();
            if let Some(winner) = self.supersede_before_cas.lock().unwrap().take() {
                *current = Some(StoredGeneration::new(winner));
            }
            let Some(existing) = current.clone() else {
                return Ok(CompareAndStore::Missing);
            };
            let successor = StoredGeneration::new(successor.to_string());
            if existing.generation == successor.generation {
                return Ok(CompareAndStore::AlreadyCurrent(existing));
            }
            if existing.generation != expected_generation {
                return Ok(CompareAndStore::Superseded(existing));
            }
            *current = Some(successor.clone());
            Ok(CompareAndStore::Persisted(successor))
        }

        fn is_rejected(&self, _: &AccountIdentity, credentials: &str) -> Result<bool, String> {
            Ok(self.rejected_generation.lock().unwrap().as_deref()
                == Some(credential_generation(credentials).as_str()))
        }

        fn reject_if_current(
            &self,
            _: &AccountIdentity,
            expected_generation: &str,
            _: &str,
        ) -> Result<bool, String> {
            let current = self.current.lock().unwrap();
            let matches = current
                .as_ref()
                .is_some_and(|stored| stored.generation == expected_generation);
            if matches {
                *self.rejected_generation.lock().unwrap() = Some(expected_generation.to_string());
            }
            Ok(matches)
        }
    }

    struct ScriptedNetwork {
        refreshes: Mutex<VecDeque<RefreshOutcome>>,
        usages: Mutex<VecDeque<Result<Value, UsageFetchError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl OAuthNetwork for ScriptedNetwork {
        fn refresh<'a>(&'a self, credentials: &'a str) -> OAuthFuture<'a, RefreshOutcome> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("refresh:{}", credential_generation(credentials)));
                self.refreshes.lock().unwrap().pop_front().unwrap()
            })
        }

        fn fetch_usage<'a>(
            &'a self,
            access_token: &'a str,
        ) -> OAuthFuture<'a, Result<Value, UsageFetchError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("usage:{access_token}"));
                self.usages.lock().unwrap().pop_front().unwrap()
            })
        }
    }

    #[derive(Default)]
    struct NoopLease;
    struct Guard;
    impl RefreshLeaseProvider for NoopLease {
        fn acquire<'a>(
            &'a self,
            _: &'a str,
        ) -> OAuthFuture<'a, Result<Box<dyn LeaseGuard>, String>> {
            Box::pin(async { Ok(Box::new(Guard) as Box<dyn LeaseGuard>) })
        }
    }

    struct FixedClock(f64);
    impl Clock for FixedClock {
        fn now_ms(&self) -> f64 {
            self.0
        }
    }

    struct FailedLease;
    impl RefreshLeaseProvider for FailedLease {
        fn acquire<'a>(
            &'a self,
            _: &'a str,
        ) -> OAuthFuture<'a, Result<Box<dyn LeaseGuard>, String>> {
            Box::pin(async { Err("busy".to_string()) })
        }
    }

    fn coordinator(store: Arc<FakeStore>, network: Arc<ScriptedNetwork>) -> RefreshCoordinator {
        RefreshCoordinator::new(
            network,
            store,
            Arc::new(NoopLease),
            Arc::new(FixedClock(10_000.0)),
        )
    }

    #[tokio::test]
    async fn rotation_is_persisted_before_healthy_usage_is_returned() {
        let old = credentials("a-old", "r-old", 1.0);
        let new = credentials("a-new", "r-new", 9_999_999.0);
        let store = Arc::new(FakeStore::with(old));
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(new.clone()),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::from([Ok(
                json!({"five_hour":{"utilization":15.0}}),
            )])),
            calls: Mutex::new(Vec::new()),
        });

        let usage = coordinator(Arc::clone(&store), network)
            .fetch_inactive_usage(&identity())
            .await
            .unwrap();
        assert_eq!(usage.five_hour.unwrap().pct, 15.0);
        assert_eq!(store.read(&identity()).unwrap().unwrap().credentials, new);
    }

    #[tokio::test]
    async fn persistence_failure_is_not_reported_as_healthy_usage() {
        let old = credentials("a-old", "r-old", 1.0);
        let new = credentials("a-new", "r-new", 9_999_999.0);
        let store = Arc::new(FakeStore::with(old));
        *store.fail_write.lock().unwrap() = true;
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(new),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });

        let error = coordinator(store, network)
            .fetch_inactive_usage(&identity())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RefreshCoordinatorError::PersistenceFailed("disk full".to_string())
        );
    }

    #[tokio::test]
    async fn invalid_grant_quarantines_only_the_still_current_generation() {
        let old = credentials("a-old", "r-old", 1.0);
        let store = Arc::new(FakeStore::with(old));
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::InvalidGrant),
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });

        assert_eq!(
            coordinator(Arc::clone(&store), network)
                .freshen_for_activation(&identity())
                .await
                .unwrap_err(),
            RefreshCoordinatorError::ReloginRequired
        );
        let current = store.read(&identity()).unwrap().unwrap();
        assert!(store
            .is_rejected(&identity(), &current.credentials)
            .unwrap());
    }

    #[tokio::test]
    async fn usage_401_refreshes_persists_and_retries_once() {
        let old = credentials("a-old", "r-old", 9_999_999.0);
        let new = credentials("a-new", "r-new", 9_999_999.0);
        let store = Arc::new(FakeStore::with(old));
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(new.clone()),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::from([
                Err(UsageFetchError::Http {
                    status: 401,
                    retry_after_s: None,
                }),
                Ok(json!({"seven_day":{"utilization":20.0}})),
            ])),
            calls: Mutex::new(Vec::new()),
        });

        let usage = coordinator(Arc::clone(&store), network)
            .fetch_inactive_usage(&identity())
            .await
            .unwrap();
        assert_eq!(usage.seven_day.unwrap().pct, 20.0);
        assert_eq!(store.read(&identity()).unwrap().unwrap().credentials, new);
    }

    #[tokio::test]
    async fn competing_rotation_wins_and_stale_successor_never_overwrites_it() {
        let old = credentials("a-old", "r-old", 1.0);
        let stale_successor = credentials("a-stale", "r-stale", 9_999_999.0);
        let winner = credentials("a-winner", "r-winner", 9_999_999.0);
        let store = Arc::new(FakeStore::with(old));
        *store.supersede_before_cas.lock().unwrap() = Some(winner.clone());
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: Some(stale_successor),
                error: None,
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });

        let validated = coordinator(Arc::clone(&store), network)
            .freshen_for_activation(&identity())
            .await
            .unwrap();
        assert_eq!(validated.credentials, winner);
        assert_eq!(
            store.read(&identity()).unwrap().unwrap().credentials,
            validated.credentials
        );
    }

    #[tokio::test]
    async fn missing_or_moved_slot_never_becomes_a_successful_validation() {
        let store = Arc::new(FakeStore::default());
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });
        assert_eq!(
            coordinator(store, network)
                .freshen_for_activation(&identity())
                .await
                .unwrap_err(),
            RefreshCoordinatorError::Missing
        );
    }

    #[tokio::test]
    async fn local_expiry_is_refresh_needed_not_quarantine_evidence() {
        let old = credentials("a-old", "r-old", 1.0);
        let store = Arc::new(FakeStore::with(old));
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::from([RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::Transient),
                token_account: None,
            }])),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });
        assert_eq!(
            coordinator(Arc::clone(&store), network)
                .freshen_for_activation(&identity())
                .await
                .unwrap_err(),
            RefreshCoordinatorError::RefreshFailed(Some(RefreshError::Transient))
        );
        let current = store.read(&identity()).unwrap().unwrap();
        assert!(!store
            .is_rejected(&identity(), &current.credentials)
            .unwrap());
    }

    #[tokio::test]
    async fn lease_failure_stops_before_read_or_network() {
        let store = Arc::new(FakeStore::with(credentials("a", "r", 1.0)));
        let network = Arc::new(ScriptedNetwork {
            refreshes: Mutex::new(VecDeque::new()),
            usages: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });
        let coordinator = RefreshCoordinator::new(
            network,
            store,
            Arc::new(FailedLease),
            Arc::new(FixedClock(10_000.0)),
        );
        assert_eq!(
            coordinator
                .freshen_for_activation(&identity())
                .await
                .unwrap_err(),
            RefreshCoordinatorError::Lease("busy".to_string())
        );
    }
}
