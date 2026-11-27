use actix_web::{
    dev::HttpServiceFactory,
    middleware::{DefaultHeaders, from_fn},
    services,
    web::{self},
};

use crate::api::{
    app::{get_app_image, get_apps},
    auth::auth_middleware,
    backup::{export_backup, import_backup},
    host::{delete_host, get_host, list_hosts, pair_host, patch_host, post_host, wake_host},
    role::{add_role, delete_role, get_role, list_roles, patch_role},
    session::{delete_all_sessions, delete_session, list_sessions},
    settings::{get_default_settings, get_permissions},
    user::{add_user, delete_user, get_user, list_users, patch_user},
};

pub mod app;
pub mod auth;
pub mod backup;
pub mod clipboard;
pub mod health;
pub mod host;
pub mod role;
pub mod session;
pub mod settings;
pub mod stream;
pub mod user;

pub mod response_streaming;

pub fn api_service() -> impl HttpServiceFactory {
    web::scope("/api")
        // API responses can contain account/host state and must never be stored.
        .wrap(
            DefaultHeaders::new()
                .add((
                    "Cache-Control",
                    "no-store, no-cache, must-revalidate, private",
                ))
                .add(("Pragma", "no-cache"))
                .add(("Expires", "0")),
        )
        .wrap(from_fn(auth_middleware))
        .service(services![
            // -- Health
            health::health,
            health::health_details,
        ])
        .service(services![
            // -- Auth
            auth::login,
            auth::logout,
            auth::authenticate
        ])
        .service(services![
            // -- Host
            clipboard::clipboard_status,
            clipboard::get_clipboard,
            clipboard::set_clipboard,
            clipboard::set_clipboard_image,
            clipboard::get_clipboard_image,
            list_hosts,
            get_host,
            post_host,
            patch_host,
            wake_host,
            delete_host,
            pair_host,
        ])
        .service(services![
            // -- Apps
            get_apps,
            get_app_image,
        ])
        .service(services![
            // -- Users
            get_user,
            add_user,
            patch_user,
            delete_user,
            list_users,
        ])
        .service(services![
            // -- Roles
            get_role,
            add_role,
            patch_role,
            delete_role,
            list_roles,
        ])
        .service(services![
            // -- Settings
            get_default_settings,
            get_permissions
        ])
        .service(services![
            // -- Admin sessions / backup
            list_sessions,
            delete_session,
            delete_all_sessions,
            export_backup,
            import_backup,
        ])
        .service(services![
            // -- Stream
            stream::start_host,
            stream::cancel_host,
        ])
}
