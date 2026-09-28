use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

use super::{ClipboardPlugin, ClipboardSnapshot, ClipboardSyncError};
use crate::{api::ApiProblem, core::PluginContext};

#[derive(Clone)]
struct ClipboardState {
    plugin: Arc<ClipboardPlugin>,
    ctx: PluginContext,
}

pub(super) fn routes(plugin: Arc<ClipboardPlugin>, ctx: PluginContext) -> Router {
    Router::new()
        .route(
            "/clipboard",
            get(get_clipboard).put(put_clipboard).patch(patch_clipboard),
        )
        .route(
            "/devices/{device_id}/clipboard",
            post(post_device_clipboard),
        )
        .with_state(ClipboardState { plugin, ctx })
}

impl From<ClipboardSyncError> for ApiProblem {
    fn from(error: ClipboardSyncError) -> Self {
        let code = error.code();
        match error {
            ClipboardSyncError::TextTooLarge { .. } => {
                ApiProblem::new(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large", code)
            }
            ClipboardSyncError::Empty => ApiProblem::new(StatusCode::CONFLICT, "Conflict", code),
            ClipboardSyncError::Core(error) => error.into(),
        }
    }
}

async fn get_clipboard(State(state): State<ClipboardState>) -> Json<ClipboardSnapshot> {
    Json(state.plugin.snapshot(&state.ctx))
}

#[derive(Deserialize)]
struct SetClipboardRequest {
    text: String,
}

async fn put_clipboard(
    State(state): State<ClipboardState>,
    Json(request): Json<SetClipboardRequest>,
) -> Result<Json<ClipboardSnapshot>, ApiProblem> {
    Ok(Json(state.plugin.set_text(&state.ctx, request.text)?))
}

/// What `PATCH /clipboard` changes: whether sync is on.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchClipboardRequest {
    sync_enabled: bool,
}

async fn patch_clipboard(
    State(state): State<ClipboardState>,
    Json(request): Json<PatchClipboardRequest>,
) -> Result<Json<ClipboardSnapshot>, ApiProblem> {
    Ok(Json(
        state
            .plugin
            .set_sync_enabled(&state.ctx, request.sync_enabled)?,
    ))
}

/// Send this machine's clipboard text to a paired, connected device, for
/// when automatic sync missed it. Takes no body.
async fn post_device_clipboard(
    State(state): State<ClipboardState>,
    Path(device_id): Path<String>,
) -> Result<StatusCode, ApiProblem> {
    state.plugin.send_to(&state.ctx, &device_id)?;
    Ok(StatusCode::ACCEPTED)
}
