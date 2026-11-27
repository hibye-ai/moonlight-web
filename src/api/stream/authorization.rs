//! Stream authorization revalidation (MW-02).
//!
//! A `/host/stream` upgrade authenticates once and then forwards video/input for as
//! long as the socket lives. The identity that was authorized at upgrade time can be
//! deleted, its session revoked, or its role's permissions reduced while the stream is
//! running. This module re-checks that authorization so a revocation reaches streams
//! that are already open instead of waiting for the socket to close.
use std::time::Duration;

use common::api_bindings::StreamPermissions;
use log::debug;

use crate::app::{App, AppError, auth::UserAuth};

/// How often an open stream revalidates its authorization.
pub(super) const STREAM_AUTHORIZATION_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Re-checks that an open stream may keep running.
///
/// Returns `Ok(())` while the stream's authorization is still in force. Any of the
/// following ends the stream:
/// - the session token was revoked (admin revocation, password change, or logout),
/// - the session expired,
/// - the account no longer exists,
/// - the role's permissions were reduced below what the stream was started with.
///
/// `granted` is the permission snapshot that was sent to the streamer at startup; a
/// permission that is no longer granted means the running stream is doing something
/// the current role does not allow.
pub(super) async fn revalidate_stream_authorization(
    app: &App,
    auth: &UserAuth,
    granted: &StreamPermissions,
) -> Result<(), AppError> {
    // Resolving the authorization again runs the real session lookup, which enforces
    // expiry and re-reads the current user record (MW-01, MW-15).
    let mut user = app.user_by_auth(auth.clone()).await?;

    let current = user.role().await?.permissions().await?;

    if let Some(revoked) = revoked_permission(granted, &current) {
        debug!("[Stream]: authorization revoked, {revoked} is no longer granted");
        return Err(AppError::Forbidden);
    }

    Ok(())
}

/// Names the first permission that the stream holds but the current role does not.
fn revoked_permission(
    granted: &StreamPermissions,
    current: &StreamPermissions,
) -> Option<&'static str> {
    let checks = [
        (
            "allow_add_hosts",
            granted.allow_add_hosts,
            current.allow_add_hosts,
        ),
        (
            "allow_codec_h264",
            granted.allow_codec_h264,
            current.allow_codec_h264,
        ),
        (
            "allow_codec_h265",
            granted.allow_codec_h265,
            current.allow_codec_h265,
        ),
        (
            "allow_codec_av1",
            granted.allow_codec_av1,
            current.allow_codec_av1,
        ),
        ("allow_hdr", granted.allow_hdr, current.allow_hdr),
        (
            "allow_transport_webrtc",
            granted.allow_transport_webrtc,
            current.allow_transport_webrtc,
        ),
        (
            "allow_transport_websockets",
            granted.allow_transport_websockets,
            current.allow_transport_websockets,
        ),
    ];

    for (name, was_granted, still_granted) in checks {
        if was_granted && !still_granted {
            return Some(name);
        }
    }

    // A bitrate ceiling that got tighter is also a reduction of what the stream may do.
    match (granted.maximum_bitrate_kbps, current.maximum_bitrate_kbps) {
        (None, Some(_)) => Some("maximum_bitrate_kbps"),
        (Some(granted_max), Some(current_max)) if current_max < granted_max => {
            Some("maximum_bitrate_kbps")
        }
        _ => None,
    }
}
