//! The AG-UI endpoint: `POST /agent` accepts a [`RunAgentInput`] and streams the run back as
//! AG-UI events over HTTP + Server-Sent Events, per the protocol's default binding.

use super::{ApiFailure, AppState};
use crate::agui::{
    render_external_context, resume_decision, split_input, RunAgentInput, Translator,
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
use futures::{stream, Stream, StreamExt};
use silver_core::{error::CoreError, session::Session};
use silver_protocol::{
    ApprovalDecisionRequest, ApprovalId, CreateRunRequest, EventPayload, MessageInput,
};
use std::{collections::BTreeSet, convert::Infallible, sync::Arc, time::Duration};

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
    } = input;
    if thread_id.trim().is_empty() || run_id.trim().is_empty() {
        return Err(ApiFailure(CoreError::InvalidRequest(
            "threadId and runId are required".into(),
        )));
    }
    let (session, fresh) = state
        .runs
        .resolve_session(
            None,
            None,
            SESSION_SOURCE.to_string(),
            Some(thread_id.clone()),
        )
        .await?;

    // A resuming input answers this thread's open interrupts; the decisions must be validated
    // before any of them is applied, and the run's continuation streams back on this connection.
    if !resume.is_empty() {
        return resume_run(&state, session, thread_id, run_id, resume).await;
    }

    // Refuse a new message while a run is active. When the thread is parked on an interrupt,
    // say so: the fix is a `resume` input, not a new message.
    if let Some(active_run) = state
        .db
        .session_active_runs(&[session.id])
        .await?
        .remove(&session.id)
    {
        let pending = state.runs.pending_approvals(active_run);
        if !pending.is_empty() {
            let pending = pending
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ApiFailure(CoreError::Conflict(format!(
                "this thread is waiting on interrupt(s) {pending}; answer with a resume input"
            ))));
        }
        return Err(ApiFailure(CoreError::SessionBusy(session.id)));
    }

    let (seed, prompt) = split_input(messages, session.id).map_err(CoreError::InvalidRequest)?;
    // Seed the conversation only when this call created the session: after the first run the
    // transcript silver builds is authoritative; rewriting it from the client's copy would
    // degrade it. Earlier messages edited or branched in the client are not reflected.
    if fresh {
        for message in seed {
            state.db.append_message(message).await?;
        }
    }

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
    if let Some(monitor) = crate::monitoring::current().filter(|monitor| monitor.is_active()) {
        tokio::spawn(crate::monitoring::watch_run(
            monitor,
            Arc::clone(&state.runs),
            created.run_id,
            created.session_id,
        ));
    }
    let subscription = state.runs.subscribe(created.run_id, None).await?;
    let translator = Translator::new(thread_id, run_id);
    Ok(sse_response(translate_stream(subscription, translator)))
}

/// Answer a resuming input: every entry must name a pending approval of this thread's active
/// run, nothing may be left unanswered, and only when the whole list checks out are the
/// decisions applied. The run's continuation then streams to its end on this connection.
async fn resume_run(
    state: &AppState,
    session: Session,
    thread_id: String,
    run_id: String,
    resume: Vec<crate::agui::ResumeEntry>,
) -> Result<Response, ApiFailure> {
    // The approvals this thread is waiting on. The idle run of a thread with nothing pending
    // makes every entry stale.
    let active_run = state
        .db
        .session_active_runs(&[session.id])
        .await?
        .remove(&session.id)
        .ok_or(ApiFailure(CoreError::InvalidRequest(
            "resume answers an interrupt this thread has not raised; the interrupted run is no longer waiting".into(),
        )))?;
    let pending: BTreeSet<ApprovalId> = state
        .runs
        .pending_approvals(active_run)
        .into_iter()
        .collect();
    let mut answered: BTreeSet<ApprovalId> = BTreeSet::new();
    let mut decisions: Vec<ApprovalDecisionRequest> = Vec::new();
    for entry in resume {
        let approval_id = entry.interrupt_id.parse::<ApprovalId>().map_err(|_parse| {
            ApiFailure(CoreError::InvalidRequest(format!(
                "resume entry names no interrupt this thread has raised: {}",
                entry.interrupt_id
            )))
        })?;
        if !pending.contains(&approval_id) {
            return Err(ApiFailure(CoreError::InvalidRequest(format!(
                "resume entry names no pending interrupt on this thread: {}",
                entry.interrupt_id
            ))));
        }
        if !answered.insert(approval_id) {
            return Err(ApiFailure(CoreError::InvalidRequest(format!(
                "resume answers interrupt {} more than once",
                entry.interrupt_id
            ))));
        }
        let (decision, answer) = resume_decision(&entry);
        decisions.push(ApprovalDecisionRequest {
            approval_id,
            decision,
            answer,
        });
    }
    for pending_id in pending {
        if !answered.contains(&pending_id) {
            return Err(ApiFailure(CoreError::InvalidRequest(
                "resume leaves an interrupt unanswered; every pending approval needs an entry"
                    .into(),
            )));
        }
    }
    // The whole list validated; only now are the decisions applied.
    for request in decisions {
        state
            .runs
            .decide_approval(active_run, request)
            .map_err(ApiFailure)?;
    }

    // Stream the run from after its last interrupt, so the continuation the answers unblocked
    // is everything the client has not seen. Approvals raised inside subagents count too.
    let events = state.db.list_events_after(active_run, 0).await?;
    let resume_after = events
        .iter()
        .filter(|event| is_approval_event(&event.payload))
        .map(|event| event.event_id.0)
        .max()
        .unwrap_or(0);
    let subscription = state.runs.subscribe(active_run, Some(resume_after)).await?;
    let translator = Translator::new(thread_id, run_id);
    Ok(sse_response(translate_stream(subscription, translator)))
}

/// Whether an event is a permission-required one, top-level or raised inside a subagent.
fn is_approval_event(payload: &EventPayload) -> bool {
    matches!(payload, EventPayload::ApprovalRequired { .. })
        || matches!(
            payload,
            EventPayload::SubagentStep { event, .. }
                if matches!(event.as_ref(), EventPayload::ApprovalRequired { .. })
        )
}

/// The SSE stream of one AG-UI exchange: `RUN_STARTED` first, then every translated event until
/// the run's terminal event closes the stream.
fn translate_stream(
    subscription: crate::run_manager::EventSubscription,
    translator: Translator,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let started = translator.run_started();
    let first = stream::once(futures::future::ready(Ok(frame(&started))));
    let rest = stream::unfold(
        (subscription, translator),
        |(mut subscription, mut translator)| async move {
            if translator.is_ended() {
                return None;
            }
            let event = subscription.next().await?;
            let frames = translator
                .push(&event)
                .into_iter()
                .map(|value| Ok(frame(&value)))
                .collect::<Vec<_>>();
            Some((stream::iter(frames), (subscription, translator)))
        },
    )
    .flatten();
    first.chain(rest)
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
    Event::default().data(value.to_string())
}
