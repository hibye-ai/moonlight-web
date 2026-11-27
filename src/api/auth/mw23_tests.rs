//! MW-23 regressions: login admission must count in-flight attempts, bound the
//! failure history per account / per client / instance-wide, and must not let one
//! successful login erase somebody else's failure history.
//!
//! The defects these cover, all in the pre-fix code:
//!
//! 1. the limit check and the failure record were two separate lock sections with the
//!    whole password verification in between, so a concurrent burst was never counted:
//!    every request in the burst passed "am I limited?" before the first failure
//!    existed, and there was no in-flight accounting at all;
//! 2. the only bucket was the peer address, so one address behind a shared proxy or
//!    NAT was the entire budget for every account behind it, and guessing many
//!    accounts from many addresses was unbounded;
//! 3. a successful login removed the whole peer bucket, so any successful login from
//!    an address wiped the failure history recorded against *other* accounts from that
//!    same address.
//!
//! Everything here drives the real `login` handler over the real `App`/`JsonStorage`
//! with fixture-owned accounts and loopback peers only. Nothing touches a live account.
//!
//! This file deliberately depends only on symbols that exist in the pre-fix source, so
//! it can be compiled against it to produce the RED evidence.
use std::path::PathBuf;

use actix_web::{App as ActixApp, http::StatusCode, test, web::Data};

use super::*;
use crate::app::{
    auth::UserAuth, password::StoragePassword, storage::StorageUserAdd, user::AuthenticatedUser,
};

const ALICE: &str = "alice";
const BOB: &str = "bob";
const ALICE_PASSWORD: &str = "correct-horse-1";
const BOB_PASSWORD: &str = "correct-horse-2";

/// The documented production bounds. `password::tests` pins the real constants to
/// these values, so the expectations below cannot silently drift.
const SPEC_ACCOUNT_MAX_FAILURES: usize = 10;
const SPEC_MAX_IN_FLIGHT: usize = 16;

#[path = "repair_tests.rs"]
mod repair_tests;

struct Fixture {
    app: Data<App>,
    _dir: PathBuf,
}

/// Builds a real app with two fixture-owned accounts: `alice` (the bootstrapped
/// admin) and `bob` (a plain user).
async fn fixture(label: &str, configure: impl FnOnce(&mut common::config::Config)) -> Fixture {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-mw23-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let file = dir.join("data.json");

    let mut config = common::config::Config {
        data_storage: common::config::StorageConfig::Json {
            path: file.to_string_lossy().to_string(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    configure(&mut config);

    let app = App::new(config).await.expect("build app");

    // Bootstrap through the real first-login path, not a hand-written store.
    app.try_add_first_login(ALICE.to_string(), ALICE_PASSWORD.to_string())
        .await
        .expect("bootstrap admin");

    let alice: AuthenticatedUser = app
        .user_by_auth(UserAuth::UserPassword {
            username: ALICE.to_string(),
            password: ALICE_PASSWORD.to_string(),
        })
        .await
        .expect("authenticate alice");
    let admin = alice.into_admin().await.expect("alice is admin");

    let role_id = app.default_role().await.expect("default role").id();
    app.add_user(
        &admin,
        StorageUserAdd {
            role_id,
            name: BOB.to_string(),
            password: Some(StoragePassword::new(BOB_PASSWORD).expect("hash bob")),
            client_unique_id: BOB.to_string(),
        },
    )
    .await
    .expect("add bob");

    Fixture {
        app: Data::new(app),
        _dir: dir,
    }
}

fn request(name: &str, password: &str, peer: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/login")
        .peer_addr(peer.parse().expect("peer addr"))
        .set_json(serde_json::json!({ "name": name, "password": password }))
}

/// A distinct loopback peer per attempt, so the per-client bound can never be the
/// thing that stops a request in the account-bound tests.
fn peer(index: usize) -> String {
    format!("127.0.1.{}:5000", (index % 200) + 2)
}

/// Posts one login through the real handler and returns its status.
///
/// A macro rather than a function because the actix test service's type is opaque:
/// expanding in place lets inference resolve it, without naming actix's request type
/// (which this crate does not depend on directly).
macro_rules! post {
    ($service:expr, $name:expr, $password:expr, $peer:expr) => {
        test::call_service(&$service, request($name, $password, $peer).to_request())
            .await
            .status()
    };
}

/// MW-23: the account bucket is what actually stops a brute force, because it follows
/// the account rather than the address. Ten wrong guesses from ten different addresses
/// must exhaust it, and the eleventh — from yet another address, with the *correct*
/// password — must be refused while the window lasts.
#[actix_web::test]
async fn account_failure_budget_is_exhausted_across_distinct_peers() {
    let fixture = fixture("account-bound", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;

    for index in 0..SPEC_ACCOUNT_MAX_FAILURES {
        let status = post!(service, ALICE, "wrong-password", &peer(index));
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "guess {} must be checked and rejected with 401, got {status}",
            index + 1
        );
    }

    let status = post!(service, ALICE, "wrong-password", &peer(60));
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the account budget must stop guess {} from a fresh address",
        SPEC_ACCOUNT_MAX_FAILURES + 1
    );

    // The correct password is refused too: the bound is about the account, not about
    // the guess being wrong. That is the deliberate lockout trade-off.
    let status = post!(service, ALICE, ALICE_PASSWORD, &peer(61));
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the account stays locked for the rest of the window, got {status}"
    );

    // A different account behind all of that is unaffected.
    let status = post!(service, BOB, BOB_PASSWORD, &peer(62));
    assert_eq!(
        status,
        StatusCode::OK,
        "one locked account must not lock another account, got {status}"
    );
}

/// MW-23: a concurrent burst must be counted. Every request in the burst is released
/// at once; whatever is not admitted must be refused explicitly, and the number that
/// reach a password check must stay inside the in-flight cap.
#[actix_web::test]
async fn concurrent_burst_is_admitted_or_refused_but_never_all_checked() {
    let fixture = fixture("burst", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;

    const BURST: usize = 64;

    let calls = (0..BURST).map(|index| {
        let attempt = request(ALICE, "wrong-password", &peer(index)).to_request();
        let service = &service;
        async move { test::call_service(service, attempt).await.status() }
    });

    let statuses: Vec<StatusCode> = futures::future::join_all(calls).await;

    let count = |want: StatusCode| statuses.iter().filter(|status| **status == want).count();
    let checked = count(StatusCode::UNAUTHORIZED);
    let throttled = count(StatusCode::TOO_MANY_REQUESTS);
    let busy = count(StatusCode::SERVICE_UNAVAILABLE);
    let other = BURST - checked - throttled - busy;

    assert_eq!(other, 0, "unexpected statuses in burst: {statuses:?}");
    assert!(
        checked > 0,
        "some requests must actually reach the password check"
    );
    assert!(
        checked <= SPEC_MAX_IN_FLIGHT,
        "at most the in-flight cap may run a password check, {checked} did"
    );
    assert!(
        busy + throttled > 0,
        "a {BURST}-wide burst must be partly refused at admission, none were: {statuses:?}"
    );

    // Admission slots must be released once the burst is done, and the failures it
    // recorded must still be counted.
    let status = post!(service, ALICE, ALICE_PASSWORD, &peer(150));
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the burst's failures must survive and still lock the account, got {status}"
    );
    let status = post!(service, BOB, BOB_PASSWORD, &peer(151));
    assert_eq!(
        status,
        StatusCode::OK,
        "in-flight slots must be released and other accounts unaffected, got {status}"
    );
}

/// MW-23: the in-flight cap on its own. Correct passwords, so no failure bucket can be
/// the reason for a refusal — only the concurrency bound.
#[actix_web::test]
async fn in_flight_cap_limits_concurrent_password_checks_and_releases_slots() {
    let fixture = fixture("in-flight", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;

    const BURST: usize = 64;

    // Split correct logins across two accounts so their outstanding reservations
    // do not hit the tighter account budget before the independent in-flight cap.
    let calls = (0..BURST).map(|index| {
        let (name, password) = if index % 2 == 0 {
            (ALICE, ALICE_PASSWORD)
        } else {
            (BOB, BOB_PASSWORD)
        };
        let attempt = request(name, password, &peer(index)).to_request();
        let service = &service;
        async move { test::call_service(service, attempt).await.status() }
    });

    let statuses: Vec<StatusCode> = futures::future::join_all(calls).await;
    let accepted = statuses
        .iter()
        .filter(|status| **status == StatusCode::OK)
        .count();
    let busy = statuses
        .iter()
        .filter(|status| **status == StatusCode::SERVICE_UNAVAILABLE)
        .count();

    assert_eq!(
        accepted + busy,
        BURST,
        "a burst of valid logins may only be accepted or refused as busy: {statuses:?}"
    );
    assert!(accepted > 0, "the cap must not reject every login");
    assert!(
        accepted <= SPEC_MAX_IN_FLIGHT,
        "{accepted} concurrent logins passed the cap of {SPEC_MAX_IN_FLIGHT}"
    );
    assert!(
        busy > 0,
        "a {BURST}-wide burst must exceed the in-flight cap, nothing was refused"
    );

    // Sequential logins still work: the slots are released on completion.
    for index in 0..3 {
        let status = post!(service, BOB, BOB_PASSWORD, &peer(100 + index));
        assert_eq!(
            status,
            StatusCode::OK,
            "slot release must allow subsequent logins, got {status}"
        );
    }
}

/// MW-23: the success bug. `SPEC_ACCOUNT_MAX_FAILURES` failures for `alice` are
/// recorded from one address, then `bob` logs in successfully from that same address.
/// The old code dropped the whole peer bucket there, which cleared alice's history and
/// handed an attacker a fresh budget. Alice's history must survive.
#[actix_web::test]
async fn successful_login_does_not_clear_another_accounts_failure_history() {
    let fixture = fixture("success-isolation", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;

    const SHARED_PEER: &str = "127.0.0.1:41000";

    // Enough to exhaust either the old per-peer budget (20) or the account budget (10),
    // so this is a real regression on both implementations rather than only on one.
    const MAX_GUESSES: usize = 25;

    let mut refused_after = None;
    for attempt in 1..=MAX_GUESSES {
        match post!(service, ALICE, "wrong-password", SHARED_PEER) {
            StatusCode::UNAUTHORIZED => {}
            StatusCode::TOO_MANY_REQUESTS => {
                refused_after = Some(attempt);
                break;
            }
            other => panic!("unexpected status {other} at attempt {attempt}"),
        }
    }
    let refused_after = refused_after.expect(
        "alice must be locked out after repeated failures from one address; admitting all \
         {MAX_GUESSES} guesses is the unbounded-brute-force defect",
    );

    let status = post!(service, BOB, BOB_PASSWORD, SHARED_PEER);
    assert_eq!(
        status,
        StatusCode::OK,
        "bob's own login must still work from the shared address, got {status}"
    );

    let status = post!(service, ALICE, "wrong-password", SHARED_PEER);
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a successful login for bob must not reset alice's failure history \
         (alice was refused from attempt {refused_after} on)"
    );
}

/// MW-23: the wipe bug, isolated. Repeated successful logins from the shared address
/// must not keep refreshing another account's budget. The pre-fix code cleared the
/// entire peer bucket on every success, so this loop of "guess, then have bob log in"
/// bought an attacker an unlimited number of checked guesses from one address.
#[actix_web::test]
async fn interleaved_successes_cannot_restore_a_locked_accounts_budget() {
    let fixture = fixture("wipe", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;

    const SHARED_PEER: &str = "127.0.0.1:41001";
    const ROUNDS: usize = 4;

    let mut checked = 0usize;
    for round in 0..ROUNDS {
        for _ in 0..SPEC_ACCOUNT_MAX_FAILURES {
            match post!(service, ALICE, "wrong-password", SHARED_PEER) {
                StatusCode::UNAUTHORIZED => checked += 1,
                StatusCode::TOO_MANY_REQUESTS => break,
                other => panic!("unexpected status {other}"),
            }
        }
        // An unrelated successful login from the same address. The pre-fix code dropped
        // the whole peer bucket here, so the next round started from zero again.
        let status = post!(service, BOB, BOB_PASSWORD, SHARED_PEER);
        assert_eq!(
            status,
            StatusCode::OK,
            "round {round}: bob must still be able to log in, got {status}"
        );
    }

    assert_eq!(
        checked, SPEC_ACCOUNT_MAX_FAILURES,
        "a locked account must only ever receive its budget of checked guesses; \
         {checked} were checked across {ROUNDS} rounds, so a successful login is \
         restoring the failure history"
    );
}

/// MW-23: the ordinary cases must keep working. A wrong password is 401, the correct
/// one is 200 with a session cookie, an unknown account is 401, and a success does
/// clear the failures of the account that just authenticated (so a user who
/// fat-fingers a few times is not locked out after succeeding).
#[actix_web::test]
async fn correct_and_incorrect_passwords_behave_as_before() {
    let fixture = fixture("credentials", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;
    let peer_addr = "127.0.0.1:5000";

    let response = test::call_service(
        &service,
        request(ALICE, "wrong-password", peer_addr).to_request(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a wrong password must be 401"
    );

    let response = test::call_service(
        &service,
        request(ALICE, ALICE_PASSWORD, peer_addr).to_request(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the correct password must be accepted"
    );
    assert!(
        response
            .headers()
            .get(actix_web::http::header::SET_COOKIE)
            .is_some(),
        "a successful login must still issue a session cookie"
    );

    let response = test::call_service(
        &service,
        request("nobody", "whatever", peer_addr).to_request(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an unknown account must not be distinguishable from a wrong password"
    );

    // Nine failures, then a success: the account's own budget resets, but it must not
    // have been exhausted before that.
    for _ in 0..(SPEC_ACCOUNT_MAX_FAILURES - 1) {
        assert_eq!(
            post!(service, ALICE, "wrong-password", peer_addr),
            StatusCode::UNAUTHORIZED,
            "failures below the bound must still be checked"
        );
    }
    assert_eq!(
        post!(service, ALICE, ALICE_PASSWORD, peer_addr),
        StatusCode::OK,
        "the correct password must still be accepted below the bound"
    );
    assert_eq!(
        post!(service, ALICE, "wrong-password", peer_addr),
        StatusCode::UNAUTHORIZED,
        "a successful login must reset the failures of that account"
    );
}

/// MW-23/AR-07: forwarding headers must not steer the client bucket. An untrusted peer
/// rotating `X-Forwarded-For` on every attempt must not gain anything, and neither may a
/// peer the operator explicitly trusts for the *transport*: trusting a proxy to assert
/// the scheme or a username is not trusting it to name the client.
///
/// API adaptation: the trusted proxy is configured through the dedicated transport list
/// (`trusted_forwarded_proto_ips`) instead of `ForwardedHeaders::trusted_proxy_ips`.
/// Every assertion is preserved — untrusted peers are still refused at the transport
/// boundary, and trusted-peer failures are still counted against the real peer — while
/// header user authentication is now independently *disabled*, so this test proves the
/// transport trust does not confer any header identity.
#[actix_web::test]
async fn forwarding_headers_cannot_grant_a_fresh_client_budget() {
    let fixture = fixture("forwarded", |config| {
        config.web_server.session_cookie_secure = true;
        config.web_server.trusted_forwarded_proto_ips = vec!["198.51.100.7".parse().expect("ip")];
        assert!(
            config.web_server.forwarded_header.is_none(),
            "transport trust must not enable header user authentication"
        );
    })
    .await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;

    // An untrusted peer cannot assert https, so its attempts are refused at the
    // transport boundary before any password is checked — the spoofed client address
    // never even comes into play.
    for index in 0..SPEC_ACCOUNT_MAX_FAILURES {
        let attempt = request(ALICE, "wrong-password", "203.0.113.5:41000")
            .insert_header(("X-Forwarded-Proto", "https"))
            .insert_header(("X-Forwarded-For", format!("192.0.2.{}", index + 1)))
            .to_request();
        let status = test::call_service(&service, attempt).await.status();
        assert_eq!(
            status,
            StatusCode::UPGRADE_REQUIRED,
            "an untrusted peer cannot assert https, so attempt {} is refused at the transport",
            index + 1
        );
    }

    // A peer trusted for the transport reaches the password check, and its attempts are
    // counted against its real address rather than against the rotated
    // X-Forwarded-For values it supplies.
    for index in 0..SPEC_ACCOUNT_MAX_FAILURES {
        let attempt = request(ALICE, "wrong-password", "127.0.0.1:41000")
            .insert_header(("X-Forwarded-Proto", "https"))
            .insert_header(("X-Forwarded-For", format!("192.0.2.{}", index + 100)))
            .to_request();
        let status = test::call_service(&service, attempt).await.status();
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "trusted-proxy attempt {} must be checked and counted",
            index + 1
        );
    }

    let spoofed_target = request(ALICE, ALICE_PASSWORD, "127.0.0.1:41000")
        .insert_header(("X-Forwarded-Proto", "https"))
        .insert_header(("X-Forwarded-For", "192.0.2.250"))
        .to_request();
    let status = test::call_service(&service, spoofed_target).await.status();
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a rotating X-Forwarded-For must not restore the account budget"
    );

    // The explicitly trusted proxy address is still honoured, and an account that is
    // not under a bound can still log in through it with a Secure cookie.
    let trusted_proxy_login = request(BOB, BOB_PASSWORD, "198.51.100.7:41000")
        .insert_header(("X-Forwarded-Proto", "https"))
        .insert_header(("X-Forwarded-For", "192.0.2.42"))
        .to_request();
    let response = test::call_service(&service, trusted_proxy_login).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a trusted proxy must still be able to log a user in"
    );
    assert!(
        response
            .headers()
            .get(actix_web::http::header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|cookie| cookie
                .split(';')
                .any(|part| part.trim().eq_ignore_ascii_case("secure"))),
        "a trusted-proxy login must still get a Secure cookie"
    );
}
