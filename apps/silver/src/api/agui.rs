//! The AG-UI endpoint: `POST /agent` accepts a [`RunAgentInput`] and streams the run back as
//! AG-UI events over HTTP + Server-Sent Events, per the protocol's default binding.

use super::{ApiFailure, AppState};
use crate::agui::{
    render_external_context, split_input, ResumeEntry, ResumeStatus, RunAgentInput, Translator,
    SESSION_SOURCE,
};
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use chrono::Utc;
use futures::Stream;
use silver_core::{error::CoreError, session::Session};
use silver_protocol::{ApprovalId, CreateRunRequest, EventPayload, MessageInput, RunId, SessionId};
use std::{collections::VecDeque, convert::Infallible, time::Duration};

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
        context,
        forwarded_props,
        resume,
        _unknown,
        ..
    } = input;
    if thread_id.trim().is_empty() || run_id.trim().is_empty() {
        return Err(ApiFailure(CoreError::InvalidRequest(
            "threadId and runId are required".into(),
        )));
    }
    if !_unknown.is_empty() {
        tracing::warn!(
            "stripping unrecognised AG-UI input members: {:?}",
            _unknown.keys().collect::<Vec<_>>()
        );
    }
    let session = resolve_session(&state, &thread_id).await?;

    // A resuming input answers the interrupts of an earlier run on this thread; when it really
    // answers one, the parked run's continuation streams back on this connection.
    if !resume.is_empty() {
        if let Some(response) = resume_stream(
            &state,
            session.id,
            resume,
            String::clone(&thread_id),
            String::clone(&run_id),
        )
        .await?
        {
            return Ok(response);
        }
        // Nothing was answered and nothing is still waiting: the entries were unrecognised, so
        // the input runs as an ordinary new run.
    }

    // Refuse before rewriting history: a concurrent run on the same thread is a 409, not a
    // destructive replace underneath it.
    if state.db.has_active_run(session.id).await? {
        return Err(ApiFailure(CoreError::SessionBusy(session.id)));
    }
    let (seed, prompt) = split_input(messages, session.id).map_err(CoreError::InvalidRequest)?;
    // The AG-UI messages array is the authoritative conversation, so every run rewrites the
    // session history from it before the run starts.
    state.db.replace_session_messages(session.id, &seed).await?;
    let external_context = render_external_context(&context, forwarded_props.as_ref());

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
            external_context,
        })
        .await?;
    let subscription = state.runs.subscribe(created.run_id, None).await?;
    let translator = Translator::new(thread_id, run_id);
    Ok(sse_response(translate_stream(subscription, translator)))
}

/// Answer `resume` entries against the thread's pending approvals. Some when an approval was
/// answered: the response streams that run's continuation after its interrupt, until the run
/// ends. None when no entry named a pending approval, meaning the input proceeds as a new run.
async fn resume_stream(
    state: &AppState,
    session_id: SessionId,
    resume: Vec<ResumeEntry>,
    thread_id: String,
    run_id: String,
) -> Result<Option<Response>, ApiFailure> {
    let active_run = state
        .db
        .session_active_runs(&[session_id])
        .await?
        .remove(&session_id);
    let mut resolved: Option<RunId> = None;
    for entry in resume {
        // The interrupt id we minted is the approval id; anything else names no interrupt here.
        let Ok(approval_id) = entry.interrupt_id.parse::<ApprovalId>() else {
            tracing::warn!(
                "resume entry names no interrupt this server raised: {}",
                entry.interrupt_id
            );
            continue;
        };
        let outcome = match entry.status {
            ResumeStatus::Resolved => match entry.payload {
                // A `resolved` entry with a payload answers an ask_user_question call.
                Some(serde_json::Value::String(answer)) => {
                    silver_core::agent::ApprovalOutcome::Answered(answer)
                }
                _ => silver_core::agent::ApprovalOutcome::Approved,
            },
            ResumeStatus::Cancelled => silver_core::agent::ApprovalOutcome::Denied,
        };
        match state.runs.resolve_approval(approval_id, outcome) {
            Some(run) => resolved = Some(run),
            None => tracing::warn!(
                "resume entry names no pending approval: {}",
                entry.interrupt_id
            ),
        }
    }

    // An interrupt still open on this thread and not answered by the list must not slip past:
    // keeping it open by refusing the input is the conforming choice.
    if let Some(active_run) = active_run {
        if state.runs.pending_approval(active_run).is_some() {
            return Err(ApiFailure(CoreError::InvalidRequest(
                "resume leaves an interrupt unanswered; every pending approval needs an entry"
                    .into(),
            )));
        }
    }

    let Some(run) = resolved else {
        return Ok(None);
    };
    let events = state.db.list_events_after(run, 0).await?;
    let resume_after = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ApprovalRequired { .. }))
        .map(|event| event.event_id.0)
        .max()
        .unwrap_or(0);
    let subscription = state.runs.subscribe(run, Some(resume_after)).await?;
    let translator = Translator::new(thread_id, run_id);
    Ok(Some(sse_response(translate_stream(
        subscription,
        translator,
    ))))
}

/// The SSE stream of one AG-UI exchange: `RUN_STARTED` first, then every translated event until
/// the run's terminal event closes the stream.
fn translate_stream(
    subscription: crate::run_manager::EventSubscription,
    translator: Translator,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    futures::stream::unfold(
        (VecDeque::new(), false, subscription, translator),
        |(mut pending, mut started, mut subscription, mut translator)| async move {
            loop {
                if let Some(frame) = pending.pop_front() {
                    return Some((
                        Ok::<Event, Infallible>(frame),
                        (pending, started, subscription, translator),
                    ));
                }
                if !started {
                    // The input's identity opens the exchange before any silver event.
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
            }
        },
    )
}

/// A finished SSE response with the binding's framing and keep-alive.
fn sse_response(
    stream: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
) -> Response {
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
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
