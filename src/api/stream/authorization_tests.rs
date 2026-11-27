//! MW-02 regressions: an open stream must lose its authorization when the session,
//! the account, or the role permissions behind it are revoked.
//!
//! These drive the real `App`/`JsonStorage` authorization path. The defect: the
//! stream authenticated once at upgrade time and never re-checked, so an admin
//! session revocation, a password rotation, or a permission reduction never reached a
//! running video/input stream.
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::api_bindings::StreamPermissions;
use common::config::{Config, StorageConfig};

use super::authorization::revalidate_stream_authorization;
use crate::app::{
    App, AppError,
    auth::{SessionToken, UserAuth},
    password::StoragePassword,
    role::RoleId,
    storage::{StorageRoleModify, StorageRolePermissions, StorageUserAdd},
    user::{Admin, AuthenticatedUser, UserId},
};

async fn test_app(label: &str) -> (Arc<App>, std::path::PathBuf) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-mw02-{label}-{}-{unique}",
        std::process::id()
    ));
    tokio::fs::create_dir_all(&dir).await.expect("create dir");

    let config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().to_string(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };

    let app = App::new(config).await.expect("build app");
    (Arc::new(app), dir)
}

/// Provisions an admin account with a password and returns its user id plus a live
/// session token, mirroring what a browser holds after `/login`.
async fn admin_with_session(app: &Arc<App>) -> (UserId, SessionToken) {
    let admin = app
        .try_add_first_login("alice".to_string(), "password-1".to_string())
        .await
        .expect("first login admin");
    let id = admin.id();
    let session = admin
        .new_session(Duration::from_secs(3600))
        .await
        .expect("session");
    (id, session)
}

/// The authenticated handle for a live session, i.e. the same identity a request
/// would resolve to.
async fn authenticated(app: &Arc<App>, session: SessionToken) -> AuthenticatedUser {
    app.user_by_session(session).await.expect("session user")
}

async fn admin_handle(app: &Arc<App>, session: SessionToken) -> Admin {
    authenticated(app, session)
        .await
        .into_admin()
        .await
        .expect("admin")
}

/// The caller's own role id (needs a mutable handle; `Admin` only derefs immutably).
async fn session_role_id(app: &Arc<App>, session: SessionToken) -> RoleId {
    authenticated(app, session)
        .await
        .role_id()
        .await
        .expect("role id")
}

fn all_permissions() -> StreamPermissions {
    StreamPermissions {
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

/// `StorageRoleModify` has no `Default`; build one that changes only permissions.
fn permission_modify(permissions: StorageRolePermissions) -> StorageRoleModify {
    StorageRoleModify {
        name: None,
        ty: None,
        default_settings: None,
        permissions: Some(permissions),
    }
}

/// MW-02: revoking the session (admin "delete session") must end an already-open
/// stream. Baseline behavior kept streaming because nothing re-read the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_session_stops_an_open_stream() {
    let (app, dir) = test_app("revoked-session").await;
    let (_, session) = admin_with_session(&app).await;

    let auth = UserAuth::Session(session);
    let granted = all_permissions();

    assert!(
        revalidate_stream_authorization(&app, &auth, &granted)
            .await
            .is_ok(),
        "a live session must keep its stream authorized"
    );

    // An admin revokes exactly this session (same store call as DELETE /session).
    app.delete_session(session).await.expect("delete session");

    assert!(
        matches!(
            revalidate_stream_authorization(&app, &auth, &granted).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "a revoked session must stop the stream it authorized"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// MW-02: deleting the account must stop its open streams, not just fail new requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_account_stops_an_open_stream() {
    let (app, dir) = test_app("deleted-account").await;
    let (id, session) = admin_with_session(&app).await;

    let auth = UserAuth::Session(session);
    let granted = all_permissions();

    // A second admin exists so the deletion is not refused by the last-admin guard.
    let admin = admin_handle(&app, session).await;
    let role_id = session_role_id(&app, session).await;
    app.add_user(
        &admin,
        StorageUserAdd {
            role_id,
            name: "bob".to_string(),
            password: Some(StoragePassword::new("password-2").expect("hash")),
            client_unique_id: "bob".to_string(),
        },
    )
    .await
    .expect("second admin");

    let victim = app.user_by_id(id).await.expect("victim");
    victim.delete(&admin).await.expect("delete user");

    assert!(
        matches!(
            revalidate_stream_authorization(&app, &auth, &granted).await,
            Err(AppError::SessionTokenNotFound | AppError::UserNotFound | AppError::Unauthorized)
        ),
        "a deleted account must stop the stream it authorized"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// MW-02: a password rotation must end the sessions (and therefore streams) that
/// were authorized with the old password.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_change_stops_an_open_stream() {
    let (app, dir) = test_app("password-change").await;
    let (_, session) = admin_with_session(&app).await;

    let auth = UserAuth::Session(session);
    let granted = all_permissions();

    let mut user = authenticated(&app, session).await;
    user.set_password(StoragePassword::new("password-2").expect("hash"))
        .await
        .expect("change password");

    assert!(
        matches!(
            revalidate_stream_authorization(&app, &auth, &granted).await,
            Err(AppError::SessionTokenNotFound | AppError::Unauthorized)
        ),
        "a password change must stop streams authorized by the old password"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// MW-02: reducing a role's permissions must stop a stream that is using a
/// permission it no longer has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_reduction_stops_an_open_stream() {
    let (app, dir) = test_app("permission-reduction").await;
    let (_, session) = admin_with_session(&app).await;

    let auth = UserAuth::Session(session);
    let granted = all_permissions();

    let admin = admin_handle(&app, session).await;
    let role_id = session_role_id(&app, session).await;

    // The stream is authorized and everything is still granted.
    assert!(
        revalidate_stream_authorization(&app, &auth, &granted)
            .await
            .is_ok()
    );

    // The role loses WebRTC transport while the stream is running.
    let role = app.role_by_id(role_id).await.expect("role");
    role.modify(
        &admin,
        permission_modify(StorageRolePermissions {
            allow_transport_webrtc: false,
            ..Default::default()
        }),
    )
    .await
    .expect("modify role");

    assert!(
        matches!(
            revalidate_stream_authorization(&app, &auth, &granted).await,
            Err(AppError::Forbidden)
        ),
        "a revoked transport permission must stop the running stream"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// MW-02: a *grant* added after the stream started is not a reason to kill it, and an
/// unchanged role must keep the stream alive. This guards against a re-check that
/// tears down healthy streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_or_widened_permissions_keep_the_stream() {
    let (app, dir) = test_app("widened").await;
    let (_, session) = admin_with_session(&app).await;

    let auth = UserAuth::Session(session);

    let admin = admin_handle(&app, session).await;
    let role_id = session_role_id(&app, session).await;

    // The stream was granted a *narrower* permission set than the role allows.
    let granted = StreamPermissions {
        allow_transport_webrtc: false,
        ..all_permissions()
    };

    let role = app.role_by_id(role_id).await.expect("role");
    role.modify(&admin, permission_modify(StorageRolePermissions::default()))
        .await
        .expect("modify role");

    assert!(
        revalidate_stream_authorization(&app, &auth, &granted)
            .await
            .is_ok(),
        "widening permissions must not terminate a running stream"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}
