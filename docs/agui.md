# AG-UI endpoint

silver speaks the [AG-UI protocol](https://docs.ag-ui.com/introduction) (Agent-User
Interaction, an open event-based standard for connecting agent backends to user-facing apps) on
its HTTP + SSE binding, so any AG-UI client — CopilotKit, the community SDKs, a hand-rolled
consumer — can drive silver.

## Endpoint

    POST /agent
    Content-Type: application/json
    Accept: text/event-stream

The body is the AG-UI `RunAgentInput`:

    {"threadId": "my-thread", "runId": "run-1", "messages": [{"role": "user", "content": "hello"}]}

The response is `200 text/event-stream`; each SSE `data:` payload is exactly one AG-UI event

    data: {"type":"RUN_STARTED","threadId":"my-thread","runId":"run-1","protocolVersion":"1.0"}
    data: {"type":"TEXT_MESSAGE_START","messageId":"…"}
    data: {"type":"TEXT_MESSAGE_CONTENT","messageId":"…","delta":"Hello."}
    data: {"type":"TEXT_MESSAGE_END","messageId":"…"}
    data: {"type":"RUN_FINISHED","threadId":"my-thread","runId":"run-1"}

The stream closes after the run's terminal event (`RUN_FINISHED` or `RUN_ERROR`). A `threadId`
keeps its conversation: the session is keyed by it, and each run's `messages` array rewrites
the session history, so a client that always sends the full conversation stays in sync.

### Errors

A request rejected before the run starts — malformed JSON, a schema violation, an empty
`messages`, a last message that is not a `user` message, a busy thread, or a resume list that
leaves an interrupt unanswered — is an HTTP error status with no stream, using the [silver
error envelope](api.md#errors). A failure after the run starts travels in-stream as
`RUN_ERROR` with the silver error code as `code`.

The bearer [redacted] configured for the API applies to `/agent` like every other route.

## What is supported

- Text and inline-image messages in and out; `toolCalls` on assistant messages and `tool`
  messages round-trip as tool calls and results. A `reasoning` message is carried onto the
  assistant message it precedes, so reasoning providers get their echo.
- The run lifecycle and streaming events: `RUN_STARTED` (with `protocolVersion: "1.0"`),
  `TEXT_MESSAGE_START/CONTENT/END`, `REASONING_MESSAGE_START/CONTENT/END`,
  `TOOL_CALL_START/ARGS/END/RESULT`, `RUN_FINISHED` (with token usage, or a `cancelled`
  outcome) and `RUN_ERROR`. Tool-call arguments in events are the sanitized preview silver
  shows in its own UI; the full arguments live in the session transcript.
- Subagents: `SUBAGENT_STARTED` / `SUBAGENT_FINISHED` / `SUBAGENT_ERROR`, and a subagent's
  tool calls stream as `TOOL_CALL_*` events attributed with the subagent's `subagentRunId`
  (minted as `<toolCallId>:<index>`).
- Interrupt/resume for approvals: a tool call that needs approval ends the run with
  `RUN_FINISHED` whose outcome is the interrupt, carrying the approval id as the interrupt id
  (`reason: "approval"`, the tool call and a human-readable message). The run stays parked
  until answered. The next input on the thread answers it with `resume`:

      {"threadId": "my-thread", "runId": "run-2",
       "resume": [{"interruptId": "<approval id>", "status": "resolved"}]}

  `resolved` approves (a string `payload` answers an `ask_user_question` call), `cancelled`
  denies. The connection then streams the parked run's continuation to its end. A resume
  entry that names no pending approval is ignored with a warning; leaving a pending approval
  unanswered refuses the input. The same approval ids also appear on the session's runs, so
  they can be answered through the web UI or [the API](api.md) instead.
- The `context` and `forwardedProps` input fields are injected into the run's system prompt
  (a `# Application Context` block), so the model sees ambient application state.
- Unknown input members are stripped with a server warning, per the protocol's processing
  model.

## What is not supported

- `tools` (frontend tools the application executes): accepted but never offered to the model,
  so nothing ever calls one. `state` (shared state): accepted; silver keeps no shared state,
  so no state events are emitted.
- Runs are global (no workspace), so workspace-bound tools are not offered; a URL or
  file-referenced media part is skipped rather than fetched.
- Activity messages are skipped; the community SDKs' higher-level features (shared state,
  generative UI) map to protocol pieces silver does not emit.

## Try it

    printf '%s' '{"threadId":"t1","runId":"r1","messages":[{"role":"user","content":"hi"}]}' \
      | curl -N -s -X POST localhost:7777/agent -H 'content-type: application/json' \
              -H 'accept: text/event-stream' -d @-