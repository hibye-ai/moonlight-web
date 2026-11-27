use super::*;

macro_rules! post {
    ($service:expr, $name:expr, $password:expr, $peer:expr) => {
        test::call_service(&$service, request($name, $password, $peer).to_request())
            .await
            .status()
    };
}

#[actix_web::test]
async fn last_account_slot_cannot_admit_a_sixteen_request_burst() {
    let fixture = fixture("independent-near-bound", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;
    for index in 0..SPEC_ACCOUNT_MAX_FAILURES - 1 {
        assert_eq!(
            post!(service, ALICE, "wrong-password", &peer(index)),
            StatusCode::UNAUTHORIZED
        );
    }
    let calls = (30..46).map(|index| {
        let attempt = request(ALICE, "wrong-password", &peer(index)).to_request();
        let service = &service;
        async move { test::call_service(service, attempt).await.status() }
    });
    let statuses = futures::future::join_all(calls).await;
    let checked = statuses
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    eprintln!("near-bound HTTP burst: checked={checked}; statuses={statuses:?}");
    assert_eq!(
        checked, 1,
        "nine recorded failures leave exactly one request-start account reservation"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        15
    );
    assert_eq!(post!(service, BOB, BOB_PASSWORD, &peer(90)), StatusCode::OK);
}

#[actix_web::test]
async fn success_from_a_different_client_cannot_erase_other_clients_account_failures() {
    let fixture = fixture("independent-other-client", |_| {}).await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;
    for _ in 0..SPEC_ACCOUNT_MAX_FAILURES - 1 {
        assert_eq!(
            post!(service, ALICE, "wrong-password", "127.0.2.1:5000"),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        post!(service, ALICE, ALICE_PASSWORD, "127.0.2.2:5000"),
        StatusCode::OK
    );
    assert_eq!(
        post!(service, ALICE, "wrong-password", "127.0.2.1:5000"),
        StatusCode::UNAUTHORIZED
    );
    let status = post!(service, ALICE, "wrong-password", "127.0.2.1:5000");
    eprintln!("other-client success followed by eleventh wrong guess: {status}");
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a different client's success must not erase the first client's nine account failures"
    );
}

#[actix_web::test]
async fn rotating_xff_cannot_bypass_client_bound_for_trusted_or_untrusted_peers() {
    let fixture = fixture("independent-xff", |config| {
        config.web_server.forwarded_header = Some(common::config::ForwardedHeaders {
            username_header: "X-Forwarded-User".into(),
            auto_create_missing_user: false,
            trusted_proxy_ips: vec![
                "127.0.3.7"
                    .parse()
                    .expect("test fixture operation must succeed"),
            ],
        });
    })
    .await;
    let service =
        test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(login)).await;
    // Unique unknown fixture names isolate the peer bucket from the account limit.
    for (label, peer) in [
        ("trusted", "127.0.3.7:5000"),
        ("untrusted", "127.0.3.8:5000"),
    ] {
        for i in 0..60 {
            let req = request(&format!("{label}-{i}"), "wrong", peer)
                .insert_header(("X-Forwarded-For", format!("192.0.2.{}", i + 1)))
                .to_request();
            assert_eq!(
                test::call_service(&service, req).await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let req = request(&format!("{label}-last"), "wrong", peer)
            .insert_header(("X-Forwarded-For", "198.51.100.254"))
            .to_request();
        assert_eq!(
            test::call_service(&service, req).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
