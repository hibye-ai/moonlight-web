use actix_web::{
    Error, FromRequest, HttpRequest, HttpResponse,
    body::MessageBody,
    cookie::{Cookie, Expiration, SameSite, time::OffsetDateTime},
    dev::{Payload, ServiceRequest, ServiceResponse},
    get,
    middleware::Next,
    post,
    web::{Data, Json},
};
use common::api_bindings::PostLoginRequest;
use futures::future::{Ready, ready};
use log::warn;
use std::{net::IpAddr, pin::Pin, time::Duration};

use crate::app::{
    App, AppError,
    auth::{SessionToken, UserAuth},
    password::AdmissionRefusal,
    user::{Admin, AuthenticatedUser},
};

mod transport;

pub const COOKIE_SESSION_TOKEN_NAME: &str = "mlSession";

/// How long a refused client is told to wait before trying again.
const LOGIN_RETRY_AFTER_SECONDS: u64 = 60;

/// The real TCP peer, which is the only client identity a request cannot choose.
///
/// A forwarding header is not an acceptable substitute: `X-Forwarded-For` is written
/// by whoever opened the socket, so honouring it here would let an attacker mint a
/// fresh failure bucket per guess. Trusting a proxy to assert the *transport* scheme
/// or a username is likewise not the same as trusting it to name the client.
fn login_peer_ip(req: &HttpRequest) -> Option<IpAddr> {
    req.peer_addr().map(|addr| addr.ip())
}

/// The refusal for an attempt that was turned away before any password was checked.
///
/// `Retry-After` is set so a client knows the refusal is temporary; the in-flight
/// refusal is distinguished (503) from a spent budget (429) because the former means
/// "the server is busy right now", not "you are out of attempts".
fn admission_refusal_response(refusal: AdmissionRefusal) -> HttpResponse {
    match refusal {
        AdmissionRefusal::InFlight => HttpResponse::ServiceUnavailable()
            .append_header(("Retry-After", "1"))
            .finish(),
        AdmissionRefusal::Account | AdmissionRefusal::Client | AdmissionRefusal::Global => {
            HttpResponse::TooManyRequests()
                .append_header(("Retry-After", LOGIN_RETRY_AFTER_SECONDS.to_string()))
                .finish()
        }
    }
}

fn forwarded_auth_peer_is_trusted(req: &HttpRequest, trusted_proxy_ips: &[IpAddr]) -> bool {
    req.peer_addr()
        .map(|addr| {
            let ip = addr.ip();
            ip.is_loopback() || trusted_proxy_ips.contains(&ip)
        })
        .unwrap_or(false)
}

impl FromRequest for UserAuth {
    type Error = AppError;

    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        ready(extract_user_auth(req))
    }
}
pub(crate) fn extract_user_auth(req: &HttpRequest) -> Result<UserAuth, AppError> {
    let app = match req.app_data::<Data<App>>() {
        None => return Err(AppError::AppDestroyed),
        Some(value) => value,
    };

    if let Some(header_auth) = &app.config().web_server.forwarded_header
        && let Some(username) = req.headers().get(&header_auth.username_header)
    {
        if !forwarded_auth_peer_is_trusted(req, &header_auth.trusted_proxy_ips) {
            return Err(AppError::HeaderAuthDisabled);
        }

        let Ok(username) = username.to_str() else {
            return Err(AppError::HeaderAuthMalformed);
        };

        Ok(UserAuth::ForwardedHeaders {
            username: username.to_string(),
        })
    } else if let Some(bearer) = req.headers().get("Authorization") {
        // Look for bearer
        let Ok(bearer) = bearer.to_str() else {
            return Err(AppError::BearerMalformed);
        };

        let token_str = bearer
            .strip_prefix("Bearer")
            .ok_or(AppError::AuthorizationNotBearer)?
            .trim();

        let token = SessionToken::decode(token_str)?;

        Ok(UserAuth::Session(token))
    } else if let Some(cookie) = req.cookie(COOKIE_SESSION_TOKEN_NAME) {
        // Look for cookie
        let token = SessionToken::decode(cookie.value())?;

        Ok(UserAuth::Session(token))
    } else {
        Ok(UserAuth::None)
    }
}

impl FromRequest for AuthenticatedUser {
    type Error = AppError;

    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let app = match req.app_data::<Data<App>>() {
            None => return Box::pin(ready(Err(AppError::AppDestroyed))),
            Some(value) => value,
        };

        let auth_future = UserAuth::from_request(req, payload);

        let app = app.clone();
        Box::pin(async move {
            let auth = auth_future.await?;

            let user = app.user_by_auth(auth).await?;

            Ok(user)
        })
    }
}

impl FromRequest for Admin {
    type Error = AppError;

    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let future = AuthenticatedUser::from_request(req, payload);

        Box::pin(async move {
            let user = future.await?;

            user.into_admin().await
        })
    }
}

#[post("/login")]
async fn login(
    app: Data<App>,
    req: HttpRequest,
    Json(request): Json<PostLoginRequest>,
) -> Result<HttpResponse, Error> {
    // MW-22: the transport policy is decided before anything else touches account state.
    // A cleartext login on an HTTPS-only deployment is refused instead of receiving a
    // plaintext session — and, crucially, before `try_add_first_login` could create the
    // first admin. The `Secure` flag follows the same decision further down.
    let Some(secure_cookie) = transport::session_cookie_policy(&req, &app.config().web_server)
    else {
        warn!("refusing login: transport policy requires a trusted TLS path");
        return Ok(HttpResponse::UpgradeRequired()
            .append_header(("Upgrade", "TLS/1.2, HTTP/1.1"))
            .finish());
    };

    let username = request.name;
    let password = request.password;
    let peer_ip = login_peer_ip(&req);
    // The submitted name is the account label for admission purposes. It is also what
    // an unknown-username attempt is charged against, so probing a name is bounded even
    // though no such account exists yet.
    let account_label = username.clone();

    // MW-23: admission is decided and the in-flight slot reserved in one atomic step,
    // *before* any password is hashed. Checking a bound and later recording the outcome
    // as two independent operations is exactly the gap a concurrent burst walks
    // through, so the guard's lifetime covers the whole attempt.
    let admission = app.login_admission().try_enter(&account_label, peer_ip);
    let mut admission = match admission {
        Ok(guard) => guard,
        Err(refusal) => {
            warn!("refusing login: admission refused ({refusal:?})");
            return Ok(admission_refusal_response(refusal));
        }
    };

    let user_result = if app.config().web_server.first_login_create_admin {
        match app
            .try_add_first_login(username.clone(), password.clone())
            .await
        {
            Ok(user) => Ok(user),
            Err(AppError::FirstUserAlreadyExists) => {
                app.user_by_auth(UserAuth::UserPassword { username, password })
                    .await
            }
            Err(err) => Err(err),
        }
    } else {
        app.user_by_auth(UserAuth::UserPassword { username, password })
            .await
    };

    let user = match user_result {
        Ok(user) => {
            // Settle this reservation without erasing other clients' or
            // overlapping requests' failures, including for the same account.
            admission.record_success();
            user
        }
        Err(AppError::CredentialsWrong | AppError::UserNotFound) => {
            admission.record_failure();
            return Err(AppError::CredentialsWrong.into());
        }
        Err(err) => return Err(err.into()),
    };

    let session_expiration = app.config().web_server.session_cookie_expiration;

    let session = user.new_session(session_expiration).await?;
    let mut session_bytes = [0; _];
    let session_str = session.encode(&mut session_bytes);

    Ok(HttpResponse::Ok()
        .cookie(build_cookie(
            &app,
            session_expiration,
            session_str,
            secure_cookie,
        ))
        .finish())
}

#[post("/logout")]
async fn logout(app: Data<App>, auth: UserAuth, req: HttpRequest) -> Result<HttpResponse, Error> {
    // Logout is intentionally idempotent. A configured default user has no
    // session token to revoke, while cookie/session users do. In either case
    // clear any stale session cookie and return success.
    if let UserAuth::Session(session) = auth {
        app.delete_session(session).await?;
    }

    let mut response = HttpResponse::Ok().finish();

    if req.cookie(COOKIE_SESSION_TOKEN_NAME).is_some() {
        // Removal must match the attributes the cookie may have been set with. When
        // cleartext is refused there is nothing to clear over this transport, but the
        // `Secure` variant is still emitted so a policy-enabled deployment clears the
        // cookie a browser holds from an earlier TLS session.
        let secure =
            transport::session_cookie_policy(&req, &app.config().web_server).unwrap_or(true);
        response.add_removal_cookie(&build_cookie(&app, Duration::ZERO, "", secure))?;
    }

    Ok(response)
}

pub async fn auth_middleware(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let Some(app) = req.app_data::<Data<App>>().cloned() else {
        return Err(AppError::AppDestroyed.into());
    };

    let mut response = next.call(req).await?;
    if let Some(err) = response.response().error()
        && let Some(AppError::SessionTokenNotFound) = err.as_error::<AppError>()
    {
        response.response_mut().add_removal_cookie(&build_cookie(
            &app,
            Duration::ZERO,
            "",
            true,
        ))?;
    }

    Ok(response)
}

pub fn build_cookie<'a>(
    app: &'a App,
    expiration: Duration,
    session_str: &'a str,
    secure: bool,
) -> Cookie<'a> {
    Cookie::build(COOKIE_SESSION_TOKEN_NAME, session_str)
        .path(&app.config().web_server.url_path_prefix)
        .same_site(SameSite::Strict)
        .http_only(true) // not accessible via js
        .secure(secure)
        .expires(Expiration::DateTime(OffsetDateTime::now_utc() + expiration))
        .finish()
}

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod mw23_tests;

#[get("/authenticate")]
async fn authenticate(_user: AuthenticatedUser) -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[cfg(test)]
mod astra_deployment_tests;

#[cfg(test)]
mod transport_trust_tests;

#[cfg(test)]
mod migration_artifact_tests;

#[cfg(test)]
mod session_issuance_tests;
