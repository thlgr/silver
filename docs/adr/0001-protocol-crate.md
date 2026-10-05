# 0001. Extract a small silver-protocol crate

Status: accepted

## Context

The crate graph allows a small protocol crate once DTOs start being duplicated, and not
before. In the implemented MVP the same wire and domain types are needed in three separate places:

- silver-core emits RunEvent / EventPayload, returns ErrorCode, and uses the identifier
  newtypes, Scope, RunStatus, MessageRole, ContentPart and RiskLevel. It must not depend on a
  transport, a database or a UI.
- silverd serialises the HTTP request/response DTOs, persists ContentPart and RunEvent as JSON,
  and maps CoreError to a stable ErrorCode.
- silver-client and the silver CLI deserialize the same DTOs and the SSE RunEvent.

Keeping three copies of those types in sync by hand would guarantee that the public contract
drifts from the code that serves it.

## Decision

Create crates/silver-protocol as the single home for the shared types:

- identifier newtypes (WorkspaceId, SessionId, RunId, MessageId, ToolCallId, ApprovalId) and
  EventId;
- Scope, RunStatus, MessageRole, ContentPart, RiskLevel, ErrorCode, ApiError, TokenUsage and the
  tool/approval enums;
- RunEvent / EventPayload and every HTTP request/response DTO.

The crate depends only on serde, serde_json, uuid, chrono and thiserror. It does not depend on
Axum, tokio-rusqlite, reqwest, silver-core or any binary. silver-core, silver-client and
silverd all depend on it; silver-core still owns all behaviour (agent loop, tools,
guards), the protocol crate owns shape only.

## Consequences

- Every wire type has exactly one definition. Changing one is a cross-cutting contract change and
  must be reflected in the client, the daemon, the SSE payloads and any fixtures.
- The crate must stay transport-agnostic. Adding an Axum extractor, a rusqlite type or a reqwest
  type here would invert the intended dependency direction and is a regression to be rejected.
- ErrorCode intentionally carries both the stable string (as_str) and the HTTP status mapping
  (http_status), so transport code maps explicitly and never infers a status from a message.
- silver-core now has a normal dependency on the protocol crate rather than defining these types
  itself. That is the duplication this crate removes: core, client and daemon all needed the same
  event and DTO definitions.
