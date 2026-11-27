//! Request-transport policy for the session cookie (MW-22).
//!
//! `session_cookie_secure` used to be advisory: the `Secure` flag was only set when
//! the *request* already looked like https, and the login itself always succeeded. So
//! a cleartext reachable listener still minted plaintext sessions, and any client that
//! could set `X-Forwarded-Proto` could steer the decision.
//!
//! This module turns it into a real policy:
//! - the connection scheme is decided from the listener's own TLS state, not from the
//!   request-target and not from client-supplied headers;
//! - an absolute-form `https://` request-target over a cleartext connection is a client
//!   claim about itself, so it never counts as proof of TLS;
//! - a forwarded scheme is honoured only from a trusted proxy peer;
//! - `allow_insecure_http_login` is the only way to permit cleartext logins.
use actix_web::{HttpRequest, http::header};
use common::config::WebServerConfig;

/// How the browser reached us, after applying the proxy-trust rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestTransport {
    /// The accepting listener itself terminated TLS.
    Https,
    /// A trusted reverse proxy in front of us terminated TLS.
    HttpsViaTrustedProxy,
    /// Cleartext.
    Http,
    /// Cleartext connection whose request-target claims `https://`. The request-target
    /// is written by the client, so this is a forgery attempt, not evidence of TLS.
    CleartextAbsoluteFormSpoof,
}

impl RequestTransport {
    fn is_secure(self) -> bool {
        matches!(self, Self::Https | Self::HttpsViaTrustedProxy)
    }
}

/// Whether this exact peer address is trusted to assert the external scheme.
///
/// Only unicast, fully-specified addresses can ever match. A misconfigured entry such
/// as `0.0.0.0` or `::` therefore grants nothing — it can never be a real peer address,
/// and treating "unspecified" as a wildcard would silently widen the trust list to
/// every peer, which is exactly the default-deny property this list exists to keep.
pub(super) fn is_trusted_transport_peer(ip: std::net::IpAddr, config: &WebServerConfig) -> bool {
    if !is_proxy_capable_address(ip) {
        return false;
    }

    config.trusted_forwarded_proto_ips.contains(&ip)
}

/// Excludes addresses that can never name a real accepting peer.
///
/// `is_unicast`/`is_unspecified` live on `Ipv4Addr`/`Ipv6Addr`, not on the `IpAddr`
/// enum, so the check is spelled out per family.
fn is_proxy_capable_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast()
        }
        std::net::IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// Whether the peer may assert a forwarded scheme.
///
/// This is a *transport* trust decision and is deliberately independent of
/// [`ForwardedHeaders`], which authenticates a user from a header (AR-07). A
/// deployment behind a TLS-terminating reverse proxy trusts that proxy to report the
/// external scheme; it must not have to enable header-based user authentication —
/// and thereby accept an identity from that header — just to keep a `Secure` cookie
/// working. `trusted_forwarded_proto_ips` is the only list consulted here, and it
/// matches exact peer addresses.
///
/// The same rule as forwarded-header authentication otherwise applies: loopback or an
/// explicitly configured proxy address. `req.connection_info()` is deliberately *not*
/// used to read the scheme, because it prefers `Forwarded`/`X-Forwarded-Proto` from
/// any peer.
fn peer_may_assert_scheme(req: &HttpRequest, config: &WebServerConfig) -> bool {
    let Some(addr) = req.peer_addr() else {
        return false;
    };
    let ip = addr.ip();

    ip.is_loopback() || is_trusted_transport_peer(ip, config)
}

fn forwarded_proto(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get("X-Forwarded-Proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

fn forwarded_header_proto_is_https(req: &HttpRequest) -> bool {
    req.headers()
        .get(header::FORWARDED)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').any(|part| {
                part.trim().split_once('=').is_some_and(|(name, value)| {
                    name.trim().eq_ignore_ascii_case("proto")
                        && value.trim().trim_matches('"').eq_ignore_ascii_case("https")
                })
            })
        })
}

/// Whether the request-target is absolute-form (`scheme://authority/path`) and claims
/// `https`. An ordinary browser request is origin-form (`POST /api/login`) and carries no
/// scheme at all, so a scheme here is always something the client wrote.
fn absolute_form_claims_https(req: &HttpRequest) -> bool {
    req.uri()
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
}

/// Classifies the request transport, ignoring untrusted forwarding headers.
///
/// `AppConfig::secure()` is the only authoritative TLS signal: actix sets it from the
/// accepting listener (`bind_openssl`/`listen_openssl`), whereas the request-target and
/// the `Forwarded` headers are both written by whoever opened the socket.
pub(super) fn request_transport(req: &HttpRequest, config: &WebServerConfig) -> RequestTransport {
    if req.app_config().secure() {
        return RequestTransport::Https;
    }

    // A trusted proxy terminating TLS in front of us is cleartext on this leg, so its
    // assertion is honoured here — and only from a peer we already trust.
    if peer_may_assert_scheme(req, config)
        && (forwarded_proto(req).is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
            || forwarded_header_proto_is_https(req))
    {
        return RequestTransport::HttpsViaTrustedProxy;
    }

    if absolute_form_claims_https(req) {
        return RequestTransport::CleartextAbsoluteFormSpoof;
    }

    RequestTransport::Http
}

/// Decides whether a login over this transport may proceed, and whether the resulting
/// cookie must carry `Secure`.
///
/// Returns `None` when the login must be refused: the client forged an `https://`
/// request-target over a cleartext connection, or the operator asked for HTTPS-only
/// sessions and this request did not arrive over a trusted TLS path.
pub(super) fn session_cookie_policy(req: &HttpRequest, config: &WebServerConfig) -> Option<bool> {
    let transport = request_transport(req, config);

    if transport == RequestTransport::CleartextAbsoluteFormSpoof {
        // Not "merely cleartext": the request asserts a scheme the socket never
        // negotiated. Neither the dev exception nor a disabled policy legitimises that.
        return None;
    }

    if !config.session_cookie_secure {
        // Explicitly not an HTTPS-only deployment.
        return Some(transport.is_secure());
    }

    if transport.is_secure() {
        return Some(true);
    }

    if config.allow_insecure_http_login {
        // Explicit, documented development exception.
        return Some(false);
    }

    // HTTPS-only: a cleartext login must not mint a session.
    None
}
