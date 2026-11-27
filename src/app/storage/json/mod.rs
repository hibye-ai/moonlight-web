use std::{
    collections::HashMap,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use anyhow::anyhow;
use async_trait::async_trait;
use futures::future::join_all;
use openssl::rand::rand_bytes;
use tokio::{
    fs,
    io::AsyncWriteExt,
    spawn,
    sync::{
        Mutex, OwnedRwLockWriteGuard, RwLock,
        mpsc::{self, Receiver, Sender, error::TrySendError},
        oneshot,
    },
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error, warn};

use crate::app::{
    AppError,
    auth::SessionToken,
    host::HostId,
    password::StoragePassword,
    role::RoleId,
    storage::{
        CredentialPin, CredentialVersion, Either, IdentityEpoch, Storage, StorageHost,
        StorageHostAdd, StorageHostCache, StorageHostModify, StorageHostPairInfo,
        StorageQueryHosts, StorageRole, StorageRoleAdd, StorageRoleDefaultSettings,
        StorageRoleModify, StorageRolePermissions, StorageSessionInfo, StorageUser, StorageUserAdd,
        StorageUserModify,
        json::versions::{
            Json, V2, V2Host, V2HostCache, V2HostPairInfo, V2UserPassword, V3, V3Role,
            V3RolePermissions, V3RoleType, V3User, migrate_to_latest,
        },
    },
    user::{RoleType, UserId},
};

mod mutations;
mod serde_helpers;
mod versions;

use mutations::{Mutation, MutationOutcome, RoleDeletionReservation, RoleDeletionReservations};

pub struct JsonStorage {
    file: PathBuf,
    store_sender: Sender<StoreRequest>,
    session_expiration_checker: JoinHandle<()>,
    // IMPORTANT: only lock those mutexes in descending order to prevent deadlocks
    users: RwLock<HashMap<u32, RwLock<V3User>>>,
    hosts: RwLock<HashMap<u32, RwLock<V2Host>>>,
    roles: RwLock<HashMap<u32, RwLock<V3Role>>>,
    sessions: RwLock<HashMap<SessionToken, Session>>,
    store_lock: Mutex<()>,
    /// Role deletions that were admitted but have not committed yet (AR-03).
    ///
    /// Leaf lock: only a set insert/contains is performed while holding it, never
    /// across an await, and never while acquiring a table lock.
    role_deletion_reservations: Arc<RoleDeletionReservations>,
    /// Advances whenever the identity snapshot is replaced by a restore (MW-19).
    identity_epoch: AtomicU64,
    /// High-water mark for account credential generations (AR-01).
    ///
    /// Only ever moves forward, is persisted with the snapshot, and is re-derived
    /// on load as `max(persisted, highest stored version + 1)`. Generations handed
    /// out from it are therefore never repeated by any restore, restart, repeated
    /// restore or account replacement.
    next_credential_version: AtomicU64,
    /// Monotonic account-id allocator (AR-01).
    ///
    /// A reinstated id would let a credential pin taken from a deleted account be
    /// replayed against its replacement, so ids are allocated from a persisted
    /// counter and the counter is raised above every id a restore installs.
    next_user_id: AtomicU64,
}

/// The writer owns accepted operations even if their callers stop awaiting them.
enum StoreRequest {
    /// `ack` is present when the caller needs the actual disk outcome (MW-17).
    Persist {
        ack: Option<oneshot::Sender<Result<(), String>>>,
    },
    RemoveRole {
        role_id: RoleId,
        ack: oneshot::Sender<Result<(), AppError>>,
        /// Keeps the role un-referenceable until the deletion has committed.
        _reservation: RoleDeletionReservation,
    },
    /// A full identity/host mutation whose commit is owned by the writer
    /// (AR-03, AR-05). The reservation guard, if any, travels with it.
    Mutation {
        mutation: Mutation,
        ack: oneshot::Sender<Result<MutationOutcome, AppError>>,
    },
    /// A restore whose ownership — candidate, exclusion guard and the obligation to
    /// publish — was handed to the writer (AR-02).
    Restore {
        data: V3,
        guard: Option<OwnedRwLockWriteGuard<()>>,
        ack: oneshot::Sender<Result<(), AppError>>,
    },
}

impl Drop for JsonStorage {
    fn drop(&mut self) {
        self.session_expiration_checker.abort();
    }
}

struct Session {
    created_at: Instant,
    expiration: Duration,
    user_id: u32,
    /// Credential generation the session was issued under (AR-01).
    credential_version: u64,
    /// Identity-snapshot epoch the session was issued in (AR-01).
    identity_epoch: IdentityEpoch,
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// The storage file holds password hashes and pairing private keys, so every
/// file this module creates (target, `.tmp` staging, `.bak` copy) is owner-only.
/// The mode is applied at creation time so the temp file is never briefly
/// world-readable before the rename.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

fn private_temp_options() -> tokio::fs::OpenOptions {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // tokio's OpenOptions exposes `mode` directly on unix.
    #[cfg(unix)]
    options.mode(PRIVATE_FILE_MODE);
    options
}

/// Removes a leftover `.tmp` from an interrupted write, then creates a fresh
/// private file. `create_new` is retried once after cleaning a stale sibling.
async fn create_private_temp_file(temp_path: &Path) -> Result<fs::File, std::io::Error> {
    match private_temp_options().open(temp_path).await {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            warn!(
                "Removing stale JSON storage temp file {}",
                temp_path.display()
            );
            fs::remove_file(temp_path).await?;
            private_temp_options().open(temp_path).await
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
async fn restrict_to_owner(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE)).await
}

#[cfg(not(unix))]
async fn restrict_to_owner(_path: &Path) -> Result<(), std::io::Error> {
    // Windows inherits the private ACL of the containing directory.
    Ok(())
}

async fn atomic_replace_file(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    let temp_path = path_with_suffix(path, ".tmp");
    let backup_path = path_with_suffix(path, ".bak");

    // Write and flush the new contents before making them visible at the final path.
    {
        let mut temp = create_private_temp_file(&temp_path).await?;
        temp.write_all(contents.as_bytes()).await?;
        temp.flush().await?;
        temp.sync_all().await?;
    }

    match fs::metadata(path).await {
        Ok(_) => {
            if let Err(err) = fs::copy(path, &backup_path).await {
                warn!(
                    "Failed to update JSON storage backup {}: {err}",
                    backup_path.display()
                );
            } else {
                // fs::copy keeps the source mode, which may predate this fix.
                if let Err(err) = restrict_to_owner(&backup_path).await {
                    warn!(
                        "Failed to restrict JSON storage backup {}: {err}",
                        backup_path.display()
                    );
                }
                if let Ok(backup) = fs::File::open(&backup_path).await
                    && let Err(err) = backup.sync_all().await
                {
                    warn!(
                        "Failed to sync JSON storage backup {}: {err}",
                        backup_path.display()
                    );
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    // Atomic on the same filesystem. The temp file is deliberately a sibling of the target.
    fs::rename(&temp_path, path).await?;

    // Persist the rename itself where directory fsync is supported (Linux container runtime).
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Ok(directory) = fs::File::open(parent).await
        && let Err(err) = directory.sync_all().await
    {
        warn!(
            "Failed to sync JSON storage directory {}: {err}",
            parent.display()
        );
    }

    Ok(())
}

/// Commits a complete mutation candidate: stage the new bytes and the previous
/// primary as a private `.*.tmp` sibling, publish the primary, then publish the
/// backup. A failed stage leaves both the primary and the backup untouched, and a
/// failed primary rename restores neither half-published state. Used by the role
/// cascade and by the transactional mutations (AR-03, AR-05).
async fn atomic_replace_staged_file(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    let temp_path = path_with_suffix(path, ".tmp");
    let backup_path = path_with_suffix(path, ".bak");
    let backup_temp_path = path_with_suffix(path, ".bak.tmp");
    let mut temp = create_private_temp_file(&temp_path).await?;
    temp.write_all(contents.as_bytes()).await?;
    temp.flush().await?;
    temp.sync_all().await?;
    drop(temp);

    let backup_ready = async {
        let previous = fs::read(path).await?;
        let mut backup = create_private_temp_file(&backup_temp_path).await?;
        backup.write_all(&previous).await?;
        backup.flush().await?;
        backup.sync_all().await
    }
    .await;
    if let Err(err) = &backup_ready {
        warn!(
            "Failed to stage JSON storage backup {}: {err}",
            backup_path.display()
        );
    }
    if let Err(err) = fs::rename(&temp_path, path).await {
        let _ = fs::remove_file(&temp_path).await;
        let _ = fs::remove_file(&backup_temp_path).await;
        return Err(err);
    }
    // From this point the transaction has committed; backup/directory maintenance
    // is best-effort, just as in atomic_replace_file, never a rejected deletion.
    if backup_ready.is_ok() {
        if let Err(err) = fs::rename(&backup_temp_path, &backup_path).await {
            warn!(
                "Failed to publish JSON storage backup {}: {err}",
                backup_path.display()
            );
        }
    } else {
        let _ = fs::remove_file(&backup_temp_path).await;
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Ok(directory) = fs::File::open(parent).await
        && let Err(err) = directory.sync_all().await
    {
        warn!(
            "Failed to sync JSON storage directory {}: {err}",
            parent.display()
        );
    }
    Ok(())
}

impl JsonStorage {
    pub async fn load(
        file: PathBuf,
        session_expiration_check_interval: Duration,
    ) -> Result<Arc<Self>, anyhow::Error> {
        let (store_sender, store_receiver) = mpsc::channel(1);

        let (this_sender, this_receiver) = oneshot::channel::<Arc<Self>>();

        let session_expiration_checker = spawn(async move {
            let this = match this_receiver.await {
                Ok(value) => value,
                Err(err) => {
                    error!(
                        "Failed to initialize session expiration checker: {err:?}. All sessions will last forever!"
                    );
                    return;
                }
            };

            loop {
                sleep(session_expiration_check_interval).await;
                debug!("Clearing all expired sessions!");

                let mut sessions = this.sessions.write().await;

                let now = Instant::now();
                sessions.retain(|_, session| {
                    let current_session_length = now - session.created_at;

                    current_session_length < session.expiration
                });
            }
        });

        let this = Self {
            file,
            store_sender,
            session_expiration_checker,
            hosts: Default::default(),
            users: Default::default(),
            roles: Default::default(),
            sessions: Default::default(),
            store_lock: Default::default(),
            role_deletion_reservations: Default::default(),
            identity_epoch: AtomicU64::new(0),
            next_credential_version: AtomicU64::new(0),
            next_user_id: AtomicU64::new(0),
        };
        let this = Arc::new(this);

        if this_sender.send(this.clone()).is_err() {
            error!(
                "Failed to send values to session expiration checker. All sessions will last forever!"
            );
        }

        this.load_internal().await?;

        spawn({
            let this = this.clone();

            async move { file_writer(store_receiver, this).await }
        });

        Ok(this)
    }

    pub fn force_write(&self) {
        if let Err(TrySendError::Closed(_)) = self
            .store_sender
            .try_send(StoreRequest::Persist { ack: None })
        {
            error!("Failed to save data because the writer task closed!");
        }
    }

    /// Persists the current state and waits until the writer reports the outcome.
    ///
    /// MW-17: security/identity changes use this so the caller does not answer
    /// "success" for a mutation that never reached disk.
    async fn flush(&self) -> Result<(), AppError> {
        let (ack_sender, ack_receiver) = oneshot::channel();

        if self
            .store_sender
            .send(StoreRequest::Persist {
                ack: Some(ack_sender),
            })
            .await
            .is_err()
        {
            error!("Failed to save data because the writer task closed!");
            return Err(AppError::StorageUnavailable);
        }

        match ack_receiver.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                error!("Failed to persist JSON storage: {err}");
                Err(AppError::StorageUnavailable)
            }
            Err(_) => {
                error!("JSON storage writer dropped the flush acknowledgement");
                Err(AppError::StorageUnavailable)
            }
        }
    }

    /// Raises a persisted high-water mark so it covers `at_least`.
    ///
    /// Used when a snapshot is installed (load, restore): the counters only ever
    /// move forward, so a file written by an older build, or a backup that predates
    /// the current state, cannot lower them.
    fn raise_counter(counter: &AtomicU64, at_least: u64) {
        counter.fetch_max(at_least, AtomicOrdering::AcqRel);
    }

    /// Keeps the generation high-water mark above a generation that is now stored.
    ///
    /// Superseded by `mutations::reserve_staged_credential_version`: identity
    /// creation now runs inside the writer transaction and reserves the generation in
    /// the candidate snapshot, so nothing allocates or reserves on the live atomics
    /// before the durable commit. Kept here would be misleading, and a second path
    /// that raises the mark outside `store_lock` is exactly what the reconciliation
    /// removed.
    ///
    /// Creates a session, optionally requiring that the credential generation and
    /// the identity epoch the caller verified are still current (AR-01).
    ///
    /// Both checks and the insert happen while the `users` read guard is held, which
    /// is the same lock the restore takes for publication. A rotation, a restore or a
    /// deletion therefore cannot interleave between the check and the insert.
    async fn create_session_token_inner(
        &self,
        user_id: UserId,
        expiration: Duration,
        expected: Option<CredentialVersion>,
        expected_epoch: Option<IdentityEpoch>,
    ) -> Result<SessionToken, AppError> {
        let users = self.users.read().await;
        let current = match users.get(&user_id.0) {
            Some(user) => user.read().await.credential_version,
            // A deleted account must not be able to mint a session, even from a
            // snapshot that was loaded before the deletion.
            None => return Err(AppError::SessionTokenNotFound),
        };
        if expected.is_some_and(|expected| expected.0 != current) {
            return Err(AppError::CredentialsWrong);
        }
        // AR-01: a credential decision taken in an older identity incarnation is not
        // usable in this one, even if the generation happens to match.
        let epoch = self.identity_epoch.load(AtomicOrdering::Acquire);
        if expected_epoch.is_some_and(|expected| expected != epoch) {
            return Err(AppError::CredentialsWrong);
        }

        let mut token;
        {
            let sessions = self.sessions.read().await;

            loop {
                token = SessionToken::new()?;
                if !sessions.contains_key(&token) {
                    break;
                }
            }
        };

        let mut sessions = self.sessions.write().await;

        sessions.insert(
            token,
            Session {
                created_at: Instant::now(),
                expiration,
                user_id: user_id.0,
                credential_version: current,
                identity_epoch: epoch,
            },
        );

        Ok(token)
    }

    async fn load_internal(&self) -> Result<(), anyhow::Error> {
        let text = match fs::read_to_string(&self.file).await {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(());
            }
            Err(err) => {
                return Err(anyhow!("Failed to read data: {err:?}"));
            }
        };

        let json = match serde_json::from_str::<Json>(&text) {
            Ok(value) => value,
            Err(err) => {
                let error = serde_json::from_str::<V2>(&text)
                    .err()
                    .map(|x| x.to_string())
                    .unwrap_or("none".to_string());

                return Err(anyhow!(
                    "Failed to deserialize data as json: {err}, Version specific error: {error}"
                ));
            }
        };

        let data = migrate_to_latest(json)?;

        // AR-01: the persisted counters are a high-water mark, but a file written by
        // an older build (or hand-edited) may hold a generation or id above them.
        // Raising the counters past everything the file contains guarantees that
        // nothing this store hands out afterwards can ever repeat a value that
        // already appeared, so a pin from before a restart stays recognisably stale.
        //
        // The derivation deliberately matches what the live allocators maintain as
        // accounts are created, so a reload and a live store describe the same state.
        Self::raise_counter(&self.identity_epoch, data.identity_epoch);
        Self::raise_counter(
            &self.next_credential_version,
            data.next_credential_version.max(
                data.users
                    .values()
                    .map(|user| user.credential_version.saturating_add(1))
                    .max()
                    .unwrap_or(0),
            ),
        );
        Self::raise_counter(
            &self.next_user_id,
            data.next_user_id.max(
                data.users
                    .keys()
                    .map(|id| u64::from(*id).saturating_add(1))
                    .max()
                    .unwrap_or(0),
            ),
        );

        {
            let mut users = self.users.write().await;
            let mut hosts = self.hosts.write().await;
            let mut roles = self.roles.write().await;

            *users = data
                .users
                .into_iter()
                .map(|(id, user)| (id, RwLock::new(user)))
                .collect();
            *hosts = data
                .hosts
                .into_iter()
                .map(|(id, host)| (id, RwLock::new(host)))
                .collect();
            *roles = data
                .roles
                .into_iter()
                .map(|(id, role)| (id, RwLock::new(role)))
                .collect();
        }

        Ok(())
    }
    async fn snapshot_json(&self) -> Json {
        let users = self.users.read().await;
        let hosts = self.hosts.read().await;
        let roles = self.roles.read().await;

        let mut users_json = HashMap::new();
        for (key, value) in users.iter() {
            users_json.insert(*key, value.read().await.clone());
        }

        let mut hosts_json = HashMap::new();
        for (key, value) in hosts.iter() {
            hosts_json.insert(*key, value.read().await.clone());
        }

        let mut roles_json = HashMap::new();
        for (key, value) in roles.iter() {
            roles_json.insert(*key, value.read().await.clone());
        }

        Json::V3(V3 {
            users: users_json,
            hosts: hosts_json,
            roles: roles_json,
            identity_epoch: self.identity_epoch.load(AtomicOrdering::Acquire),
            next_credential_version: self.next_credential_version.load(AtomicOrdering::Acquire),
            next_user_id: self.next_user_id.load(AtomicOrdering::Acquire),
        })
    }

    async fn store(&self) -> Result<(), String> {
        let _store_guard = self.store_lock.lock().await;
        let json = self.snapshot_json().await;

        let text = serde_json::to_string_pretty(&json)
            .map_err(|err| format!("failed to serialize data to json: {err:?}"))?;

        atomic_replace_file(&self.file, &text)
            .await
            .map_err(|err| format!("failed to atomically write data to file: {err:?}"))
    }

    /// Only the owned writer calls this: cancellation must not release these
    /// guards while Tokio filesystem work can still commit the candidate.
    async fn remove_role_committed(&self, role_id: RoleId) -> Result<(), AppError> {
        // Serialize with the writer and restore, then hold the complete identity
        // snapshot in the same lock order as snapshot_json/import_backup.
        let _store_guard = self.store_lock.lock().await;
        let mut users = self.users.write().await;
        let mut hosts = self.hosts.write().await;
        let mut roles = self.roles.write().await;
        let mut sessions = self.sessions.write().await;
        if !roles.contains_key(&role_id.0) {
            return Err(AppError::RoleNotFound);
        }

        let password_required = any_user_has_password(&users).await;
        let mut removed = std::collections::HashSet::new();
        let mut data = V3 {
            users: HashMap::new(),
            hosts: HashMap::new(),
            roles: HashMap::new(),
            identity_epoch: self.identity_epoch.load(AtomicOrdering::Acquire),
            next_credential_version: self.next_credential_version.load(AtomicOrdering::Acquire),
            next_user_id: self.next_user_id.load(AtomicOrdering::Acquire),
        };
        for (id, user) in users.iter() {
            let user = user.read().await;
            if user.role_id == role_id.0 {
                removed.insert(*id);
            } else {
                data.users.insert(*id, user.clone());
            }
        }
        for (id, role) in roles.iter() {
            if *id != role_id.0 {
                data.roles.insert(*id, role.read().await.clone());
            }
        }
        // Check the COMPLETE deletion set, not each user in isolation. A
        // passwordless store still needs a surviving header-auth admin; a store
        // with passwords must keep a password-capable admin outside this role.
        if !removed.is_empty()
            && !data.users.values().any(|user| {
                (!password_required || user.password.is_some())
                    && data
                        .roles
                        .get(&user.role_id)
                        .is_some_and(|role| matches!(role.ty, V3RoleType::Admin))
            })
        {
            return Err(AppError::Forbidden);
        }
        for (id, host) in hosts.iter() {
            let mut host = host.read().await.clone();
            if host.owner.is_some_and(|owner| removed.contains(&owner)) {
                host.owner = None;
            }
            data.hosts.insert(*id, host);
        }

        // Nothing live is changed until the candidate is durable. In particular,
        // failed persistence cannot revoke sessions or globalize private hosts.
        let text = serde_json::to_string_pretty(&Json::V3(data.clone()))
            .map_err(|_| AppError::StorageUnavailable)?;
        atomic_replace_staged_file(&self.file, &text).await?;

        // No await points after persistence returns and before publishing all tables.
        *users = data
            .users
            .into_iter()
            .map(|(id, user)| (id, RwLock::new(user)))
            .collect();
        *hosts = data
            .hosts
            .into_iter()
            .map(|(id, host)| (id, RwLock::new(host)))
            .collect();
        *roles = data
            .roles
            .into_iter()
            .map(|(id, role)| (id, RwLock::new(role)))
            .collect();
        sessions.retain(|_, session| !removed.contains(&session.user_id));
        Ok(())
    }

    /// Applies a validated restore: disk commit, live publication and epoch
    /// advancement as one owned transition (AR-02).
    ///
    /// This runs on the writer task, not on the caller's future, so dropping the
    /// caller after the primary rename cannot leave the disk holding the restored
    /// snapshot while the live tables still hold the old one. Everything the
    /// publication needs (the candidate, the exclusion guard, the obligation to
    /// finish) is owned here.
    ///
    /// `guard` is the caller-held identity-exclusion guard. It is dropped as the
    /// very last step, after the epoch is visible, so no reader can observe the new
    /// snapshot without the new epoch and no new identity-changing operation can
    /// start in between.
    async fn restore_committed(
        &self,
        mut data: V3,
        guard: Option<OwnedRwLockWriteGuard<()>>,
    ) -> Result<(), AppError> {
        // Serialize with the writer and with every other restore, then hold the
        // complete identity snapshot in the documented lock order for the whole
        // transition.
        //
        // The disk rename happens while these guards are held (exactly like
        // `remove_role_committed`). That is load-bearing: an observer that notices
        // the new primary file must be blocked by the identity locks until the live
        // tables have been published, otherwise "the file changed" would not imply
        // "the live state changed", which is the split AR-02 is about.
        let _store_guard = self.store_lock.lock().await;
        let mut users = self.users.write().await;
        let mut hosts = self.hosts.write().await;
        let mut roles = self.roles.write().await;
        let mut sessions = self.sessions.write().await;

        // AR-01: the restored snapshot must not be able to re-introduce a
        // generation or an id that this store has already used. Every account gets
        // a freshly allocated generation (never the backup's own value + 1, which is
        // exactly the value a pre-restore login could be pinned to), the epoch moves
        // forward, and the id allocator is raised above every id the backup installs
        // so a later account can never be handed a reinstated id.
        //
        // The new values are *staged*, not published: the counters are only advanced
        // after the durable write succeeds, so a restore that fails leaves the live
        // identity state (including the counters) exactly as it was.
        let new_epoch = self
            .identity_epoch
            .load(AtomicOrdering::Acquire)
            .max(data.identity_epoch)
            .saturating_add(1);
        // Start above both the counter and every generation the backup carries, so no
        // account can be handed a value that was live before the restore.
        let mut next_version = self
            .next_credential_version
            .load(AtomicOrdering::Acquire)
            .max(
                data.users
                    .values()
                    .map(|user| user.credential_version)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1),
            );
        // Deterministic order, so a restore of the same backup is reproducible.
        let user_ids: Vec<u32> = {
            let mut ids: Vec<u32> = data.users.keys().copied().collect();
            ids.sort_unstable();
            ids
        };
        for id in user_ids {
            let user = data.users.get_mut(&id).expect("id came from the same map");
            user.credential_version = next_version;
            next_version = next_version.saturating_add(1);
        }
        let next_user_id = self.next_user_id.load(AtomicOrdering::Acquire).max(
            data.users
                .keys()
                .map(|id| u64::from(*id).saturating_add(1))
                .max()
                .unwrap_or(0),
        );
        data.identity_epoch = new_epoch;
        data.next_credential_version = next_version;
        data.next_user_id = next_user_id;

        let text = serde_json::to_string_pretty(&Json::V3(data.clone()))
            .map_err(|_| AppError::StorageUnavailable)?;

        // Nothing live has changed yet. If this write fails, both disk and live
        // state are exactly as they were, and the guards are dropped normally.
        atomic_replace_file(&self.file, &text).await?;

        // The candidate is durable, so the reserved high-water marks become the new
        // live ones. Nothing can have allocated in between: the store lock is held.
        Self::raise_counter(&self.next_credential_version, next_version);
        Self::raise_counter(&self.next_user_id, next_user_id);

        // No await points after persistence returns and before publishing all tables,
        // so the commit and the publication are one observable transition.
        *users = data
            .users
            .into_iter()
            .map(|(id, user)| (id, RwLock::new(user)))
            .collect();
        *hosts = data
            .hosts
            .into_iter()
            .map(|(id, host)| (id, RwLock::new(host)))
            .collect();
        *roles = data
            .roles
            .into_iter()
            .map(|(id, role)| (id, RwLock::new(role)))
            .collect();
        // A restore replaces every identity the sessions were authorized against.
        sessions.clear();
        // Publish the epoch before the guards are released, so no reader can observe
        // the new snapshot with the previous epoch.
        self.identity_epoch
            .store(new_epoch, AtomicOrdering::Release);

        drop(sessions);
        drop(roles);
        drop(hosts);
        drop(users);
        drop(_store_guard);
        // Last: release the identity exclusion only once publication is complete.
        drop(guard);
        Ok(())
    }
}

async fn file_writer(mut store_receiver: Receiver<StoreRequest>, json: Arc<JsonStorage>) {
    loop {
        let Some(request) = store_receiver.recv().await else {
            return;
        };

        match request {
            StoreRequest::Persist { ack } => {
                let result = json.store().await;

                if let Err(err) = &result {
                    error!("{err}");
                }

                if let Some(ack) = ack {
                    // The caller may already have gone away; that is not an error here.
                    let _ = ack.send(result);
                }
            }
            StoreRequest::RemoveRole {
                role_id,
                ack,
                _reservation,
            } => {
                // Never cancel an accepted deletion on a closed acknowledgement:
                // disk commit, live publication and revocation must finish together.
                // The reservation stays held for the whole commit, so a concurrent
                // user creation cannot reference the role mid-deletion (AR-03).
                let result = json.remove_role_committed(role_id).await;
                drop(_reservation);
                let _ = ack.send(result);
            }
            StoreRequest::Mutation { mutation, ack } => {
                // Same contract for the transactional mutations: the writer owns
                // the request, so a dropped caller cannot split disk and live state.
                let result = json.apply_mutation(mutation).await;
                let _ = ack.send(result);
            }
            StoreRequest::Restore { data, guard, ack } => {
                // Owned exactly like a role deletion: the accepted restore finishes
                // even when the requesting future was dropped at the cancellation
                // boundary, so disk and live state cannot diverge (AR-02).
                let result = json.restore_committed(data, guard).await;
                let _ = ack.send(result);
            }
        }
    }
}

fn permissions_from_json(permissions: V3RolePermissions) -> StorageRolePermissions {
    StorageRolePermissions {
        allow_add_hosts: permissions.allow_add_hosts,
        maximum_bitrate_kbps: permissions.maximum_bitrate_kbps,
        allow_codec_h264: permissions.allow_codec_h264,
        allow_codec_h265: permissions.allow_codec_h265,
        allow_codec_av1: permissions.allow_codec_av1,
        allow_hdr: permissions.allow_hdr,
        allow_transport_webrtc: permissions.allow_transport_webrtc,
        allow_transport_websockets: permissions.allow_transport_websockets,
    }
}
fn permissions_to_json(permissions: StorageRolePermissions) -> V3RolePermissions {
    V3RolePermissions {
        allow_add_hosts: permissions.allow_add_hosts,
        maximum_bitrate_kbps: permissions.maximum_bitrate_kbps,
        allow_codec_h264: permissions.allow_codec_h264,
        allow_codec_h265: permissions.allow_codec_h265,
        allow_codec_av1: permissions.allow_codec_av1,
        allow_hdr: permissions.allow_hdr,
        allow_transport_webrtc: permissions.allow_transport_webrtc,
        allow_transport_websockets: permissions.allow_transport_websockets,
    }
}

fn role_from_json(role_id: RoleId, role: &V3Role) -> StorageRole {
    StorageRole {
        id: role_id,
        name: role.name.clone(),
        ty: match role.ty {
            V3RoleType::Admin => RoleType::Admin,
            V3RoleType::User => RoleType::User,
        },
        default_settings: StorageRoleDefaultSettings {
            value: role.default_settings.clone(),
        },
        permissions: permissions_from_json(role.permissions.clone()),
    }
}

fn user_from_json(user_id: UserId, user: &V3User) -> StorageUser {
    StorageUser {
        id: user_id,
        name: user.name.clone(),
        password: user.password.as_ref().map(|password| StoragePassword {
            salt: password.salt,
            hash: password.hash,
            iterations: password.iterations,
        }),
        role_id: RoleId(user.role_id),
        client_unique_id: user.client_unique_id.clone(),
        credential_version: CredentialVersion(user.credential_version),
    }
}

fn host_from_json(host_id: HostId, host: &V2Host) -> StorageHost {
    StorageHost {
        id: host_id,
        owner: host.owner.map(UserId),
        address: host.address.clone(),
        http_port: host.http_port,
        pair_info: host.pair_info.clone().map(|pair_info| StorageHostPairInfo {
            client_certificate: pair_info.client_certificate,
            client_private_key: pair_info.client_private_key,
            server_certificate: pair_info.server_certificate,
        }),
        cache: StorageHostCache {
            name: host.cache.name.clone(),
            mac: host.cache.mac,
        },
    }
}

fn random_number() -> Result<u32, AppError> {
    let mut id_bytes = [0u8; 4];
    rand_bytes(&mut id_bytes)?;
    Ok(u32::from_be_bytes(id_bytes))
}

/// Bounds on the stored PBKDF2 cost. A backup is attacker-influenced input, and an
/// absurd iteration count would turn every login into a CPU denial of service.
const MIN_PASSWORD_ITERATIONS: u32 = 1_000;
const MAX_PASSWORD_ITERATIONS: u32 = 4 * crate::app::password::HASH_ITERATIONS;

fn password_iterations_in_range(iterations: u32) -> bool {
    (MIN_PASSWORD_ITERATIONS..=MAX_PASSWORD_ITERATIONS).contains(&iterations)
}

/// Whether any user at all has a password. See [`keeps_admin_capability`].
async fn any_user_has_password(users: &HashMap<u32, RwLock<V3User>>) -> bool {
    for user in users.values() {
        if user.read().await.password.is_some() {
            return true;
        }
    }

    false
}

#[async_trait]
impl Storage for JsonStorage {
    async fn add_role(&self, role: StorageRoleAdd) -> Result<StorageRole, AppError> {
        let role = V3Role {
            ty: match role.ty {
                RoleType::Admin => V3RoleType::Admin,
                RoleType::User => V3RoleType::User,
            },
            name: role.name,
            default_settings: role.default_settings.value,
            permissions: permissions_to_json(role.permissions),
        };

        let mut roles = self.roles.write().await;

        let mut id;
        loop {
            id = random_number()?;

            if !roles.contains_key(&id) {
                break;
            }
        }
        roles.insert(id, RwLock::new(role.clone()));

        drop(roles);

        self.force_write();

        Ok(StorageRole {
            ty: match role.ty {
                V3RoleType::Admin => RoleType::Admin,
                V3RoleType::User => RoleType::User,
            },
            id: RoleId(id),
            name: role.name,
            default_settings: StorageRoleDefaultSettings {
                value: role.default_settings,
            },
            permissions: permissions_from_json(role.permissions),
        })
    }
    async fn persist(&self) -> Result<(), AppError> {
        self.flush().await
    }
    async fn modify_role(
        &self,
        role_id: RoleId,
        modify: StorageRoleModify,
    ) -> Result<(), AppError> {
        let roles = self.roles.read().await;

        let role_lock = roles.get(&role_id.0).ok_or(AppError::RoleNotFound)?;
        let mut role = role_lock.write().await;

        if let Some(name) = modify.name {
            role.name = name;
        }
        if let Some(ty) = modify.ty {
            role.ty = match ty {
                RoleType::Admin => V3RoleType::Admin,
                RoleType::User => V3RoleType::User,
            };
        }
        if let Some(StorageRoleDefaultSettings { value }) = modify.default_settings {
            role.default_settings = value;
        }
        if let Some(StorageRolePermissions {
            allow_add_hosts,
            maximum_bitrate_kbps,
            allow_codec_h264,
            allow_codec_h265,
            allow_codec_av1,
            allow_hdr,
            allow_transport_webrtc,
            allow_transport_websockets,
        }) = modify.permissions
        {
            role.permissions.allow_add_hosts = allow_add_hosts;
            role.permissions.maximum_bitrate_kbps = maximum_bitrate_kbps;
            role.permissions.allow_codec_h264 = allow_codec_h264;
            role.permissions.allow_codec_h265 = allow_codec_h265;
            role.permissions.allow_codec_av1 = allow_codec_av1;
            role.permissions.allow_hdr = allow_hdr;
            role.permissions.allow_transport_webrtc = allow_transport_webrtc;
            role.permissions.allow_transport_websockets = allow_transport_websockets;
        }

        drop(role);
        drop(roles);

        self.force_write();

        Ok(())
    }
    async fn get_role(&self, role_id: RoleId) -> Result<StorageRole, AppError> {
        let roles = self.roles.read().await;

        let role_lock = roles.get(&role_id.0).ok_or(AppError::RoleNotFound)?;
        let role = role_lock.read().await;

        Ok(role_from_json(role_id, &role))
    }
    async fn remove_role(&self, role_id: RoleId) -> Result<(), AppError> {
        let (ack, receiver) = oneshot::channel();
        // AR-03: reserve the role before the handoff so a user creation that runs
        // between admission and commit cannot reference it. The guard travels with
        // the request and is released when the writer finishes (success, rejection
        // or a panic) — a cancelled deletion therefore never leaves the role
        // permanently un-referenceable.
        let reservation = self.role_deletion_reservations.reserve(role_id.0);
        // Sending is the ownership handoff. Before acceptance there are no side
        // effects; afterwards the writer finishes even if this future is dropped.
        // On a rejected send the reservation is dropped with the message, so
        // nothing stays locked.
        self.store_sender
            .send(StoreRequest::RemoveRole {
                role_id,
                ack,
                _reservation: reservation,
            })
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        receiver.await.map_err(|_| AppError::StorageUnavailable)?
    }
    async fn list_roles(&self) -> Result<Either<Vec<RoleId>, Vec<StorageRole>>, AppError> {
        let roles = self.roles.read().await;

        let futures = roles.iter().map(|(id, value)| {
            let id = *id;
            async move {
                let role = value.read().await.clone();
                role_from_json(RoleId(id), &role)
            }
        });

        let out = join_all(futures).await;
        Ok(Either::Right(out))
    }

    async fn add_user(&self, user: StorageUserAdd) -> Result<StorageUser, AppError> {
        // AR-03/AR-05: role existence, name uniqueness, the insert and the durable
        // commit are one writer-owned transaction, so no reference can be created
        // against a role that is concurrently going away, and success implies the
        // account is really on disk.
        //
        // AR-01 is preserved inside that transaction: the account id and the
        // credential high-water mark are allocated from the persisted counters in the
        // *candidate* snapshot (see `mutations::allocate_staged_user_id`), so this
        // path neither reinstates a removed id nor drops the restore provenance that
        // the candidate now carries.
        self.mutate(Mutation::AddUser(user)).await?.into_user()
    }
    async fn modify_user(
        &self,
        user_id: UserId,
        modify: StorageUserModify,
    ) -> Result<(), AppError> {
        // AR-03/AR-05: the target-role reference, the last-admin protection, the
        // credential-version bump and the session revocation are all part of one
        // candidate commit, so a concurrent role cascade cannot invalidate the
        // reference after it was checked and a failed write cannot publish a
        // credential change that never reached disk.
        //
        // AR-01 is preserved: the generation bump still follows the revocation
        // decision (never a transparent rehash, which uses `rehash_password`), but
        // the new value is allocated from the persisted high-water mark inside the
        // candidate rather than `wrapping_add(1)` on the account's own value.
        self.mutate(Mutation::ModifyUser { user_id, modify })
            .await?
            .into_done()
    }
    async fn rehash_password(
        &self,
        user_id: UserId,
        expected: CredentialVersion,
        password: StoragePassword,
    ) -> Result<(), AppError> {
        let users = self.users.read().await;

        let user_lock = users.get(&user_id.0).ok_or(AppError::UserNotFound)?;
        let mut user = user_lock.write().await;

        // AR-04: exact compare-and-swap. The candidate hash was derived from the
        // password that verified against generation `expected`; if the account has
        // moved past that generation (explicit rotation, role change, restore,
        // deletion) the candidate describes a credential that no longer exists and
        // must never be written. `CredentialsWrong` keeps the login refused instead
        // of acknowledging a write that did not happen.
        if user.credential_version != expected.0 {
            return Err(AppError::CredentialsWrong);
        }

        // Deliberately does not touch `credential_version` and does not revoke
        // sessions: upgrading the hash for the same password is not a credential
        // change (AR-04).
        user.password = Some(V2UserPassword {
            salt: password.salt,
            hash: password.hash,
            iterations: password.iterations,
        });

        drop(user);
        drop(users);

        self.force_write();

        Ok(())
    }
    async fn get_user(&self, user_id: UserId) -> Result<StorageUser, AppError> {
        let users = self.users.read().await;

        let user_lock = users.get(&user_id.0).ok_or(AppError::UserNotFound)?;
        let user = user_lock.read().await;

        Ok(user_from_json(user_id, &user))
    }
    async fn get_user_by_name(
        &self,
        name: &str,
    ) -> Result<(UserId, Option<StorageUser>), AppError> {
        let users = self.users.read().await;

        let results = join_all(users.iter().map(|(user_id, user)| async move {
            let user = user.read().await;

            let user_id = UserId(*user_id);
            let user = (user.name == name).then(|| user_from_json(user_id, &user));

            (user_id, user)
        }))
        .await;

        let user = results.into_iter().find(|(_, user)| user.is_some());

        user.ok_or(AppError::UserNotFound)
    }
    async fn remove_user(&self, user_id: UserId) -> Result<(), AppError> {
        // AR-03: the removal, the last-admin protection and the owned-host
        // cleanup are one transaction on the candidate snapshot, so a deleted
        // user can never remain a host owner and the exported backup stays
        // importable. AR-05: the commit is durable before it is acknowledged, and
        // the writer — not this future — owns it.
        self.mutate(Mutation::RemoveUser { user_id })
            .await?
            .into_done()
    }
    async fn list_users(&self) -> Result<Either<Vec<UserId>, Vec<StorageUser>>, AppError> {
        let users = self.users.read().await;

        let futures = users.iter().map(|(id, value)| {
            let id = *id;
            async move {
                let user = value.read().await.clone();
                user_from_json(UserId(id), &user)
            }
        });

        let out = join_all(futures).await;
        Ok(Either::Right(out))
    }
    async fn add_first_user(&self, user: StorageUserAdd) -> Result<StorageUser, AppError> {
        // MW-16: the empty-store check and the insert share one critical section.
        // AR-05: they now also share the durable commit, so an acknowledged first
        // login can no longer run ahead of the bytes on disk (the caller gets an
        // error and no ghost account instead).
        //
        // AR-01 is preserved inside the transaction: the bootstrap account obeys the
        // same monotonic non-repeating id rule and credential high-water mark as
        // every other account, allocated in the candidate snapshot.
        self.mutate(Mutation::AddFirstUser(user)).await?.into_user()
    }

    async fn create_session_token_checked(
        &self,
        user_id: UserId,
        expiration: Duration,
        expected: Option<CredentialVersion>,
    ) -> Result<SessionToken, AppError> {
        self.create_session_token_inner(user_id, expiration, expected, None)
            .await
    }
    async fn create_session_token_pinned(
        &self,
        user_id: UserId,
        expiration: Duration,
        expected: Option<CredentialPin>,
    ) -> Result<SessionToken, AppError> {
        // AR-01: the epoch check, the generation check and the token insert are a
        // single transition under the same locks the restore uses, so a restore
        // cannot land between "the credentials were verified in epoch N" and "here
        // is a usable session".
        self.create_session_token_inner(
            user_id,
            expiration,
            expected.map(|pin| pin.version),
            expected.map(|pin| pin.epoch),
        )
        .await
    }
    async fn remove_session_token(&self, session: SessionToken) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;

        sessions.remove(&session);

        Ok(())
    }
    async fn remove_all_user_session_tokens(&self, user_id: UserId) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;

        sessions.retain(|_, session| UserId(session.user_id) != user_id);

        Ok(())
    }
    async fn get_user_by_session_token(
        &self,
        session: SessionToken,
    ) -> Result<(UserId, Option<StorageUser>), AppError> {
        // Lock order is users -> sessions, matching the documented descending
        // order in `JsonStorage`.
        let users = self.users.read().await;
        let sessions = self.sessions.read().await;

        let entry = match sessions.get(&session) {
            Some(entry) => entry,
            None => return Err(AppError::SessionTokenNotFound),
        };

        // MW-15: expiry is enforced on every authentication, not only by the
        // periodic sweeper (which now just reclaims memory).
        let age = Instant::now().saturating_duration_since(entry.created_at);
        let expired = age >= entry.expiration;
        let user_id = UserId(entry.user_id);

        if expired {
            drop(sessions);
            drop(users);
            self.sessions.write().await.remove(&session);
            return Err(AppError::SessionTokenNotFound);
        }

        // MW-01: the account must still exist at authorization time. Deleting a
        // user (directly or through a role cascade) revokes its sessions, and this
        // re-check keeps the decision tied to the current identity store.
        let user = match users.get(&entry.user_id) {
            Some(user_lock) => {
                let user = user_lock.read().await;
                Some(user_from_json(user_id, &user))
            }
            None => None,
        };

        // AR-01: a session is only valid for the credential generation it was
        // issued under. This is what makes a session minted from a pre-rotation
        // credential check harmless: it is born unusable.
        let session_is_current = user.as_ref().is_some_and(|user| {
            user.credential_version == CredentialVersion(entry.credential_version)
        });
        // AR-01/AR-02: and only within the identity incarnation it was issued in, so
        // a token cannot survive a restore that reinstated an older snapshot.
        let session_is_current = session_is_current
            && entry.identity_epoch == self.identity_epoch.load(AtomicOrdering::Acquire);
        let user = if session_is_current { user } else { None };
        drop(sessions);
        drop(users);

        match user {
            Some(user) => Ok((user_id, Some(user))),
            None => {
                let mut sessions = self.sessions.write().await;
                sessions.retain(|_, entry| entry.user_id != user_id.0);
                Err(AppError::SessionTokenNotFound)
            }
        }
    }

    async fn list_sessions(&self) -> Result<Vec<StorageSessionInfo>, AppError> {
        let sessions = self.sessions.read().await;
        let now = Instant::now();
        let mut result = sessions
            .iter()
            .filter_map(|(token, session)| {
                let age = now.saturating_duration_since(session.created_at);
                if age >= session.expiration {
                    return None;
                }
                Some(StorageSessionInfo {
                    id: token.fingerprint(),
                    user_id: UserId(session.user_id),
                    age_seconds: age.as_secs(),
                    expires_in_seconds: session.expiration.saturating_sub(age).as_secs(),
                })
            })
            .collect::<Vec<_>>();
        result.sort_by_key(|session| session.age_seconds);
        Ok(result)
    }

    async fn remove_session_by_id(&self, id: &str) -> Result<bool, AppError> {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|token, _| token.fingerprint() != id);
        Ok(sessions.len() != before)
    }

    async fn remove_all_sessions(&self) -> Result<(), AppError> {
        self.sessions.write().await.clear();
        Ok(())
    }

    async fn export_backup(&self) -> Result<serde_json::Value, AppError> {
        serde_json::to_value(self.snapshot_json().await).map_err(|_| AppError::BadRequest)
    }

    async fn import_backup(&self, value: serde_json::Value) -> Result<(), AppError> {
        self.import_backup_guarded(value, None).await
    }

    async fn import_backup_guarded(
        &self,
        value: serde_json::Value,
        guard: Option<OwnedRwLockWriteGuard<()>>,
    ) -> Result<(), AppError> {
        let json: Json = serde_json::from_value(value).map_err(|_| AppError::BadRequest)?;
        let data = migrate_to_latest(json).map_err(|_| AppError::BadRequest)?;

        // MW-20: the importer applies the same invariants as the mutation API.
        if data.roles.is_empty() {
            return Err(AppError::BadRequest);
        }
        // Re-importing an empty user set would silently re-open first-login
        // bootstrap; that is a provisioning transition, not a restore.
        if data.users.is_empty() {
            return Err(AppError::BadRequest);
        }
        if data
            .users
            .values()
            .any(|user| !data.roles.contains_key(&user.role_id))
        {
            return Err(AppError::BadRequest);
        }
        if data.hosts.values().any(|host| {
            host.owner
                .is_some_and(|owner| !data.users.contains_key(&owner))
        }) {
            return Err(AppError::BadRequest);
        }
        // Duplicate usernames would make `get_user_by_name` ambiguous.
        {
            let mut seen = std::collections::HashSet::new();
            if data
                .users
                .values()
                .any(|user| !seen.insert(user.name.as_str()))
            {
                return Err(AppError::BadRequest);
            }
        }
        // Password cost is bounded so a crafted backup cannot make every login
        // prohibitively expensive.
        if data.users.values().any(|user| {
            user.password
                .as_ref()
                .is_some_and(|password| !password_iterations_in_range(password.iterations))
        }) {
            return Err(AppError::BadRequest);
        }
        if !data.users.values().any(|user| {
            data.roles
                .get(&user.role_id)
                .is_some_and(|role| matches!(role.ty, V3RoleType::Admin))
        }) {
            return Err(AppError::BadRequest);
        }
        // If any account can log in with a password, at least one admin must be
        // able to as well; otherwise the restore installs an unadministrable store.
        let any_password = data.users.values().any(|user| user.password.is_some());
        if any_password
            && !data.users.values().any(|user| {
                user.password.is_some()
                    && data
                        .roles
                        .get(&user.role_id)
                        .is_some_and(|role| matches!(role.ty, V3RoleType::Admin))
            })
        {
            return Err(AppError::BadRequest);
        }

        // Validation is complete. AR-02: from here the application is handed to the
        // owned writer, which finishes persistence, publication and epoch
        // advancement even if this caller's future is dropped at the rename boundary.
        let (ack, receiver) = oneshot::channel();
        // Sending is the ownership handoff: before acceptance nothing has been
        // written, afterwards the writer owns the candidate, the guard and the
        // obligation to publish, so a dropped caller cannot split disk from live
        // state (AR-02). The guard travels with the request and is therefore
        // released by the transition that owns it, never by a cancelled caller.
        self.store_sender
            .send(StoreRequest::Restore { data, guard, ack })
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        receiver.await.map_err(|_| AppError::StorageUnavailable)?
    }

    fn identity_epoch(&self) -> IdentityEpoch {
        self.identity_epoch.load(AtomicOrdering::Acquire)
    }

    async fn add_host(&self, host: StorageHostAdd) -> Result<StorageHost, AppError> {
        // AR-05: the host row and its durable commit are one writer-owned
        // transaction, so a success means the host really survived a restart and an
        // I/O failure leaves no half-published host behind.
        self.mutate(Mutation::AddHost(host)).await?.into_host()
    }
    async fn modify_host(
        &self,
        host_id: HostId,
        modify: StorageHostModify,
    ) -> Result<(), AppError> {
        // AR-05: address/port/owner/pair-info/cache updates all become durable
        // before this returns, so a pair-cache write cannot be reported as applied
        // while only memory knows about it.
        self.mutate(Mutation::ModifyHost { host_id, modify })
            .await?
            .into_done()
    }
    async fn get_host(&self, host_id: HostId) -> Result<StorageHost, AppError> {
        let hosts = self.hosts.read().await;

        let host = hosts.get(&host_id.0).ok_or(AppError::HostNotFound)?;
        let host = host.read().await;

        Ok(host_from_json(host_id, &host))
    }
    async fn remove_host(&self, host_id: HostId) -> Result<(), AppError> {
        // AR-05: the removal is durable before it is acknowledged, so a failed
        // write cannot make the host look deleted while it is still on disk.
        self.mutate(Mutation::RemoveHost { host_id })
            .await?
            .into_done()
    }

    async fn list_user_hosts(
        &self,
        query: StorageQueryHosts,
    ) -> Result<Vec<(HostId, Option<StorageHost>)>, AppError> {
        let hosts = self.hosts.read().await;

        let mut user_hosts = Vec::new();
        for (host_id, host) in &*hosts {
            let host_id = HostId(*host_id);
            let host = host.read().await;

            if host.owner.is_none() || host.owner.map(UserId) == Some(query.user_id) {
                user_hosts.push((host_id, Some(host_from_json(host_id, &host))));
            }
        }

        Ok(user_hosts)
    }
}

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod bootstrap_tests;

#[cfg(test)]
mod invariant_tests;

#[cfg(test)]
mod role_cascade_tests;

#[cfg(test)]
mod durability_tests;

#[cfg(test)]
mod backup_tests;

#[cfg(test)]
mod atomic_write_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "moonlight-web-json-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// MW-18: data.json holds password hashes and pairing private keys. The
    /// atomic replace renames a freshly created temp file over the target, so the
    /// temp file has to be created private and an existing private mode must be
    /// carried over instead of reverting to the process umask (0644 with 022).
    #[cfg(unix)]
    #[test]
    fn atomic_replace_file_keeps_existing_private_mode() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            use std::os::unix::fs::PermissionsExt;

            let dir = unique_dir("mode-existing");
            fs::create_dir_all(&dir).await.expect("create temp dir");
            let file = dir.join("data.json");
            fs::write(&file, "old").await.expect("seed file");
            fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .await
                .expect("chmod seed");

            atomic_replace_file(&file, "new")
                .await
                .expect("atomic write");

            assert_eq!(fs::read_to_string(&file).await.expect("new file"), "new");
            assert_eq!(mode_of(&file), 0o600, "private mode must be preserved");

            let _ = fs::remove_dir_all(&dir).await;
        });
    }

    /// A brand new storage file must never be created world/group readable.
    #[cfg(unix)]
    #[test]
    fn atomic_replace_file_creates_private_file() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let dir = unique_dir("mode-new");
            fs::create_dir_all(&dir).await.expect("create temp dir");
            let file = dir.join("data.json");

            atomic_replace_file(&file, "new")
                .await
                .expect("atomic write");

            assert_eq!(mode_of(&file), 0o600, "new storage file must be private");

            let _ = fs::remove_dir_all(&dir).await;
        });
    }

    #[test]
    fn atomic_replace_file_preserves_previous_version() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "moonlight-web-json-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).await.expect("create temp dir");
            let file = dir.join("data.json");
            fs::write(&file, "old").await.expect("seed file");

            atomic_replace_file(&file, "new")
                .await
                .expect("atomic write");

            assert_eq!(fs::read_to_string(&file).await.expect("new file"), "new");
            assert_eq!(
                fs::read_to_string(path_with_suffix(&file, ".bak"))
                    .await
                    .expect("backup"),
                "old"
            );
            assert!(!path_with_suffix(&file, ".tmp").exists());

            let _ = fs::remove_dir_all(&dir).await;
        });
    }
}

#[cfg(test)]
mod astra_tests;

#[cfg(test)]
mod mutation_transaction_tests;

#[cfg(test)]
mod credential_version_tests;

// Parent-acceptance regressions, copied verbatim (AR-01/AR-04 restore ABA).
#[cfg(test)]
mod astra_parent_auth_tests;

// Repeated-restore, failure, reload and id-reuse regressions added with the fix.
#[cfg(test)]
mod restore_epoch_tests;

// AR-01 x AR-03/AR-05 semantic-merge regressions (reconciliation of the two intents).
#[cfg(test)]
mod reconciliation_tests;
