//! Run usage insights.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct InsightsQuery {
    days: Option<u32>,
}

/// GET /v1/insights?days=N (default 30): per (model, UTC day) run aggregation.
pub async fn insights(
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Result<Json<crate::db::Insights>, ApiFailure> {
    let days = query.days.unwrap_or(30).clamp(1, 3650);
    let report = state.db.insights(days).await?;
    Ok(Json(report))
}
