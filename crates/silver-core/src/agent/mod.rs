//! Agent loop: streaming, tool calls, approvals, cancellation and the detection guards.

pub mod compact;
pub mod turn;
pub mod verify;

pub use turn::{
    Agent, AgentConfig, ApprovalGate, ApprovalOutcome, ApprovalRequest, AutoApproveGate,
    AutoCancelGate, AutoDenyGate, DiscardTranscript, InMemoryTranscript, RunControl,
    TranscriptSink, TurnOutcome,
};
