use super::*;
use actix_web::{App as HttpApp, HttpServer};
use common::config::{Config, StorageConfig};
use std::io::{Read, Write};

#[actix_web::test]
async fn astra_revoked_uninitialized_websocket_must_not_block_restore() {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test fixture operation must succeed")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("astra-ws-init-{n}"));
    std::fs::create_dir_all(&dir).expect("test fixture operation must succeed");
    let mut cfg = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    cfg.web_server.first_login_create_admin = true;
    let app = Data::new(
        App::new(cfg)
            .await
            .expect("test fixture operation must succeed"),
    );
    let user = app
        .try_add_first_login("fixture-admin".into(), "fixture-password".into())
        .await
        .expect("test fixture operation must succeed");
    let token = user
        .new_session(Duration::from_secs(3600))
        .await
        .expect("test fixture operation must succeed");
    let admin = user
        .into_admin()
        .await
        .expect("test fixture operation must succeed");
    let backup = app
        .export_backup(&admin)
        .await
        .expect("test fixture operation must succeed");
    let mut token_bytes = [0; 64];
    let token_string = token.encode(&mut token_bytes).to_owned();
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("test fixture operation must succeed");
    let addr = listener
        .local_addr()
        .expect("test fixture operation must succeed");
    let data = app.clone();
    let server = HttpServer::new(move || HttpApp::new().app_data(data.clone()).service(start_host))
        .workers(1)
        .listen(listener)
        .expect("test fixture operation must succeed")
        .run();
    let handle = server.handle();
    actix_rt::spawn(server);
    let socket=tokio::task::spawn_blocking(move || {
        let mut s=std::net::TcpStream::connect(addr).expect("test fixture operation must succeed");
        s.set_read_timeout(Some(Duration::from_secs(3))).expect("test fixture operation must succeed");
        write!(s,"GET /host/stream HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: mlSession={token_string}\r\n\r\n").expect("test fixture operation must succeed");
        let mut response=Vec::new();
        while !response.ends_with(b"\r\n\r\n") { let mut b=[0];s.read_exact(&mut b).expect("test fixture operation must succeed");response.push(b[0]); }
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"),"upgrade refused: {}",String::from_utf8_lossy(&response));
        s
    }).await.expect("test fixture operation must succeed");
    app.delete_session(token)
        .await
        .expect("test fixture operation must succeed");
    // Revocation is normally checked at a five-second interval, but Init has not arrived.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let completed = tokio::time::timeout(
        Duration::from_millis(300),
        app.import_backup(&admin, backup.clone()),
    )
    .await
    .is_ok();
    eprintln!(
        "ASTRA_PREINIT revoked_socket_open=true restore_finished_after_revocation={completed}"
    );
    socket
        .shutdown(std::net::Shutdown::Both)
        .expect("test fixture operation must succeed");
    drop(socket);
    // Control: closing only this fixture socket must release the gate.
    tokio::time::timeout(Duration::from_secs(3), app.import_backup(&admin, backup))
        .await
        .expect("socket closure must release maintenance gate")
        .expect("test fixture operation must succeed");
    handle.stop(true).await;
    std::fs::remove_dir_all(dir).expect("test fixture operation must succeed");
    assert!(
        completed,
        "revoked WebSocket that never sent Init retained the restore gate outside the supervised lifecycle"
    );
}
