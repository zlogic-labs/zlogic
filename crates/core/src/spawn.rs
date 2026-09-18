//! Running a sub-agent.
//! This is core's side of the boundary `zlogic_tools::agent` describes: the tool holds the trait,
//! core holds the implementation, and injection breaks what would otherwise be a
//! `tools → core → tools` cycle.
//! # The child session is created first
//! Before anything runs, so the sub-agent's entries and usage have somewhere to go. Two reasons
//! that matters, both about what survives:
//! - A sub-agent that made fifteen tool calls and got it wrong is exactly the transcript someone
//!   will want to read. Rolled into the parent it would be interleaved with the parent's own work
//!   and unreadable; discarded, it would be gone.
//! - Usage has to be attributable. Charged to the parent session, a sub-agent's tokens would both
//!   distort the parent's compaction signal (which reads only `Purpose::Main`) and make "what did
//!   that sub-agent cost" unanswerable.
//! Only the conclusion comes back through the tool result. The child session id travels with it,
//! which is what lets the UI drill from the parent's tool card into the child's transcript.
//! # Why `Arc::new_cyclic`
//! A sub-agent may spawn its own sub-agent, so the child `Core` needs a spawner — this one. Holding
//! an `Arc<Self>` inside `Self` leaks; a `Weak` self-reference does not. The depth limit is the
//! separate question of how far the recursion may go, and it is checked before the child session is
//! created so a refused spawn leaves nothing behind.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use zlogic_llm::LlmClient;
use zlogic_protocol::config::ResolvedModel;
use zlogic_protocol::llm::ThinkingIntent;
use zlogic_protocol::message::{ContentPart, TextPart};
use zlogic_protocol::stream::AgentRef;
use zlogic_protocol::usage::Purpose;
use zlogic_store::NewSession;
use zlogic_tools::{AgentOutcome, AgentRequest, AgentSpawner, SkillHost};

use crate::{Core, CoreServices, EventSink, TurnPlan};

/// A sub-agent's profile: everything about *how* it runs.
/// The model and its client are resolved by the engine (which owns routing) and handed over here,
/// so a profile can be pinned to a cheaper or a thinking model without core knowing what a role is.
pub struct AgentProfile {
    pub name: String,
    pub system: Vec<String>,
    /// `None` = every registered tool. A restriction applies at materialisation, so an excluded
    /// tool is invisible to the sub-agent rather than refused when called.
    pub tools: Option<Vec<String>>,
    pub model: ResolvedModel,
    pub client: Arc<dyn LlmClient>,
    pub thinking: ThinkingIntent,
}

pub type AgentSkillFactory =
    Arc<dyn Fn(zlogic_protocol::SessionId) -> Arc<dyn SkillHost> + Send + Sync>;

pub struct CoreSpawner {
    me: Weak<CoreSpawner>,
    services: Arc<CoreServices>,
    profiles: BTreeMap<String, AgentProfile>,
    /// The fallback profile for custom agents: a name outside [`Self::profiles`] runs on this
    /// base (model / tools / system) with the request's own `system` / `tools` / `model` layered
    /// on top. `None` = no base was supplied, so an unregistered name without explicit
    /// customisation fails with the available list.
    base: Option<AgentProfile>,
    /// The run's tool set. `None` = [`CoreServices::tools`], the built-ins.
    /// A sub-agent has to see the **same** registry as its parent: a profile's allowlist may well
    /// name an MCP tool, and a child materialising against the built-ins only would report it as
    /// missing. Narrowing is [`AgentProfile::tools`]'s job, not the registry's.
    tools: Option<crate::ToolRegistry>,
    /// Creates a skill host after the child session id exists, so `${SESSION_ID}` substitutions
    /// belong to the child rather than accidentally pointing back at its parent.
    skill_factory: Option<AgentSkillFactory>,
    /// The root's sink: sub-agent events travel the same stream, tagged with their [`AgentRef`].
    sink: Arc<dyn EventSink>,
    root: std::path::PathBuf,
    /// The depth of the agent that owns this spawner. Children run one deeper.
    depth: u32,
    budget: std::sync::Mutex<Option<Arc<crate::budget::TurnBudget>>>,
}

impl CoreSpawner {
    pub fn new(
        services: Arc<CoreServices>,
        profiles: Vec<AgentProfile>,
        sink: Arc<dyn EventSink>,
        root: impl Into<std::path::PathBuf>,
        depth: u32,
    ) -> Arc<Self> {
        Self::with_tools(services, profiles, sink, root, depth, None)
    }

    /// The same, with the run's tool set.
    /// A separate constructor rather than a builder: `Arc::new_cyclic` has already handed the weak
    /// self-reference out by the time a builder could run, and rebuilding would mean cloning the
    /// profiles — which own a client each.
    #[allow(clippy::too_many_arguments)]
    pub fn with_tools(
        services: Arc<CoreServices>,
        profiles: Vec<AgentProfile>,
        sink: Arc<dyn EventSink>,
        root: impl Into<std::path::PathBuf>,
        depth: u32,
        tools: Option<crate::ToolRegistry>,
    ) -> Arc<Self> {
        Self::with_runtime(services, profiles, sink, root, depth, tools, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_runtime(
        services: Arc<CoreServices>,
        profiles: Vec<AgentProfile>,
        sink: Arc<dyn EventSink>,
        root: impl Into<std::path::PathBuf>,
        depth: u32,
        tools: Option<crate::ToolRegistry>,
        skill_factory: Option<AgentSkillFactory>,
    ) -> Arc<Self> {
        Self::with_base(
            services,
            profiles,
            None,
            sink,
            root,
            depth,
            tools,
            skill_factory,
        )
    }

    /// `with_runtime` plus the fallback profile custom agents run on.
    #[allow(clippy::too_many_arguments)]
    pub fn with_base(
        services: Arc<CoreServices>,
        profiles: Vec<AgentProfile>,
        base: Option<AgentProfile>,
        sink: Arc<dyn EventSink>,
        root: impl Into<std::path::PathBuf>,
        depth: u32,
        tools: Option<crate::ToolRegistry>,
        skill_factory: Option<AgentSkillFactory>,
    ) -> Arc<Self> {
        let profiles = profiles.into_iter().map(|p| (p.name.clone(), p)).collect();
        let root = root.into();
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            services,
            profiles,
            base,
            sink,
            root,
            depth,
            tools,
            skill_factory,
            budget: std::sync::Mutex::new(None),
        })
    }

    pub fn set_budget(&self, budget: Option<Arc<crate::budget::TurnBudget>>) {
        *self.budget.lock().unwrap_or_else(|e| e.into_inner()) = budget;
    }
}

#[async_trait]
impl AgentSpawner for CoreSpawner {
    async fn spawn(&self, req: AgentRequest) -> Result<AgentOutcome, String> {
        let child_depth = self.depth + 1;
        if child_depth > self.services.limits.max_depth {
            // Checked before anything is created: a refused spawn leaves no empty session behind.
            return Err(format!(
                "sub-agents may nest {} deep; `{}` would be {child_depth}",
                self.services.limits.max_depth, req.agent
            ));
        }

        let (profile, via_base) = match self.profiles.get(&req.agent) {
            Some(profile) => (profile, false),
            None => match (&self.base, req.is_customised()) {
                (Some(base), true) => (base, true),
                _ => {
                    return Err(format!(
                        "unknown agent `{}`. Available: {}{}",
                        req.agent,
                        self.available().join(", "),
                        if self.base.is_some() {
                            "; or pick a new name and pass system/tools/model to customise it"
                        } else {
                            ""
                        }
                    ));
                }
            },
        };

        let (model, client, thinking) = match &req.model {
            Some(model_ref) => {
                let resolver = self.services.model_resolver.as_ref().ok_or_else(|| {
                    format!(
                        "cannot resolve model `{model_ref}` for custom agent `{}`: no model \
                         resolver in this run",
                        req.agent
                    )
                })?;
                resolver
                    .resolve(model_ref)
                    .await
                    .map_err(|e| format!("cannot resolve model `{model_ref}`: {e}"))?
            }
            None => (
                profile.model.clone(),
                profile.client.clone(),
                profile.thinking,
            ),
        };

        let mut system = profile.system.clone();
        if let Some(custom) = &req.system {
            system.push(custom.clone());
        }
        let tools = req.tools.clone().or_else(|| profile.tools.clone());
        let _ = via_base; // its meaning is given by the comment above; this is only a compile-time intent marker.

        // The child inherits **where the parent runs**, not just its root: a parent that moved into
        // a worktree has its work there, and a sub-agent asked to review or extend that work has to
        // see the same files. `create` inherits workspace and agent path on its own; `exec_cwd` is
        // a deviation, so it has to be carried across explicitly.
        // The child gets no worktree host: entering one would move only the child, which is a
        // decision the parent's turn cannot see the consequences of.
        let session = self
            .services
            .store
            .with(|db| {
                let parent = db.sessions().get(req.parent_session_id)?;
                let mut child = NewSession::child(req.parent_session_id, &req.agent);
                child.exec_cwd = req.exec_cwd.clone().or(parent.exec_cwd.clone());
                db.sessions().create(child)
            })
            .map_err(|e| format!("cannot create the sub-agent's session: {e}"))?;
        if let Some(mailbox) = &req.mailbox {
            mailbox.activate(session.session_id).await;
        }

        let agent = AgentRef {
            agent_id: Some(session.session_id.to_string()),
            parent_agent_id: Some(req.parent_session_id.to_string()),
            name: req.agent.clone(),
        };

        let services = if req.unattended {
            Arc::new(CoreServices {
                store: self.services.store.clone(),
                objects: self.services.objects.clone(),
                attachment_dir: self.services.attachment_dir.clone(),
                tools: self.services.tools.clone(),
                policy: self.services.policy.clone(),
                interaction: None,
                tasks: self.services.tasks.clone(),
                runtime_paths: self.services.runtime_paths.clone(),
                model_resolver: self.services.model_resolver.clone(),
                limits: self.services.limits.clone(),
                context: self.services.context.clone(),
            })
        } else {
            self.services.clone()
        };

        let core = Core::new(
            services,
            session.session_id,
            self.root.clone(),
            self.sink.clone(),
        )
        .as_sub_agent(agent, child_depth)
        // The parent's registry, so a profile may name an extension's tool.
        .with_tools(self.tools.clone())
        .with_skills(
            self.skill_factory
                .as_ref()
                .map(|factory| factory(session.session_id)),
        )
        .with_agent_mailbox(req.mailbox.clone())
        // A sub-agent can spawn its own, bounded by the same depth limit.
        .with_spawner(self.me.upgrade().map(|s| s as Arc<dyn AgentSpawner>));

        let plan = TurnPlan::new(model, client)
            .with_system(system)
            .with_tools(tools)
            .for_purpose(Purpose::Agent(req.agent.clone()));
        let plan = TurnPlan {
            thinking,
            budget: {
                let parent = self
                    .budget
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if req.unattended {
                    parent.map(|p| std::sync::Arc::new(p.for_task_run()))
                } else {
                    parent
                }
            },
            ..plan
        };

        let task = vec![ContentPart::Text(TextPart {
            text: req.task.clone(),
            raw: None,
            truncated: false,
        })];

        // The parent's token, unchanged: one conversation, one token. A child token would mean the
        // sub-agent could be left running by a layer that forgot to forward.
        let outcome = core
            .run(
                zlogic_protocol::TurnId::new(),
                plan,
                task,
                req.cancel.clone(),
            )
            .await;
        if let Some(mailbox) = &req.mailbox {
            mailbox.close().await;
        }
        let outcome = outcome.map_err(|e| format!("sub-agent `{}` failed: {e}", req.agent))?;

        Ok(AgentOutcome {
            session_id: session.session_id,
            // An empty answer is reported as such rather than as a success with nothing in it:
            // the caller would otherwise read "" as "the task produced no findings".
            answer: if outcome.answer.trim().is_empty() {
                format!(
                    "`{}` finished with status {:?} and produced no text. Its transcript is in \
                     session {}.",
                    req.agent, outcome.status, session.session_id
                )
            } else {
                outcome.answer
            },
        })
    }

    fn available(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }
}
