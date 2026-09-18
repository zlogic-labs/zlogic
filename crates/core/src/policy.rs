//! The gate every tool call passes through.
//! Core calls this **synchronously in the tool loop**, before the tool runs. The layered classifier
//! itself (rule floor, live grants, model review, deep model review) lives behind the trait so it
//! can grow without touching the loop.
//! # Why a trait call and not an interaction
//! The overwhelming majority of calls — every `read_file`, every `git status` — are settled by the
//! deterministic rule layer with no user involvement. Routing them all through the interaction
//! channel would create a pending record per call and make cancellation semantics far harder for no
//! benefit. Only [`PolicyDecision::Ask`] reaches the user.
//! # Fail closed
//! An implementation that cannot decide must answer [`PolicyDecision::Ask`], never `Allow` — and
//! the caller treats every unresolved `Ask` as a refusal. That second half is what actually
//! provides the guarantee, and it lives in `round::RoundCtx::gate`: no interface attached, a broken
//! transport, an answer of the wrong shape, and a panic that takes the turn down all end with the
//! tool not running.
//! There is deliberately no `FailClosed` wrapper here. A combinator around an `async fn` cannot
//! catch a panic across an await point, so it could only forward what the gate returned — an
//! identity function advertising a safety property it does not add.

use async_trait::async_trait;
use zlogic_protocol::interaction::{GrantScope, InteractionBody};
use zlogic_protocol::{SessionId, TurnId};
use zlogic_tools::{ToolMeta, ToolRisk};

/// What the gate is being asked about.
#[derive(Debug, Clone)]
pub struct PolicyRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub tool: ToolMeta,
    /// The verbatim JSON arguments.
    pub args: String,
    /// Where tools run for this session, so a gate can do path arithmetic.
    pub exec_cwd: std::path::PathBuf,
    pub root: std::path::PathBuf,
    /// The turn's ledger, for a gate whose decision costs an LLM call.
    /// The model-review layers (`approval`, `approval_deep`) are billed like any other request, and
    /// they fire **per uncertain tool call** — a turn with a dozen of those spends real money that
    /// used to appear in no total anywhere. The gate charges this rather than core, because only the
    /// gate knows whether it called a model at all.
    /// `None` in a host that does not do turn accounting (tests).
    pub budget: Option<std::sync::Arc<crate::budget::TurnBudget>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PolicyDecision {
    Allow,
    Deny {
        reason: String,
    },
    /// The call itself is malformed — arguments the gate cannot parse. Not a policy verdict and
    /// never put to the user: the model wrote the call, so core reports the parse error back to
    /// the model as a precheck failure and it can fix the JSON and retry.
    PrecheckFailed {
        reason: String,
    },
    /// Put it to the user. The body is built by the gate, which is the only layer that knows *why*
    /// it is asking.
    Ask {
        body: InteractionBody,
    },
}

#[async_trait]
pub trait PolicyGate: Send + Sync {
    async fn evaluate(&self, req: &PolicyRequest) -> PolicyDecision;

    /// Records a **durable** grant the user just chose (`session` / `workspace` / `global`).
    /// Core calls this and does not know what happens next — writing a rule somewhere is the gate's
    /// business, because the gate is the only layer that knows how it decides. Core's own
    /// [`GrantSet`] covers the current turn and nothing else.
    /// Default no-op, so a gate with no durable storage (tests, `AllowAll`) is complete without
    /// pretending: choosing "for this session" against such a gate is simply not offered, because
    /// `offered_scopes` comes from the same gate.
    /// Never returns an error to core. A grant that could not be written is the gate's to report —
    /// the tool call itself was still approved, and failing it now would be a confusing second
    /// decision the user never made.
    async fn record_grant(&self, _req: &PolicyRequest, _scope: GrantScope) {}
}

/// Approves everything. **Tests and `--dangerously-skip-permissions` only.**
pub struct AllowAll;

#[async_trait]
impl PolicyGate for AllowAll {
    async fn evaluate(&self, _req: &PolicyRequest) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

/// The smallest gate that is defensible without a classifier.
/// Reads pass; anything that writes or reaches outside is put to the user. Deliberately cruder than
/// the eventual pipeline — it errs towards asking, which is the safe direction.
pub struct AskForWrites;

#[async_trait]
impl PolicyGate for AskForWrites {
    async fn evaluate(&self, req: &PolicyRequest) -> PolicyDecision {
        match req.tool.risk {
            ToolRisk::Read => PolicyDecision::Allow,
            ToolRisk::Write | ToolRisk::High => PolicyDecision::Ask {
                body: InteractionBody::Permission {
                    tool: req.tool.name.clone(),
                    args_preview: preview(&req.args),
                    reason: format!("{:?} risk", req.tool.risk),
                    caveats: Vec::new(),
                    offered_scopes: vec![GrantScope::Once, GrantScope::Session],
                    grant_preview: None,
                },
            },
        }
    }
}

/// What the user approved **during this turn**.
/// Consulted before the gate, so an approved call is never asked about twice inside one turn — the
/// commonest complaint about a permission system is being asked three times about near-identical
/// commands while it works.
/// # This is a turn-lived cache, not the grant store
/// A [`Core`](crate::Core) is one turn, so anything kept here dies with it. That used to make
/// `GrantScope::Session` behave almost like `Once`, which is the worst kind of bug in a permission
/// system: the user believes they answered. Durable scopes now go to the gate
/// ([`PolicyGate::record_grant`]), which turns them into rules it will read again next turn — and
/// this set keeps covering the rest of *this* turn so the same call is not re-asked before that
/// write is visible.
/// **The key is the exact tool and the exact arguments**, and that is right for a per-turn cache:
/// widening belongs to the rule the gate writes, where the user can see and revoke it.
#[derive(Default)]
pub struct GrantSet {
    granted: std::sync::Mutex<std::collections::HashSet<(String, String)>>,
}

impl GrantSet {
    pub fn allows(&self, tool: &str, args: &str) -> bool {
        self.granted
            .lock()
            .unwrap()
            .contains(&(tool.to_string(), args.to_string()))
    }

    /// Records a grant for the rest of this turn.
    /// [`GrantScope::Once`] records nothing — that is what "once" means. Everything else is
    /// remembered here *as well as* wherever the gate persists it.
    pub fn record(&self, scope: GrantScope, tool: &str, args: &str) {
        if scope == GrantScope::Once {
            return;
        }
        self.granted
            .lock()
            .unwrap()
            .insert((tool.to_string(), args.to_string()));
    }
}

/// Trims arguments for a human to read.
/// Cut on a character boundary, because a permission prompt showing a broken UTF-8 sequence is both
/// ugly and, for a path, actively misleading about what is being approved.
pub fn preview(args: &str) -> String {
    const MAX: usize = 300;
    let chars: Vec<char> = args.chars().collect();
    if chars.len() <= MAX {
        return args.to_string();
    }
    format!("{}…", chars[..MAX].iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(risk: ToolRisk) -> PolicyRequest {
        PolicyRequest {
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            tool: ToolMeta {
                name: "t".into(),
                source: "builtin",
                risk,
            },
            args: "{}".into(),
            exec_cwd: "/work".into(),
            root: "/work".into(),
            budget: None,
        }
    }

    #[tokio::test]
    async fn reads_pass_and_writes_are_put_to_the_user() {
        let g = AskForWrites;
        assert_eq!(
            g.evaluate(&req(ToolRisk::Read)).await,
            PolicyDecision::Allow
        );
        assert!(matches!(
            g.evaluate(&req(ToolRisk::Write)).await,
            PolicyDecision::Ask { .. }
        ));
        assert!(matches!(
            g.evaluate(&req(ToolRisk::High)).await,
            PolicyDecision::Ask { .. }
        ));
    }

    #[tokio::test]
    async fn the_prompt_names_the_tool_and_offers_scopes() {
        match AskForWrites.evaluate(&req(ToolRisk::Write)).await {
            PolicyDecision::Ask {
                body:
                    InteractionBody::Permission {
                        tool,
                        offered_scopes,
                        ..
                    },
            } => {
                assert_eq!(tool, "t");
                assert!(
                    offered_scopes.contains(&GrantScope::Once),
                    "Once is always available"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A permission prompt is the last place a broken character should appear — for a path it
    /// misrepresents what is being approved.
    #[test]
    fn the_preview_cuts_on_character_boundaries() {
        let args = format!("{{\"path\":\"{}\"}}", "é".repeat(400));
        let p = preview(&args);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), 301);
        // Round-trips as valid UTF-8 by construction.
        assert_eq!(p, String::from_utf8(p.clone().into_bytes()).unwrap());
    }

    #[test]
    fn a_short_preview_is_untouched() {
        assert_eq!(preview("{\"a\":1}"), "{\"a\":1}");
    }

    /// "Allow for this session" must not behave like "allow once".
    #[test]
    fn a_session_grant_is_remembered_and_once_is_not() {
        let g = GrantSet::default();
        assert!(!g.allows("shell", "{}"));

        g.record(GrantScope::Once, "shell", "{}");
        assert!(!g.allows("shell", "{}"), "once means once");

        g.record(GrantScope::Session, "shell", "{}");
        assert!(g.allows("shell", "{}"));
        // Exact arguments: a grant must not widen to a different call.
        assert!(!g.allows("shell", "{\"cmd\":\"rm -rf /\"}"));
        assert!(!g.allows("write_file", "{}"));
    }

    /// The blunt gate: only for a run that has explicitly given up on approval.
    #[tokio::test]
    async fn allow_all_approves_even_the_riskiest_call() {
        assert_eq!(
            AllowAll.evaluate(&req(ToolRisk::High)).await,
            PolicyDecision::Allow
        );
    }
}
