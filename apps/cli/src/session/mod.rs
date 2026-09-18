//! `CoreSession` — the UI's ONLY dependency on the backend.
//! Methods align to the engine's `EngineApi` surface. The one implementation is
//! `EngineSession` (real, in-process — `src/session/engine.rs`).
//! RPC-shaped methods have default impls returning empty/default so an impl only
//! overrides what it actually drives — the "reserved interface" is declared here in
//! full without forcing boilerplate.

pub mod dto;
// `EngineSession` (src/session/engine.rs) is the real in-process engine host —
// the desktop/cli flow. It supersedes the deferred `BridgeSession` plan
// there is no stdio bridge subprocess any more.
pub mod engine;

use std::sync::mpsc::{self, Receiver};

pub use dto::*;

pub trait CoreSession: Send + Sync {
    // ── event stream & commands (turn lifecycle) — required ──

    /// Stream of core events, consumed by the render loop as `Msg::Stream`.
    /// Interactive events (`CoreEvent::is_interactive`) block core until a
    /// `Command:Respond` — the frontend MUST answer them.
    fn subscribe(&self) -> Receiver<CoreEvent>;

    /// Subscribe to core-wide invalidation events. Payload data is deliberately
    /// fetched through the matching API rather than copied onto the bus.
    fn subscribe_bus(&self) -> Receiver<CoreBusEvent> {
        let (_tx, rx) = mpsc::channel();
        rx
    }

    /// Submit / cancel / steer / respond.
    fn send(&self, cmd: Command) -> bool;

    fn current_session_id(&self) -> Option<String> {
        None
    }

    /// Current unconsumed mailbox entries for the active turn/session.
    fn mailbox(&self) -> Vec<MailboxEntry> {
        Vec::new()
    }

    /// Withdraw one unconsumed mailbox entry. Core emits MailboxChanged when it
    /// succeeds; already-consumed or unknown ids return false.
    fn remove_mailbox(&self, _id: &str) -> bool {
        false
    }

    // ── models ──
    fn provider_catalog(&self) -> Vec<ProviderEntry> {
        Vec::new()
    }
    fn model_catalog(&self) -> Vec<ModelEntry> {
        Vec::new()
    }
    fn set_session_model(&self, _model: &str) {}
    fn test_connectivity(&self, _model: &str) -> Option<ConnResult> {
        None
    }
    fn key_catalog(&self) -> Vec<KeyEntry> {
        Vec::new()
    }
    fn models_config(&self) -> Option<String> {
        None
    }
    fn add_provider(&self, _name: &str, _sdk: &str, _base_url: Option<&str>) -> Result<(), String> {
        Err("not supported".into())
    }
    fn add_model(&self, _provider: &str, _model: &str, _context_window: u32) -> bool {
        false
    }
    /// Update an existing provider's sdk / base_url. `Err` carries the engine rejection
    /// message so the form can show *why* (invalid name, bad URL, builtin id, …).
    fn update_provider(
        &self,
        _name: &str,
        _sdk: &str,
        _base_url: Option<&str>,
    ) -> Result<(), String> {
        Err("not supported".into())
    }
    /// Update a model's context window (tokens). Returns true if it existed.
    fn set_model_context(&self, _model: &str, _context_window: u32) -> bool {
        false
    }
    /// Remove a provider and all its models/key. Returns true if it existed.
    fn remove_provider(&self, _provider: &str) -> bool {
        false
    }
    /// Remove one model by its full name. Returns true if it existed.
    fn remove_model(&self, _model: &str) -> bool {
        false
    }
    fn set_api_key(&self, _provider: &str, _value: &str, _storage: &str) -> bool {
        false
    }
    fn delete_api_key(&self, _provider: &str) -> bool {
        false
    }

    // ── usage ──
    fn usage_overview(&self) -> UsageSnapshot {
        UsageSnapshot::default()
    }
    fn usage_by_model(&self) -> Vec<ModelUsage> {
        Vec::new()
    }
    fn usage_timeline(&self) -> Vec<TurnUsage> {
        Vec::new()
    }

    // ── session management ──
    fn list_sessions(&self) -> Vec<SessionSummary> {
        Vec::new()
    }

    // ── workspace management (`/workspace`, startup pick) ──
    fn list_workspaces(&self) -> Vec<WorkspaceSummary> {
        Vec::new()
    }
    fn current_workspace(&self) -> String {
        String::new()
    }
    fn session_cwd(&self) -> Option<String> {
        None
    }
    fn workspace_sessions(&self, _workspace_id: &str) -> Vec<SessionSummary> {
        Vec::new()
    }
    fn switch_workspace(&self, _workspace_id: &str, _session_id: Option<&str>) -> Option<String> {
        None
    }
    fn search_sessions(&self, _query: &str) -> Vec<SessionSummary> {
        Vec::new()
    }
    /// Fetch the persisted user and assistant records for one session.
    fn session_history(&self, _id: &str) -> Vec<HistoryItem> {
        Vec::new()
    }

    fn session_turns(
        &self,
        _id: &str,
        _after_turn_seq: Option<u32>,
        _offset: Option<u32>,
        _limit: Option<u32>,
    ) -> Vec<zlogic_protocol::query::TurnItem> {
        Vec::new()
    }

    fn session_turn_entries(
        &self,
        _id: &str,
        _turn_seq: u32,
    ) -> Vec<zlogic_protocol::query::TranscriptEntry> {
        Vec::new()
    }
    fn rename_session(&self, _id: &str, _title: &str) {}
    /// Delete one or more sessions. If the active session is deleted, core switches
    /// to the newest remaining one and returns its ID.
    fn delete_sessions(&self, _ids: &[String]) -> Option<String> {
        None
    }
    // NOTE: deliberately NO per-turn history delete — dropping a turn from the middle
    // of a session breaks context reconstruction on resume (assistant `tool_calls`
    // must stay paired with their `role:'tool'` results). Whole-session delete,
    // fork and rewind are the supported history mutations.
    fn reopen_session(&self, _id: &str) {}
    /// Create a fresh empty session ("New session" draft title, current model),
    /// switch to it, and return its ID. `None` when the backend can't create one.
    fn new_session(&self) -> Option<String> {
        None
    }
    fn fork_history(&self, _session_id: &str, _history_id: &str) -> Option<String> {
        None
    }
    /// Rewind a session to the given history entry: the anchor entry's turn is
    /// KEPT, everything after it is dropped (fork-consistent turn granularity).
    fn rewind_history(&self, _session_id: &str, _history_id: &str) {}
    /// Mark a session archived; it drops out of `list_sessions` but its history
    /// stays loadable.
    fn archive_session(&self, _id: &str) {}

    // ── plan mode (`/plan`): read-only planning posture ──
    /// Whether Plan (read-only planning) mode is currently enabled.
    fn plan_mode(&self) -> bool {
        false
    }
    /// Enable/disable Plan mode. Returns the resulting state.
    fn set_plan_mode(&self, enabled: bool) -> bool {
        enabled
    }
    /// Toggle Plan mode. Returns the resulting state.
    fn toggle_plan_mode(&self) -> bool {
        false
    }

    // ── config / keys ──
    fn config_snapshot(&self) -> ConfigView {
        ConfigView::default()
    }
    /// Whether any configured model currently has a usable credential. The UI uses
    /// this to tell "no model / no key — point the user at config + `zlogic key set`"
    /// apart from "session is busy", both of which make a turn fail to start.
    fn has_usable_model(&self) -> bool {
        true
    }
    fn config_mutate(&self, _op: ConfigOp) {}
    fn set_language(&self, _locale: &str) -> bool {
        true
    }

    // ── command catalog for `/` completion: built-ins + skills ──
    fn command_catalog(&self) -> Vec<CommandSpec> {
        builtins()
    }
}
