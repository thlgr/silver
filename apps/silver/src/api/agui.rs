//! The AG-UI endpoint: `POST /agent` accepts a [`RunAgentInput`] and streams the run back as
//! AG-UI events over HTTP + Server-Sent Events, per the protocol's default binding.

use super::{ApiFailure, AppState};
use crate::agui::{split_input, RunAgentInput, Translator, SESSION_SOURCE};
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use chrono::Utc;
use silver_core::{error::CoreError, session::Session};
use silver_protocol::{CreateRunRequest, EventPayload, MessageInput, SessionId};
use std::{collections::VecDeque, convert::Infallible, sync::Arc, time::Duration};

pub async fn run(
    State(state): State<AppState>,
    Json(input): Json<RunAgentInput>,
) -> Result<Response, ApiFailure> {
    if state.runs.is_paused() {
        return Ok(super::runs::paused_response());
    }
    let RunAgentInput {
        thread_id,
        run_id,
        messages,
        ..
    } = input;
    if thread_id.trim().is_empty() || run_id.trim().is_empty() {
        return Err(ApiFailure(CoreError::InvalidRequest(
            "threadId and runId are required".into(),
        )));
    }
    let session = resolve_session(&state, &thread_id).await?;
    // Refuse before rewriting history: a concurrent run on the same thread is a 409, not a
    // destructive replace underneath it.
    if state.db.has_active_run(session.id).await? {
        return Err(ApiFailure(CoreError::SessionBusy(session.id)));
    }
    let (seed, prompt) = split_input(messages, session.id).map_err(CoreError::InvalidRequest)?;
    // The AG-UI messages array is the authoritative conversation, so every run rewrites the
    // session history from it before the run starts.
    state.db.replace_session_messages(session.id, &seed).await?;

    let created = state
        .runs
        .create_run(CreateRunRequest {
            workspace_id: None,
            session_id: Some(session.id),
            source: SESSION_SOURCE.to_string(),
            external_key: None,
            message: MessageInput { content: prompt },
            model: None,
            reasoning_effort: None,
            yolo: None,
            preset: None,
            plan_mode: None,
            goal_budget: None,
        })
        .await?;
    let subscription = state.runs.subscribe(created.run_id, None).await?;

    let stream = futures::stream::unfold(
        (
            VecDeque::new(),
            false,
            subscription,
            Translator::new(thread_id, created.run_id.to_string()),
            Arc::clone(&state.runs),
        ),
        |(mut pending, mut started, mut subscription, mut translator, runs)| async move {
            loop {
                if let Some(frame) = pending.pop_front() {
                    return Some((
                        Ok::<Event, Infallible>(frame),
                        (pending, started, subscription, translator, runs),
                    ));
                }
                if !started {
                    // The run input's identity opens the exchange before any silver event.
                    pending.push_back(frame(&translator.run_started()));
                    started = true;
                    continue;
                }
                if translator.is_ended() {
                    return None;
                }
                let event = subscription.next().await?;
                for value in translator.push(&event) {
                    pending.push_back(frame(&value));
                }
                // Approvals cannot be expressed over AG-UI yet; RUN_ERROR was emitted and the
                // parked run is stopped so it does not hang.
                if matches!(event.payload, EventPayload::ApprovalRequired { .. }) {
                    let run_id = event.run_id;
                    let runs = Arc::clone(&runs);
                    tokio::spawn(async move {
                        drop(runs.stop_run(run_id).await);
                    });
                }
            }
        },
    );

    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response())
}

/// One SSE frame carrying exactly one AG-UI event as its `data` payload.
fn frame(value: &serde_json::Value) -> Event {
    let payload = serde_json::to_string(value).expect("AG-UI event serializes");
    Event::default().data(payload)
}

/// The session behind an AG-UI thread: sessions are keyed by `(source, externalKey)` so a
/// thread keeps its conversation across runs, reconnecting to the same session.
async fn resolve_session(state: &AppState, thread_id: &str) -> Result<Session, ApiFailure> {
    if let Some(existing) = state
        .db
        .find_session_by_external_key(SESSION_SOURCE, thread_id, None)
        .await?
    {
        return Ok(existing);
    }
    let now = Utc::now();
    let session = Session {
        id: SessionId::new(),
        workspace_id: None,
        source: SESSION_SOURCE.to_string(),
        external_key: Some(thread_id.to_string()),
        title: None,
        created_at: now,
        updated_at: now,
    };
    Ok(state.db.create_session(session).await?)
}
