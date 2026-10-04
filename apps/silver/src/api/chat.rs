//! Bot chat endpoints: the roster, each chat's messages, sending, reactions, approval cards and
//! the live stream. The hub routes everything; these only carry requests to it.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use silver_protocol::chat::{
    AnswerRequest, BotView, ChatEntry, ChatEvent, CreateBotRequest, ReactRequest, ReadRequest,
    SendMessageRequest, UpdateBotRequest,
};
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

pub async fn bots(State(state): State<AppState>) -> Result<Json<Value>, ApiFailure> {
    Ok(Json(json!({ "bots": state.chat.bots().await? })))
}

pub async fn create(
    State(state): State<AppState>,
    Json(request): Json<CreateBotRequest>,
) -> Result<(StatusCode, Json<BotView>), ApiFailure> {
    Ok((
        StatusCode::CREATED,
        Json(state.chat.create_bot(request).await?),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateBotRequest>,
) -> Result<Json<BotView>, ApiFailure> {
    Ok(Json(state.chat.update_bot(&id, request).await?))
}

pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    state.chat.delete_bot(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct EntriesQuery {
    /// A thread's root entry; absent for the main chat.
    thread: Option<String>,
    /// Only entries older than this `seq`.
    before: Option<i64>,
    limit: Option<u32>,
}

/// GET /v1/chat/bots/{id}/entries: the newest page of a chat or thread, oldest first.
pub async fn entries(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<EntriesQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let entries = state
        .chat
        .entries(
            &id,
            query.thread.as_deref(),
            query.before,
            query.limit.unwrap_or(50),
        )
        .await?;
    Ok(Json(json!({ "entries": entries })))
}

pub async fn send(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<ChatEntry>), ApiFailure> {
    Ok((
        StatusCode::CREATED,
        Json(state.chat.send(&id, request).await?),
    ))
}

pub async fn stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    state.chat.stop(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ReadRequest>,
) -> Result<StatusCode, ApiFailure> {
    state.chat.read(&id, request.thread_id.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn new_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    state.chat.new_session(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn react(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ReactRequest>,
) -> Result<Json<ChatEntry>, ApiFailure> {
    Ok(Json(state.chat.react(&id, &request.emoji).await?))
}

pub async fn answer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<AnswerRequest>,
) -> Result<StatusCode, ApiFailure> {
    state.chat.answer(&id, request).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn frame(event: &ChatEvent) -> Event {
    Event::default()
        .event(event.name())
        .data(serde_json::to_string(event).unwrap_or_else(|_| "{}".into()))
}

/// GET /v1/chat/events: every change to the roster and to the chats, as it happens. A client
/// connects first, then loads what it shows, and upserts by id; `resync` means it fell behind.
pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures::stream::unfold(state.chat.subscribe(), |mut events| async move {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => ChatEvent::Resync,
            Err(RecvError::Closed) => return None,
        };
        Some((Ok::<Event, Infallible>(frame(&event)), events))
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
