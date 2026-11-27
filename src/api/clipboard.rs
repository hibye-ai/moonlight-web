use std::time::Duration;

use actix_web::{
    HttpRequest, HttpResponse, get,
    http::{
        StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
    post,
    web::{Data, Json, Payload, Query},
};
use common::config::ClipboardBridgeConfig;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};

use crate::app::{App, AppError, host::HostId, user::AuthenticatedUser};

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

// PNG image contract shared with web/clipboard_png.ts and the Windows helper.
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_PNG_CHUNKS: usize = 16 * 1024;
const MAX_IMAGE_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
/// Explicit body limit for POST /host/clipboard/image only; other routes keep the
/// framework defaults.
const MAX_IMAGE_REQUEST_BYTES: usize = MAX_IMAGE_BASE64_BYTES + 1024;
const MAX_IMAGE_RESPONSE_BYTES: usize = MAX_IMAGE_BASE64_BYTES + 1024;
const MAX_IMAGE_READ_REQUEST_BYTES: usize = 1024;
const MAX_CAPABILITIES_RESPONSE_BYTES: usize = 4096;
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(2);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub struct ClipboardQuery {
    host_id: u32,
}

#[derive(Deserialize)]
pub struct ClipboardRequest {
    host_id: u32,
    text: String,
}

#[derive(Deserialize, Serialize)]
struct ClipboardContents {
    has_text: bool,
    text: String,
}

#[derive(Deserialize)]
struct ClipboardImageRequest {
    host_id: u32,
    png_base64: String,
}

#[derive(Deserialize, Serialize)]
struct ClipboardImageContents {
    has_image: bool,
    png_base64: String,
}

#[post("/host/clipboard/read")]
pub async fn get_clipboard(
    app: Data<App>,
    request: HttpRequest,
    mut user: AuthenticatedUser,
    Json(query): Json<ClipboardQuery>,
) -> Result<HttpResponse, AppError> {
    if !super::stream::websocket_origin_is_allowed(&request) {
        return Err(AppError::Forbidden);
    }
    user.host(HostId(query.host_id)).await?;
    let Some(bridge) = app.config().clipboard_bridges.get(&query.host_id) else {
        return Ok(HttpResponse::Conflict().json(json!({ "error": "not_configured" })));
    };
    match timeout(Duration::from_secs(10), bridge_request(bridge, "GET", None)).await {
        Ok(Ok((200, body))) => match serde_json::from_slice::<ClipboardContents>(&body) {
            Ok(contents)
                if contents.text.len() <= MAX_TEXT_BYTES
                    && !contents.text.contains('\0')
                    && (contents.has_text || contents.text.is_empty()) =>
            {
                Ok(HttpResponse::Ok()
                    .insert_header(("Cache-Control", "no-store"))
                    .json(contents))
            }
            _ => Ok(HttpResponse::BadGateway().finish()),
        },
        Ok(Ok((413, _))) => {
            Ok(HttpResponse::PayloadTooLarge().json(json!({ "error": "text_too_large" })))
        }
        _ => Ok(HttpResponse::BadGateway().json(json!({ "error": "bridge_unavailable" }))),
    }
}

/// Status never reads clipboard contents. Image support is reported only after the
/// helper itself declared it through GET /capabilities; a helper that predates that
/// endpoint answers 404 and stays text-only.
#[get("/host/clipboard")]
pub async fn clipboard_status(
    app: Data<App>,
    request: HttpRequest,
    mut user: AuthenticatedUser,
    Query(query): Query<ClipboardQuery>,
) -> Result<HttpResponse, AppError> {
    // Same-origin GETs carry no Origin header; a mismatching one is a cross-site probe.
    if !super::stream::websocket_origin_is_allowed(&request) {
        return Err(AppError::Forbidden);
    }
    user.host(HostId(query.host_id)).await?;
    let capabilities = match app.config().clipboard_bridges.get(&query.host_id) {
        Some(bridge) => Some(query_capabilities(bridge, CAPABILITIES_TIMEOUT).await),
        None => None,
    };
    Ok(HttpResponse::Ok().json(status_body(capabilities)))
}

#[post("/host/clipboard")]
pub async fn set_clipboard(
    app: Data<App>,
    request: HttpRequest,
    mut user: AuthenticatedUser,
    Json(payload): Json<ClipboardRequest>,
) -> Result<HttpResponse, AppError> {
    if !super::stream::websocket_origin_is_allowed(&request) {
        return Err(AppError::Forbidden);
    }
    // Apply the same host ownership check used by streaming, before contacting
    // any bridge. Endpoint and credentials come only from private server config.
    user.host(HostId(payload.host_id)).await?;
    if payload.text.len() > MAX_TEXT_BYTES || payload.text.contains('\0') {
        return Ok(HttpResponse::BadRequest().json(json!({ "error": "invalid_text" })));
    }
    let Some(bridge) = app.config().clipboard_bridges.get(&payload.host_id) else {
        return Ok(HttpResponse::Conflict().json(json!({ "error": "not_configured" })));
    };
    match timeout(
        Duration::from_secs(6),
        write_clipboard(bridge, &payload.text),
    )
    .await
    {
        Ok(Ok(())) => Ok(HttpResponse::NoContent().finish()),
        _ => Ok(HttpResponse::BadGateway().json(json!({ "error": "bridge_unavailable" }))),
    }
}

/// POST /host/clipboard/image/read `{host_id}` -> `{has_image, png_base64}`.
///
/// The body is read only after authentication (extractor) and the Origin check, and
/// the helper is contacted only after host ownership is confirmed.
#[post("/host/clipboard/image/read")]
pub async fn get_clipboard_image(
    app: Data<App>,
    request: HttpRequest,
    mut user: AuthenticatedUser,
    payload: Payload,
) -> Result<HttpResponse, AppError> {
    if !super::stream::websocket_origin_is_allowed(&request) {
        return Err(AppError::Forbidden);
    }
    let body = match read_bounded_json(
        &request,
        payload,
        MAX_IMAGE_READ_REQUEST_BYTES,
        "request_too_large",
    )
    .await
    {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };
    let Ok(query) = serde_json::from_slice::<ClipboardQuery>(&body) else {
        return Ok(clipboard_error(StatusCode::BAD_REQUEST, "invalid_request"));
    };
    user.host(HostId(query.host_id)).await?;
    let Some(bridge) = app.config().clipboard_bridges.get(&query.host_id) else {
        return Ok(clipboard_error(StatusCode::CONFLICT, "not_configured"));
    };
    Ok(read_image_flow(bridge, IMAGE_TIMEOUT).await)
}

/// POST /host/clipboard/image `{host_id, png_base64}` -> 204 only after the helper
/// confirms the native clipboard write.
///
/// Order: authentication (extractor), Origin, bounded body read, host ownership, then
/// base64/PNG validation and finally the helper.
#[post("/host/clipboard/image")]
pub async fn set_clipboard_image(
    app: Data<App>,
    request: HttpRequest,
    mut user: AuthenticatedUser,
    payload: Payload,
) -> Result<HttpResponse, AppError> {
    if !super::stream::websocket_origin_is_allowed(&request) {
        return Err(AppError::Forbidden);
    }
    let body = match read_bounded_json(
        &request,
        payload,
        MAX_IMAGE_REQUEST_BYTES,
        "image_too_large",
    )
    .await
    {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };
    let Ok(image) = serde_json::from_slice::<ClipboardImageRequest>(&body) else {
        return Ok(clipboard_error(StatusCode::BAD_REQUEST, "invalid_request"));
    };
    drop(body);
    user.host(HostId(image.host_id)).await?;
    let Some(bridge) = app.config().clipboard_bridges.get(&image.host_id) else {
        return Ok(clipboard_error(StatusCode::CONFLICT, "not_configured"));
    };
    Ok(write_image_flow(bridge, &image.png_base64, IMAGE_TIMEOUT).await)
}

fn clipboard_error(status: StatusCode, code: &'static str) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header(("Cache-Control", "no-store"))
        .json(json!({ "error": code }))
}

/// Reads an `application/json` body of at most `limit` bytes. The declared length is
/// checked before any byte is read; a body without one is cut off at the limit.
async fn read_bounded_json(
    request: &HttpRequest,
    mut payload: Payload,
    limit: usize,
    too_large: &'static str,
) -> Result<Vec<u8>, HttpResponse> {
    let is_json = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Err(clipboard_error(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    if let Some(length) = request.headers().get(CONTENT_LENGTH) {
        let Some(length) = length
            .to_str()
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
        else {
            return Err(clipboard_error(StatusCode::BAD_REQUEST, "invalid_request"));
        };
        if length > limit {
            return Err(clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, too_large));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = payload.next().await {
        let Ok(chunk) = chunk else {
            return Err(clipboard_error(StatusCode::BAD_REQUEST, "invalid_request"));
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, too_large));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

// -- Capability negotiation ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperCapabilities {
    /// The helper answered /capabilities with 404, or declared no image format.
    TextOnly,
    Image { max_image_bytes: usize },
    /// Unreachable, timed out or malformed: nothing beyond text is advertised.
    Unknown,
}

fn status_body(capabilities: Option<HelperCapabilities>) -> serde_json::Value {
    let mut body = json!({
        "available": capabilities.is_some(),
        "max_bytes": MAX_TEXT_BYTES,
    });
    match capabilities {
        Some(HelperCapabilities::TextOnly) => {
            body["formats"] = json!(["text/plain"]);
        }
        Some(HelperCapabilities::Image { max_image_bytes }) => {
            body["formats"] = json!(["text/plain", "image/png"]);
            body["max_image_bytes"] = json!(max_image_bytes);
        }
        Some(HelperCapabilities::Unknown) | None => {}
    }
    body
}

fn parse_capabilities(body: &[u8]) -> HelperCapabilities {
    #[derive(Deserialize)]
    struct Wire {
        formats: Vec<String>,
        #[serde(default)]
        max_image_bytes: Option<u64>,
    }
    let Ok(wire) = serde_json::from_slice::<Wire>(body) else {
        return HelperCapabilities::Unknown;
    };
    let png = wire.formats.iter().any(|format| format == "image/png");
    match wire.max_image_bytes {
        Some(max) if png && max > 0 => HelperCapabilities::Image {
            max_image_bytes: usize::try_from(max)
                .unwrap_or(MAX_IMAGE_BYTES)
                .min(MAX_IMAGE_BYTES),
        },
        _ => HelperCapabilities::TextOnly,
    }
}

async fn fetch_capabilities(bridge: &ClipboardBridgeConfig) -> HelperCapabilities {
    match bridge_request_at(
        bridge,
        "GET",
        "/capabilities",
        None,
        MAX_CAPABILITIES_RESPONSE_BYTES,
    )
    .await
    {
        Ok((200, body)) => parse_capabilities(&body),
        Ok((404, _)) => HelperCapabilities::TextOnly,
        _ => HelperCapabilities::Unknown,
    }
}

async fn query_capabilities(
    bridge: &ClipboardBridgeConfig,
    deadline: Duration,
) -> HelperCapabilities {
    timeout(deadline, fetch_capabilities(bridge))
        .await
        .unwrap_or(HelperCapabilities::Unknown)
}

// -- Image transfer ----------------------------------------------------------------------

async fn read_image_flow(bridge: &ClipboardBridgeConfig, deadline: Duration) -> HttpResponse {
    match timeout(deadline, read_image_from_bridge(bridge)).await {
        Ok(response) => response,
        Err(_) => clipboard_error(StatusCode::GATEWAY_TIMEOUT, "bridge_timeout"),
    }
}

async fn read_image_from_bridge(bridge: &ClipboardBridgeConfig) -> HttpResponse {
    let max_image_bytes = match fetch_capabilities(bridge).await {
        HelperCapabilities::Image { max_image_bytes } => max_image_bytes,
        HelperCapabilities::TextOnly => {
            return clipboard_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported");
        }
        HelperCapabilities::Unknown => {
            return clipboard_error(StatusCode::BAD_GATEWAY, "bridge_unavailable");
        }
    };
    match bridge_request_at(
        bridge,
        "GET",
        "/clipboard/image",
        None,
        MAX_IMAGE_RESPONSE_BYTES,
    )
    .await
    {
        Ok((200, body)) => {
            let Ok(contents) = serde_json::from_slice::<ClipboardImageContents>(&body) else {
                return clipboard_error(StatusCode::BAD_GATEWAY, "invalid_bridge_response");
            };
            if !contents.has_image {
                return if contents.png_base64.is_empty() {
                    HttpResponse::Ok()
                        .insert_header(("Cache-Control", "no-store"))
                        .json(contents)
                } else {
                    clipboard_error(StatusCode::BAD_GATEWAY, "invalid_bridge_response")
                };
            }
            match decode_png_base64(&contents.png_base64, max_image_bytes) {
                Ok(_) => HttpResponse::Ok()
                    .insert_header(("Cache-Control", "no-store"))
                    .json(contents),
                Err(ImageRejection::TooLarge) => {
                    clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, "image_too_large")
                }
                Err(ImageRejection::Invalid) => {
                    clipboard_error(StatusCode::BAD_GATEWAY, "invalid_bridge_response")
                }
            }
        }
        Ok((404 | 405 | 415, _)) => {
            clipboard_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported")
        }
        Ok((413, _)) => clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, "image_too_large"),
        _ => clipboard_error(StatusCode::BAD_GATEWAY, "bridge_unavailable"),
    }
}

async fn write_image_flow(
    bridge: &ClipboardBridgeConfig,
    png_base64: &str,
    deadline: Duration,
) -> HttpResponse {
    // Rejected payloads never reach the helper.
    let decoded_len = match decode_png_base64(png_base64, MAX_IMAGE_BYTES) {
        Ok(bytes) => bytes.len(),
        Err(rejection) => return rejection.response(),
    };
    match timeout(deadline, write_image_to_bridge(bridge, png_base64, decoded_len)).await {
        Ok(response) => response,
        Err(_) => clipboard_error(StatusCode::GATEWAY_TIMEOUT, "bridge_timeout"),
    }
}

async fn write_image_to_bridge(
    bridge: &ClipboardBridgeConfig,
    png_base64: &str,
    decoded_len: usize,
) -> HttpResponse {
    match fetch_capabilities(bridge).await {
        HelperCapabilities::Image { max_image_bytes } if decoded_len <= max_image_bytes => {}
        HelperCapabilities::Image { .. } => {
            return clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, "image_too_large");
        }
        HelperCapabilities::TextOnly => {
            return clipboard_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported");
        }
        HelperCapabilities::Unknown => {
            return clipboard_error(StatusCode::BAD_GATEWAY, "bridge_unavailable");
        }
    }
    let Ok(body) = serde_json::to_vec(&json!({ "png_base64": png_base64 })) else {
        return clipboard_error(StatusCode::INTERNAL_SERVER_ERROR, "encode_failed");
    };
    match bridge_request_at(
        bridge,
        "POST",
        "/clipboard/image",
        Some(&body),
        MAX_RESPONSE_BYTES,
    )
    .await
    {
        Ok((204, _)) => HttpResponse::NoContent().finish(),
        Ok((404 | 405 | 415, _)) => {
            clipboard_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported")
        }
        Ok((413, _)) => clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, "image_too_large"),
        // The helper decodes the image before writing; a decode failure is a bad image.
        Ok((400, _)) => clipboard_error(StatusCode::BAD_REQUEST, "invalid_image"),
        _ => clipboard_error(StatusCode::BAD_GATEWAY, "bridge_unavailable"),
    }
}

// -- PNG / base64 validation -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageRejection {
    TooLarge,
    Invalid,
}

impl ImageRejection {
    fn response(self) -> HttpResponse {
        match self {
            Self::TooLarge => clipboard_error(StatusCode::PAYLOAD_TOO_LARGE, "image_too_large"),
            Self::Invalid => clipboard_error(StatusCode::BAD_REQUEST, "invalid_image"),
        }
    }
}

/// Strict base64 (standard alphabet, padded, no whitespace) of a structurally valid
/// PNG no larger than `max_bytes`. The encoded length is bounded before decoding.
fn decode_png_base64(text: &str, max_bytes: usize) -> Result<Vec<u8>, ImageRejection> {
    let max_bytes = max_bytes.min(MAX_IMAGE_BYTES);
    if text.len() > max_bytes.div_ceil(3) * 4 {
        return Err(ImageRejection::TooLarge);
    }
    let bytes = decode_base64_strict(text).ok_or(ImageRejection::Invalid)?;
    validate_png(&bytes, max_bytes)?;
    Ok(bytes)
}

fn decode_base64_strict(text: &str) -> Option<Vec<u8>> {
    let input = text.as_bytes();
    if input.len() % 4 != 0 {
        return None;
    }
    let quads = input.len() / 4;
    let mut out = Vec::with_capacity(quads * 3);
    for (index, quad) in input.chunks_exact(4).enumerate() {
        let last = index + 1 == quads;
        let mut values = [0u32; 4];
        let mut padding = 0;
        for (position, &byte) in quad.iter().enumerate() {
            if padding > 0 && byte != b'=' {
                return None;
            }
            values[position] = match byte {
                b'A'..=b'Z' => u32::from(byte - b'A'),
                b'a'..=b'z' => u32::from(byte - b'a') + 26,
                b'0'..=b'9' => u32::from(byte - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' if last && position >= 2 => {
                    padding += 1;
                    0
                }
                _ => return None,
            };
        }
        // Match the native helper's canonical base64 requirement: unused padding
        // bits must be zero, even if permissive decoders produce identical bytes.
        if (padding == 2 && values[1] & 15 != 0) || (padding == 1 && values[2] & 3 != 0) {
            return None;
        }
        let joined = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        let [_, first, second, third] = joined.to_be_bytes();
        out.push(first);
        if padding < 2 {
            out.push(second);
        }
        if padding < 1 {
            out.push(third);
        }
    }
    Some(out)
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &byte in bytes {
        c = CRC_TABLE[((c ^ u32::from(byte)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Structural PNG check: signature, first-chunk IHDR with legal fields, per-axis and
/// pixel bounds, chunk framing and CRCs, only known critical chunks, at least one IDAT
/// and a final empty IEND. Pixel data is not decompressed here; the native helper
/// performs a real decode (bounded by these IHDR limits) before any clipboard write.
fn validate_png(bytes: &[u8], max_bytes: usize) -> Result<(u32, u32), ImageRejection> {
    use ImageRejection::{Invalid, TooLarge};
    if bytes.len() > max_bytes.min(MAX_IMAGE_BYTES) {
        return Err(TooLarge);
    }
    if bytes.len() < 8 + 25 || bytes[..8] != PNG_SIGNATURE {
        return Err(Invalid);
    }
    if be_u32(bytes, 8) != Some(13) || &bytes[12..16] != b"IHDR" {
        return Err(Invalid);
    }
    let width = be_u32(bytes, 16).ok_or(Invalid)?;
    let height = be_u32(bytes, 20).ok_or(Invalid)?;
    let (depth, color) = (bytes[24], bytes[25]);
    let depth_ok = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    if width == 0 || height == 0 || !depth_ok || bytes[26] != 0 || bytes[27] != 0 || bytes[28] > 1
    {
        return Err(Invalid);
    }
    if width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(TooLarge);
    }
    let mut offset = 8usize;
    let mut chunks = 0usize;
    let mut idat = false;
    loop {
        let length = usize::try_from(be_u32(bytes, offset).ok_or(Invalid)?).map_err(|_| Invalid)?;
        let data_start = offset.checked_add(8).ok_or(Invalid)?;
        let data_end = data_start.checked_add(length).ok_or(Invalid)?;
        let end = data_end.checked_add(4).ok_or(Invalid)?;
        if end > bytes.len() {
            return Err(Invalid);
        }
        let kind: [u8; 4] = bytes[offset + 4..data_start]
            .try_into()
            .map_err(|_| Invalid)?;
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(Invalid);
        }
        if crc32(&bytes[offset + 4..data_end]) != be_u32(bytes, data_end).ok_or(Invalid)? {
            return Err(Invalid);
        }
        chunks += 1;
        if chunks > MAX_PNG_CHUNKS {
            return Err(Invalid);
        }
        match &kind {
            b"IHDR" if offset != 8 => return Err(Invalid),
            b"IHDR" | b"PLTE" => {}
            b"IDAT" => idat = true,
            b"IEND" => {
                return if idat && length == 0 && end == bytes.len() {
                    Ok((width, height))
                } else {
                    Err(Invalid)
                };
            }
            // Unknown critical chunks must not be ignored (PNG spec 5.4).
            _ if kind[0].is_ascii_uppercase() => return Err(Invalid),
            _ => {}
        }
        offset = end;
    }
}

// -- Helper transport --------------------------------------------------------------------

async fn write_clipboard(bridge: &ClipboardBridgeConfig, text: &str) -> std::io::Result<()> {
    let body = serde_json::to_vec(&json!({ "text": text }))?;
    let (status, _) = bridge_request(bridge, "POST", Some(&body)).await?;
    if status == 204 {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "clipboard bridge did not confirm the write",
        ))
    }
}

async fn bridge_request(
    bridge: &ClipboardBridgeConfig,
    method: &str,
    body: Option<&[u8]>,
) -> std::io::Result<(u16, Vec<u8>)> {
    bridge_request_at(bridge, method, "/clipboard", body, MAX_RESPONSE_BYTES).await
}

/// One request per connection to the private helper. `max_response` bounds the body
/// for this path only (text, capabilities and images have separate limits).
async fn bridge_request_at(
    bridge: &ClipboardBridgeConfig,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    max_response: usize,
) -> std::io::Result<(u16, Vec<u8>)> {
    use std::io::{Error, ErrorKind};
    if bridge.token.len() < 32 || !bridge.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid clipboard bridge token",
        ));
    }
    let body = body.unwrap_or_default();
    let mut socket = TcpStream::connect(bridge.endpoint).await?;
    let header = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bridge.endpoint,
        bridge.token,
        body.len(),
    );
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(body).await?;
    let mut reader = BufReader::new(socket);
    let mut headers = Vec::new();
    while headers.len() < 8192 {
        headers.push(reader.read_u8().await?);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    if !headers.ends_with(b"\r\n\r\n") {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "bridge response headers too large",
        ));
    }
    let headers = std::str::from_utf8(&headers)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid bridge headers"))?;
    let mut lines = headers.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "invalid bridge HTTP version",
        ));
    }
    let status = parts
        .next()
        .and_then(|part| part.parse::<u16>().ok())
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "invalid bridge status"))?;
    let mut length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            if length.is_some() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "duplicate bridge content length",
                ));
            }
            length = Some(value.trim().parse::<usize>().map_err(|_| {
                Error::new(ErrorKind::InvalidData, "invalid bridge content length")
            })?);
        }
    }
    let length = if status == 200 {
        length.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing bridge content length"))?
    } else {
        0
    };
    if length > max_response {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "bridge clipboard response too large",
        ));
    }
    let mut response = vec![0; length];
    reader.read_exact(&mut response).await?;
    Ok((status, response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::TcpListener,
    };

    #[test]
    fn moonlight_http_client_has_crypto_provider_in_server_only_build() {
        use moonlight_common::http::client::{
            async_client::RequestClient, tokio_hyper::TokioHyperClient,
        };
        TokioHyperClient::with_defaults()
            .expect("host HTTP client must initialize without a panic");
        TokioHyperClient::with_defaults_long_timeout()
            .expect("pairing HTTP client must initialize without a panic");
    }

    #[tokio::test]
    async fn bridge_reads_fragmented_utf8_and_bounds_response_size() {
        for oversized in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test fixture operation must succeed");
            let endpoint = listener
                .local_addr()
                .expect("test fixture operation must succeed");
            let expected = json!({ "has_text": true, "text": "CachyOS 한글 😀\n두 번째 줄\t\"text\"".repeat(100) });
            let payload =
                serde_json::to_vec(&expected).expect("test fixture operation must succeed");
            let task = tokio::spawn(async move {
                let (socket, _) = listener
                    .accept()
                    .await
                    .expect("test fixture operation must succeed");
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("test fixture operation must succeed");
                assert_eq!(line, "GET /clipboard HTTP/1.1\r\n");
                loop {
                    line.clear();
                    reader
                        .read_line(&mut line)
                        .await
                        .expect("test fixture operation must succeed");
                    if line == "\r\n" {
                        break;
                    }
                }
                let length = if oversized {
                    MAX_RESPONSE_BYTES + 1
                } else {
                    payload.len()
                };
                reader
                    .get_mut()
                    .write_all(
                        format!("HTTP/1.0 200 OK\r\nContent-Length: {length}\r\n\r\n").as_bytes(),
                    )
                    .await
                    .expect("test fixture operation must succeed");
                if !oversized {
                    for chunk in payload.chunks(17) {
                        reader
                            .get_mut()
                            .write_all(chunk)
                            .await
                            .expect("test fixture operation must succeed");
                    }
                }
            });
            let bridge = ClipboardBridgeConfig {
                endpoint,
                token: "a".repeat(64),
            };
            let result = bridge_request(&bridge, "GET", None).await;
            if oversized {
                assert!(result.is_err());
            } else {
                let (status, bytes) = result.expect("test fixture operation must succeed");
                assert_eq!(status, 200);
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes)
                        .expect("test fixture operation must succeed"),
                    expected
                );
            }
            task.await.expect("test fixture operation must succeed");
        }
    }

    #[tokio::test]
    async fn bridge_sends_utf8_json_and_requires_confirmation() {
        for status in ["204 No Content", "503 Service Unavailable"] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test fixture operation must succeed");
            let endpoint = listener
                .local_addr()
                .expect("test fixture operation must succeed");
            let text = "한글 😀\nsecond line\t\"quoted\"".repeat(100);
            let expected = text.clone();
            let task = tokio::spawn(async move {
                let (socket, _) = listener
                    .accept()
                    .await
                    .expect("test fixture operation must succeed");
                let mut reader = BufReader::new(socket);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader
                        .read_line(&mut line)
                        .await
                        .expect("test fixture operation must succeed");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length: ") {
                        length = value
                            .trim()
                            .parse::<usize>()
                            .expect("test fixture operation must succeed");
                    }
                }
                let mut body = vec![0; length];
                reader
                    .read_exact(&mut body)
                    .await
                    .expect("test fixture operation must succeed");
                let value: serde_json::Value =
                    serde_json::from_slice(&body).expect("test fixture operation must succeed");
                assert_eq!(value["text"], expected);
                reader
                    .get_mut()
                    .write_all(format!("HTTP/1.0 {status}\r\n\r\n").as_bytes())
                    .await
                    .expect("test fixture operation must succeed");
            });
            let bridge = ClipboardBridgeConfig {
                endpoint,
                token: "a".repeat(64),
            };
            assert_eq!(
                write_clipboard(&bridge, &text).await.is_ok(),
                status.starts_with("204")
            );
            task.await.expect("test fixture operation must succeed");
        }
    }
}

#[cfg(test)]
mod image_tests;
