use std::{
    fmt::{Debug, Display, Formatter},
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

use common::api_bindings::{self, DetailedUser};
use moonlight_common::{
    high::MoonlightClientError,
    http::{
        ClientInfo,
        client::{RequestError, async_client::RequestClient},
        server_info::{ServerInfoEndpoint, ServerInfoRequest},
    },
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{
    AppError, AppRef, MoonlightClient,
    auth::{SessionToken, UserAuth},
    host::{Host, HostId},
    password::StoragePassword,
    role::{Role, RoleId},
    storage::{
        CredentialPin, StorageHostAdd, StorageHostCache, StorageQueryHosts, StorageUser,
        StorageUserModify,
    },
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RoleType {
    User,
    Admin,
}

impl From<RoleType> for api_bindings::RoleType {
    fn from(value: RoleType) -> Self {
        match value {
            RoleType::User => Self::User,
            RoleType::Admin => Self::Admin,
        }
    }
}

impl From<api_bindings::RoleType> for RoleType {
    fn from(value: common::api_bindings::RoleType) -> Self {
        use common::api_bindings::RoleType;

        match value {
            RoleType::User => Self::User,
            RoleType::Admin => Self::Admin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub u32);

impl Display for UserId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone)]
pub struct User {
    pub(super) app: AppRef,
    pub(super) id: UserId,
    pub(super) cache_storage: Option<Arc<StorageUser>>,
}

impl Debug for User {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.id)
    }
}

impl User {
    pub fn id(&self) -> UserId {
        self.id
    }

    async fn storage_user(&mut self) -> Result<Arc<StorageUser>, AppError> {
        if let Some(storage) = self.cache_storage.as_ref() {
            return Ok(storage.clone());
        }

        let app = self.app.access()?;

        let user = app.storage.get_user(self.id).await?;
        let user = Arc::new(user);

        self.cache_storage = Some(user.clone());

        Ok(user)
    }

    pub async fn is_default_user(&self) -> Result<bool, AppError> {
        let app = self.app.access()?;

        Ok(app.config.web_server.default_user_id.map(UserId) == Some(self.id))
    }

    pub async fn role_id(&mut self) -> Result<RoleId, AppError> {
        let storage = self.storage_user().await?;

        Ok(storage.role_id)
    }
    pub async fn role(&mut self) -> Result<Role, AppError> {
        let role = Role {
            app: self.app.clone(),
            id: self.role_id().await?,
            cache_storage: None,
        };

        Ok(role)
    }

    pub async fn detailed_user(
        &mut self,
        requesting_user: &mut AuthenticatedUser,
    ) -> Result<DetailedUser, AppError> {
        if requesting_user.role().await?.ty().await? == RoleType::Admin
            || self.id() == requesting_user.id()
        {
            self.detailed_user_no_auth().await
        } else {
            Err(AppError::Forbidden)
        }
    }
    pub async fn detailed_user_no_auth(&mut self) -> Result<DetailedUser, AppError> {
        let storage = self.storage_user().await?;
        let role = self.role().await?.ty().await?;

        Ok(DetailedUser {
            id: self.id.0,
            is_default_user: self.is_default_user().await?,
            name: storage.name.clone(),
            role_id: storage.role_id.0,
            role: role.into(),
            client_unique_id: storage.client_unique_id.clone(),
        })
    }

    pub async fn modify(&mut self, _: &Admin, modify: StorageUserModify) -> Result<(), AppError> {
        let app = self.app.access()?;

        self.cache_storage = None;

        app.storage.modify_user(self.id, modify).await?;

        // MW-17: a password/role change must be on disk before we report success.
        app.storage.persist().await?;

        Ok(())
    }
    pub async fn delete(self, _: &Admin) -> Result<(), AppError> {
        let app = self.app.access()?;

        app.storage.remove_user(self.id).await?;
        app.storage.persist().await?;

        Ok(())
    }

    pub async fn authenticate(mut self, auth: &UserAuth) -> Result<AuthenticatedUser, AppError> {
        match auth {
            UserAuth::None if self.is_default_user().await? => Ok(AuthenticatedUser {
                inner: self,
                pin: None,
            }),
            UserAuth::UserPassword { username, password } => {
                let storage = self.storage_user().await?;

                if username.as_str() != storage.name.as_str() {
                    return Err(AppError::Unauthorized);
                }

                // MW-23: PBKDF2 costs tens of milliseconds of CPU at the current
                // iteration count. It must not run on the reactor worker, and it must
                // not be possible to start more of them at once than the bounded pool
                // allows. The stored hash format and the accept/reject answer are
                // unchanged; the legacy rehash is computed in the same blocking task.
                let Some(storage_password) = storage.password.as_ref() else {
                    return Err(AppError::CredentialsWrong);
                };

                let app = self.app.access()?;

                // AR-01: capture the identity incarnation the snapshot was read in
                // *before* releasing the reactor for the PBKDF2 wait, so every later
                // step can prove it still describes the same generation and the same
                // identity snapshot.
                let observed_epoch = app.storage.identity_epoch();
                let observed_version = storage.credential_version;

                // Test-only: let an interleaving regression place its rotation
                // exactly between "snapshot read" and "credential checked". No-op in
                // production (compiled out) and when no test has armed the gate.
                #[cfg(test)]
                app.credential_check_gate.arrive(username).await;

                let verification = app
                    .password_checks
                    .verify(storage_password.clone(), password.clone())
                    .await?;

                if !verification.verified {
                    return Err(AppError::CredentialsWrong);
                }

                // AR-01: the check above ran off-reactor, so an entire password
                // rotation, role change, deletion or restore can have landed since
                // this snapshot was read. Re-read the account and require that the
                // credential we just accepted is still the current one. Without
                // this, a verification that started before a rotation would convert
                // into a brand new valid session after it.
                //
                // The generation is the authority for "is this the same credential
                // set as when the password was checked"; a *transparent* rehash
                // deliberately keeps it (see `Storage::rehash_password`), so it
                // cannot cause a spurious rejection of a concurrent correct login.
                // A password that was removed out of band is refused explicitly.
                let current = app.storage.get_user(self.id).await?;
                let current_epoch = app.storage.identity_epoch();
                if current.credential_version != observed_version
                    || current_epoch != observed_epoch
                    || current.password.is_none()
                {
                    return Err(AppError::CredentialsWrong);
                }

                let user_id = self.id;
                let mut authenticated = AuthenticatedUser {
                    inner: self,
                    // Pin both the generation and the identity incarnation the
                    // credential was actually verified against, so `new_session`
                    // cannot launder a stale verification into a usable token either.
                    pin: Some(CredentialPin::new(observed_version, observed_epoch)),
                };

                if let Some(rehashed) = verification.rehash {
                    // AR-04: a transparent upgrade is a compare-and-swap on the
                    // generation that was verified, and it does not revoke sessions.
                    // If a rotation raced this login the CAS fails and the login is
                    // refused, so a stale rehash can never overwrite the new
                    // password.
                    match app
                        .storage
                        .rehash_password(user_id, observed_version, rehashed)
                        .await
                    {
                        Ok(()) => authenticated.cache_storage = None,
                        Err(AppError::CredentialsWrong) => {
                            return Err(AppError::CredentialsWrong);
                        }
                        Err(err) => return Err(err),
                    }
                }
                Ok(authenticated)
            }
            UserAuth::Session(session) => {
                let app = self.app.access()?;

                let (id, user) = app.storage.get_user_by_session_token(*session).await?;

                if self.id != id {
                    return Err(AppError::SessionTokenNotFound);
                }

                let pin = user.as_ref().map(|user| {
                    CredentialPin::new(user.credential_version, app.storage.identity_epoch())
                });
                self.cache_storage = self.cache_storage.or(user.map(Arc::new));

                Ok(AuthenticatedUser { inner: self, pin })
            }
            UserAuth::ForwardedHeaders { username } => {
                let app = self.app.access()?;

                if app.config.web_server.forwarded_header.is_none() {
                    return Err(AppError::HeaderAuthDisabled);
                }

                let storage = self.storage_user().await?;
                if storage.name.as_str() == username.as_str() {
                    Ok(AuthenticatedUser {
                        inner: self,
                        pin: None,
                    })
                } else {
                    Err(AppError::Forbidden)
                }
            }
            _ => Err(AppError::Unauthorized),
        }
    }
}

#[derive(Clone)]
pub struct AuthenticatedUser {
    pub(super) inner: User,
    /// The credential generation and identity epoch this request was authenticated
    /// against (AR-01).
    ///
    /// `None` means the authentication did not depend on a password/hash snapshot
    /// (default user, trusted forwarded header), so there is nothing to pin.
    pub(super) pin: Option<CredentialPin>,
}

impl Deref for AuthenticatedUser {
    type Target = User;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for AuthenticatedUser {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl AuthenticatedUser {
    pub async fn detailed_user(&mut self) -> Result<DetailedUser, AppError> {
        self.detailed_user_no_auth().await
    }

    pub async fn set_password(&mut self, password: StoragePassword) -> Result<(), AppError> {
        let app = self.app.access()?;

        self.cache_storage = None;

        app.storage
            .modify_user(
                self.id,
                StorageUserModify {
                    password: Some(Some(password)),
                    // A user-requested password change ends the account's other
                    // sessions so a stolen cookie cannot outlive the rotation.
                    revoke_sessions: true,
                    ..Default::default()
                },
            )
            .await?;

        // The password rotation must be durable before it is acknowledged.
        app.storage.persist().await?;

        Ok(())
    }

    pub async fn new_session(&self, expiration: Duration) -> Result<SessionToken, AppError> {
        let app = self.app.access()?;

        // AR-01: when this handle came from a credential check, the token may only
        // be minted while the account is still at the generation that was verified
        // *and* the store is still the identity incarnation that verification
        // happened in. The storage performs every check and the insert as one
        // transition.
        let token = app
            .storage
            .create_session_token_pinned(self.id, expiration, self.pin)
            .await?;

        Ok(token)
    }

    pub async fn host_unique_id(&mut self) -> Result<String, AppError> {
        let user = self.storage_user().await?;

        Ok(user.client_unique_id.clone())
    }

    pub async fn hosts(&mut self) -> Result<Vec<Host>, AppError> {
        let app = self.app.access()?;

        let hosts = app
            .storage
            .list_user_hosts(StorageQueryHosts { user_id: self.id })
            .await?
            .into_iter()
            .map(|(host_id, host)| Host {
                app: self.app.clone(),
                id: host_id,
                cache_storage: host,
                cache_host_info: None,
            })
            .collect();

        Ok(hosts)
    }

    pub async fn host(&mut self, host_id: HostId) -> Result<Host, AppError> {
        let app = self.app.access()?;

        let host = app.storage.get_host(host_id).await?;

        if host.owner.is_none() || host.owner == Some(self.id) {
            Ok(Host {
                app: self.app.clone(),
                id: host.id,
                cache_storage: Some(host),
                cache_host_info: None,
            })
        } else {
            Err(AppError::Forbidden)
        }
    }

    pub async fn host_add(&mut self, address: String, http_port: u16) -> Result<Host, AppError> {
        let mut role = self.role().await?;
        if !role.permissions().await?.allow_add_hosts {
            return Err(AppError::Forbidden);
        }

        let unique_id = self.host_unique_id().await?;

        let client = MoonlightClient::with_defaults()
            .map_err(|err| MoonlightClientError::Backend(Box::new(err)))?;

        let info = match client
            .send_http::<ServerInfoEndpoint>(
                ClientInfo {
                    uuid: Uuid::new_v4(),
                    unique_id,
                },
                &format!("{}:{}", address, http_port),
                &ServerInfoRequest {},
            )
            .await
        {
            Ok(info) => info,
            Err(err) if err.is_connect() => {
                return Err(AppError::HostNotFound);
            }
            Err(err) => return Err(MoonlightClientError::Backend(Box::new(err)).into()),
        };

        let app = self.app.access()?;

        let host = app
            .storage
            .add_host(StorageHostAdd {
                owner: Some(self.id),
                address,
                http_port,
                pair_info: None,
                cache: StorageHostCache {
                    name: info.host_name,
                    mac: info.mac,
                },
            })
            .await?;

        Ok(Host {
            app: self.app.clone(),
            id: host.id,
            cache_storage: Some(host),
            cache_host_info: None,
        })
    }

    pub async fn host_delete(&mut self, host_id: HostId) -> Result<(), AppError> {
        let host = self.host(host_id).await?;

        host.delete(self).await?;

        Ok(())
    }

    pub async fn into_admin(self) -> Result<Admin, AppError> {
        match Admin::try_from(self).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(AppError::Forbidden),
            Err(err) => Err(err),
        }
    }
}

pub struct Admin(AuthenticatedUser);

impl Admin {
    pub async fn try_from(
        mut user: AuthenticatedUser,
    ) -> Result<Result<Admin, AuthenticatedUser>, AppError> {
        match user.role().await?.ty().await? {
            RoleType::Admin => Ok(Ok(Self(user))),
            _ => Ok(Err(user)),
        }
    }
}

impl Deref for Admin {
    type Target = AuthenticatedUser;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
