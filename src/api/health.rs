use actix_web::{HttpResponse, get};
use serde_json::json;

use crate::{
    api::stream::{active_stream_count, stream_diagnostics},
    app::user::Admin,
};

#[get("/health")]
pub async fn health() -> HttpResponse {
    HttpResponse::Ok().json(json!({ "status": "ok" }))
}

#[get("/health/details")]
pub async fn health_details(_admin: Admin) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "active_streams": active_stream_count(),
        "streams": stream_diagnostics(),
    }))
}
