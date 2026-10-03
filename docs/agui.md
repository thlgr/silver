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
`messages`, a last message that is not a `user` message, a busy thread — is an HTTP error status
with no stream, using the [silver error envelope](api.md#errors). A failure after the run starts
travels in-stream as `RUN_ERROR` with the silver error code as `code`.

The bearer token configured for the API applies to `/agent` like every other route.

## What is supported

- Text and inline-image messages in and out; `toolCalls` on assistant messages and `tool`
  messages round-trip as tool calls and results.
- The run lifecycle and streaming events: `RUN_STARTED` (with `protocolVersion: "1.0"`),
  `TEXT_MESSAGE_START/CONTENT/END`, `REASONING_MESSAGE_START/CONTENT/END`,
  `TOOL_CALL_START/ARGS/END/RESULT`, `RUN_FINISHED` (with token usage) and `RUN_ERROR`.
  Tool-call arguments in events are the sanitized preview silver shows in its own UI; the full
  arguments live in the session transcript.
- Runs are global (no workspace), so workspace-bound tools are not offered.

## What is not supported yet

- `tools` (frontend tools the app executes), `context`, `state` and `forwardedProps` on the
  input are accepted and ignored.
- Interrupt/resume (approvals): a tool call that needs approval ends the run with
  `RUN_ERROR` code `approval_required` and stops the run, instead of hanging. Approve the call
  in the silver UI or via [the API](api.md) and re-run.
- Subagent events, activity messages and shared-state events are not translated; activity and
  reasoning messages in the input are skipped.
- A URL or file-referenced media part is skipped rather than fetched.

## Try it

    printf '%s' '{"threadId":"t1","runId":"r1","messages":[{"role":"user","content":"hi"}]}' \
      | curl -N -s -X POST localhost:7777/agent -H 'content-type: application/json' \
              -H 'accept: text/event-stream' -d @-