use actix_web::{
    HttpResponse, delete, get,
    web::{Data, Json},
};
use common::api_bindings::{DeleteSessionRequest, GetSessionsResponse, SessionInfo};

use crate::app::{App, AppError, auth::UserAuth, user::Admin};

#[get("/sessions")]
pub async fn list_sessions(
    app: Data<App>,
    admin: Admin,
    auth: UserAuth,
) -> Result<Json<GetSessionsResponse>, AppError> {
    let current_id = match auth {
        UserAuth::Session(token) => Some(token.fingerprint()),
        _ => None,
    };

    let mut sessions = Vec::new();
    for session in app.sessions(&admin).await? {
        let user_name = match app.user_by_id(session.user_id).await {
            Ok(mut user) => user
                .detailed_user_no_auth()
                .await
                .map(|user| user.name)
                .unwrap_or_else(|_| format!("User {}", session.user_id.0)),
            Err(_) => format!("User {}", session.user_id.0),
        };
        sessions.push(SessionInfo {
            current: current_id.as_deref() == Some(session.id.as_str()),
            id: session.id,
            user_id: session.user_id.0,
            user_name,
            age_seconds: session.age_seconds,
            expires_in_seconds: session.expires_in_seconds,
        });
    }

    Ok(Json(GetSessionsResponse { sessions }))
}

#[delete("/session")]
pub async fn delete_session(
    app: Data<App>,
    admin: Admin,
    Json(request): Json<DeleteSessionRequest>,
) -> Result<HttpResponse, AppError> {
    app.delete_session_by_id(&admin, &request.id).await?;
    Ok(HttpResponse::Ok().finish())
}

#[delete("/sessions")]
pub async fn delete_all_sessions(app: Data<App>, admin: Admin) -> Result<HttpResponse, AppError> {
    app.delete_all_sessions(&admin).await?;
    Ok(HttpResponse::Ok().finish())
}
