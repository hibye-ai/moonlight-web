use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use common::config::StorageConfig;
use moonlight_common::mac::MacAddress;
use pem::Pem;
use serde_json::Value;

use crate::app::{
    AppError,
    auth::SessionToken,
    host::HostId,
    password::StoragePassword,
    role::RoleId,
    storage::json::JsonStorage,
    user::{RoleType, UserId},
};

pub mod json;

pub async fn create_storage(
    config: StorageConfig,
) -> Result<Arc<dyn Storage + Send + Sync>, anyhow::Error> {
    match config {
        StorageConfig::Json {
            path,
            session_expiration_check_interval,
        } => {
            let storage = JsonStorage::load(path.into(), session_expiration_check_interval).await?;

            Ok(storage)
        }
    }
}

// Storages:
// - If two options are in a Modify struct it means: First option = change the field, second option = is this value null

// --- User ---
#[derive(Clone)]
pub struct StorageSessionInfo {
    pub id: String,
    pub user_id: UserId,
    pub age_seconds: u64,
    pub expires_in_seconds: u64,
}

/// Generation of one account's credentials.
///
/// AR-01: a credential check is only meaningful for the generation it read. The
/// version advances on every password/role transition that ends the account's
/// sessions and on every restore, so a snapshot taken before the transition can
/// detect that its answer is stale and refuse to issue a session from it.
///
/// Generations are handed out from a persisted monotonic counter, never from the
/// value found in a backup. That is the property that closes the ABA window: a
/// version is never issued twice in the lifetime of a store, so a version that
/// was superseded can never become current again, not even by restoring an older
/// backup, restoring the same backup twice, or deleting and re-adding an account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CredentialVersion(pub u64);

/// Epoch of the identity snapshot.
///
/// AR-01/AR-02: advances by exactly one on every accepted restore. It names *which
/// incarnation of the whole store* a decision belongs to, and is stamped into
/// every session so a token cannot outlive the snapshot it was issued against.
/// It is persisted with that snapshot, so a restart cannot rewind it and an older
/// backup can never lower it.
/// Represented as a plain `u64` so it can be compared against a stored value
/// without a wrapper conversion at every call site.
pub type IdentityEpoch = u64;

/// A credential decision and the identity-snapshot incarnation it was taken in.
///
/// AR-01/AR-04: the generation identifies the account's credential set; the epoch
/// identifies which incarnation of the whole store that set belonged to. A
/// decision is only honoured while both still match, so a decision taken before a
/// restore can never be mistaken for a current one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CredentialPin {
    pub version: CredentialVersion,
    pub epoch: IdentityEpoch,
}

impl CredentialPin {
    pub fn new(version: CredentialVersion, epoch: IdentityEpoch) -> Self {
        Self { version, epoch }
    }
}

#[derive(Clone)]
pub struct StorageUser {
    pub id: UserId,
    pub name: String,
    pub password: Option<StoragePassword>,
    pub role_id: RoleId,
    pub client_unique_id: String,
    /// See [`CredentialVersion`].
    pub credential_version: CredentialVersion,
}
#[derive(Clone)]
pub struct StorageUserAdd {
    pub role_id: RoleId,
    pub name: String,
    pub password: Option<StoragePassword>,
    pub client_unique_id: String,
}
#[derive(Default, Clone)]
pub struct StorageUserModify {
    pub role_id: Option<RoleId>,
    pub password: Option<Option<StoragePassword>>,
    pub client_unique_id: Option<String>,
    /// Set when the password change was explicitly requested (user or admin), as
    /// opposed to a transparent hash upgrade. Explicit changes end the account's
    /// other sessions; a rehash must not log anyone out.
    pub revoke_sessions: bool,
}

// --- Roles ---
#[derive(Clone, Default)]
pub struct StorageRoleDefaultSettings {
    pub value: Value,
}

#[derive(Clone)]
pub struct StorageRolePermissions {
    pub allow_add_hosts: bool,
    pub maximum_bitrate_kbps: Option<u32>,
    pub allow_codec_h264: bool,
    pub allow_codec_h265: bool,
    pub allow_codec_av1: bool,
    pub allow_hdr: bool,
    pub allow_transport_webrtc: bool,
    pub allow_transport_websockets: bool,
}

impl Default for StorageRolePermissions {
    fn default() -> Self {
        Self {
            allow_add_hosts: true,
            maximum_bitrate_kbps: None,
            allow_codec_h264: true,
            allow_codec_h265: true,
            allow_codec_av1: true,
            allow_hdr: true,
            allow_transport_webrtc: true,
            allow_transport_websockets: true,
        }
    }
}

#[derive(Clone)]
pub struct StorageRole {
    pub id: RoleId,
    pub name: String,
    pub ty: RoleType,
    pub default_settings: StorageRoleDefaultSettings,
    pub permissions: StorageRolePermissions,
}

#[derive(Clone)]
pub struct StorageRoleAdd {
    pub name: String,
    pub ty: RoleType,
    pub default_settings: StorageRoleDefaultSettings,
    pub permissions: StorageRolePermissions,
}

#[derive(Clone)]
pub struct StorageRoleModify {
    pub name: Option<String>,
    pub ty: Option<RoleType>,
    pub default_settings: Option<StorageRoleDefaultSettings>,
    pub permissions: Option<StorageRolePermissions>,
}

// --- Hosts ---
#[derive(Clone)]
pub struct StorageHost {
    pub id: HostId,
    // If this is none it means the host is accessible by everyone
    pub owner: Option<UserId>,
    pub address: String,
    pub http_port: u16,
    pub pair_info: Option<StorageHostPairInfo>,
    pub cache: StorageHostCache,
}
#[derive(Clone)]
pub struct StorageHostAdd {
    pub owner: Option<UserId>,
    pub address: String,
    pub http_port: u16,
    pub pair_info: Option<StorageHostPairInfo>,
    pub cache: StorageHostCache,
}
#[derive(Clone)]
pub struct StorageHostCache {
    pub name: String,
    pub mac: Option<MacAddress>,
}
#[derive(Clone)]
pub struct StorageHostPairInfo {
    pub client_private_key: Pem,
    pub client_certificate: Pem,
    pub server_certificate: Pem,
}
#[derive(Default, Clone)]
pub struct StorageHostModify {
    pub owner: Option<Option<UserId>>,
    pub address: Option<String>,
    pub http_port: Option<u16>,
    pub pair_info: Option<Option<StorageHostPairInfo>>,
    pub cache_name: Option<String>,
    pub cache_mac: Option<Option<MacAddress>>,
}

#[derive(Clone)]
pub struct StorageQueryHosts {
    pub user_id: UserId,
}

pub enum Either<L, R> {
    #[allow(dead_code)]
    Left(L),
    Right(R),
}

#[async_trait]
pub trait Storage {
    // -- Roles --
    async fn add_role(&self, role: StorageRoleAdd) -> Result<StorageRole, AppError>;
    /// Waits until the current in-memory state is durably on disk.
    ///
    /// Ordinary mutations stay fire-and-forget for latency. Management endpoints
    /// that change security-relevant state call this so they never answer
    /// "success" for a change that a restart could undo (MW-17).
    async fn persist(&self) -> Result<(), AppError>;
    async fn modify_role(&self, role_id: RoleId, host: StorageRoleModify) -> Result<(), AppError>;
    async fn get_role(&self, role_id: RoleId) -> Result<StorageRole, AppError>;
    /// Deletes a role.
    ///
    /// All users that are in that role should also be delete.
    async fn remove_role(&self, role_id: RoleId) -> Result<(), AppError>;
    /// The returned tuple can contain a Vec<RoleId> or Vec<StorageRole> if the Storage thinks it's more efficient to query all data directly
    async fn list_roles(&self) -> Result<Either<Vec<RoleId>, Vec<StorageRole>>, AppError>;

    // -- Users --
    /// No duplicate names are allowed!
    async fn add_user(&self, user: StorageUserAdd) -> Result<StorageUser, AppError>;
    async fn modify_user(&self, user_id: UserId, user: StorageUserModify) -> Result<(), AppError>;
    /// Transparent hash upgrade (AR-04).
    ///
    /// Applies `password` only while the stored [`CredentialVersion`] still equals
    /// `expected`, i.e. only while the hash that was verified is still the current
    /// one. It is deliberately *not* a credential change: the generation does not
    /// advance and no session is revoked, so an upgrade during a normal login does
    /// not log the account's other sessions out. Returns
    /// [`AppError::CredentialsWrong`] when a real rotation (or a restore) won the
    /// race, so a stale rehash can never overwrite a newer password. A restore
    /// allocates fresh generations for every account, so it always wins this race.
    ///
    /// A caller that wants a real password change must use [`Storage::modify_user`]
    /// with `revoke_sessions: true`, which does advance the generation.
    async fn rehash_password(
        &self,
        user_id: UserId,
        expected: CredentialVersion,
        password: StoragePassword,
    ) -> Result<(), AppError>;
    async fn get_user(&self, user_id: UserId) -> Result<StorageUser, AppError>;
    /// The returned tuple can contain a StorageUser if the Storage thinks it's more efficient to query all data directly
    async fn get_user_by_name(&self, name: &str)
    -> Result<(UserId, Option<StorageUser>), AppError>;
    async fn remove_user(&self, user_id: UserId) -> Result<(), AppError>;
    /// The returned tuple can contain a Vec<UserId> or Vec<StorageUser> if the Storage thinks it's more efficient to query all data directly
    async fn list_users(&self) -> Result<Either<Vec<UserId>, Vec<StorageUser>>, AppError>;
    /// Atomically provisions the first user.
    ///
    /// Returns [`AppError::FirstUserAlreadyExists`] when any user already exists,
    /// so two concurrent first-login requests can never both become admin.
    async fn add_first_user(&self, user: StorageUserAdd) -> Result<StorageUser, AppError>;

    // -- Session Tokens --
    /// Creates a session for the account's current credential generation.
    ///
    /// Kept as the unpinned convenience path (and for existing callers/tests). The
    /// implementation is a default method so a second `Storage` implementation only
    /// has to provide [`Storage::create_session_token_checked`].
    ///
    /// Production logins go through the checked form (via `AuthenticatedUser`), so
    /// this is only exercised by the test suite, like
    /// [`Storage::remove_all_user_session_tokens`].
    #[allow(dead_code)]
    async fn create_session_token(
        &self,
        user_id: UserId,
        expires_after: Duration,
    ) -> Result<SessionToken, AppError> {
        self.create_session_token_checked(user_id, expires_after, None)
            .await
    }
    /// Creates a session only while the account's credential generation is still
    /// `expected` (AR-01).
    ///
    /// The generation check and the session insert are one transition, so a
    /// credential check that finished before a password rotation cannot mint a
    /// session after it. `None` means "no snapshot to pin" and accepts the current
    /// generation; callers that did verify credentials must pass the version they
    /// verified.
    async fn create_session_token_checked(
        &self,
        user_id: UserId,
        expires_after: Duration,
        expected: Option<CredentialVersion>,
    ) -> Result<SessionToken, AppError>;
    /// Creates a session only while the identity snapshot the caller verified is
    /// still the current one (AR-01).
    ///
    /// This is the form production logins use: it additionally rejects a pin whose
    /// [`CredentialPin::epoch`] is no longer current, so a session cannot be minted
    /// into an identity snapshot that replaced the one the password was checked
    /// against. The default implementation is the version-only check plus an epoch
    /// comparison; implementations may fold both into a single transition.
    async fn create_session_token_pinned(
        &self,
        user_id: UserId,
        expires_after: Duration,
        expected: Option<CredentialPin>,
    ) -> Result<SessionToken, AppError> {
        if let Some(pin) = expected
            && pin.epoch != self.identity_epoch()
        {
            return Err(AppError::CredentialsWrong);
        }

        self.create_session_token_checked(user_id, expires_after, expected.map(|pin| pin.version))
            .await
    }
    async fn remove_session_token(&self, session: SessionToken) -> Result<(), AppError>;
    #[allow(dead_code)]
    async fn remove_all_user_session_tokens(&self, user_id: UserId) -> Result<(), AppError>;
    /// The returned tuple can contain a StorageUser if the Storage thinks it's more efficient to query all data directly
    async fn get_user_by_session_token(
        &self,
        session: SessionToken,
    ) -> Result<(UserId, Option<StorageUser>), AppError>;
    async fn list_sessions(&self) -> Result<Vec<StorageSessionInfo>, AppError>;
    async fn remove_session_by_id(&self, id: &str) -> Result<bool, AppError>;
    async fn remove_all_sessions(&self) -> Result<(), AppError>;

    // -- Backup / Restore --
    async fn export_backup(&self) -> Result<Value, AppError>;
    async fn import_backup(&self, value: Value) -> Result<(), AppError>;
    /// Restore while holding an externally owned exclusion guard (AR-02).
    ///
    /// The caller passes ownership of its guard into the storage. The storage's
    /// owned transition then keeps the guard alive until the restored snapshot is
    /// durably written *and* published to the live tables, so cancelling the caller
    /// anywhere after acceptance cannot split disk from live state and cannot leave
    /// the exclusive scope open.
    ///
    /// The default implementation keeps the unguarded contract for storages that
    /// have no external exclusion to coordinate with.
    async fn import_backup_guarded(
        &self,
        value: Value,
        _guard: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> Result<(), AppError> {
        self.import_backup(value).await
    }
    /// Epoch of the current identity snapshot (AR-01/AR-02).
    ///
    /// Advanced inside the restore transition, before the exclusive store lock is
    /// released, so no reader can observe a replaced identity snapshot together
    /// with the previous epoch.
    fn identity_epoch(&self) -> IdentityEpoch;

    // -- Hosts --
    async fn add_host(&self, host: StorageHostAdd) -> Result<StorageHost, AppError>;
    async fn modify_host(&self, host_id: HostId, host: StorageHostModify) -> Result<(), AppError>;
    async fn get_host(&self, host_id: HostId) -> Result<StorageHost, AppError>;
    async fn remove_host(&self, host_id: HostId) -> Result<(), AppError>;

    /// Returns all hosts that either have no owner (global) or have the specified user_id as an owner
    ///
    /// The returned tuple in the Vec can contain a StorageHost if the Storage thinks it's more efficient to query all data directly
    async fn list_user_hosts(
        &self,
        query: StorageQueryHosts,
    ) -> Result<Vec<(HostId, Option<StorageHost>)>, AppError>;
}
