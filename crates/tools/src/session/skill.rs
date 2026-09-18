//! `skill` — load a written procedure and follow it.
//! A skill is a markdown file somebody wrote for one recurring kind of task ("cut a release", "fill
//! a PDF form"). The system prompt carries only a **catalog** (name, one line, path); this tool is
//! how the body actually arrives. That split is the whole design: ten skills' bodies would fill the
//! context, and nine of them are irrelevant to whatever is being done right now.
//! # Why a tool at all, when `read_file` could open the same file
//! Three things `read_file` cannot do, and they are the reasons this exists:
//! - **Arguments.** `skill { name: "release", args: "1.4.0" }` records a separate invocation that
//!   tells the model how to interpret `$ARGUMENTS` / `$1`. The durable definition stays reusable.
//! - **A name instead of a path.** The model picks from the catalog it was shown, and an invented
//!   name comes back with the real list — the same shape as `create_agent`. A path invites
//!   near-misses that read as "file not found".
//! - **A boundary.** Loading a skill is a distinct, auditable act. The later additions that need one
//!   (running a skill in a sub-agent, pre-authorising the commands it declares) all need a moment
//!   that says "this skill starts here" — `read_file` has no such moment.
//! # The result stays small; the definition is separate context state
//! The tool result audits the load and tells the model to proceed. Core persists the body as a
//! dedicated `SkillLoad` entry and projects the active definition as user-role context, avoiding a
//! duplicate copy in the tool result.
//! # What is deliberately not here
//! **No `allowed_tools` pre-authorisation and no `context: fork` yet.** Both are real features of
//! the reference; both need machinery that does not exist yet (a scoped grant frame, a spawner
//! reachable from here). The frontmatter fields are read and reported so a skill that declares them
//! is not silently misunderstood.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::{Result, Tool, ToolCtx, ToolError, ToolExecResult, ToolMeta, ToolRisk, parse_args};

/// A skill, loaded and ready to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub name: String,
    /// Stable digest of the complete source `SKILL.md`.
    pub revision: String,
    /// Frontmatter-free source body. All placeholders remain intact and are expanded only while
    /// projecting the current session's context.
    pub raw_body: String,
    /// Absolute path of the `SKILL.md`, so the result can say where this came from.
    pub path: String,
    /// Frontmatter this build understands but cannot honour yet. Reported in the result rather than
    /// ignored: a skill that says `allowed_tools: [...]` and gets none of it must not look as though
    /// it worked as written.
    pub unsupported: Vec<String>,
}

/// Reading the skill library. Implemented in engine — discovery needs the data directory, the
/// workspace root and the file system, none of which this crate knows about.
#[async_trait]
pub trait SkillHost: Send + Sync {
    /// Names the model may use, in catalog order.
    /// Used to answer an invented name with the real list, so a typo costs one round rather than a
    /// confusing failure deeper in.
    async fn available(&self) -> Vec<String>;

    /// Loads one definition by name. Invocation arguments are deliberately stored separately.
    /// `Err` is for "this skill cannot be loaded" (gone from disk, unreadable) — the model can pick
    /// another. An unknown name is answered by the tool, not here.
    async fn load(&self, name: &str) -> std::result::Result<LoadedSkill, String>;
}

#[derive(Debug, Deserialize)]
struct Args {
    name: String,
    args: Option<String>,
}

pub struct Skill;

#[async_trait]
impl Tool for Skill {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "skill".into(),
            source: "builtin",
            // A model-initiated load changes durable session instructions. Treat it like a write
            // so normal policy asks the user before a model promotes installed content into
            // context state. A user slash invocation does not go through the tool gate.
            risk: ToolRisk::Write,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "skill".into(),
            description:
                "Load one of the skills listed in your context and follow it. A skill is a \
                          written procedure for a recurring kind of task — when one matches what \
                          you are about to do, it is more specific than anything you would work \
                          out yourself, so load it first. Do not load skills speculatively."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill's name, exactly as listed in your context"
                    },
                    "args": {
                        "type": "string",
                        "description": "Arguments for this invocation. The model interprets them as the skill's `$ARGUMENTS` / `$1` placeholders"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        let name = a.name.trim().trim_start_matches('/').to_string();
        if name.is_empty() {
            return Ok(ToolExecResult::failed("name is required"));
        }

        let Some(host) = &ctx.skills else {
            return Err(ToolError::Unsupported(
                "skills are not available in this run",
            ));
        };

        // An invented name goes back with the real list — it can fix that itself, so it must not
        // end the turn.
        let available = host.available().await;
        if !available.iter().any(|n| n == &name) {
            return Ok(ToolExecResult::failed(format!(
                "no skill named {name:?}. Available: {}",
                match available.is_empty() {
                    true => "(none installed)".to_string(),
                    false => available.join(", "),
                }
            )));
        }

        let loaded = match host.load(&name).await {
            Ok(l) => l,
            Err(e) => return Ok(ToolExecResult::failed(format!("skill {name:?}: {e}"))),
        };

        // The procedure itself is persisted as a dedicated SkillLoad entry by core. Keeping the
        // tool result small avoids sending the same body once as a tool result and again as the
        // replaceable loaded-skill state.
        Ok(ToolExecResult::success(format!(
            "Loaded skill {:?} from {}. Follow the loaded procedure now{}.",
            loaded.name,
            loaded.path,
            a.args
                .as_deref()
                .map(|args| format!(" with arguments {args:?}"))
                .unwrap_or_default()
        ))
        .with_skill_load(loaded))
    }
}

/// Expands only placeholders whose value is stable for the session. Argument placeholders remain
/// in the durable definition and are interpreted by each separate skill invocation.
pub fn substitute_context(body: &str, skill_dir: &str, session_id: &str) -> String {
    body.replace("${SKILL_DIR}", skill_dir)
        .replace("${SESSION_ID}", session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeHost {
        names: Vec<String>,
        loaded: Mutex<Vec<(String, Option<String>)>>,
        unsupported: Vec<String>,
        fail: bool,
    }

    #[async_trait]
    impl SkillHost for FakeHost {
        async fn available(&self) -> Vec<String> {
            self.names.clone()
        }
        async fn load(&self, name: &str) -> std::result::Result<LoadedSkill, String> {
            self.loaded.lock().unwrap().push((name.to_string(), None));
            if self.fail {
                return Err("its file is gone".into());
            }
            Ok(LoadedSkill {
                name: name.to_string(),
                revision: "test-revision".into(),
                raw_body: "step one for $ARGUMENTS".into(),
                path: format!("/skills/{name}/SKILL.md"),
                unsupported: self.unsupported.clone(),
            })
        }
    }

    fn ctx_with(host: Arc<FakeHost>) -> ToolCtx {
        let mut c = test_ctx(Path::new("/work"));
        c.max_result_chars = 100_000;
        c.skills = Some(host);
        c
    }

    fn host(names: &[&str]) -> Arc<FakeHost> {
        Arc::new(FakeHost {
            names: names.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        })
    }

    /// The tool result acknowledges the load and carries the durable definition separately.
    #[tokio::test]
    async fn a_loaded_skill_is_framed_as_something_to_follow() {
        let h = host(&["release"]);
        let out = Skill
            .execute(&ctx_with(h.clone()), r#"{"name":"release","args":"1.4.0"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        let text = out.model_text();
        assert!(text.contains("Follow the loaded procedure now"), "{text}");
        assert!(
            text.contains("1.4.0"),
            "the args must reach the host: {text}"
        );
        assert!(
            text.contains("/skills/release/SKILL.md"),
            "it must say where the body comes from"
        );
        assert_eq!(
            out.skill_load
                .as_ref()
                .map(|loaded| loaded.raw_body.as_str()),
            Some("step one for $ARGUMENTS")
        );
        assert_eq!(*h.loaded.lock().unwrap(), [("release".to_string(), None)]);
    }

    #[tokio::test]
    async fn an_invented_name_comes_back_with_the_real_list() {
        let h = host(&["release", "review"]);
        let out = Skill
            .execute(&ctx_with(h.clone()), r#"{"name":"releese"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("release, review"),
            "{}",
            out.model_text()
        );
        assert!(
            h.loaded.lock().unwrap().is_empty(),
            "an invented skill name must not be loaded"
        );
    }

    #[tokio::test]
    async fn a_leading_slash_is_tolerated() {
        let h = host(&["release"]);
        let out = Skill
            .execute(&ctx_with(h.clone()), r#"{"name":"/release"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
    }

    #[tokio::test]
    async fn unsupported_frontmatter_is_disclosed_to_the_model() {
        let h = Arc::new(FakeHost {
            names: vec!["deploy".into()],
            unsupported: vec!["allowed_tools".into()],
            ..Default::default()
        });
        let out = Skill
            .execute(&ctx_with(h), r#"{"name":"deploy"}"#)
            .await
            .unwrap();
        assert_eq!(
            out.skill_load.unwrap().unsupported,
            ["allowed_tools".to_string()]
        );
    }

    #[tokio::test]
    async fn a_skill_that_cannot_be_loaded_is_reported_not_fatal() {
        let h = Arc::new(FakeHost {
            names: vec!["gone".into()],
            fail: true,
            ..Default::default()
        });
        let out = Skill
            .execute(&ctx_with(h), r#"{"name":"gone"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("its file is gone"));
    }

    #[tokio::test]
    async fn without_a_host_it_fails_closed() {
        let ctx = test_ctx(Path::new("/work"));
        assert!(matches!(
            Skill.execute(&ctx, r#"{"name":"x"}"#).await.unwrap_err(),
            ToolError::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn an_empty_name_is_refused_before_asking_the_host() {
        let h = host(&["release"]);
        let out = Skill
            .execute(&ctx_with(h.clone()), r#"{"name":"  "}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(h.loaded.lock().unwrap().is_empty());
    }

    #[test]
    fn context_placeholders_are_expanded_but_arguments_remain_dynamic() {
        assert_eq!(
            substitute_context(
                "run ${SKILL_DIR}/check.sh in ${SESSION_ID} for $ARGUMENTS ($1)",
                "/s/x",
                "sess-1"
            ),
            "run /s/x/check.sh in sess-1 for $ARGUMENTS ($1)"
        );
    }
}
