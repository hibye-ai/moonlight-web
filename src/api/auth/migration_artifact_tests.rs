//! AR-07 (test/integration gap repaired under AR-09): the staged migration artifact is
//! real, loadable configuration.
//!
//! `docs/https-login-policy.staged.example.json` is the review point an operator copies
//! before a cutover. Prose that would not parse is worse than no template, so this test
//! deserializes the actual file through the real `Config` type and asserts the safe
//! properties the migration document promises:
//!
//! - the template parses as a `Config`;
//! - it is default-deny (no peer is trusted to assert the scheme);
//! - it does not silently enable the cleartext exception;
//! - it does not enable header-based user authentication;
//! - `session_cookie_secure` stays on.
//!
//! The template is read in place, never written or copied, and no network or live
//! configuration is touched.
//!
//! Layout: the template ships *inside* the source tree (`docs/`), not in a sibling
//! `deployment/` directory of the worker workspace. A clean checkout has no sibling
//! directories at all, so the path is derived from `CARGO_MANIFEST_DIR`, which is the
//! crate root and therefore the repository root.
use std::path::PathBuf;

use common::config::Config;

fn staged_template_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("https-login-policy.staged.example.json")
}

/// A `web_server` object carrying every field `WebServerConfig` requires without a serde
/// default, with the transport trust list under the caller's control.
///
/// `first_login_create_admin` and `first_login_assign_global_hosts` have no default, so
/// omitting them fails the parse for a reason unrelated to transport trust. That would
/// make a "must be rejected" assertion pass vacuously, which is why the negative test
/// below first proves the trust-free baseline parses.
fn web_server_json(trust_entries: Option<&str>) -> String {
    let trust = trust_entries
        .map(|entries| format!(r#", "trusted_forwarded_proto_ips": {entries}"#))
        .unwrap_or_default();
    format!(
        concat!(
            r#"{{ "web_server": {{ "bind_address": "0.0.0.0:8080", "certificate": null,"#,
            r#" "session_cookie_secure": true, "allow_insecure_http_login": false,"#,
            r#" "first_login_create_admin": false, "first_login_assign_global_hosts": false,"#,
            r#" "default_user_id": null, "default_role_id": null, "forwarded_header": null{} }} }}"#,
        ),
        trust
    )
}

/// The staged template must deserialize through the production `Config` type.
#[test]
fn staged_migration_template_is_loadable_configuration() {
    let path = staged_template_path();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("staged template must exist at {}: {err}", path.display()));

    let config: Config = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("staged template must parse as Config: {err}"));

    assert!(
        config.web_server.session_cookie_secure,
        "the staged template must keep session_cookie_secure on"
    );
    assert!(
        !config.web_server.allow_insecure_http_login,
        "the staged template must not enable the cleartext exception"
    );
    assert!(
        config.web_server.forwarded_header.is_none(),
        "the staged template must keep header user authentication disabled"
    );
    assert!(
        config.web_server.trusted_forwarded_proto_ips.is_empty(),
        "the staged template ships default-deny; the operator adds the exact peer at Stage 0"
    );
}

/// Removing the field entirely must also deserialize as default-deny, so a deployment
/// upgrading from a configuration written before this setting existed inherits deny
/// rather than an error or an implicit trust.
#[test]
fn absent_setting_deserializes_to_default_deny() {
    let fixture = web_server_json(None);
    let config: Config = serde_json::from_str(&fixture)
        .unwrap_or_else(|err| panic!("a pre-migration configuration must still load: {err}"));

    assert!(
        config.web_server.trusted_forwarded_proto_ips.is_empty(),
        "an absent transport trust list must mean deny-all, not an error and not trust"
    );
    assert!(
        !config.web_server.allow_insecure_http_login,
        "the fixture must keep the cleartext exception off"
    );
}

/// A configuration that names invalid trust entries (a hostname, a CIDR, a wildcard
/// string) must be rejected at load time instead of being silently dropped or widened.
///
/// The identical configuration without a trust list is parsed first: if it did not load,
/// every rejection below could be caused by an unrelated missing required field rather
/// than by the entry under test.
#[test]
fn non_address_trust_entries_are_rejected_at_load() {
    let baseline = web_server_json(None);
    serde_json::from_str::<Config>(&baseline).unwrap_or_else(|err| {
        panic!("the negative-control fixture must parse before a trust entry is added: {err}")
    });

    for bad in [
        r#"["10.23.45.0/24"]"#,
        r#"["proxy.internal"]"#,
        r#"["*"]"#,
    ] {
        let fixture = web_server_json(Some(bad));
        assert!(
            serde_json::from_str::<Config>(&fixture).is_err(),
            "a range/hostname/wildcard must not be accepted as an exact peer: {bad}"
        );
    }
}

/// A valid seed represented as JSON parses — the field is a plain address list, so a
/// correct deployment configuration is expressible without any other change.
#[test]
fn exact_peer_entry_parses_as_configuration() {
    let fixture = web_server_json(Some(r#"["10.23.45.150"]"#));
    let config: Config =
        serde_json::from_str(&fixture).expect("an exact peer address must be accepted");

    assert_eq!(
        config.web_server.trusted_forwarded_proto_ips.len(),
        1,
        "exactly the configured peer must be present"
    );
    assert!(
        config.web_server.forwarded_header.is_none(),
        "naming a transport peer must not imply header user authentication"
    );
    assert!(
        !config.web_server.allow_insecure_http_login,
        "trusting a transport peer must not enable the cleartext exception"
    );
}
