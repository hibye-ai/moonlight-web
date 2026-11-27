use actix_web::{
    HttpResponse, get, post,
    web::{Data, Json},
};
use serde_json::Value;

use crate::{
    api::stream::active_stream_count,
    app::{App, AppError, user::Admin},
};

#[get("/backup")]
pub async fn export_backup(app: Data<App>, admin: Admin) -> Result<Json<Value>, AppError> {
    Ok(Json(app.export_backup(&admin).await?))
}

#[post("/backup")]
pub async fn import_backup(
    app: Data<App>,
    admin: Admin,
    Json(value): Json<Value>,
) -> Result<HttpResponse, AppError> {
    // Replacing identities/hosts under a running stream would leave that child
    // process holding stale state. The check and the replacement are now one
    // transition inside `App::import_backup`, which takes the exclusive side of
    // the maintenance gate that every stream holds for its lifetime (MW-19).
    if active_stream_count() != 0 {
        return Err(AppError::BadRequest);
    }
    app.import_backup(&admin, value).await?;
    Ok(HttpResponse::Ok().finish())
}
