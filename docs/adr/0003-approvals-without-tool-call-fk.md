# 0003. approvals.tool_call_id has no foreign key to tool_calls(id)

Status: accepted

## Context

The original conceptual schema declared:

    tool_call_id TEXT NOT NULL REFERENCES tool_calls(id)

The MVP persists the tool-call lifecycle as tool.started / tool.completed run events in run_events
and as ContentPart::ToolCall / ContentPart::ToolResult rows in messages. It never inserts a
placeholder row into tool_calls; that table has no production writer (the only INSERT is a test
helper in apps/silverd/src/db.rs). The approvals row, however, is inserted by the daemon approval
gate as soon as a risky tool call starts waiting.

PRAGMA foreign_keys is ON (Db::initialize). With the REFERENCES clause, every
approval insert failed with a constraint violation because no matching tool_calls row existed. The
failure is swallowed at the call site (let _ = self.db.create_approval(...)), so the durable audit
row was silently lost.

## Decision

Migration 0001 declares:

    tool_call_id TEXT NOT NULL,

with no foreign-key clause. The run_id column keeps its REFERENCES runs(id) constraint. The
binding between an approval and the tool call it guards is enforced at the application level:

- the ApprovalRegistry only resolves an approval_id that is currently pending for the same run_id
  (ApprovalRegistry::resolve_for_run);
- the approval row stores arguments_hash, the sha256 of the sorted-key compact JSON of the tool
  arguments (canonical_tool_args), so a decision is tied to the arguments that were requested.

The tool_calls table stays in the schema but is unused by the MVP.

## Consequences

- Approval rows insert without a placeholder tool-call row, so the audit trail is preserved.
- There is no database-level referential integrity for tool_call_id. The run-level foreign key, the
  pending-registry run check and the stored arguments hash are the only guards.
- An orphaned tool_call_id is possible if a future writer removes the referenced row; nothing
  enforces otherwise today.
- If the MVP later persists tool_calls rows, a new migration can reintroduce the foreign key.
- The doc comment on Db::create_approval still says "the referenced run and tool call must exist".
  That comment is inaccurate for tool_call_id and should be corrected to match this decision; the
  code is the source of truth here.
