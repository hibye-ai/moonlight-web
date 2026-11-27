//! F2 image clipboard: strict PNG/base64 validation, helper capability negotiation,
//! mock-helper wire format and real-handler check ordering.
//!
//! The "helper" here is a loopback TcpListener scripted per test; nothing touches a real
//! clipboard. Handler tests run the real actix handlers with a real App/session and
//! prove that rejected requests never open a connection to the helper.
use super::*;
use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use actix_web::{App as ActixApp, body::to_bytes, cookie::Cookie, test as actix_test};
use common::config::{Config, StorageConfig};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

use crate::api::auth::COOKIE_SESSION_TOKEN_NAME;

/// 1x1 RGBA PNG produced by an independent encoder (CRCs and zlib stream verified with
/// Node's zlib before being embedded here).
const PIXEL_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
const IMAGE_CAPABILITIES: &str = r#"{"formats":["text/plain","image/png"],"max_text_bytes":65536,"max_image_bytes":4194304,"max_image_dimension":4096,"max_image_pixels":16777216}"#;
const HOST_ID: u32 = 4242;

fn pixel_png() -> Vec<u8> {
    decode_base64_strict(PIXEL_PNG_BASE64).expect("fixture is canonical base64")
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        &u32::try_from(data.len())
            .expect("fixture chunk fits u32")
            .to_be_bytes(),
    );
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

/// Structurally valid PNG with the given IHDR size (pixel data is not decodable; the
/// server does not decompress, the helper does).
fn png_with(width: u32, height: u32, extra: &[Vec<u8>]) -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(chunk(b"IHDR", &ihdr));
    for extra in extra {
        out.extend_from_slice(extra);
    }
    out.extend(chunk(b"IDAT", &[0x78, 0x9c, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01]));
    out.extend(chunk(b"IEND", &[]));
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= group.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Pure validation
// ---------------------------------------------------------------------------

#[test]
fn crc32_matches_reference_vector() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn strict_base64_round_trips_and_rejects_noncanonical_input() {
    for length in 0..16usize {
        let bytes: Vec<u8> = (0..length).map(|i| (i * 37 + length) as u8).collect();
        assert_eq!(
            decode_base64_strict(&base64(&bytes)).as_deref(),
            Some(bytes.as_slice()),
            "round trip of {length} bytes"
        );
    }
    for bad in [
        "abc", "ab=c", "a=bc", "ab c", "ab\ncd", "-_ab", "====", "abcd=", "ab==cd==", "a===",
        "\u{e9}bcd", "AB==", "A/==", "AAB=", "AA/=", "Zh==", "Zm9=",
    ] {
        assert_eq!(decode_base64_strict(bad), None, "{bad:?} must be rejected");
    }
    assert_eq!(pixel_png().len(), 70, "fixture decodes to its known size");
}

#[test]
fn real_png_is_accepted() {
    assert_eq!(validate_png(&pixel_png(), MAX_IMAGE_BYTES), Ok((1, 1)));
    assert_eq!(
        decode_png_base64(PIXEL_PNG_BASE64, MAX_IMAGE_BYTES).map(|bytes| bytes.len()),
        Ok(70)
    );
    let with_text = png_with(3, 2, &[chunk(b"tEXt", b"Comment\0ok")]);
    assert_eq!(
        validate_png(&with_text, MAX_IMAGE_BYTES),
        Ok((3, 2)),
        "known ancillary chunks are allowed"
    );
}

#[test]
fn malformed_png_structure_is_rejected() {
    let good = pixel_png();
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut signature = good.clone();
    signature[1] = b'Q';
    cases.push(("signature", signature));
    let mut ihdr_length = good.clone();
    ihdr_length[11] = 12;
    cases.push(("IHDR length", ihdr_length));
    let mut ihdr_crc = good.clone();
    ihdr_crc[29] ^= 0xff;
    cases.push(("IHDR CRC", ihdr_crc));
    let mut idat_data = good.clone();
    idat_data[41] ^= 0x01;
    cases.push(("IDAT CRC mismatch", idat_data));
    cases.push(("truncated", good[..good.len() - 5].to_vec()));
    let mut trailing = good.clone();
    trailing.extend_from_slice(b"junk");
    cases.push(("data after IEND", trailing));
    let mut depth = good.clone();
    depth[24] = 4;
    cases.push(("illegal bit depth", depth));
    cases.push(("zero width", png_with(0, 1, &[])));
    cases.push((
        "unknown critical chunk",
        png_with(1, 1, &[chunk(b"ZZZZ", b"x")]),
    ));
    cases.push(("non-letter chunk type", png_with(1, 1, &[chunk(b"t3Xt", b"x")])));
    cases.push(("second IHDR", png_with(1, 1, &[chunk(b"IHDR", &[0; 13])])));
    let mut no_idat = PNG_SIGNATURE.to_vec();
    no_idat.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]));
    no_idat.extend(chunk(b"IEND", &[]));
    cases.push(("missing IDAT", no_idat));
    cases.push(("JPEG", vec![0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 0xff, 0xd9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    for (name, bytes) in cases {
        assert_eq!(
            validate_png(&bytes, MAX_IMAGE_BYTES),
            Err(ImageRejection::Invalid),
            "{name} must be invalid"
        );
    }
}

#[test]
fn png_dimension_pixel_and_byte_bounds() {
    assert_eq!(
        validate_png(&png_with(4096, 4096, &[]), MAX_IMAGE_BYTES),
        Ok((4096, 4096)),
        "4096x4096 (16M pixels) is the inclusive maximum"
    );
    for (width, height) in [(4097, 1), (1, 4097), (u32::MAX, u32::MAX)] {
        assert_eq!(
            validate_png(&png_with(width, height, &[]), MAX_IMAGE_BYTES),
            Err(ImageRejection::TooLarge),
            "{width}x{height}"
        );
    }
    let big = png_with(1, 1, &[chunk(b"tEXt", &vec![b'a'; MAX_IMAGE_BYTES])]);
    assert_eq!(
        validate_png(&big, MAX_IMAGE_BYTES),
        Err(ImageRejection::TooLarge)
    );
    assert_eq!(
        validate_png(&pixel_png(), 69),
        Err(ImageRejection::TooLarge),
        "a smaller negotiated cap applies"
    );
    // The encoded length is rejected before any decoding work.
    let oversized = "A".repeat(MAX_IMAGE_BASE64_BYTES + 4);
    assert_eq!(
        decode_png_base64(&oversized, MAX_IMAGE_BYTES),
        Err(ImageRejection::TooLarge)
    );
    assert_eq!(
        decode_png_base64("not base64!", MAX_IMAGE_BYTES),
        Err(ImageRejection::Invalid)
    );
    assert_eq!(MAX_IMAGE_BASE64_BYTES, 5_592_408);
}

#[test]
fn capability_parsing_and_status_shapes() {
    assert_eq!(
        parse_capabilities(IMAGE_CAPABILITIES.as_bytes()),
        HelperCapabilities::Image {
            max_image_bytes: MAX_IMAGE_BYTES
        }
    );
    assert_eq!(
        parse_capabilities(br#"{"formats":["text/plain","image/png"],"max_image_bytes":99999999999}"#),
        HelperCapabilities::Image {
            max_image_bytes: MAX_IMAGE_BYTES
        },
        "helper limits are clamped to the server maximum"
    );
    assert_eq!(
        parse_capabilities(br#"{"formats":["text/plain"]}"#),
        HelperCapabilities::TextOnly
    );
    assert_eq!(
        parse_capabilities(br#"{"formats":["text/plain","image/png"]}"#),
        HelperCapabilities::TextOnly,
        "image without a declared limit is not negotiated"
    );
    assert_eq!(
        parse_capabilities(br#"{"formats":["text/html","files"],"max_image_bytes":10}"#),
        HelperCapabilities::TextOnly,
        "HTML/files are never advertised"
    );
    assert_eq!(parse_capabilities(b"not json"), HelperCapabilities::Unknown);

    let none = status_body(None);
    assert_eq!(none, json!({ "available": false, "max_bytes": MAX_TEXT_BYTES }));
    let old = status_body(Some(HelperCapabilities::TextOnly));
    assert_eq!(
        old,
        json!({ "available": true, "max_bytes": MAX_TEXT_BYTES, "formats": ["text/plain"] })
    );
    let image = status_body(Some(HelperCapabilities::Image {
        max_image_bytes: 1234,
    }));
    assert_eq!(
        image,
        json!({ "available": true, "max_bytes": MAX_TEXT_BYTES, "formats": ["text/plain", "image/png"], "max_image_bytes": 1234 })
    );
    let unknown = status_body(Some(HelperCapabilities::Unknown));
    assert_eq!(
        unknown,
        json!({ "available": true, "max_bytes": MAX_TEXT_BYTES }),
        "an unreachable helper keeps the original text-only status shape"
    );
}

// ---------------------------------------------------------------------------
// Mock helper
// ---------------------------------------------------------------------------

struct Seen {
    request_line: String,
    headers: Vec<String>,
    body: Vec<u8>,
}

fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )
}

async fn serve_one(listener: &TcpListener, response: &str) -> Seen {
    let (socket, _) = listener.accept().await.expect("helper accept");
    let mut reader = BufReader::new(socket);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .expect("request line");
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("header line");
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = value.trim().parse().expect("content length");
        }
        headers.push(line);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.expect("request body");
    reader
        .get_mut()
        .write_all(response.as_bytes())
        .await
        .expect("reply");
    Seen {
        request_line,
        headers,
        body,
    }
}

async fn helper() -> (TcpListener, ClipboardBridgeConfig) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let bridge = ClipboardBridgeConfig {
        endpoint: listener.local_addr().expect("addr"),
        token: "a".repeat(64),
    };
    (listener, bridge)
}

/// Serves the scripted replies in order, one connection each, then hands the listener
/// back so the test can prove no further connection was attempted.
fn spawn_helper(
    listener: TcpListener,
    responses: Vec<String>,
) -> JoinHandle<(TcpListener, Vec<Seen>)> {
    tokio::spawn(async move {
        let mut seen = Vec::new();
        for response in responses {
            seen.push(serve_one(&listener, &response).await);
        }
        (listener, seen)
    })
}

async fn assert_not_contacted(listener: &TcpListener) {
    assert!(
        tokio::time::timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "the helper must not be contacted"
    );
}

async fn body_json(response: HttpResponse) -> serde_json::Value {
    let bytes = to_bytes(response.into_body()).await.expect("response body");
    serde_json::from_slice(&bytes).expect("json body")
}

#[tokio::test]
async fn capabilities_wire_never_reads_content_and_negotiates() {
    let (listener, bridge) = helper().await;
    let server = spawn_helper(
        listener,
        vec![
            reply("200 Reply", IMAGE_CAPABILITIES),
            reply("404 Reply", ""),
            reply("200 Reply", "{garbage"),
        ],
    );
    assert_eq!(
        fetch_capabilities(&bridge).await,
        HelperCapabilities::Image {
            max_image_bytes: MAX_IMAGE_BYTES
        }
    );
    assert_eq!(
        fetch_capabilities(&bridge).await,
        HelperCapabilities::TextOnly,
        "an old helper's 404 means text-only"
    );
    assert_eq!(fetch_capabilities(&bridge).await, HelperCapabilities::Unknown);
    let (listener, seen) = server.await.expect("helper task");
    for request in &seen {
        assert_eq!(request.request_line, "GET /capabilities HTTP/1.1\r\n");
        assert!(request.body.is_empty());
        assert!(
            request
                .headers
                .iter()
                .any(|line| line == &format!("Authorization: Bearer {}\r\n", "a".repeat(64)))
        );
    }
    assert_not_contacted(&listener).await;
}

#[tokio::test]
async fn capabilities_timeout_is_unknown() {
    let (listener, bridge) = helper().await;
    let hold = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(socket);
    });
    assert_eq!(
        query_capabilities(&bridge, Duration::from_millis(200)).await,
        HelperCapabilities::Unknown
    );
    hold.abort();
}

#[tokio::test]
async fn image_write_wire_sends_validated_png_after_negotiation() {
    let (listener, bridge) = helper().await;
    let server = spawn_helper(
        listener,
        vec![
            reply("200 Reply", IMAGE_CAPABILITIES),
            reply("204 Reply", ""),
        ],
    );
    let response = write_image_flow(&bridge, PIXEL_PNG_BASE64, Duration::from_secs(5)).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (listener, seen) = server.await.expect("helper task");
    assert_eq!(seen[0].request_line, "GET /capabilities HTTP/1.1\r\n");
    assert_eq!(seen[1].request_line, "POST /clipboard/image HTTP/1.1\r\n");
    assert!(
        seen[1]
            .headers
            .iter()
            .any(|line| line == "Content-Type: application/json\r\n")
    );
    let body: serde_json::Value = serde_json::from_slice(&seen[1].body).expect("json");
    assert_eq!(body, json!({ "png_base64": PIXEL_PNG_BASE64 }));
    assert_not_contacted(&listener).await;
}

#[tokio::test]
async fn image_write_rejects_bad_payloads_before_contacting_the_helper() {
    let (listener, bridge) = helper().await;
    let oversized_dims = base64(&png_with(5000, 1, &[]));
    let jpeg = base64(&[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8]);
    let too_long = "A".repeat(MAX_IMAGE_BASE64_BYTES + 4);
    for (payload, expected) in [
        ("@@@@", StatusCode::BAD_REQUEST),
        ("", StatusCode::BAD_REQUEST),
        (jpeg.as_str(), StatusCode::BAD_REQUEST),
        (oversized_dims.as_str(), StatusCode::PAYLOAD_TOO_LARGE),
        (too_long.as_str(), StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let response = write_image_flow(&bridge, payload, Duration::from_secs(5)).await;
        assert_eq!(response.status(), expected);
    }
    assert_not_contacted(&listener).await;
}

#[tokio::test]
async fn image_write_to_text_only_or_limited_helper_is_refused_without_posting() {
    for (capabilities, expected) in [
        (reply("404 Reply", ""), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        (
            reply("200 Reply", r#"{"formats":["text/plain"]}"#),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            reply(
                "200 Reply",
                r#"{"formats":["text/plain","image/png"],"max_image_bytes":10}"#,
            ),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (reply("503 Reply", ""), StatusCode::BAD_GATEWAY),
    ] {
        let (listener, bridge) = helper().await;
        let server = spawn_helper(listener, vec![capabilities]);
        let response = write_image_flow(&bridge, PIXEL_PNG_BASE64, Duration::from_secs(5)).await;
        assert_eq!(response.status(), expected);
        let (listener, _) = server.await.expect("helper task");
        assert_not_contacted(&listener).await;
    }
}

#[tokio::test]
async fn image_write_maps_helper_errors() {
    for (helper_status, expected, code) in [
        ("413 Reply", StatusCode::PAYLOAD_TOO_LARGE, "image_too_large"),
        ("400 Reply", StatusCode::BAD_REQUEST, "invalid_image"),
        ("404 Reply", StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported"),
        ("503 Reply", StatusCode::BAD_GATEWAY, "bridge_unavailable"),
        ("401 Reply", StatusCode::BAD_GATEWAY, "bridge_unavailable"),
    ] {
        let (listener, bridge) = helper().await;
        let server = spawn_helper(
            listener,
            vec![
                reply("200 Reply", IMAGE_CAPABILITIES),
                reply(helper_status, ""),
            ],
        );
        let response = write_image_flow(&bridge, PIXEL_PNG_BASE64, Duration::from_secs(5)).await;
        assert_eq!(response.status(), expected, "helper {helper_status}");
        assert_eq!(body_json(response).await, json!({ "error": code }));
        server.await.expect("helper task");
    }
}

#[tokio::test]
async fn image_read_wire_returns_validated_png_or_explicit_empty() {
    let image = format!(r#"{{"has_image":true,"png_base64":"{PIXEL_PNG_BASE64}"}}"#);
    let (listener, bridge) = helper().await;
    let server = spawn_helper(
        listener,
        vec![
            reply("200 Reply", IMAGE_CAPABILITIES),
            reply("200 Reply", &image),
            reply("200 Reply", IMAGE_CAPABILITIES),
            reply("200 Reply", r#"{"has_image":false,"png_base64":""}"#),
        ],
    );
    let response = read_image_flow(&bridge, Duration::from_secs(5)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({ "has_image": true, "png_base64": PIXEL_PNG_BASE64 })
    );
    let response = read_image_flow(&bridge, Duration::from_secs(5)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({ "has_image": false, "png_base64": "" })
    );
    let (_, seen) = server.await.expect("helper task");
    assert_eq!(seen[1].request_line, "GET /clipboard/image HTTP/1.1\r\n");
    assert!(seen[1].body.is_empty());
}

#[tokio::test]
async fn image_read_rejects_bad_helper_payloads_and_maps_errors() {
    let jpeg = format!(
        r#"{{"has_image":true,"png_base64":"{}"}}"#,
        base64(&[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4])
    );
    let huge = format!(
        r#"{{"has_image":true,"png_base64":"{}"}}"#,
        base64(&png_with(4097, 4097, &[]))
    );
    for (image_reply, expected) in [
        (reply("200 Reply", &jpeg), StatusCode::BAD_GATEWAY),
        (reply("200 Reply", &huge), StatusCode::PAYLOAD_TOO_LARGE),
        (
            reply("200 Reply", r#"{"has_image":false,"png_base64":"AAAA"}"#),
            StatusCode::BAD_GATEWAY,
        ),
        (reply("200 Reply", "{not json"), StatusCode::BAD_GATEWAY),
        (reply("413 Reply", ""), StatusCode::PAYLOAD_TOO_LARGE),
        (reply("404 Reply", ""), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        (reply("503 Reply", ""), StatusCode::BAD_GATEWAY),
    ] {
        let (listener, bridge) = helper().await;
        let server = spawn_helper(
            listener,
            vec![reply("200 Reply", IMAGE_CAPABILITIES), image_reply],
        );
        let response = read_image_flow(&bridge, Duration::from_secs(5)).await;
        assert_eq!(response.status(), expected);
        server.await.expect("helper task");
    }
    // A text-only helper is never asked for an image.
    let (listener, bridge) = helper().await;
    let server = spawn_helper(listener, vec![reply("404 Reply", "")]);
    let response = read_image_flow(&bridge, Duration::from_secs(5)).await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (listener, _) = server.await.expect("helper task");
    assert_not_contacted(&listener).await;
}

#[tokio::test]
async fn image_routes_time_out_with_a_distinct_code() {
    for write in [false, true] {
        let (listener, bridge) = helper().await;
        let hold = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(3)).await;
            drop(socket);
        });
        let response = if write {
            write_image_flow(&bridge, PIXEL_PNG_BASE64, Duration::from_millis(300)).await
        } else {
            read_image_flow(&bridge, Duration::from_millis(300)).await
        };
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body_json(response).await, json!({ "error": "bridge_timeout" }));
        hold.abort();
    }
}

// ---------------------------------------------------------------------------
// Real handlers: authentication, Origin, body bounds and ownership come first
// ---------------------------------------------------------------------------

struct HandlerFixture {
    app: Data<App>,
    session: String,
    dir: PathBuf,
}

impl Drop for HandlerFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn handler_fixture(label: &str, endpoint: SocketAddr) -> HandlerFixture {
    let dir = std::env::temp_dir().join(format!(
        "clipboard-image-{label}-{}",
        std::process::id()
    ));
    tokio::fs::create_dir_all(&dir).await.expect("fixture dir");
    let mut bridges = HashMap::new();
    bridges.insert(
        HOST_ID,
        ClipboardBridgeConfig {
            endpoint,
            token: "a".repeat(64),
        },
    );
    let app = Data::new(
        App::new(Config {
            data_storage: StorageConfig::Json {
                path: dir.join("data.json").to_string_lossy().into_owned(),
                session_expiration_check_interval: Duration::from_secs(3600),
            },
            clipboard_bridges: bridges,
            ..Default::default()
        })
        .await
        .expect("fixture app"),
    );
    let user = app
        .try_add_first_login("clipboard-fixture".into(), "fixture-password".into())
        .await
        .expect("fixture user");
    let token = user
        .new_session(Duration::from_secs(3600))
        .await
        .expect("fixture session");
    let mut bytes = [0u8; 64];
    let session = token.encode(&mut bytes).to_owned();
    HandlerFixture { app, session, dir }
}

fn image_body() -> String {
    json!({ "host_id": HOST_ID, "png_base64": PIXEL_PNG_BASE64 }).to_string()
}

#[actix_web::test]
async fn image_handlers_check_auth_origin_bounds_and_ownership_before_the_helper() {
    let (listener, bridge) = helper().await;
    let fixture = handler_fixture("order", bridge.endpoint).await;
    let service = actix_test::init_service(
        ActixApp::new()
            .app_data(fixture.app.clone())
            .service(set_clipboard_image)
            .service(get_clipboard_image)
            .service(clipboard_status),
    )
    .await;
    let session = || Cookie::new(COOKIE_SESSION_TOKEN_NAME, fixture.session.clone());
    let read_body = json!({ "host_id": HOST_ID }).to_string();

    // Unauthenticated.
    for (uri, body) in [
        ("/host/clipboard/image", image_body()),
        ("/host/clipboard/image/read", read_body.clone()),
    ] {
        let request = actix_test::TestRequest::post()
            .uri(uri)
            .insert_header((CONTENT_TYPE, "application/json"))
            .set_payload(body)
            .to_request();
        assert_eq!(
            actix_test::call_service(&service, request).await.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} unauthenticated"
        );
    }

    // Cross-origin, authenticated.
    for (uri, body) in [
        ("/host/clipboard/image", image_body()),
        ("/host/clipboard/image/read", read_body.clone()),
    ] {
        let request = actix_test::TestRequest::post()
            .uri(uri)
            .cookie(session())
            .insert_header(("Host", "moonlight.local"))
            .insert_header(("Origin", "https://attacker.example"))
            .insert_header((CONTENT_TYPE, "application/json"))
            .set_payload(body)
            .to_request();
        assert_eq!(
            actix_test::call_service(&service, request).await.status(),
            StatusCode::FORBIDDEN,
            "{uri} cross-origin"
        );
    }
    let request = actix_test::TestRequest::get()
        .uri(&format!("/host/clipboard?host_id={HOST_ID}"))
        .cookie(session())
        .insert_header(("Host", "moonlight.local"))
        .insert_header(("Origin", "https://attacker.example"))
        .to_request();
    assert_eq!(
        actix_test::call_service(&service, request).await.status(),
        StatusCode::FORBIDDEN,
        "status cross-origin"
    );

    // Body bounds and content type, before ownership or decoding.
    let oversized = "A".repeat(MAX_IMAGE_REQUEST_BYTES + 1);
    let request = actix_test::TestRequest::post()
        .uri("/host/clipboard/image")
        .cookie(session())
        .insert_header((CONTENT_TYPE, "application/json"))
        .set_payload(oversized)
        .to_request();
    assert_eq!(
        actix_test::call_service(&service, request).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let request = actix_test::TestRequest::post()
        .uri("/host/clipboard/image/read")
        .cookie(session())
        .insert_header((CONTENT_TYPE, "application/json"))
        .set_payload(" ".repeat(MAX_IMAGE_READ_REQUEST_BYTES + 1))
        .to_request();
    assert_eq!(
        actix_test::call_service(&service, request).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let request = actix_test::TestRequest::post()
        .uri("/host/clipboard/image")
        .cookie(session())
        .insert_header((CONTENT_TYPE, "text/plain"))
        .set_payload(image_body())
        .to_request();
    assert_eq!(
        actix_test::call_service(&service, request).await.status(),
        StatusCode::BAD_REQUEST
    );

    // Authenticated, same-origin, valid body, but the host is not owned/known: the
    // configured helper must still not be contacted.
    for (uri, body) in [
        ("/host/clipboard/image", image_body()),
        ("/host/clipboard/image/read", read_body.clone()),
    ] {
        let request = actix_test::TestRequest::post()
            .uri(uri)
            .cookie(session())
            .insert_header((CONTENT_TYPE, "application/json"))
            .set_payload(body)
            .to_request();
        assert_eq!(
            actix_test::call_service(&service, request).await.status(),
            StatusCode::NOT_FOUND,
            "{uri} unknown host"
        );
    }
    let request = actix_test::TestRequest::get()
        .uri(&format!("/host/clipboard?host_id={HOST_ID}"))
        .cookie(session())
        .to_request();
    assert_eq!(
        actix_test::call_service(&service, request).await.status(),
        StatusCode::NOT_FOUND,
        "status for an unknown host"
    );

    assert_not_contacted(&listener).await;
}

async fn fixture_session(app: &Data<App>) -> crate::app::auth::SessionToken {
    let owner = app.user_by_id(crate::app::user::UserId(0)).await.unwrap()
        .authenticate(&crate::app::auth::UserAuth::UserPassword {
            username:"owner".into(),password:"synthetic-password".into()
        }).await.unwrap();
    owner.new_session(Duration::from_secs(3600)).await.unwrap()
}

// Drive the production factory, not a test-only list of individual handlers.
#[actix_web::test]
async fn production_factory_image_routes_preserve_security_and_confirm_helper_writes() {
    let (listener, bridge) = helper().await;
    let dir = std::env::temp_dir().join(format!("clipboard-factory-{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let data = dir.join("data.json");
    let host = |owner| json!({"owner":owner,"address":"127.0.0.1","http_port":1,"pair_info":null,"cache":{"name":"fixture","mac":null}});
    let password = crate::app::password::StoragePassword::new("synthetic-password").unwrap();
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let password = json!({"salt":hex(&password.salt),"hash":hex(&password.hash),"iterations":password.iterations});
    tokio::fs::write(&data, json!({"version":"2","users":{
        "0":{"role":"User","name":"owner","password":password,"client_unique_id":"fixture-owner"},
        "1":{"role":"User","name":"other","password":null,"client_unique_id":"fixture-other"}
    },"hosts":{"0":host(0),"1":host(1)}}).to_string()).await.unwrap();
    let mut bridges = HashMap::new();
    bridges.insert(0, bridge.clone());
    bridges.insert(1, bridge);
    let app = Data::new(App::new(Config {
        data_storage: StorageConfig::Json { path:data.to_string_lossy().into_owned(),session_expiration_check_interval:Duration::from_secs(3600) },
        clipboard_bridges:bridges,..Default::default()
    }).await.unwrap());
    let token = fixture_session(&app).await;
    let mut encoded = [0u8;64];
    let fixture = HandlerFixture {app,session:token.encode(&mut encoded).to_owned(),dir};
    let cookie = || Cookie::new(COOKIE_SESSION_TOKEN_NAME, fixture.session.clone());
    let service = actix_test::init_service(ActixApp::new().app_data(fixture.app.clone()).service(crate::api::api_service())).await;
    for uri in ["/api/host/clipboard/image","/api/host/clipboard/image/read"] {
        let body = json!({"host_id":0,"png_base64":PIXEL_PNG_BASE64});
        let response = actix_test::call_service(&service,actix_test::TestRequest::post().uri(uri).set_json(&body).to_request()).await;
        assert_eq!(response.status(),StatusCode::UNAUTHORIZED,"factory registration and auth: {uri}");
        let response = actix_test::call_service(&service,actix_test::TestRequest::post().uri(uri).cookie(cookie()).insert_header(("Host","fixture.local")).insert_header(("Origin","https://other.invalid")).set_json(&body).to_request()).await;
        assert_eq!(response.status(),StatusCode::FORBIDDEN);
        for (host_id,expected) in [(1,StatusCode::FORBIDDEN),(99,StatusCode::NOT_FOUND)] {
            let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri(uri).cookie(cookie()).set_json(json!({"host_id":host_id,"png_base64":"@@@@"})).to_request()).await;
            assert_eq!(response.status(),expected,"ownership precedes decode {uri}");
        }
        let limit=if uri.ends_with("/read") {MAX_IMAGE_READ_REQUEST_BYTES} else {MAX_IMAGE_REQUEST_BYTES};
        let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri(uri).cookie(cookie()).insert_header((CONTENT_TYPE,"application/json")).set_payload(" ".repeat(limit+1)).to_request()).await;
        assert_eq!(response.status(),StatusCode::PAYLOAD_TOO_LARGE);
    }
    let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri("/api/host/clipboard/image").cookie(cookie()).set_json(json!({"host_id":0,"png_base64":"@@@@"})).to_request()).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    // No default/global JsonConfig enlargement leaked into the old text route.
    let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri("/api/host/clipboard").cookie(cookie()).set_json(json!({"host_id":0,"text":"x".repeat(3*1024*1024)})).to_request()).await;
    assert_eq!(response.status(),StatusCode::PAYLOAD_TOO_LARGE);
    assert_not_contacted(&listener).await;
    let image=json!({"has_image":true,"png_base64":PIXEL_PNG_BASE64}).to_string();
    let server=spawn_helper(listener,vec![
        reply("200 Reply",IMAGE_CAPABILITIES), // status only, no clipboard read
        reply("200 Reply",IMAGE_CAPABILITIES),reply("204 Reply",""),
        reply("200 Reply",IMAGE_CAPABILITIES),reply("200 Reply",&image),
        reply("404 Reply",""), // old helper status stays available and text-only
        reply("200 Reply",r#"{"has_text":true,"text":"synthetic old helper"}"#),
    ]);
    let response=actix_test::call_service(&service,actix_test::TestRequest::get().uri("/api/host/clipboard?host_id=0").cookie(cookie()).to_request()).await;
    assert_eq!(response.status(),StatusCode::OK);
    let status:serde_json::Value=actix_test::read_body_json(response).await;
    assert_eq!(status["max_image_bytes"],MAX_IMAGE_BYTES);
    assert!(status.get("token").is_none());
    let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri("/api/host/clipboard/image").cookie(cookie()).set_json(json!({"host_id":0,"png_base64":PIXEL_PNG_BASE64})).to_request()).await;
    assert_eq!(response.status(),StatusCode::NO_CONTENT);
    let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri("/api/host/clipboard/image/read").cookie(cookie()).set_json(json!({"host_id":0})).to_request()).await;
    assert_eq!(response.status(),StatusCode::OK);
    let contents:serde_json::Value=actix_test::read_body_json(response).await;
    assert_eq!(contents,json!({"has_image":true,"png_base64":PIXEL_PNG_BASE64}));
    let response=actix_test::call_service(&service,actix_test::TestRequest::get().uri("/api/host/clipboard?host_id=0").cookie(cookie()).to_request()).await;
    assert_eq!(response.status(),StatusCode::OK);
    let status:serde_json::Value=actix_test::read_body_json(response).await;
    assert_eq!(status,json!({"available":true,"max_bytes":MAX_TEXT_BYTES,"formats":["text/plain"]}));
    let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri("/api/host/clipboard/read").cookie(cookie()).set_json(json!({"host_id":0})).to_request()).await;
    assert_eq!(response.status(),StatusCode::OK);
    let contents:serde_json::Value=actix_test::read_body_json(response).await;
    assert_eq!(contents["text"],"synthetic old helper");
    let (listener,seen)=tokio::time::timeout(Duration::from_secs(2),server).await.unwrap().unwrap();
    assert_eq!(seen.iter().map(|r|r.request_line.trim()).collect::<Vec<_>>(),vec!["GET /capabilities HTTP/1.1","GET /capabilities HTTP/1.1","POST /clipboard/image HTTP/1.1","GET /capabilities HTTP/1.1","GET /clipboard/image HTTP/1.1","GET /capabilities HTTP/1.1","GET /clipboard HTTP/1.1"]);
    fixture.app.delete_session(token).await.unwrap();
    let response=actix_test::call_service(&service,actix_test::TestRequest::post().uri("/api/host/clipboard/image").cookie(cookie()).set_json(json!({"host_id":0,"png_base64":PIXEL_PNG_BASE64})).to_request()).await;
    assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
    assert_not_contacted(&listener).await;
}

#[test]
fn image_request_limit_is_route_scoped() {
    // Only the image write route reads up to the base64 PNG bound; the read route takes
    // a tiny body and the original text routes keep their own Json extractor defaults.
    assert_eq!(MAX_IMAGE_REQUEST_BYTES, MAX_IMAGE_BASE64_BYTES + 1024);
    assert_eq!(MAX_IMAGE_READ_REQUEST_BYTES, 1024);
    assert!(MAX_RESPONSE_BYTES < MAX_IMAGE_RESPONSE_BYTES);
}
