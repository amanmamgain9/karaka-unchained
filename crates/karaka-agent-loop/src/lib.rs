//! The default agent loop, a port of dsh's `ReactLoopAgent`
//! (`packages/core/agent-loop`, `dsh-agent-loop`), and [`AgentLoopPlugin`],
//! which provides it as the `Agents` port.
//!
//! Each session gets an agent of its own. A turn runs steps until one ends
//! without calling tools: each step asks the model, logs its answer, and
//! runs the tools it called, one after another. Every request is derived
//! from the session's log, never from what the loop holds itself, so a
//! session read back from storage carries on exactly where it was.
//!
//! - `agent.rs`: one session's agent and the task that drives its turns.
//! - `inbox.rs`: the agent's durable pending input, the `inbox` projection.
//! - `turn.rs`: one turn and its steps, and the events they log.
//! - `request.rs`: the request header and route context logged before
//!   each model call.
//! - `live.rs`: the live frames of each model call.
//! - `runtime_context.rs`: the runtime-context snapshot, sent when it
//!   changes.
//! - `failure.rs`: a failed model call as the failure a turn ends with.
//! - `plugin.rs`: the agents port, and the plugin that provides it.
//!
//! Step one leaves out much of dsh's loop. `docs/port-notes.md` lists what,
//! under "Agent loop, step one", with the dsh file each part lives in.

mod agent;
mod failure;
mod inbox;
mod live;
mod plugin;
mod request;
mod runtime_context;
mod turn;

pub use inbox::{InboxProjection, InboxState, InboxView, InvalidSplice};
pub use plugin::{AgentLoop, AgentLoopPlugin, LoopConfig, LoopHooks};
