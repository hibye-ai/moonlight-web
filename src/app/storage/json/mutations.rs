//! AR-03 / AR-05: transactional identity and host mutations.
//!
//! These paths used to validate against the live tables, mutate them, and only
//! then ask the writer to persist "eventually" through `force_write`. Two defects
//! followed from that shape:
//!
//! * a reference check that read the role table outside the insert itself could
//!   commit a user against a role deleted in between, and a direct user delete
//!   left `host.owner` pointing at a user that no longer existed (AR-03);
//! * a failed disk write was still reported as success, and a caller that stopped
//!   awaiting half-way could leave disk and live state split (AR-05).
//!
//! Every mutation here therefore runs as one writer-owned transaction: build a
//! *candidate* snapshot under the identity locks, validate the candidate, commit
//! it durably, then publish it and revoke sessions. Because the owned writer
//! performs the work, dropping the calling future can no longer split the
//! transaction; because the candidate is validated as a whole, reference and
//! last-admin checks belong to the same linearization as the write.
//!
//! Lock order is unchanged from the rest of the module: `store_lock` -> users ->
//! hosts -> roles -> sessions. The role-deletion reservation mutex is a leaf lock:
//! it is only held for a set insert/contains and never across an await.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex as StdMutex},
};

use super::*;

/// Shared record of role deletions that have been *admitted* but not committed.
///
/// A role deletion is admitted when its request is handed to the writer; from
/// that point the role is going away and a user creation must not reference it.
/// Checking the reservation and inserting the user happen inside one transaction,
/// so the two orderings are the only possible outcomes: either the creation
/// commits first (and the cascade removes the user with its role), or the
/// deletion is already admitted and the creation is refused.
#[derive(Default)]
pub(super) struct RoleDeletionReservations {
    reserved: StdMutex<HashSet<u32>>,
}

impl RoleDeletionReservations {
    fn guard(&self) -> std::sync::MutexGuard<'_, HashSet<u32>> {
        // A poisoning panic can only have happened between two set operations, so
        // the set itself is still consistent; recovering keeps a cancellation or
        // failure from wedging every later mutation.
        self.reserved.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Admits a deletion of `role_id`. The reservation is released when the guard
    /// is dropped: after the writer commits or rejects, or immediately when the
    /// request is cancelled before the handoff.
    pub(super) fn reserve(self: &Arc<Self>, role_id: u32) -> RoleDeletionReservation {
        self.guard().insert(role_id);
        RoleDeletionReservation {
            reservations: Arc::clone(self),
            role_id,
        }
    }

    fn contains(&self, role_id: u32) -> bool {
        self.guard().contains(&role_id)
    }
}

/// Owned reservation guard: it travels with the writer request, so the admission
/// lasts exactly as long as the deletion is pending or committing.
pub(super) struct RoleDeletionReservation {
    reservations: Arc<RoleDeletionReservations>,
    role_id: u32,
}

impl Drop for RoleDeletionReservation {
    fn drop(&mut self) {
        self.reservations.guard().remove(&self.role_id);
    }
}

/// One transactional mutation, owned by the writer once it is queued.
pub(super) enum Mutation {
    AddUser(StorageUserAdd),
    AddFirstUser(StorageUserAdd),
    ModifyUser {
        user_id: UserId,
        modify: StorageUserModify,
    },
    RemoveUser {
        user_id: UserId,
    },
    AddHost(StorageHostAdd),
    ModifyHost {
        host_id: HostId,
        modify: StorageHostModify,
    },
    RemoveHost {
        host_id: HostId,
    },
}

/// What the caller needs back from a committed mutation.
pub(super) enum MutationOutcome {
    User(StorageUser),
    Host(StorageHost),
    Done,
}

impl MutationOutcome {
    pub(super) fn into_user(self) -> Result<StorageUser, AppError> {
        match self {
            Self::User(user) => Ok(user),
            _ => Err(AppError::StorageUnavailable),
        }
    }

    pub(super) fn into_host(self) -> Result<StorageHost, AppError> {
        match self {
            Self::Host(host) => Ok(host),
            _ => Err(AppError::StorageUnavailable),
        }
    }

    pub(super) fn into_done(self) -> Result<(), AppError> {
        match self {
            Self::Done => Ok(()),
            _ => Err(AppError::StorageUnavailable),
        }
    }
}

fn password_to_json(password: StoragePassword) -> V2UserPassword {
    V2UserPassword {
        salt: password.salt,
        hash: password.hash,
        iterations: password.iterations,
    }
}

fn pair_info_to_json(pair_info: StorageHostPairInfo) -> V2HostPairInfo {
    V2HostPairInfo {
        client_private_key: pair_info.client_private_key,
        client_certificate: pair_info.client_certificate,
        server_certificate: pair_info.server_certificate,
    }
}

/// Whether the candidate store keeps an admin that could still sign in.
///
/// This is the same rule the role cascade applies for a direct user removal: a
/// store with no passwords at all is header-auth only and is left alone, and
/// otherwise at least one password-capable admin must survive.
fn keeps_login_capable_admin(data: &V3, exclude: Option<u32>) -> bool {
    let password_required = data.users.values().any(|user| user.password.is_some());
    if !password_required {
        return true;
    }

    data.users.iter().any(|(id, user)| {
        Some(*id) != exclude
            && user.password.is_some()
            && data
                .roles
                .get(&user.role_id)
                .is_some_and(|role| matches!(role.ty, V3RoleType::Admin))
    })
}

/// Commits the staged high-water marks to the live atomics.
///
/// Called only after the candidate is durable and while the writer still holds
/// `store_lock`, so no other allocator can have moved them in between. `fetch_max`
/// makes the marks monotonic: a transaction can raise them but never lower or drop
/// them, whatever the candidate happened to contain.
fn publish_counters(storage: &JsonStorage, data: &V3) {
    JsonStorage::raise_counter(
        &storage.next_credential_version,
        data.next_credential_version,
    );
    JsonStorage::raise_counter(&storage.next_user_id, data.next_user_id);
}

/// Allocates an account id from the *candidate* counter (AR-01).
///
/// The caller's `users` map is the set the id must be free of, and the counter is
/// advanced inside the candidate only: a mutation that fails to commit therefore
/// leaves the live allocator exactly where it was, and an id is never handed out
/// twice because the counter can only move forward.
fn allocate_staged_user_id(data: &mut V3) -> Result<u32, AppError> {
    // A file written by an older build may hold ids above the persisted counter, so
    // start above everything the candidate already contains.
    let in_use = data
        .users
        .keys()
        .map(|id| u64::from(*id).saturating_add(1))
        .max()
        .unwrap_or(0);
    let mut next = data.next_user_id.max(in_use);
    loop {
        // Ids are 32 bit on the wire; a store that exhausted the space cannot be
        // served safely, so refuse rather than wrap into a reinstated id.
        let candidate = u32::try_from(next).map_err(|_| AppError::StorageUnavailable)?;
        next = next.saturating_add(1);
        if !data.users.contains_key(&candidate) {
            data.next_user_id = next;
            return Ok(candidate);
        }
    }
}

/// Hands out a credential generation that has never been used in this store (AR-01).
///
/// Same rule as `JsonStorage::allocate_credential_version`, but applied to the
/// staged candidate: the value is strictly above both the persisted high-water mark
/// and `at_least`, and the candidate counter is advanced past it. A restore that
/// installs lower generations therefore cannot make a later rotation reuse one.
fn allocate_staged_credential_version(data: &mut V3, at_least: u64) -> u64 {
    let in_use = data
        .users
        .values()
        .map(|user| user.credential_version.saturating_add(1))
        .max()
        .unwrap_or(0);
    let next = data.next_credential_version.max(at_least).max(in_use);
    data.next_credential_version = next.saturating_add(1);
    next
}

/// Records that a generation is now stored, without minting a new one.
///
/// A newly created account deliberately still starts at generation zero; this only
/// keeps the persisted high-water mark above it, matching
/// `JsonStorage::reserve_credential_version`.
fn reserve_staged_credential_version(data: &mut V3, version: u64) {
    data.next_credential_version = data.next_credential_version.max(version.saturating_add(1));
}

impl JsonStorage {
    /// Accepts a mutation into the owned writer and waits for its real outcome.
    ///
    /// Sending is the ownership handoff: from then on the writer finishes the
    /// transaction even if this future is dropped.
    pub(super) async fn mutate(&self, mutation: Mutation) -> Result<MutationOutcome, AppError> {
        let (ack, receiver) = oneshot::channel();

        self.store_sender
            .send(StoreRequest::Mutation { mutation, ack })
            .await
            .map_err(|_| AppError::StorageUnavailable)?;

        receiver.await.map_err(|_| AppError::StorageUnavailable)?
    }

    /// Stages, commits and publishes one mutation. Only the owned writer calls this.
    pub(super) async fn apply_mutation(
        &self,
        mutation: Mutation,
    ) -> Result<MutationOutcome, AppError> {
        // Serialize with the other writer transactions (persist, role cascade,
        // restore) and hold the complete identity snapshot in the documented lock
        // order across the disk commit.
        let _store_guard = self.store_lock.lock().await;
        let mut users = self.users.write().await;
        let mut hosts = self.hosts.write().await;
        let mut roles = self.roles.write().await;
        let mut sessions = self.sessions.write().await;

        // AR-01 provenance: the candidate must carry the persisted identity epoch and
        // the allocator high-water marks, otherwise this transaction would write a
        // snapshot that drops them and the next restore/reload would restart them
        // from zero. They are the live values here; the allocation helpers below move
        // the counters forward in the candidate only.
        let mut data = V3 {
            users: HashMap::new(),
            hosts: HashMap::new(),
            roles: HashMap::new(),
            identity_epoch: self.identity_epoch.load(AtomicOrdering::Acquire),
            next_credential_version: self.next_credential_version.load(AtomicOrdering::Acquire),
            next_user_id: self.next_user_id.load(AtomicOrdering::Acquire),
        };
        for (id, user) in users.iter() {
            data.users.insert(*id, user.read().await.clone());
        }
        for (id, host) in hosts.iter() {
            data.hosts.insert(*id, host.read().await.clone());
        }
        for (id, role) in roles.iter() {
            data.roles.insert(*id, role.read().await.clone());
        }

        // Session state that may only be dropped once the candidate is durable.
        let mut revoked_user = None;

        let outcome = match mutation {
            Mutation::AddUser(add) => {
                // AR-03: role existence, name uniqueness, the insert and the commit
                // lock are one transaction. A deletion that was already admitted is
                // part of this decision, so an insert can never overtake it and
                // reference a role that is going away.
                if self.role_deletion_reservations.contains(add.role_id.0) {
                    return Err(AppError::RoleNotFound);
                }
                if !data.roles.contains_key(&add.role_id.0) {
                    return Err(AppError::RoleNotFound);
                }
                if data.users.values().any(|user| user.name == add.name) {
                    return Err(AppError::UserAlreadyExists);
                }

                // AR-01: ids come from the persisted monotonic allocator, never a
                // random draw. A reinstated id would let a credential pin taken from a
                // removed account be replayed against its replacement. The counter is
                // advanced in the *candidate*, so a failed commit publishes nothing.
                let id = allocate_staged_user_id(&mut data)?;

                let user = V3User {
                    role_id: add.role_id.0,
                    name: add.name,
                    password: add.password.map(password_to_json),
                    client_unique_id: add.client_unique_id,
                    credential_version: 0,
                };
                data.users.insert(id, user.clone());
                // Generation zero is now in use; record it while the candidate is
                // still being built so the persisted counter cannot lag behind it.
                reserve_staged_credential_version(&mut data, user.credential_version);

                MutationOutcome::User(user_from_json(UserId(id), &user))
            }
            Mutation::AddFirstUser(add) => {
                // "Only while the store is empty" is still the rule, but the check,
                // the insert and the durable commit are one transition, so a
                // successful bootstrap implies a durable account.
                if self.role_deletion_reservations.contains(add.role_id.0) {
                    return Err(AppError::RoleNotFound);
                }
                if !data.users.is_empty() {
                    return Err(AppError::FirstUserAlreadyExists);
                }
                if !data.roles.contains_key(&add.role_id.0) {
                    return Err(AppError::RoleNotFound);
                }

                // AR-01: the bootstrap account obeys the same non-repeating id rule as
                // every other account, so a pin cannot survive a delete/re-bootstrap
                // cycle. A wipe-and-rebootstrap therefore never reinstates id 0.
                let id = allocate_staged_user_id(&mut data)?;

                let user = V3User {
                    role_id: add.role_id.0,
                    name: add.name,
                    password: add.password.map(password_to_json),
                    client_unique_id: add.client_unique_id,
                    credential_version: 0,
                };
                data.users.insert(id, user.clone());
                reserve_staged_credential_version(&mut data, user.credential_version);

                MutationOutcome::User(user_from_json(UserId(id), &user))
            }
            Mutation::ModifyUser { user_id, modify } => {
                // AR-03: a role change is the same reference decision as a
                // creation, so it validates the target role against the candidate
                // inside this transaction instead of against a table that a
                // concurrent cascade may already be deleting.
                if let Some(role_id) = modify.role_id
                    && (self.role_deletion_reservations.contains(role_id.0)
                        || !data.roles.contains_key(&role_id.0))
                {
                    return Err(AppError::RoleNotFound);
                }

                let Some(current_role) = data.users.get(&user_id.0).map(|user| user.role_id) else {
                    return Err(AppError::UserNotFound);
                };

                // Demoting the last login-capable admin is still refused, and the
                // check now runs on the candidate the write will produce.
                if let Some(role_id) = modify.role_id
                    && current_role != role_id.0
                {
                    let currently_admin = data
                        .roles
                        .get(&current_role)
                        .is_some_and(|role| matches!(role.ty, V3RoleType::Admin));
                    let target_is_admin = data
                        .roles
                        .get(&role_id.0)
                        .is_some_and(|role| matches!(role.ty, V3RoleType::Admin));

                    if currently_admin
                        && !target_is_admin
                        && !keeps_login_capable_admin(&data, Some(user_id.0))
                    {
                        warn!("Refusing user demotion: it would leave no admin able to log in");
                        return Err(AppError::Forbidden);
                    }
                }

                let mut credential_changed = false;
                let previous_version;

                {
                    let Some(user) = data.users.get_mut(&user_id.0) else {
                        return Err(AppError::UserNotFound);
                    };

                    if let Some(password) = modify.password {
                        user.password = password.map(password_to_json);
                    }
                    if let Some(role_id) = modify.role_id {
                        credential_changed |= user.role_id != role_id.0;
                        user.role_id = role_id.0;
                    }
                    if let Some(client_unique_id) = modify.client_unique_id {
                        user.client_unique_id = client_unique_id;
                    }

                    previous_version = user.credential_version;
                }

                // AR-01 semantics are unchanged: the generation follows the
                // revocation decision, and a transparent rehash never comes through
                // here. The value comes from the *persisted* high-water mark rather
                // than `previous + 1` (B's earlier `wrapping_add`), because a restore
                // can install a lower generation and an incremented value could then
                // collide with one a concurrent credential check is pinned to.
                credential_changed |= modify.revoke_sessions;
                if credential_changed {
                    let at_least = previous_version.saturating_add(1);
                    let version = allocate_staged_credential_version(&mut data, at_least);
                    if let Some(user) = data.users.get_mut(&user_id.0) {
                        user.credential_version = version;
                    }
                }

                if modify.revoke_sessions {
                    revoked_user = Some(user_id.0);
                }

                MutationOutcome::Done
            }
            Mutation::RemoveUser { user_id } => {
                if !data.users.contains_key(&user_id.0) {
                    return Err(AppError::UserNotFound);
                }
                // Last-admin protection is unchanged; it is evaluated on the
                // candidate so the decision and the removal cannot be split.
                if !keeps_login_capable_admin(&data, Some(user_id.0)) {
                    warn!("Refusing user deletion: it would leave no admin able to log in");
                    return Err(AppError::Forbidden);
                }

                data.users.remove(&user_id.0);

                // AR-03: a direct deletion leaves the same footprint as the role
                // cascade. Hosts the user owned become global instead of keeping a
                // dangling owner that makes the app reject its own exported backup.
                for host in data.hosts.values_mut() {
                    if host.owner == Some(user_id.0) {
                        host.owner = None;
                    }
                }

                revoked_user = Some(user_id.0);
                MutationOutcome::Done
            }
            Mutation::AddHost(add) => {
                // The cascade policy only ever leaves real owner ids behind, so a
                // created host may not reference a user that does not exist.
                if let Some(owner) = add.owner
                    && !data.users.contains_key(&owner.0)
                {
                    return Err(AppError::UserNotFound);
                }

                let mut id;
                loop {
                    id = random_number()?;
                    if !data.hosts.contains_key(&id) {
                        break;
                    }
                }

                let host = V2Host {
                    owner: add.owner.map(|user_id| user_id.0),
                    address: add.address,
                    http_port: add.http_port,
                    pair_info: add.pair_info.map(pair_info_to_json),
                    cache: V2HostCache {
                        name: add.cache.name,
                        mac: add.cache.mac,
                    },
                };
                data.hosts.insert(id, host.clone());

                MutationOutcome::Host(host_from_json(HostId(id), &host))
            }
            Mutation::ModifyHost { host_id, modify } => {
                // Ownership, pair info and the cached server name/mac are all
                // written through here, so all of them share one commit boundary.
                let Some(host) = data.hosts.get_mut(&host_id.0) else {
                    return Err(AppError::HostNotFound);
                };

                if let Some(new_owner) = modify.owner {
                    if let Some(new_owner) = new_owner
                        && !data.users.contains_key(&new_owner.0)
                    {
                        return Err(AppError::UserNotFound);
                    }
                    host.owner = new_owner.map(|user_id| user_id.0);
                }
                if let Some(new_address) = modify.address {
                    host.address = new_address;
                }
                if let Some(new_http_port) = modify.http_port {
                    host.http_port = new_http_port;
                }
                if let Some(new_pair_info) = modify.pair_info {
                    host.pair_info = new_pair_info.map(pair_info_to_json);
                }
                if let Some(new_cache_name) = modify.cache_name {
                    host.cache.name = new_cache_name;
                }
                if let Some(new_cache_mac) = modify.cache_mac {
                    host.cache.mac = new_cache_mac;
                }

                MutationOutcome::Done
            }
            Mutation::RemoveHost { host_id } => {
                if data.hosts.remove(&host_id.0).is_none() {
                    return Err(AppError::HostNotFound);
                }

                MutationOutcome::Done
            }
        };

        let text = serde_json::to_string_pretty(&Json::V3(data.clone()))
            .map_err(|_| AppError::StorageUnavailable)?;

        // Nothing live is touched until the candidate is durable, so a failed or
        // interrupted write cannot publish a half-applied identity/host change.
        atomic_replace_staged_file(&self.file, &text).await?;

        // The candidate is durable, so the high-water marks it reserved become the
        // live ones. This is the only place they are published, and `store_lock` is
        // still held, so an allocation cannot have run in between. A failed write
        // above therefore leaves every published counter untouched, and a host-only
        // mutation cannot lower or drop the identity marks it carried.
        publish_counters(self, &data);

        // No await points between the durable commit and publishing every table.
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
        if let Some(user_id) = revoked_user {
            sessions.retain(|_, session| session.user_id != user_id);
        }

        Ok(outcome)
    }
}
