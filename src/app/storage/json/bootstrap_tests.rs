//! Bootstrap and identity uniqueness regressions (MW-16, MW-20).
//! These drive the real `JsonStorage` concurrently to reproduce the
//! check-then-act windows.
use super::*;
use crate::app::storage::Storage;
use std::sync::Arc;

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-bootstrap-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).await.expect("create temp dir");
    let file = dir.join("data.json");
    let storage = JsonStorage::load(file, Duration::from_secs(3600))
        .await
        .expect("load storage");
    (storage, dir)
}

fn admin_role() -> StorageRoleAdd {
    StorageRoleAdd {
        name: "Admin".to_string(),
        ty: RoleType::Admin,
        default_settings: Default::default(),
        permissions: Default::default(),
    }
}

fn user_add(name: &str, role_id: RoleId) -> StorageUserAdd {
    StorageUserAdd {
        role_id,
        name: name.to_string(),
        password: None,
        client_unique_id: name.to_string(),
    }
}

/// MW-16: two concurrent first-login bootstraps must not both become admin.
/// Exactly one caller may win the empty-store check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_admin_provisioning_creates_one_admin() {
    let (storage, dir) = storage_in("first-admin").await;
    let role = storage.add_role(admin_role()).await.expect("role").id;

    let mut handles = Vec::new();
    for i in 0..8 {
        let storage = storage.clone();
        handles.push(tokio::spawn(async move {
            storage
                .add_first_user(user_add(&format!("admin{i}"), role))
                .await
        }));
    }

    let mut created = 0;
    let mut rejected = 0;
    for handle in handles {
        match handle.await.expect("join") {
            Ok(_) => created += 1,
            Err(AppError::FirstUserAlreadyExists) => rejected += 1,
            Err(err) => panic!("unexpected bootstrap error: {err}"),
        }
    }

    assert_eq!(created, 1, "exactly one bootstrap may provision an admin");
    assert_eq!(rejected, 7, "the losing bootstraps must be rejected");
    assert_eq!(
        storage.list_users().await.expect("users").len_checked(),
        1,
        "the store must hold exactly one user"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-16: concurrent creation with the same username must not produce duplicate
/// names. The uniqueness check and the insert belong to the same critical section.
/// Run under the same pressure that reproduced the baseline window (32 callers x
/// 20 rounds) so this is a real regression guard, not a lucky single shot.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_same_name_creation_is_rejected() {
    const CALLERS: usize = 32;
    const ROUNDS: usize = 20;

    for round in 0..ROUNDS {
        let (storage, dir) = storage_in(&format!("dup-name-{round}")).await;
        let role = storage.add_role(admin_role()).await.expect("role").id;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CALLERS));

        let mut handles = Vec::new();
        for _ in 0..CALLERS {
            let storage = storage.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                storage.add_user(user_add("alice", role)).await
            }));
        }

        let mut created = 0;
        let mut conflicts = 0;
        for handle in handles {
            match handle.await.expect("join") {
                Ok(_) => created += 1,
                Err(AppError::UserAlreadyExists) => conflicts += 1,
                Err(err) => panic!("unexpected create error: {err}"),
            }
        }

        assert_eq!(created, 1, "round {round}: only one 'alice' may be created");
        assert_eq!(conflicts, CALLERS - 1, "round {round}: rest must conflict");
        assert_eq!(
            storage.list_users().await.expect("users").len_checked(),
            1,
            "round {round}: duplicate username rows must not exist"
        );

        let _ = fs::remove_dir_all(&dir).await;
    }
}

/// Helper so the assertions above read clearly without depending on `Either`.
trait LenChecked {
    fn len_checked(&self) -> usize;
}

impl LenChecked for Either<Vec<UserId>, Vec<StorageUser>> {
    fn len_checked(&self) -> usize {
        match self {
            Either::Left(ids) => ids.len(),
            Either::Right(users) => users.len(),
        }
    }
}
