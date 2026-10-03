//! SSE event stream with replay and live delivery.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::Stream;
use silver_protocol::RunId;
use std::convert::Infallible;
use std::time::Duration;

pub async fn events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiFailure> {
    let id: RunId = run_id.parse()?;
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let subscription = state.runs.subscribe(id, last_event_id).await?;

    let stream = futures::stream::unfold(subscription, |mut subscription| async move {
        let event = subscription.next().await?;
        let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        let sse_event = Event::default()
            .id(event.event_id.0.to_string())
            .event(event.name())
            .data(payload);
        Some((Ok::<Event, Infallible>(sse_event), subscription))
    });

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}
