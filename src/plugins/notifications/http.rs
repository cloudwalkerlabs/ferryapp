use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};

use super::{Notification, NotificationError, NotificationsPlugin};
use crate::{api::ApiProblem, core::PluginContext};

#[derive(Clone)]
struct NotificationsState {
    plugin: Arc<NotificationsPlugin>,
    ctx: PluginContext,
}

// A notification's id is the device's own and may hold any character
// (Android's keys have `|`), so it travels in the query or body, never in
// the path.
pub(super) fn routes(plugin: Arc<NotificationsPlugin>, ctx: PluginContext) -> Router {
    Router::new()
        .route(
            "/devices/{device_id}/notifications",
            get(get_notifications).delete(delete_notification),
        )
        .route(
            "/devices/{device_id}/notifications/enabled",
            put(put_notifications_enabled),
        )
        .route(
            "/devices/{device_id}/notifications/icon",
            get(get_notification_icon),
        )
        .route(
            "/devices/{device_id}/notifications/reply",
            post(post_notification_reply),
        )
        .route(
            "/devices/{device_id}/notifications/action",
            post(post_notification_action),
        )
        .with_state(NotificationsState { plugin, ctx })
}

impl From<NotificationError> for ApiProblem {
    fn from(error: NotificationError) -> Self {
        let code = error.code();
        match error {
            NotificationError::Core(error) => error.into(),
            NotificationError::NotFound => ApiProblem::not_found(code),
            NotificationError::EmptyReply => ApiProblem::bad_request(code),
            NotificationError::NotRepliable
            | NotificationError::NotDismissable
            | NotificationError::UnknownAction => {
                ApiProblem::new(StatusCode::CONFLICT, "Conflict", code)
            }
        }
    }
}

#[derive(Deserialize)]
struct IdQuery {
    id: Option<String>,
}

impl IdQuery {
    fn required(self) -> Result<String, ApiProblem> {
        self.id.ok_or_else(|| ApiProblem::bad_request("missing_id"))
    }
}

async fn get_notifications(
    State(state): State<NotificationsState>,
    Path(device_id): Path<String>,
) -> Result<Json<Vec<Notification>>, ApiProblem> {
    Ok(Json(state.plugin.notifications(&state.ctx, &device_id)?))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}

/// Turn showing the device's notifications here on or off. Answers with
/// the setting, as the device's snapshot now has it under
/// `plugins.notifications`.
async fn put_notifications_enabled(
    State(state): State<NotificationsState>,
    Path(device_id): Path<String>,
    Json(request): Json<Enabled>,
) -> Result<Json<Enabled>, ApiProblem> {
    state
        .plugin
        .set_enabled(&state.ctx, &device_id, request.enabled)?;
    Ok(Json(request))
}

/// Dismiss one on the device: `?id=`.
async fn delete_notification(
    State(state): State<NotificationsState>,
    Path(device_id): Path<String>,
    Query(query): Query<IdQuery>,
) -> Result<StatusCode, ApiProblem> {
    let id = query.required()?;
    state.plugin.dismiss(&state.ctx, &device_id, &id)?;
    Ok(StatusCode::ACCEPTED)
}

/// A notification's icon as PNG: `?id=`.
async fn get_notification_icon(
    State(state): State<NotificationsState>,
    Path(device_id): Path<String>,
    Query(query): Query<IdQuery>,
) -> Result<Response, ApiProblem> {
    let id = query.required()?;
    state.plugin.notifications(&state.ctx, &device_id)?;
    let icon = state
        .plugin
        .icon(&device_id, &id)
        .ok_or_else(|| ApiProblem::not_found("icon_not_found"))?;
    Ok((
        [(CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        icon,
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyRequest {
    id: String,
    message: String,
}

async fn post_notification_reply(
    State(state): State<NotificationsState>,
    Path(device_id): Path<String>,
    Json(request): Json<ReplyRequest>,
) -> Result<StatusCode, ApiProblem> {
    state
        .plugin
        .reply(&state.ctx, &device_id, &request.id, &request.message)?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    id: String,
    action: String,
}

async fn post_notification_action(
    State(state): State<NotificationsState>,
    Path(device_id): Path<String>,
    Json(request): Json<ActionRequest>,
) -> Result<StatusCode, ApiProblem> {
    state
        .plugin
        .run_action(&state.ctx, &device_id, &request.id, &request.action)?;
    Ok(StatusCode::ACCEPTED)
}
