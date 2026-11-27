//! Identity/host invariant regressions (MW-20).
//!
//! The mutation paths and the backup import path must agree on the same
//! invariants: role references exist, the store keeps at least one usable admin,
//! password cost stays bounded, and deleting a role does not leave orphaned
//! references behind.
use super::*;
use crate::app::storage::Storage;
use std::sync::Arc;

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-invariants-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).await.expect("create temp dir");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("load storage");
    (storage, dir)
}

fn role_add(name: &str, ty: RoleType) -> StorageRoleAdd {
    StorageRoleAdd {
        name: name.to_string(),
        ty,
        default_settings: Default::default(),
        permissions: Default::default(),
    }
}

fn user_add(name: &str, role_id: RoleId, password: Option<&str>) -> StorageUserAdd {
    StorageUserAdd {
        role_id,
        name: name.to_string(),
        password: password.map(|p| StoragePassword::new(p).expect("hash")),
        client_unique_id: name.to_string(),
    }
}

/// MW-20: creating a user that references a role which does not exist produces an
/// orphan record. Every mutation must reject it.
#[tokio::test]
async fn add_user_rejects_unknown_role() {
    let (storage, dir) = storage_in("unknown-role").await;

    let err = match storage
        .add_user(user_add("alice", RoleId(4242), Some("pw")))
        .await
    {
        Ok(_) => panic!("a user with an unknown role must be rejected"),
        Err(err) => err,
    };

    assert!(
        matches!(err, AppError::RoleNotFound),
        "expected RoleNotFound, got {err}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: moving a user to a role that does not exist has the same effect.
#[tokio::test]
async fn modify_user_rejects_unknown_role() {
    let (storage, dir) = storage_in("modify-role").await;
    let role = storage
        .add_role(role_add("User", RoleType::User))
        .await
        .expect("role");
    let alice = storage
        .add_user(user_add("alice", role.id, Some("pw")))
        .await
        .expect("alice");

    let err = storage
        .modify_user(
            alice.id,
            StorageUserModify {
                role_id: Some(RoleId(4242)),
                ..Default::default()
            },
        )
        .await
        .expect_err("an unknown target role must be rejected");

    assert!(
        matches!(err, AppError::RoleNotFound),
        "expected RoleNotFound, got {err}"
    );
    assert_eq!(
        storage.get_user(alice.id).await.expect("user").role_id,
        role.id,
        "the rejected change must not be applied"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: deleting the last admin locks every administrator out of the instance.
#[tokio::test]
async fn deleting_last_admin_is_rejected() {
    let (storage, dir) = storage_in("last-admin").await;
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role");
    let alice = storage
        .add_user(user_add("alice", admin_role.id, Some("pw")))
        .await
        .expect("alice");

    let err = storage
        .remove_user(alice.id)
        .await
        .expect_err("the last admin must not be deletable");
    assert!(
        matches!(err, AppError::Forbidden),
        "expected Forbidden, got {err}"
    );
    assert!(
        storage.get_user(alice.id).await.is_ok(),
        "the admin must still exist"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: demoting the last admin to a non-admin role has the same lockout effect.
#[tokio::test]
async fn demoting_last_admin_is_rejected() {
    let (storage, dir) = storage_in("demote-admin").await;
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role");
    let user_role = storage
        .add_role(role_add("User", RoleType::User))
        .await
        .expect("role");
    let alice = storage
        .add_user(user_add("alice", admin_role.id, Some("pw")))
        .await
        .expect("alice");

    let err = storage
        .modify_user(
            alice.id,
            StorageUserModify {
                role_id: Some(user_role.id),
                ..Default::default()
            },
        )
        .await
        .expect_err("the last admin must not be demoted");
    assert!(
        matches!(err, AppError::Forbidden),
        "expected Forbidden, got {err}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: deleting the admin role cascades into deleting its users, which would
/// leave the instance with no administrator.
#[tokio::test]
async fn deleting_last_admin_role_is_rejected() {
    let (storage, dir) = storage_in("last-admin-role").await;
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role");
    let alice = storage
        .add_user(user_add("alice", admin_role.id, Some("pw")))
        .await
        .expect("alice");

    let err = storage
        .remove_role(admin_role.id)
        .await
        .expect_err("removing the last admin role would lock the instance out");

    assert!(
        matches!(err, AppError::Forbidden),
        "expected Forbidden, got {err}"
    );
    assert!(storage.get_user(alice.id).await.is_ok());
    assert!(storage.get_role(admin_role.id).await.is_ok());

    let _ = fs::remove_dir_all(&dir).await;
}

/// Removing an unused role must still work: the invariant is about admins, not
/// about forbidding role deletion in general.
#[tokio::test]
async fn deleting_unused_role_succeeds() {
    let (storage, dir) = storage_in("unused-role").await;
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role");
    let _alice = storage
        .add_user(user_add("alice", admin_role.id, Some("pw")))
        .await
        .expect("alice");
    let unused = storage
        .add_role(role_add("Unused", RoleType::User))
        .await
        .expect("role");

    storage
        .remove_role(unused.id)
        .await
        .expect("an unused non-admin role must be removable");

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: deleting a role must not leave hosts pointing at users that were
/// cascaded away.
#[tokio::test]
async fn deleting_role_cleans_orphaned_host_ownership() {
    let (storage, dir) = storage_in("orphan-host").await;
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role");
    // A second admin keeps the store administrable so the cascade is allowed.
    let _root = storage
        .add_user(user_add("root", admin_role.id, Some("pw")))
        .await
        .expect("root");
    let role = storage
        .add_role(role_add("User", RoleType::User))
        .await
        .expect("role");
    let alice = storage
        .add_user(user_add("alice", role.id, Some("pw")))
        .await
        .expect("alice");
    let host = storage
        .add_host(StorageHostAdd {
            owner: Some(alice.id),
            address: "10.0.0.5".to_string(),
            http_port: 47989,
            pair_info: None,
            cache: StorageHostCache {
                name: "desktop".to_string(),
                mac: None,
            },
        })
        .await
        .expect("host");

    storage.remove_role(role.id).await.expect("remove role");

    // The audit's boundary: an orphaned owner reference must not survive. Either
    // the host becomes global (owner cleared) or it is removed; both are safe, a
    // dangling id is not.
    match storage.get_host(host.id).await {
        Ok(host) => assert_eq!(
            host.owner, None,
            "a host whose owner disappeared must be re-owned as global"
        ),
        Err(AppError::HostNotFound) => {}
        Err(err) => panic!("unexpected host lookup error: {err}"),
    }

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: the importer must apply the same reference checks as the mutation API,
/// so a hostile/broken backup cannot install an orphan user.
#[tokio::test]
async fn import_rejects_orphan_user_role() {
    let (storage, dir) = storage_in("import-orphan").await;

    let mut users = serde_json::Map::new();
    users.insert(
        "1".to_string(),
        serde_json::json!({
            "role_id": 999,
            "name": "alice",
            "password": {
                "salt": "00000000000000000000000000000000",
                "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                "iterations": 600000
            },
            "client_unique_id": "alice"
        }),
    );
    let mut roles = serde_json::Map::new();
    roles.insert(
        "1".to_string(),
        serde_json::json!({
            "name": "Admin",
            "ty": "Admin",
            "default_settings": {},
            "permissions": {
                "allow_add_hosts": true,
                "maximum_bitrate_kbps": null,
                "allow_codec_h264": true,
                "allow_codec_h265": true,
                "allow_codec_av1": true,
                "allow_hdr": true,
                "allow_transport_webrtc": true,
                "allow_transport_websockets": true
            }
        }),
    );

    let backup = serde_json::json!({
        "version": "3",
        "users": users,
        "hosts": {},
        "roles": roles
    });

    let err = storage
        .import_backup(backup)
        .await
        .expect_err("an orphan user role must be rejected");
    assert!(
        matches!(err, AppError::BadRequest),
        "expected BadRequest, got {err}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: a backup must not be able to install an unbounded password cost, which
/// would turn every login into a CPU denial of service.
#[tokio::test]
async fn import_rejects_excessive_password_iterations() {
    let (storage, dir) = storage_in("import-iterations").await;
    let backup = backup_with_iterations(u32::MAX);

    let err = storage
        .import_backup(backup)
        .await
        .expect_err("an excessive iteration count must be rejected");
    assert!(
        matches!(err, AppError::BadRequest),
        "expected BadRequest, got {err}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-20: importing an empty user list re-opens first-login bootstrap, which is a
/// provisioning transition rather than a restore. Reject it explicitly.
#[tokio::test]
async fn import_rejects_empty_users() {
    let (storage, dir) = storage_in("import-empty").await;
    let backup = serde_json::json!({
        "version": "3",
        "users": {},
        "hosts": {},
        "roles": {
            "1": {
                "name": "Admin",
                "ty": "Admin",
                "default_settings": {},
                "permissions": {
                    "allow_add_hosts": true,
                    "maximum_bitrate_kbps": null,
                    "allow_codec_h264": true,
                    "allow_codec_h265": true,
                    "allow_codec_av1": true,
                    "allow_hdr": true,
                    "allow_transport_webrtc": true,
                    "allow_transport_websockets": true
                }
            }
        }
    });

    let err = storage
        .import_backup(backup)
        .await
        .expect_err("an empty user set must not be importable");
    assert!(
        matches!(err, AppError::BadRequest),
        "expected BadRequest, got {err}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

fn backup_with_iterations(iterations: u32) -> serde_json::Value {
    serde_json::json!({
        "version": "3",
        "users": {
            "1": {
                "role_id": 1,
                "name": "alice",
                "password": {
                    "salt": "00000000000000000000000000000000",
                    "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "iterations": iterations
                },
                "client_unique_id": "alice"
            }
        },
        "hosts": {},
        "roles": {
            "1": {
                "name": "Admin",
                "ty": "Admin",
                "default_settings": {},
                "permissions": {
                    "allow_add_hosts": true,
                    "maximum_bitrate_kbps": null,
                    "allow_codec_h264": true,
                    "allow_codec_h265": true,
                    "allow_codec_av1": true,
                    "allow_hdr": true,
                    "allow_transport_webrtc": true,
                    "allow_transport_websockets": true
                }
            }
        }
    })
}
