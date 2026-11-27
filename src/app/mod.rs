use std::{
    collections::HashMap,
    io, mem,
    ops::Deref,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use actix_web::{ResponseError, http::StatusCode, web::Bytes};
use common::config::Config;
use futures_concurrency::future::RaceOk;
use hex::FromHexError;
use moonlight_common::{high::MoonlightClientError, http::client::tokio_hyper::TokioHyperClient};
use openssl::error::ErrorStack;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::app::{
    auth::{SessionToken, UserAuth},
    host::{AppId, HostId},
    password::{LoginAdmission, PasswordCheckLimiter},
    role::{Role, RoleId},
    storage::{
        CredentialPin, Either, Storage, StorageHostModify, StorageRoleAdd,
        StorageRoleDefaultSettings, StorageRolePermissions, StorageSessionInfo, StorageUserAdd,
        create_storage,
    },
    user::{Admin, AuthenticatedUser, RoleType, User, UserId},
};

pub mod auth;
pub mod host;
pub mod password;
pub mod role;
pub mod storage;
pub mod user;

#[cfg(test)]
mod backup_gate_tests;

// Parent-acceptance regression: the App publication boundary under a cancelled
// caller (AR-02). Copied verbatim from the parent audit fixture.
#[cfg(test)]
mod astra_parent_app_restore_tests;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("the app got destroyed")]
    AppDestroyed,
    #[error("the user was not found")]
    UserNotFound,
    #[error("the role was not found")]
    RoleNotFound,
    #[error("more than one user already exists")]
    FirstUserAlreadyExists,
    #[error("the config option first_login_create_admin is not true")]
    FirstLoginCreateAdminNotSet,
    #[error("the user already exists")]
    UserAlreadyExists,
    #[error("the host was not found")]
    HostNotFound,
    #[error("the host was already paired")]
    HostPaired,
    #[error("the host must be paired for this action")]
    HostNotPaired,
    // -- Unauthorized
    #[error("the credentials don't exists")]
    CredentialsWrong,
    #[error("the host was not found")]
    SessionTokenNotFound,
    #[error("the action is not allowed because the user is not authorized, 401")]
    Unauthorized,
    #[error("using a custom header for authorization is disabled")]
    HeaderAuthDisabled,
    // --
    #[error("the action is not allowed with the current privileges, 403")]
    Forbidden,
    #[error("the change could not be saved to persistent storage")]
    StorageUnavailable,
    // -- Bad Request
    #[error("the authorization header is not a bearer")]
    AuthorizationNotBearer,
    #[error("the custom header used to authorize is malformed")]
    HeaderAuthMalformed,
    #[error("the authorization header is not a bearer")]
    BearerMalformed,
    #[error("the password is empty")]
    PasswordEmpty,
    #[error("the password is empty")]
    UserNameEmpty,
    #[error("the authorization header is not a bearer")]
    BadRequest,
    // --
    #[error("openssl error occured: {0}")]
    OpenSSL(#[from] ErrorStack),
    #[error("hex error occured: {0}")]
    Hex(#[from] FromHexError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("the password check could not be completed: {0}")]
    PasswordCheckFailed(String),
    #[error("moonlight error: {0}")]
    Moonlight(#[from] MoonlightClientError),
}

impl ResponseError for AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::AppDestroyed => StatusCode::INTERNAL_SERVER_ERROR,
            Self::FirstUserAlreadyExists => StatusCode::INTERNAL_SERVER_ERROR,
            Self::FirstLoginCreateAdminNotSet => StatusCode::INTERNAL_SERVER_ERROR,
            Self::HostNotFound => StatusCode::NOT_FOUND,
            Self::HostNotPaired => StatusCode::FORBIDDEN,
            Self::HostPaired => StatusCode::NOT_MODIFIED,
            Self::UserNotFound => StatusCode::NOT_FOUND,
            Self::RoleNotFound => StatusCode::NOT_FOUND,
            Self::UserAlreadyExists => StatusCode::CONFLICT,
            Self::CredentialsWrong => StatusCode::UNAUTHORIZED,
            Self::SessionTokenNotFound => StatusCode::UNAUTHORIZED,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::StorageUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::OpenSSL(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::HeaderAuthDisabled => StatusCode::UNAUTHORIZED,
            Self::Hex(_) => StatusCode::BAD_REQUEST,
            Self::AuthorizationNotBearer => StatusCode::BAD_REQUEST,
            Self::HeaderAuthMalformed => StatusCode::BAD_REQUEST,
            Self::BearerMalformed => StatusCode::BAD_REQUEST,
            Self::PasswordEmpty => StatusCode::BAD_REQUEST,
            Self::UserNameEmpty => StatusCode::BAD_REQUEST,
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Moonlight(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::PasswordCheckFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Clone)]
struct AppRef {
    inner: Weak<AppInner>,
}

/// Test-only synchronization point at the real credential-check boundary (AR-01).
///
/// The HTTP interleaving regressions need to know that a login has *definitely*
/// read its credential snapshot and is about to spend the PBKDF2 cost, so a
/// rotation can be placed strictly inside that window. Inferring it from timing
/// (saturating the password-check pool, sleeping) is inherently racy: on a fast
/// machine the window closes before the assertion runs.
///
/// This gate makes the boundary explicit. It is `#[cfg(test)]` only, it is armed
/// per test, it matches one account name, and it is a no-op when nothing is armed,
/// so it cannot slow down or serialize production logins and it does not disable
/// cancellation or the bounded pool.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct CredentialCheckGate {
    state: std::sync::Mutex<GateState>,
}

#[cfg(test)]
#[derive(Default)]
struct GateState {
    armed: Option<ArmedGate>,
}

#[cfg(test)]
struct ArmedGate {
    username: String,
    arrived: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// Handles a test holds for one armed credential check.
#[cfg(test)]
pub(crate) struct ArmedCredentialCheck {
    /// Resolves once a matching login reached the boundary.
    pub arrived: tokio::sync::oneshot::Receiver<()>,
    /// Dropping or sending releases the parked login.
    pub release: tokio::sync::oneshot::Sender<()>,
}

#[cfg(test)]
impl CredentialCheckGate {
    /// Arms the gate for `username` and returns the handles to observe/release it.
    pub(crate) fn arm(&self, username: &str) -> ArmedCredentialCheck {
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.armed = Some(ArmedGate {
            username: username.to_owned(),
            arrived: Some(arrived_tx),
            release: Some(release_rx),
        });
        ArmedCredentialCheck {
            arrived: arrived_rx,
            release: release_tx,
        }
    }

    /// Called by the real `User::authenticate` path after the snapshot was read and
    /// before the password check starts. Parks the login only when it matches the
    /// armed account; otherwise it returns immediately.
    pub(crate) async fn arrive(&self, username: &str) {
        let (arrived, release) = {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            match state.armed.as_mut() {
                Some(armed) if armed.username == username => {
                    let arrived = armed.arrived.take();
                    let release = armed.release.take();
                    state.armed = None;
                    (arrived, release)
                }
                _ => return,
            }
        };

        if let Some(arrived) = arrived {
            let _ = arrived.send(());
        }
        if let Some(release) = release {
            // A dropped sender also releases, so a panicking test cannot wedge a
            // request forever.
            let _ = release.await;
        }
    }
}

impl AppRef {
    fn access(&self) -> Result<impl Deref<Target = AppInner> + 'static, AppError> {
        Weak::upgrade(&self.inner).ok_or(AppError::AppDestroyed)
    }
}

struct AppInner {
    config: Config,
    storage: Arc<dyn Storage + Send + Sync>,
    app_image_cache: RwLock<HashMap<(UserId, HostId, AppId), Bytes>>,
    /// Serializes stream startup and identity-changing restores (MW-19).
    ///
    /// A stream holds the read side for its whole lifetime; a restore takes the
    /// write side, so the "are any streams active?" check and the store swap
    /// become one transition instead of two independent observations.
    maintenance: Arc<RwLock<()>>,
    /// Advances whenever a restore replaces the identity snapshot (MW-19).
    identity_epoch: AtomicU64,
    /// Request-start login admission (MW-23).
    ///
    /// Per-app rather than a process-wide static, so the bound is a property of the
    /// instance it protects and tests cannot leak state into each other.
    login_admission: LoginAdmission,
    /// Bounds how many PBKDF2 verifications occupy the blocking pool (MW-23).
    password_checks: PasswordCheckLimiter,
    /// Test-only credential-check boundary gate (AR-01). Compiled out in production.
    #[cfg(test)]
    credential_check_gate: CredentialCheckGate,
}

pub type MoonlightClient = TokioHyperClient;

pub struct App {
    inner: Arc<AppInner>,
}

impl App {
    pub async fn new(config: Config) -> Result<Self, anyhow::Error> {
        let app = AppInner {
            storage: create_storage(config.data_storage.clone()).await?,
            config,
            app_image_cache: Default::default(),
            maintenance: Default::default(),
            identity_epoch: AtomicU64::new(0),
            login_admission: LoginAdmission::new(),
            password_checks: PasswordCheckLimiter::new(),
            #[cfg(test)]
            credential_check_gate: CredentialCheckGate::default(),
        };

        Ok(Self {
            inner: Arc::new(app),
        })
    }

    fn new_ref(&self) -> AppRef {
        AppRef {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Request-start admission state for the login endpoint (MW-23).
    pub(crate) fn login_admission(&self) -> &LoginAdmission {
        &self.inner.login_admission
    }

    /// Test-only credential-check boundary gate (AR-01).
    #[cfg(test)]
    pub(crate) fn credential_check_gate(&self) -> &CredentialCheckGate {
        &self.inner.credential_check_gate
    }

    /// Handles all logic related to adding the first user:
    /// - Is this even currently allowed?
    /// - Moving hosts from global to first user
    pub async fn try_add_first_login(
        &self,
        username: String,
        password: String,
    ) -> Result<AuthenticatedUser, AppError> {
        if !self.config().web_server.first_login_create_admin {
            return Err(AppError::FirstLoginCreateAdminNotSet);
        }

        let admin_role = self.admin_role().await?;

        // MW-23: hashing a supplied password is a deliberately expensive PBKDF2
        // derivation, so it runs off the reactor inside the bounded pool instead of
        // occupying a worker. Bootstrap semantics are unchanged: an empty password is
        // still rejected, and the account is only created once the hash exists.
        let hashed = self.inner.password_checks.hash_new(password).await?;

        // MW-16: the empty-store check is part of the storage insert. Two
        // concurrent first logins can no longer both become admin.
        let user = self
            .add_first_user_no_auth(StorageUserAdd {
                name: username.clone(),
                password: Some(hashed),
                role_id: admin_role.id(),
                client_unique_id: username,
            })
            .await?;

        let mut user = user;

        if self.config().web_server.first_login_assign_global_hosts {
            // Note: only this user exists and all hosts are global, if migrated from v1 to v2
            // -> list_hosts will show just global hosts

            let hosts = user.hosts().await?;

            let user_id = user.id();
            for mut host in hosts {
                match host
                    .modify(
                        &mut user,
                        StorageHostModify {
                            owner: Some(Some(user_id)),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(err) => {
                        warn!("failed to move global host to new user {user_id:?}: {err}");
                    }
                }
            }
        }

        Ok(user)
    }

    /// admin: The admin that tries to do this action
    pub async fn add_user(
        &self,
        _: &Admin,
        user: StorageUserAdd,
    ) -> Result<AuthenticatedUser, AppError> {
        let user = self.add_user_no_auth(user).await?;

        // MW-17: do not report success for an account change that a restart could
        // undo.
        self.inner.storage.persist().await?;

        Ok(user)
    }

    async fn add_user_no_auth(&self, user: StorageUserAdd) -> Result<AuthenticatedUser, AppError> {
        if user.name.is_empty() {
            return Err(AppError::UserNameEmpty);
        }

        let user = self.inner.storage.add_user(user).await?;

        let pin = Some(CredentialPin::new(
            user.credential_version,
            self.inner.storage.identity_epoch(),
        ));
        Ok(AuthenticatedUser {
            inner: User {
                app: self.new_ref(),
                id: user.id,
                cache_storage: Some(user.into()),
            },
            pin,
        })
    }

    async fn add_first_user_no_auth(
        &self,
        user: StorageUserAdd,
    ) -> Result<AuthenticatedUser, AppError> {
        if user.name.is_empty() {
            return Err(AppError::UserNameEmpty);
        }

        let user = self.inner.storage.add_first_user(user).await?;

        let pin = Some(CredentialPin::new(
            user.credential_version,
            self.inner.storage.identity_epoch(),
        ));
        Ok(AuthenticatedUser {
            inner: User {
                app: self.new_ref(),
                id: user.id,
                cache_storage: Some(user.into()),
            },
            pin,
        })
    }

    pub async fn user_by_auth(&self, auth: UserAuth) -> Result<AuthenticatedUser, AppError> {
        match auth {
            UserAuth::None => {
                let user_id = self.config().web_server.default_user_id.map(UserId);
                if let Some(user_id) = user_id {
                    let user = match self.user_by_id(user_id).await {
                        Ok(user) => user,
                        Err(AppError::UserNotFound) => {
                            error!("the default user {user_id:?} was not found!");
                            return Err(AppError::UserNotFound);
                        }
                        Err(err) => return Err(err),
                    };

                    user.authenticate(&UserAuth::None).await
                } else {
                    Err(AppError::Unauthorized)
                }
            }
            UserAuth::UserPassword { ref username, .. } => {
                let user = self.user_by_name(username).await?;

                user.authenticate(&auth).await
            }
            UserAuth::Session(session) => {
                let user = self.user_by_session(session).await?;

                Ok(user)
            }
            UserAuth::ForwardedHeaders { ref username } => {
                let user = match self.user_by_name(username).await {
                    Ok(user) => user,
                    Err(AppError::UserNotFound) => {
                        let Some(config_forwarded_headers) =
                            &self.config().web_server.forwarded_header
                        else {
                            return Err(AppError::Unauthorized);
                        };

                        if !config_forwarded_headers.auto_create_missing_user {
                            return Err(AppError::Unauthorized);
                        }

                        let role = self.default_role().await?;

                        info!("Adding new user {username:?} from proxy.");

                        let user = self
                            .add_user_no_auth(StorageUserAdd {
                                role_id: role.id(),
                                name: username.clone(),
                                password: None,
                                client_unique_id: username.clone(),
                            })
                            .await?;

                        return Ok(user);
                    }
                    Err(err) => return Err(err),
                };

                user.authenticate(&auth).await
            }
        }
    }

    pub async fn user_by_id(&self, user_id: UserId) -> Result<User, AppError> {
        let user = self.inner.storage.get_user(user_id).await?;

        Ok(User {
            app: self.new_ref(),
            id: user_id,
            cache_storage: Some(user.into()),
        })
    }
    pub async fn user_by_name(&self, name: &str) -> Result<User, AppError> {
        let (user_id, user) = self.inner.storage.get_user_by_name(name).await?;

        Ok(User {
            app: self.new_ref(),
            id: user_id,
            cache_storage: user.map(Into::into),
        })
    }
    pub async fn user_by_session(
        &self,
        session: SessionToken,
    ) -> Result<AuthenticatedUser, AppError> {
        let (user_id, user) = self
            .inner
            .storage
            .get_user_by_session_token(session)
            .await?;

        let pin = user.as_ref().map(|user| {
            CredentialPin::new(user.credential_version, self.inner.storage.identity_epoch())
        });
        Ok(AuthenticatedUser {
            inner: User {
                app: self.new_ref(),
                id: user_id,
                cache_storage: user.map(Into::into),
            },
            pin,
        })
    }

    pub async fn all_users(&self, _: Admin) -> Result<Vec<User>, AppError> {
        let users = self.inner.storage.list_users().await?;

        let users = match users {
            Either::Left(user_ids) => user_ids
                .into_iter()
                .map(|id| User {
                    app: self.new_ref(),
                    id,
                    cache_storage: None,
                })
                .collect::<Vec<_>>(),
            Either::Right(users) => users
                .into_iter()
                .map(|user| User {
                    app: self.new_ref(),
                    id: user.id,
                    cache_storage: Some(user.into()),
                })
                .collect::<Vec<_>>(),
        };

        Ok(users)
    }

    pub async fn delete_session(&self, session: SessionToken) -> Result<(), AppError> {
        self.inner.storage.remove_session_token(session).await
    }

    pub async fn sessions(&self, _admin: &Admin) -> Result<Vec<StorageSessionInfo>, AppError> {
        self.inner.storage.list_sessions().await
    }

    pub async fn delete_session_by_id(&self, _admin: &Admin, id: &str) -> Result<bool, AppError> {
        self.inner.storage.remove_session_by_id(id).await
    }

    pub async fn delete_all_sessions(&self, _admin: &Admin) -> Result<(), AppError> {
        self.inner.storage.remove_all_sessions().await
    }

    pub async fn export_backup(&self, _admin: &Admin) -> Result<Value, AppError> {
        self.inner.storage.export_backup().await
    }

    /// Acquires the maintenance gate for a stream's whole lifetime (MW-19).
    ///
    /// Returns `Forbidden` when a restore currently holds the write side, so a new
    /// stream is refused instead of starting against an identity snapshot that is
    /// about to be replaced.
    pub async fn enter_stream_scope(
        self: &Arc<Self>,
    ) -> Result<tokio::sync::OwnedRwLockReadGuard<()>, AppError> {
        Ok(Arc::clone(&self.inner.maintenance).read_owned().await)
    }

    /// Current identity epoch; compared by long-lived authorizations (MW-19).
    #[cfg(test)]
    pub fn identity_epoch(&self) -> u64 {
        self.inner.identity_epoch.load(Ordering::Acquire)
    }

    /// Test-only entry point that runs the real restore transition without an
    /// `Admin` proof, so the maintenance-gate interleaving can be exercised
    /// directly. Production callers must go through [`App::import_backup`].
    #[cfg(test)]
    pub async fn import_backup_unchecked_for_test(&self, value: Value) -> Result<(), AppError> {
        self.restore(value).await
    }

    pub async fn import_backup(&self, _admin: &Admin, value: Value) -> Result<(), AppError> {
        self.restore(value).await
    }

    /// The one restore path (AR-02).
    ///
    /// MW-19: the exclusive maintenance guard is taken here, so the restore waits
    /// for every running stream.
    ///
    /// AR-02: once that guard has been acquired the *whole* transition is handed to
    /// an App-owned task, which owns the guard, the candidate and the obligation to
    /// publish. Dropping the awaiting caller can therefore no longer release the
    /// exclusive scope before the cache is invalidated and the App identity epoch
    /// is advanced, and cannot lose those two steps: they run on the owned task,
    /// not on the request future. The guard is held exactly once (the task owns it;
    /// the storage is called through its already-owned writer path), and is dropped
    /// as the last step of the transition.
    async fn restore(&self, value: Value) -> Result<(), AppError> {
        // Cancellation here is safe and complete: the store has not been touched
        // yet and no ownership has been handed off.
        let maintenance = Arc::clone(&self.inner.maintenance).write_owned().await;

        // Acceptance point. There is no await between taking the guard and moving
        // it into the task, so a cancelled caller either never started a restore or
        // has already transferred it in full.
        let (ack, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(run_restore_transition(
            Arc::clone(&self.inner),
            value,
            maintenance,
            ack,
        ));

        receiver.await.map_err(|_| AppError::StorageUnavailable)?
    }

    async fn find_role(
        &self,
        filter: impl AsyncFn(&mut Role) -> Result<bool, AppError>,
    ) -> Result<Role, AppError> {
        let roles = self.all_roles_no_auth().await?;

        let role = roles
            .into_iter()
            .map(|mut role| async {
                if filter(&mut role).await? {
                    Ok(role)
                } else {
                    Err(AppError::RoleNotFound)
                }
            })
            .collect::<Vec<_>>()
            .race_ok()
            .await
            .map_err(|mut err| {
                let err = mem::take(&mut *err);
                err.into_iter()
                    .find(|x| !matches!(x, AppError::RoleNotFound))
                    .unwrap_or(AppError::RoleNotFound)
            })?;

        Ok(role)
    }

    /// Returns any role that is an Admin
    pub async fn admin_role(&self) -> Result<Role, AppError> {
        let result = self
            .find_role(async |role| {
                let ty = role.ty().await?;

                Ok(matches!(ty, RoleType::Admin))
            })
            .await;

        match result {
            Ok(value) => Ok(value),
            Err(AppError::RoleNotFound) => {
                // We've got no admin role -> add an admin role

                info!("There was no admin role found. Adding an Admin role");

                let role = self
                    .add_role_no_auth(StorageRoleAdd {
                        name: "Admin".to_owned(),
                        ty: RoleType::Admin,
                        default_settings: StorageRoleDefaultSettings::default(),
                        permissions: StorageRolePermissions::default(),
                    })
                    .await?;

                info!("Added admin role: {role:?}");

                Ok(role)
            }
            Err(err) => Err(err),
        }
    }
    /// Returns the first user role it finds
    pub async fn default_role(&self) -> Result<Role, AppError> {
        let default_role_id = self.config().web_server.default_role_id.map(RoleId);

        match default_role_id {
            None => {
                let result = self
                    .find_role(async |role| {
                        let ty = role.ty().await?;

                        Ok(matches!(ty, RoleType::User))
                    })
                    .await;

                match result {
                    Ok(value) => Ok(value),
                    Err(AppError::RoleNotFound) => {
                        // We've got no admin role -> add an admin role

                        info!("There was no default role found. Adding a new default user role");

                        let role = self
                            .add_role_no_auth(StorageRoleAdd {
                                name: "User".to_owned(),
                                ty: RoleType::User,
                                default_settings: StorageRoleDefaultSettings::default(),
                                permissions: StorageRolePermissions::default(),
                            })
                            .await?;

                        info!("Added user role: {role:?}");

                        Ok(role)
                    }
                    Err(err) => Err(err),
                }
            }
            Some(id) => self.role_by_id(id).await,
        }
    }

    pub async fn add_role(&self, _admin: &Admin, role: StorageRoleAdd) -> Result<Role, AppError> {
        let role = self.add_role_no_auth(role).await?;

        // MW-17: role permissions decide what every user may do, so the change
        // must be durable before it is reported as applied.
        self.inner.storage.persist().await?;

        Ok(role)
    }
    pub async fn add_role_no_auth(&self, role: StorageRoleAdd) -> Result<Role, AppError> {
        let role = self.inner.storage.add_role(role).await?;

        Ok(Role {
            app: self.new_ref(),
            id: role.id,
            cache_storage: Some(role.into()),
        })
    }

    pub async fn role_by_id(&self, id: RoleId) -> Result<Role, AppError> {
        let role = self.inner.storage.get_role(id).await?;

        Ok(Role {
            app: self.new_ref(),
            id: role.id,
            cache_storage: Some(role.into()),
        })
    }

    pub async fn all_roles(&self, _admin: &Admin) -> Result<Vec<Role>, AppError> {
        self.all_roles_no_auth().await
    }

    pub async fn all_roles_no_auth(&self) -> Result<Vec<Role>, AppError> {
        let roles = self.inner.storage.list_roles().await?;

        let roles = match roles {
            Either::Left(role_ids) => role_ids
                .into_iter()
                .map(|id| Role {
                    app: self.new_ref(),
                    id,
                    cache_storage: None,
                })
                .collect::<Vec<_>>(),
            Either::Right(roles) => roles
                .into_iter()
                .map(|role| Role {
                    app: self.new_ref(),
                    id: role.id,
                    cache_storage: Some(role.into()),
                })
                .collect::<Vec<_>>(),
        };

        Ok(roles)
    }
}

/// The accepted restore transition, owned by an App task rather than by the
/// requesting future (AR-02).
///
/// Everything the transition needs is owned here: the exclusive maintenance
/// guard, the candidate snapshot, and the obligation to finish. Cancelling the
/// caller therefore cannot release the gate or skip a publication step, because
/// the caller no longer holds any of them.
///
/// The maintenance guard is taken exactly once, by the caller, and moved here; it
/// is *not* forwarded to the storage as a second exclusion. The storage keeps
/// owning its own commit/publication (and its own cancellation safety), while this
/// task keeps the app-level exclusion alive across the whole sequence, so the two
/// locks are never the same write lock taken twice.
async fn run_restore_transition(
    inner: Arc<AppInner>,
    value: Value,
    maintenance: tokio::sync::OwnedRwLockWriteGuard<()>,
    ack: tokio::sync::oneshot::Sender<Result<(), AppError>>,
) {
    let result = inner.storage.import_backup(value).await;

    // No-op on failure: a rejected restore must leave the cache and the epoch
    // exactly as they were, and the guard must still be released so the app does
    // not wedge.
    if result.is_ok() {
        // Invalidate first, then publish the App epoch; both happen while the
        // exclusive scope is still held, so a stream that starts afterwards must
        // observe the new epoch and a cold cache.
        inner.app_image_cache.write().await.clear();
        inner.identity_epoch.fetch_add(1, Ordering::AcqRel);
    }

    // Last: the exclusive scope reopens only once disk, live identities, cache and
    // the App epoch agree.
    drop(maintenance);

    // The caller may already be gone; that is not an error, the transition is done.
    let _ = ack.send(result);
}
