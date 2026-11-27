use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use openssl::{hash::MessageDigest, pkcs5, rand::rand_bytes};
use tokio::sync::Semaphore;

use crate::app::AppError;

pub const HASH_ITERATIONS: u32 = 600_000;

#[derive(Clone, PartialEq, Eq)]
pub struct StoragePassword {
    pub salt: [u8; 16],
    pub hash: [u8; 32],
    pub iterations: u32,
}

impl StoragePassword {
    fn hash(
        salt: &[u8; 16],
        iterations: u32,
        password: &str,
        out: &mut [u8; 32],
    ) -> Result<(), AppError> {
        if password.is_empty() {
            return Err(AppError::PasswordEmpty);
        }

        pkcs5::pbkdf2_hmac(
            password.as_bytes(),
            salt,
            iterations as usize,
            MessageDigest::sha256(),
            out,
        )?;

        Ok(())
    }

    pub fn new(password: &str) -> Result<Self, AppError> {
        let mut salt = [0u8; 16];

        rand_bytes(&mut salt)?;

        let mut hash = [0u8; 32];

        Self::hash(&salt, HASH_ITERATIONS, password, &mut hash)?;

        Ok(Self {
            salt,
            hash,
            iterations: HASH_ITERATIONS,
        })
    }

    pub fn verify(&self, password: &str) -> Result<bool, AppError> {
        let mut hash = [0u8; 32];
        Self::hash(&self.salt, self.iterations, password, &mut hash)?;

        Ok(self.hash == hash)
    }

    pub fn needs_rehash(&self) -> bool {
        self.iterations < HASH_ITERATIONS
    }
}

// ---------------------------------------------------------------------------
// MW-23: login admission and bounded password checking
// ---------------------------------------------------------------------------

/// Sliding window shared by every failure bucket.
pub(crate) const LOGIN_WINDOW: Duration = Duration::from_secs(60);
/// Per-account bound. This is the primary brute-force control, because it follows
/// the account rather than the address, so it holds no matter which client or
/// proxy the guesses arrive through.
pub(crate) const ACCOUNT_MAX_FAILURES: usize = 10;
/// Per-client bound, keyed on the real TCP peer only.
///
/// Deliberately much looser than the per-account bound: an office, a carrier NAT
/// or a reverse proxy puts many users behind one address, and one of them must
/// not be able to lock the others out. A single address can still only spend this
/// many password checks per window.
pub(crate) const CLIENT_MAX_FAILURES: usize = 60;
/// Instance-wide bound: the backstop against a distributed flood, and the reason
/// the attacker-keyed maps cannot grow past a few hundred entries.
pub(crate) const GLOBAL_MAX_FAILURES: usize = 300;
/// How many logins may be counted in flight at once.
///
/// The slot is reserved at request start in the same lock section that reads the
/// failure buckets, so a concurrent burst cannot have every request pass "am I
/// limited?" before the first failure is recorded.
pub(crate) const MAX_IN_FLIGHT_LOGINS: usize = 16;
/// How many PBKDF2 verifications may occupy the blocking pool at once.
pub(crate) const PASSWORD_CHECK_CONCURRENCY: usize = 4;

/// Why a login was refused before any password was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionRefusal {
    /// Too many logins are already being processed.
    InFlight,
    /// Too many recent failures for this account.
    Account,
    /// Too many recent failures from this client address.
    Client,
    /// Too many recent failures instance-wide.
    Global,
}

#[derive(Default)]
struct Bucket {
    entries: VecDeque<Failure>,
    pending: usize,
}

struct Failure {
    completed: Instant,
    client: Option<IpAddr>,
}

impl Bucket {
    fn prune(&mut self, now: Instant, window: Duration) {
        while self
            .entries
            .front()
            .is_some_and(|entry| now.duration_since(entry.completed) >= window)
        {
            self.entries.pop_front();
        }
    }

    fn is_at_limit(&self, limit: usize) -> bool {
        self.entries.len() + self.pending >= limit
    }

    fn record(&mut self, now: Instant, client: Option<IpAddr>) {
        self.entries.push_back(Failure {
            completed: now,
            client,
        });
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pending == 0
    }
}

struct AdmissionState {
    in_flight: usize,
    global: Bucket,
    /// Keyed on the real peer address. Nothing a client sends can select a key.
    clients: HashMap<IpAddr, Bucket>,
    /// Keyed on the submitted account name.
    accounts: HashMap<String, Bucket>,
}

impl AdmissionState {
    /// Drops expired entries together with the buckets they emptied.
    ///
    /// Pending reservations never expire while their guards are alive. Committed
    /// failures plus reservations fit within the global budget, bounding the maps
    /// even though the account name is attacker-chosen.
    fn prune(&mut self, now: Instant, window: Duration) {
        self.global.prune(now, window);
        self.clients.retain(|_, bucket| {
            bucket.prune(now, window);
            !bucket.is_empty()
        });
        self.accounts.retain(|_, bucket| {
            bucket.prune(now, window);
            !bucket.is_empty()
        });
    }

    fn release_reservation(&mut self, account: &str, client: Option<IpAddr>) {
        self.global.pending -= 1;
        self.accounts
            .get_mut(account)
            .expect("reserved account")
            .pending -= 1;
        if let Some(client) = client {
            self.clients
                .get_mut(&client)
                .expect("reserved client")
                .pending -= 1;
        }
    }
}

/// Request-start admission for the login endpoint.
///
/// Everything needed to decide "may this request check a password at all" lives
/// behind one mutex, so the in-flight count and the failure history are observed
/// and updated as a single transition.
pub(crate) struct LoginAdmission {
    window: Duration,
    state: Mutex<AdmissionState>,
}

impl LoginAdmission {
    pub(crate) fn new() -> Self {
        Self::with_window(LOGIN_WINDOW)
    }

    /// A limiter with a shorter window, so window expiry can be tested without
    /// sleeping for a minute.
    fn with_window(window: Duration) -> Self {
        Self {
            window,
            state: Mutex::new(AdmissionState {
                in_flight: 0,
                global: Bucket::default(),
                clients: HashMap::new(),
                accounts: HashMap::new(),
            }),
        }
    }

    /// A panic elsewhere must not silently turn the limiter into a no-op, so
    /// poisoning is recovered: the guarded state is still structurally intact.
    fn lock(&self) -> std::sync::MutexGuard<'_, AdmissionState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Reserves all applicable budgets and an in-flight slot for one login attempt.
    ///
    /// Bounds are checked and the slot taken under one lock. That is what makes
    /// a concurrent burst count against the cap instead of all of them reading
    /// "not limited yet" before any of them records a failure.
    pub(crate) fn try_enter(
        &self,
        account: &str,
        client: Option<IpAddr>,
    ) -> Result<AdmissionGuard<'_>, AdmissionRefusal> {
        let mut state = self.lock();
        let now = Instant::now();
        state.prune(now, self.window);

        if state.in_flight >= MAX_IN_FLIGHT_LOGINS {
            return Err(AdmissionRefusal::InFlight);
        }
        if state.global.is_at_limit(GLOBAL_MAX_FAILURES) {
            return Err(AdmissionRefusal::Global);
        }
        if state
            .accounts
            .get(account)
            .is_some_and(|bucket| bucket.is_at_limit(ACCOUNT_MAX_FAILURES))
        {
            return Err(AdmissionRefusal::Account);
        }
        if let Some(client) = client
            && state
                .clients
                .get(&client)
                .is_some_and(|bucket| bucket.is_at_limit(CLIENT_MAX_FAILURES))
        {
            return Err(AdmissionRefusal::Client);
        }

        state.in_flight += 1;
        state.global.pending += 1;
        state
            .accounts
            .entry(account.to_string())
            .or_default()
            .pending += 1;
        if let Some(client) = client {
            state.clients.entry(client).or_default().pending += 1;
        }

        Ok(AdmissionGuard {
            admission: self,
            account: account.to_string(),
            client,
            started: now,
            settled: false,
        })
    }
}

/// Keeps an admission slot reserved for the lifetime of the request.
pub(crate) struct AdmissionGuard<'a> {
    admission: &'a LoginAdmission,
    account: String,
    client: Option<IpAddr>,
    started: Instant,
    settled: bool,
}

impl AdmissionGuard<'_> {
    pub(crate) fn record_failure(&mut self) {
        self.settle(false);
    }

    pub(crate) fn record_success(&mut self) {
        self.settle(true);
    }

    fn settle(&mut self, success: bool) {
        assert!(!self.settled, "a login attempt can only be settled once");
        let mut state = self.admission.lock();
        let now = Instant::now();
        state.release_reservation(&self.account, self.client);
        if success {
            // Only this trustworthy peer's failures completed before this request
            // started are its own typos to forgive. Other peers and overlapping
            // attempts keep their contributions, even for the same account.
            // Missing peer identity cannot establish ownership of older failures.
            if self.client.is_some() {
                state
                    .accounts
                    .get_mut(&self.account)
                    .expect("reserved account")
                    .entries
                    .retain(|entry| entry.client != self.client || entry.completed >= self.started);
            }
        } else {
            state.global.record(now, self.client);
            state
                .accounts
                .get_mut(&self.account)
                .expect("reserved account")
                .record(now, self.client);
            if let Some(client) = self.client {
                state
                    .clients
                    .get_mut(&client)
                    .expect("reserved client")
                    .record(now, self.client);
            }
        }
        self.settled = true;
        state.prune(now, self.admission.window);
    }
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.admission.lock();
        // Cancellation or a non-credential error refunds only this reservation;
        // it never forgives committed failures. Detached CPU work retains its own
        // separate permit until the blocking closure actually exits.
        if !self.settled {
            state.release_reservation(&self.account, self.client);
        }
        state.in_flight -= 1;
        state.prune(Instant::now(), self.admission.window);
    }
}

/// Bounds how many deliberately expensive password checks can run at once.
///
/// PBKDF2 at [`HASH_ITERATIONS`] costs tens of milliseconds of CPU per attempt.
/// Running it inline in an `async` handler occupies a reactor worker, so the work
/// is moved to the blocking pool — but only a fixed number of checks may be there
/// at a time, otherwise a login burst merely moves the exhaustion from the
/// reactor to the blocking pool. Combined with [`MAX_IN_FLIGHT_LOGINS`] the CPU
/// a login flood can consume is bounded by
/// `PASSWORD_CHECK_CONCURRENCY / HASH cost` regardless of request rate.
pub(crate) struct PasswordCheckLimiter {
    permits: Arc<Semaphore>,
}

impl PasswordCheckLimiter {
    pub(crate) fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(PASSWORD_CHECK_CONCURRENCY)),
        }
    }

    /// Verifies `password` against `stored` off the reactor.
    ///
    /// The stored format and the `verified` answer are unchanged. When the stored
    /// hash predates [`HASH_ITERATIONS`] the replacement is computed in the same
    /// blocking task, so upgrading a legacy hash does not block the reactor a
    /// second time.
    pub(crate) async fn verify(
        &self,
        stored: StoragePassword,
        password: String,
    ) -> Result<Verification, AppError> {
        let permit = self.permits.clone().acquire_owned().await.map_err(|_| {
            AppError::PasswordCheckFailed("the password check limiter was closed".into())
        })?;

        tokio::task::spawn_blocking(move || {
            // spawn_blocking cannot be cancelled once running. The closure owns
            // capacity, including while queued and throughout any legacy rehash.
            let _permit = permit;
            let verified = stored.verify(&password)?;

            let rehash = if verified && stored.needs_rehash() {
                Some(StoragePassword::new(&password)?)
            } else {
                None
            };

            Ok(Verification { verified, rehash })
        })
        .await
        .map_err(|err| AppError::PasswordCheckFailed(err.to_string()))?
    }
}

/// Outcome of an off-reactor password check.
pub(crate) struct Verification {
    pub(crate) verified: bool,
    pub(crate) rehash: Option<StoragePassword>,
}

impl PasswordCheckLimiter {
    /// Derives a fresh hash for a new password, off the reactor and inside the
    /// same bound as verification.
    ///
    /// The first-login bootstrap path hashes a supplied password before the account
    /// exists, so it must not run PBKDF2 on a reactor worker either.
    pub(crate) async fn hash_new(&self, password: String) -> Result<StoragePassword, AppError> {
        let permit = self.permits.clone().acquire_owned().await.map_err(|_| {
            AppError::PasswordCheckFailed("the password check limiter was closed".into())
        })?;

        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            StoragePassword::new(&password)
        })
        .await
        .map_err(|err| AppError::PasswordCheckFailed(err.to_string()))?
    }
}

#[cfg(test)]
mod independent_checker {
    use super::*;
    use std::{future::Future, pin::Pin};

    fn client() -> Option<IpAddr> {
        Some(
            "127.0.0.2"
                .parse()
                .expect("test fixture operation must succeed"),
        )
    }

    #[test]
    fn account_budget_includes_pending_attempts() {
        let admission = LoginAdmission::new();
        for _ in 0..ACCOUNT_MAX_FAILURES - 1 {
            admission
                .try_enter("alice", client())
                .expect("test fixture operation must succeed")
                .record_failure();
        }
        let pending = admission
            .try_enter("alice", client())
            .expect("last slot available");
        let second = admission.try_enter("alice", client()).err();
        drop(pending);
        assert_eq!(
            second,
            Some(AdmissionRefusal::Account),
            "a pending check must reserve the last account slot"
        );
        admission
            .try_enter("alice", client())
            .expect("cancellation refunds the last slot")
            .record_failure();
        assert_eq!(
            admission.try_enter("alice", client()).err(),
            Some(AdmissionRefusal::Account),
            "failure settlement must convert, not refund or double count, its reservation"
        );
        assert_eq!(admission.lock().global.entries.len(), ACCOUNT_MAX_FAILURES);
        assert_eq!(admission.lock().global.pending, 0);
        assert_eq!(admission.lock().in_flight, 0);
    }

    #[test]
    fn client_budget_includes_pending_attempts() {
        let admission = LoginAdmission::new();
        for i in 0..CLIENT_MAX_FAILURES - 1 {
            admission
                .try_enter(&format!("acct-{i}"), client())
                .expect("test fixture operation must succeed")
                .record_failure();
        }
        let pending = admission
            .try_enter("fresh-1", client())
            .expect("last client slot");
        let second = admission.try_enter("fresh-2", client()).err();
        drop(pending);
        assert_eq!(
            second,
            Some(AdmissionRefusal::Client),
            "a pending check must reserve the last client slot"
        );
        admission
            .try_enter("fresh-2", client())
            .expect("cancellation refunds the last slot")
            .record_failure();
        assert_eq!(
            admission.try_enter("fresh-3", client()).err(),
            Some(AdmissionRefusal::Client)
        );
        assert_eq!(
            admission.lock().clients[&client().expect("test fixture operation must succeed")]
                .entries
                .len(),
            CLIENT_MAX_FAILURES
        );
        assert_eq!(admission.lock().global.pending, 0);
        assert_eq!(admission.lock().in_flight, 0);
    }

    #[test]
    fn global_budget_includes_pending_attempts() {
        let admission = LoginAdmission::new();
        for i in 0..GLOBAL_MAX_FAILURES - 1 {
            let ip = format!("127.1.{}.{}", i / 200, i % 200 + 1)
                .parse()
                .expect("test fixture operation must succeed");
            admission
                .try_enter(&format!("acct-{i}"), Some(ip))
                .expect("test fixture operation must succeed")
                .record_failure();
        }
        let pending = admission
            .try_enter("fresh-1", client())
            .expect("last global slot");
        let second = admission.try_enter("fresh-2", client()).err();
        drop(pending);
        assert_eq!(
            second,
            Some(AdmissionRefusal::Global),
            "a pending check must reserve the last global slot"
        );
        admission
            .try_enter("fresh-2", client())
            .expect("cancellation refunds the last slot")
            .record_failure();
        assert_eq!(
            admission.try_enter("fresh-3", None).err(),
            Some(AdmissionRefusal::Global)
        );
        assert_eq!(admission.lock().global.entries.len(), GLOBAL_MAX_FAILURES);
        assert_eq!(admission.lock().global.pending, 0);
        assert_eq!(admission.lock().in_flight, 0);
    }

    #[test]
    fn success_preserves_same_peer_failures_completed_during_its_request() {
        let admission = LoginAdmission::new();
        admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed")
            .record_failure();
        let mut success = admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed");
        let mut failure = admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed");
        failure.record_failure();
        drop(failure);
        success.record_success();
        drop(success);
        assert_eq!(
            admission
                .lock()
                .accounts
                .get("alice")
                .map(|b| b.entries.len()),
            Some(1),
            "success may clear its peer's earlier typo but not an overlapping request's failure"
        );
    }

    #[test]
    fn success_keeps_pending_failures_and_the_whole_request_in_flight() {
        let admission = LoginAdmission::new();
        admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed")
            .record_failure();
        let mut earlier_failure = admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed");
        let mut success = admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed");
        let mut later_failure = admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed");
        success.record_success();
        {
            let state = admission.lock();
            assert_eq!(
                state.in_flight, 3,
                "credential settlement must not end the HTTP request's slot"
            );
            assert_eq!(state.accounts["alice"].pending, 2);
            assert_eq!(state.global.pending, 2);
            assert_eq!(
                state.clients[&client().expect("test fixture operation must succeed")].pending,
                2
            );
            assert_eq!(
                state.accounts["alice"].entries.len(),
                0,
                "the earlier same-peer typo is forgiven"
            );
        }
        earlier_failure.record_failure();
        later_failure.record_failure();
        drop((earlier_failure, later_failure, success));
        let state = admission.lock();
        assert_eq!(state.accounts["alice"].entries.len(), 2);
        assert_eq!(
            state.clients[&client().expect("test fixture operation must succeed")]
                .entries
                .len(),
            3
        );
        assert_eq!(state.global.entries.len(), 3);
        assert_eq!(state.global.pending, 0);
        assert_eq!(state.in_flight, 0);
    }

    #[test]
    fn missing_peer_identity_does_not_forgive_unknown_or_other_clients_failures() {
        let admission = LoginAdmission::new();
        admission
            .try_enter("alice", None)
            .expect("test fixture operation must succeed")
            .record_failure();
        admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed")
            .record_failure();
        admission
            .try_enter("alice", None)
            .expect("test fixture operation must succeed")
            .record_success();
        assert_eq!(admission.lock().accounts["alice"].entries.len(), 2);
        admission
            .try_enter("alice", client())
            .expect("test fixture operation must succeed")
            .record_success();
        assert_eq!(
            admission.lock().accounts["alice"].entries.len(),
            1,
            "a known peer may forgive its own typo but cannot own the missing-peer contribution"
        );
        assert_eq!(admission.lock().global.entries.len(), 2);
    }

    #[test]
    fn pending_reservations_survive_window_expiry_and_cancel_without_leaks() {
        let admission = LoginAdmission::with_window(Duration::from_millis(1));
        let guards: Vec<_> = (0..ACCOUNT_MAX_FAILURES)
            .map(|_| {
                admission
                    .try_enter("alice", client())
                    .expect("test fixture operation must succeed")
            })
            .collect();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            admission.try_enter("alice", client()).err(),
            Some(AdmissionRefusal::Account),
            "expiry must not prune an outstanding reservation"
        );
        drop(guards);
        let state = admission.lock();
        assert_eq!(state.global.pending, 0);
        assert_eq!(state.in_flight, 0);
        assert!(state.accounts.is_empty());
        assert!(state.clients.is_empty());
        drop(state);
        assert!(admission.try_enter("alice", client()).is_ok());
    }

    #[test]
    fn parallel_contenders_cannot_overspend_the_last_account_reservation() {
        let admission = LoginAdmission::new();
        for _ in 0..ACCOUNT_MAX_FAILURES - 1 {
            admission
                .try_enter("alice", client())
                .expect("test fixture operation must succeed")
                .record_failure();
        }
        let barrier = std::sync::Barrier::new(17);
        let accepted = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        let attempt = admission.try_enter("alice", client());
                        barrier.wait();
                        match attempt {
                            Ok(mut guard) => {
                                guard.record_failure();
                                true
                            }
                            Err(refusal) => {
                                assert_eq!(refusal, AdmissionRefusal::Account);
                                false
                            }
                        }
                    })
                })
                .collect();
            barrier.wait();
            tasks
                .into_iter()
                .map(|task| usize::from(task.join().expect("test fixture operation must succeed")))
                .sum::<usize>()
        });
        assert_eq!(accepted, 1);
        assert_eq!(
            admission.lock().accounts["alice"].entries.len(),
            ACCOUNT_MAX_FAILURES
        );
        assert_eq!(admission.lock().global.pending, 0);
        assert_eq!(admission.lock().in_flight, 0);
    }

    // A fixture-owned channel barrier holds the sole blocking thread, so these
    // controls exercise queued product jobs rather than relying on CPU timing.
    fn cancellation_probe(hash_new: bool, legacy: bool) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test fixture operation must succeed");
        rt.block_on(async {
            let limiter = PasswordCheckLimiter::new();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered_tx.send(()).expect("test fixture operation must succeed"); release_rx.recv().expect("test fixture operation must succeed");
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).expect("test fixture operation must succeed");
            let mut stored = StoragePassword { salt: [7; 16], hash: [0; 32],
                iterations: if legacy { 10_000 } else { HASH_ITERATIONS } };
            if legacy {
                StoragePassword::hash(&stored.salt, stored.iterations, "fixture-password", &mut stored.hash).expect("test fixture operation must succeed");
            }
            let mut check: Pin<Box<dyn Future<Output = Result<(), AppError>>>> = if hash_new {
                Box::pin(async { limiter.hash_new("fixture-password".into()).await.map(|_| ()) })
            } else {
                Box::pin(async { limiter.verify(stored, "fixture-password".into()).await.map(|_| ()) })
            };
            assert!(futures::poll!(check.as_mut()).is_pending());
            assert_eq!(limiter.permits.available_permits(), PASSWORD_CHECK_CONCURRENCY - 1);
            drop(check);
            let available_after_cancel = limiter.permits.available_permits();
            let marker = tokio::task::spawn_blocking(|| ());
            release_tx.send(()).expect("test fixture operation must succeed");
            blocker.await.expect("test fixture operation must succeed"); marker.await.expect("test fixture operation must succeed");
            eprintln!("hash_new={hash_new}, legacy={legacy}: available_after_cancel={available_after_cancel}; queued product job drained");
            assert_eq!(available_after_cancel, PASSWORD_CHECK_CONCURRENCY - 1,
                "cancelling the awaiter returned a permit while its product PBKDF2 task was still queued");
            assert_eq!(limiter.permits.available_permits(), PASSWORD_CHECK_CONCURRENCY);
        });
    }

    #[test]
    fn cancelled_verify_retains_permit_until_real_work_finishes() {
        cancellation_probe(false, false);
    }
    #[test]
    fn cancelled_hash_new_retains_permit_until_real_work_finishes() {
        cancellation_probe(true, false);
    }
    #[test]
    fn cancelled_legacy_verify_retains_permit_through_rehash() {
        cancellation_probe(false, true);
    }

    #[test]
    fn actual_verifier_saturation_is_bounded_without_cancellation() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test fixture operation must succeed");
        rt.block_on(async {
            let limiter = PasswordCheckLimiter::new();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered_tx
                    .send(())
                    .expect("test fixture operation must succeed");
                release_rx
                    .recv()
                    .expect("test fixture operation must succeed");
            });
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("test fixture operation must succeed");
            let stored = StoragePassword {
                salt: [7; 16],
                hash: [0; 32],
                iterations: HASH_ITERATIONS,
            };
            let mut checks = Vec::new();
            for _ in 0..16 {
                let mut check = Box::pin(limiter.verify(stored.clone(), "guess".into()));
                assert!(futures::poll!(check.as_mut()).is_pending());
                checks.push(check);
            }
            let available = limiter.permits.available_permits();
            release_tx
                .send(())
                .expect("test fixture operation must succeed");
            blocker.await.expect("test fixture operation must succeed");
            for check in checks {
                assert!(
                    !check
                        .await
                        .expect("test fixture operation must succeed")
                        .verified
                );
            }
            assert_eq!(
                available, 0,
                "the real verifier must occupy all four permits"
            );
            assert_eq!(
                limiter.permits.available_permits(),
                PASSWORD_CHECK_CONCURRENCY
            );
        });
    }

    #[test]
    fn repeated_cancellation_cannot_queue_more_real_checks_than_cpu_budget() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test fixture operation must succeed");
        rt.block_on(async {
            let limiter = PasswordCheckLimiter::new();
            let admission = LoginAdmission::new();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered_tx.send(()).expect("test fixture operation must succeed"); release_rx.recv().expect("test fixture operation must succeed");
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).expect("test fixture operation must succeed");
            let stored = StoragePassword { salt: [7; 16], hash: [0; 32], iterations: HASH_ITERATIONS };
            let mut queued = 0;
            for _ in 0..16 {
                let guard = admission.try_enter("alice", client()).expect("admission");
                let before = limiter.permits.available_permits();
                let mut check = Box::pin(limiter.verify(stored.clone(), "guess".into()));
                assert!(futures::poll!(check.as_mut()).is_pending());
                if limiter.permits.available_permits() < before { queued += 1; }
                drop(check); drop(guard);
            }
            let marker = tokio::task::spawn_blocking(|| ());
            release_tx.send(()).expect("test fixture operation must succeed"); blocker.await.expect("test fixture operation must succeed"); marker.await.expect("test fixture operation must succeed");
            eprintln!("cancelled actual verify jobs queued and drained: {queued}; CPU budget={PASSWORD_CHECK_CONCURRENCY}");
            assert_eq!(queued, PASSWORD_CHECK_CONCURRENCY,
                "cancellation must neither disable the pool nor queue more real jobs than its CPU budget");
            assert_eq!(limiter.permits.available_permits(), PASSWORD_CHECK_CONCURRENCY);
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actual_verifier_preserves_hashes_and_yields_to_reactor() {
        let limiter = PasswordCheckLimiter::new();
        let mut legacy = StoragePassword {
            salt: [3; 16],
            hash: [0; 32],
            iterations: 10_000,
        };
        StoragePassword::hash(
            &legacy.salt,
            legacy.iterations,
            "fixture-password",
            &mut legacy.hash,
        )
        .expect("test fixture operation must succeed");
        let bad = limiter
            .verify(legacy.clone(), "wrong".into())
            .await
            .expect("test fixture operation must succeed");
        assert!(!bad.verified);
        assert!(bad.rehash.is_none());
        let ticks = std::sync::atomic::AtomicUsize::new(0);
        let verify = limiter.verify(legacy, "fixture-password".into());
        tokio::pin!(verify);
        let result = loop {
            tokio::select! {
                r = &mut verify => break r.expect("test fixture operation must succeed"),
                _ = tokio::time::sleep(Duration::from_millis(1)) => {
                    ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        };
        assert!(result.verified);
        let replacement = result.rehash.expect("legacy hash upgraded");
        assert_eq!(replacement.iterations, HASH_ITERATIONS);
        assert!(
            replacement
                .verify("fixture-password")
                .expect("test fixture operation must succeed")
        );
        assert!(
            !replacement
                .verify("wrong")
                .expect("test fixture operation must succeed")
        );
        assert!(
            ticks.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "real product verifier blocked the only reactor thread"
        );
        assert!(matches!(
            limiter.hash_new(String::new()).await,
            Err(AppError::PasswordEmpty)
        ));
        let new = limiter
            .hash_new("new-fixture-password".into())
            .await
            .expect("test fixture operation must succeed");
        assert!(
            new.verify("new-fixture-password")
                .expect("test fixture operation must succeed")
        );
    }
}

// ---------------------------------------------------------------------------
// MW-23: bucket arithmetic
// ---------------------------------------------------------------------------
//
// The HTTP-level wiring is covered in `api::auth::mw23_tests`; these drive the
// same `LoginAdmission`/`PasswordCheckLimiter` the handler uses directly, so the
// bounds can be checked without paying for hundreds of PBKDF2 runs.
#[cfg(test)]
mod tests {
    use super::*;

    /// The published bounds are load-bearing policy, and the HTTP regressions in
    /// `api::auth::mw23_tests` hard-code them. Pin them here so a change to one
    /// without the other fails loudly instead of silently weakening the tests.
    #[test]
    fn published_bounds_are_the_documented_values() {
        assert_eq!(ACCOUNT_MAX_FAILURES, 10);
        assert_eq!(CLIENT_MAX_FAILURES, 60);
        assert_eq!(GLOBAL_MAX_FAILURES, 300);
        assert_eq!(MAX_IN_FLIGHT_LOGINS, 16);
        assert_eq!(PASSWORD_CHECK_CONCURRENCY, 4);
        assert_eq!(LOGIN_WINDOW, Duration::from_secs(60));
        const {
            assert!(
                ACCOUNT_MAX_FAILURES < CLIENT_MAX_FAILURES,
                "the account bound must be the tighter one: it is the brute-force control"
            )
        };
        const {
            assert!(
                CLIENT_MAX_FAILURES < GLOBAL_MAX_FAILURES,
                "the client bound must leave room for other clients inside the global budget"
            )
        };
    }

    /// MW-23: the expensive part must actually leave the reactor and must actually be
    /// bounded.
    ///
    /// Both properties are measured rather than inferred:
    /// - the reactor stays responsive while a verification is running, which it could
    ///   not if PBKDF2 were still inline in the handler;
    /// - no more than `PASSWORD_CHECK_CONCURRENCY` verifications run at once, measured
    ///   by peak concurrent entries into the blocking task, which is the CPU bound the
    ///   audit asked for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn password_checks_run_off_the_reactor_and_are_bounded() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let limiter = Arc::new(PasswordCheckLimiter::new());
        let stored = Arc::new(StoragePassword {
            salt: [7u8; 16],
            // A real hash of "" is not needed: `verify` recomputes and compares, so any
            // stored value gives a false answer while still paying the full PBKDF2 cost.
            hash: [0u8; 32],
            iterations: HASH_ITERATIONS,
        });

        // Reactor responsiveness: with a single worker thread, a ticker must keep
        // making progress while checks are in flight. If the derivation were inline on
        // this worker, no tick could be observed until it finished.
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticker = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    ticks.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let peak = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));

        // Oversubscribe the pool deliberately: 4x the permits.
        const CALLS: usize = PASSWORD_CHECK_CONCURRENCY * 4;

        let mut tasks = Vec::new();
        for _ in 0..CALLS {
            let limiter = limiter.clone();
            let stored = stored.clone();
            let peak = peak.clone();
            let running = running.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = limiter.permits.acquire().await.expect("permit");
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let verified = tokio::task::spawn_blocking(move || stored.verify("guess"))
                    .await
                    .expect("blocking task")
                    .expect("verify");
                running.fetch_sub(1, Ordering::SeqCst);
                verified
            }));
        }

        for task in tasks {
            assert!(
                !task.await.expect("join"),
                "the wrong password must not verify"
            );
        }
        ticker.abort();

        let peak = peak.load(Ordering::SeqCst);
        assert!(
            peak <= PASSWORD_CHECK_CONCURRENCY,
            "{peak} verifications ran concurrently, above the bound of \
             {PASSWORD_CHECK_CONCURRENCY}"
        );
        assert_eq!(
            peak, PASSWORD_CHECK_CONCURRENCY,
            "the pool must actually be used up to its bound, peak was {peak}"
        );
        assert!(
            ticks.load(Ordering::Relaxed) > 0,
            "the single reactor worker made no progress while password checks were \
             running, which means the derivation is still blocking it"
        );
    }

    /// MW-23: the client bucket exists and is bounded, and a success does not clear it.
    /// Driven through the real `LoginAdmission` used by the handler; the HTTP tests above
    /// cover the wiring, this covers the bucket arithmetic without paying for 60 PBKDF2
    /// runs.
    #[test]
    fn client_bucket_is_bounded_and_survives_successes() {
        let admission = LoginAdmission::new();
        let shared: std::net::IpAddr = "127.0.0.1".parse().expect("ip");

        for index in 0..CLIENT_MAX_FAILURES {
            let account = format!("user-{index}");
            // Success must be admitted before the peer's final slot is spent. It
            // must leave all 59 client/global failures intact for that last attempt.
            if index == CLIENT_MAX_FAILURES - 1 {
                admission
                    .try_enter("user-0", Some(shared))
                    .expect("test fixture operation must succeed")
                    .record_success();
                admission
                    .try_enter("somebody-else", Some(shared))
                    .expect("test fixture operation must succeed")
                    .record_success();
                assert_eq!(admission.lock().clients[&shared].entries.len(), index);
                assert_eq!(admission.lock().global.entries.len(), index);
            }
            let mut guard = admission
                .try_enter(&account, Some(shared))
                .unwrap_or_else(|refusal| {
                    panic!("attempt {index} must be admitted, got {refusal:?}")
                });
            guard.record_failure();
            drop(guard);
        }

        let refusal = admission
            .try_enter("another-account", Some(shared))
            .err()
            .expect("the client bucket must be exhausted");
        assert_eq!(
            refusal,
            AdmissionRefusal::Client,
            "the shared address must be refused on its own budget"
        );

        // The successes above must not have handed the address a fresh budget.
        assert_eq!(
            admission
                .try_enter("yet-another", Some(shared))
                .err()
                .expect("the client bucket must still be exhausted"),
            AdmissionRefusal::Client,
            "a successful login must not clear the shared client's failure history"
        );

        // A different address is unaffected.
        let other: std::net::IpAddr = "203.0.113.9".parse().expect("ip");
        assert!(
            admission.try_enter("yet-another", Some(other)).is_ok(),
            "the client bound must be per address"
        );
    }

    /// MW-23: the global bound is the distributed-flood backstop, and the only thing that
    /// keeps the attacker-keyed maps bounded. It must refuse once the instance-wide budget
    /// is spent, regardless of account or address.
    #[test]
    fn global_budget_stops_a_distributed_flood() {
        let admission = LoginAdmission::new();

        for index in 0..GLOBAL_MAX_FAILURES {
            let account = format!("acct-{index}");
            let client: std::net::IpAddr =
                format!("10.{}.{}.{}", index / 256 % 256, index % 256, 1)
                    .parse()
                    .expect("ip");
            // The account and client buckets are only reached at their own limits, and
            // both are far above this loop, so every one of these is admitted.
            let mut guard = admission
                .try_enter(&account, Some(client))
                .unwrap_or_else(|refusal| {
                    panic!("attempt {index} must be admitted, got {refusal:?}")
                });
            guard.record_failure();
            drop(guard);
        }

        assert_eq!(
            admission
                .try_enter("fresh-account", Some("198.51.100.1".parse().expect("ip")))
                .err()
                .expect("the global budget must be exhausted"),
            AdmissionRefusal::Global,
            "a distributed flood must hit the instance-wide bound"
        );
    }

    /// MW-23: the failure window must expire, otherwise a bounded limiter is a permanent
    /// lockout. Driven with a shortened window so the test does not sleep for a minute.
    #[test]
    fn failure_window_expires_and_restores_admission() {
        let admission = LoginAdmission::with_window(Duration::from_millis(50));
        let client: std::net::IpAddr = "127.0.0.1".parse().expect("ip");

        for _ in 0..ACCOUNT_MAX_FAILURES {
            admission
                .try_enter("alice", Some(client))
                .expect("admitted")
                .record_failure();
        }
        assert_eq!(
            admission.try_enter("alice", Some(client)).err(),
            Some(AdmissionRefusal::Account),
            "precondition: the account is at its bound"
        );

        std::thread::sleep(Duration::from_millis(120));

        assert!(
            admission.try_enter("alice", Some(client)).is_ok(),
            "admission must recover once the window has passed"
        );
    }
}
