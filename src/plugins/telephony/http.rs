use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};

use super::{Call, TelephonyError, current_call, mute_ringer};
use crate::{api::ApiProblem, core::PluginContext};

pub(super) fn routes(ctx: PluginContext) -> Router {
    Router::new()
        .route("/devices/{device_id}/call", get(get_call))
        .route("/devices/{device_id}/call/mute", post(post_mute))
        .with_state(ctx)
}

impl From<TelephonyError> for ApiProblem {
    fn from(error: TelephonyError) -> Self {
        let code = error.code();
        match error {
            TelephonyError::Core(error) => error.into(),
            TelephonyError::NotRinging => ApiProblem::new(StatusCode::CONFLICT, "Conflict", code),
        }
    }
}

/// The call going on on the device, or `null`.
async fn get_call(
    State(ctx): State<PluginContext>,
    Path(device_id): Path<String>,
) -> Result<Json<Option<Call>>, ApiProblem> {
    Ok(Json(current_call(&ctx, &device_id)?))
}

/// Ask the device to mute its ringer for the call ringing now.
async fn post_mute(
    State(ctx): State<PluginContext>,
    Path(device_id): Path<String>,
) -> Result<StatusCode, ApiProblem> {
    mute_ringer(&ctx, &device_id)?;
    Ok(StatusCode::ACCEPTED)
}
