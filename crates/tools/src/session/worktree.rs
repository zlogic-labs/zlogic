//! `enter_worktree` / `exit_worktree` — moving where this session's tools run.
//! # Why these are session tools and not file tools
//! Nothing here reads or writes a file the model named. What they change is the **session**: after
//! `enter_worktree`, every later tool call in this session resolves its relative paths against a
//! different directory, and `exit_worktree` puts it back. That is a fact about the conversation,
//! which is what this domain is for.
//! # The host owns git; these tools own the rules
//! Same boundary as [`crate::agent`]: creating a checkout means touching the user's repository, and
//! a tool that did it directly would need git, the session row and the workspace root — none of
//! which this crate has. So the capability arrives as [`WorktreeHost`], injected into
//! [`crate::ToolCtx`], and without it these tools **fail closed** rather than pretending.
//! What stays here is everything the model can get wrong, so that both hosts behave the same:
//! - a name that is not a safe path segment is refused before anything is created;
//! - entering twice is refused, because "which worktree am I in" must have one answer;
//! - `remove` with uncommitted files or unmerged commits is refused unless the caller says
//!   explicitly that the work is disposable — and an *unknown* state counts as unsafe.
//! # `exec_cwd` is the whole mechanism
//! There is no process `chdir`: several sessions share this process, so changing the process's
//! directory would move every one of them. The host records the deviation on the session row and
//! [`crate::ToolCtx::exec_cwd`] is read fresh for every call, which is what makes the migration
//! take effect from the very next tool — including a tool already queued in the same batch.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::{Result, Tool, ToolCtx, ToolError, ToolExecResult, ToolMeta, ToolRisk, parse_args};

/// The longest a worktree name may be, in characters.
/// It becomes a directory name and a branch name, so this is about staying inside the shortest
/// path limit that matters rather than about taste.
const MAX_NAME_CHARS: usize = 64;

/// An isolated checkout this session has moved into.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorktreeState {
    /// The name the worktree was created under — also its branch's name.
    pub name: String,
    pub path: PathBuf,
    /// `None` only for a repository with no commits yet, where there is no branch to base on.
    pub branch: Option<String>,
    /// Where the session runs when it is **not** in a worktree: the workspace root.
    /// Carried so a result can say where exiting will land without the tool having to know how the
    /// root is resolved.
    pub base_dir: PathBuf,
    /// What the host had to do to make the checkout usable, in the host's own words.
    /// A fresh checkout is not the same thing as a working directory: local settings and
    /// gitignored files (`.env` and friends) are not in git, so a host may have to carry them
    /// over. That is worth **one line in the result** rather than being silent — a model that
    /// does not know a file was copied cannot know it exists, and a user who does not know it
    /// was copied is holding a stale copy of their own secret.
    /// The tool does not interpret these; it prints them. Empty is the normal case.
    pub notes: Vec<String>,
}

/// Work that would be lost by removing a worktree.
/// Both counts, not one number: "three files you never committed" and "three commits nobody
/// merged" are lost in different ways and a person deciding needs to see which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorktreeChanges {
    pub changed_files: u32,
    /// Commits on the worktree's branch that are not reachable from where it started.
    pub commits: u32,
}

impl WorktreeChanges {
    pub fn is_clean(&self) -> bool {
        self.changed_files == 0 && self.commits == 0
    }

    /// "2 uncommitted files and 1 commit" — for a refusal the user has to read.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.changed_files > 0 {
            let plural = if self.changed_files == 1 {
                "file"
            } else {
                "files"
            };
            parts.push(format!("{} uncommitted {plural}", self.changed_files));
        }
        if self.commits > 0 {
            let plural = if self.commits == 1 {
                "commit"
            } else {
                "commits"
            };
            parts.push(format!("{} unmerged {plural}", self.commits));
        }
        parts.join(" and ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAction {
    /// Leave the directory and the branch on disk.
    Keep,
    /// Delete both.
    Remove,
}

/// What leaving a worktree did.
#[derive(Debug, Clone, PartialEq)]
pub struct ExitedWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Where the session runs now.
    pub base_dir: PathBuf,
    /// Whether the checkout and its branch are gone.
    pub removed: bool,
}

/// The capability of putting a session in an isolated checkout.
/// Implemented outside this crate — it needs git, the session row and the workspace root. Every
/// method describes **this session**: a host is bound to one, so there is no session id to pass and
/// no way for one session to move another.
#[async_trait]
pub trait WorktreeHost: Send + Sync {
    /// The worktree this session entered, or `None` when it runs at the workspace root.
    /// Scoped to worktrees **this mechanism** created: one the user made with `git worktree add`,
    /// or one an earlier session entered and left behind, is not reported here and is therefore
    /// never something `exit_worktree` can delete.
    async fn current(&self) -> Option<WorktreeState>;

    /// Creates a checkout and moves the session into it.
    /// `name` has already been validated as a path-safe slug; `None` means the host picks one.
    async fn enter(&self, name: Option<&str>) -> std::result::Result<WorktreeState, String>;

    /// What would be lost by removing the current worktree.
    /// `None` = **could not be determined** (git failed, no baseline to compare against). Callers
    /// must treat that as unsafe rather than as "nothing to lose" — a silent `0/0` is how real work
    /// gets deleted.
    async fn changes(&self) -> Option<WorktreeChanges>;

    /// Moves the session back to the workspace root, keeping or deleting the checkout.
    /// The safety question is settled before this is called; a host does not second-guess it.
    async fn exit(&self, action: ExitAction) -> std::result::Result<ExitedWorktree, String>;
}

/// Whether a name can become a directory and a branch without surprises.
/// `/` is allowed **between** segments, because `feature/login` is what a person would name a
/// branch, and rejecting it would push them towards `feature-login` for no reason. Everything that
/// makes a path mean something other than itself — `..`, an empty segment, a leading or trailing
/// separator — is refused, so a name can never point outside where worktrees live.
pub fn validate_worktree_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("a worktree name cannot be empty".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "a worktree name may be at most {MAX_NAME_CHARS} characters"
        ));
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("a worktree name cannot start or end with `/`".into());
    }
    for segment in name.split('/') {
        if segment.is_empty() {
            return Err("a worktree name cannot contain an empty path segment".into());
        }
        if segment == "." || segment == ".." {
            return Err("`.` and `..` are not usable as name segments".into());
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(format!(
                "{segment:?} is not usable: each segment may contain only letters, digits, dots, \
                 underscores and dashes"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct EnterArgs {
    name: Option<String>,
}

pub struct EnterWorktree;

#[async_trait]
impl Tool for EnterWorktree {
    fn meta(&self) -> ToolMeta {
        // High: it creates a branch and a checkout in the user's repository, and it changes where
        // every later tool call in the session runs. Neither is something to do unannounced.
        ToolMeta {
            name: "enter_worktree".into(),
            source: "builtin",
            risk: ToolRisk::High,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "enter_worktree".into(),
            description: "Create an isolated git worktree and move this session into it, so the \
                          work happens on its own branch and checkout instead of the user's \
                          working directory. Every later tool call — reads, writes, shell — then \
                          runs there, and relative paths resolve there. Use it only when the user \
                          asks for a worktree: an ordinary branch switch or a fix in place is a \
                          git command, not this. Leave with exit_worktree."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": format!(
                            "Name for the worktree and its branch, e.g. `login-retry` or \
                             `feature/login`. Letters, digits, dots, underscores, dashes and `/` \
                             between segments; at most {MAX_NAME_CHARS} characters. Omit to have \
                             one generated."
                        )
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: EnterArgs = parse_args(args)?;

        let Some(host) = &ctx.worktree else {
            return Err(ToolError::Unsupported(
                "worktrees need a host to create them",
            ));
        };

        let name = a
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        if let Some(name) = &name
            && let Err(why) = validate_worktree_name(name)
        {
            // A name the model can fix, so it must not end the turn.
            return Ok(ToolExecResult::failed(why));
        }

        // One answer to "which worktree am I in". Nesting would also make `exit_worktree`
        // ambiguous about what it returns to.
        if let Some(existing) = host.current().await {
            return Ok(ToolExecResult::failed(format!(
                "this session is already in the worktree {} ({}). Leave it with exit_worktree \
                 before creating another.",
                existing.name,
                existing.path.display()
            )));
        }

        match host.enter(name.as_deref()).await {
            Ok(state) => {
                let branch = match &state.branch {
                    Some(b) => format!(" on branch {b}"),
                    None => String::new(),
                };
                // Printed, not interpreted — see `WorktreeState::notes`.
                let notes = match state.notes.is_empty() {
                    true => String::new(),
                    false => format!("\n{}", state.notes.join("\n")),
                };
                Ok(ToolExecResult::success(format!(
                    "Created the worktree {} at {}{branch}.\nEvery tool call from here on runs in \
                     that directory, and relative paths resolve against it. The user's working \
                     directory {} is untouched.{notes}\nCall exit_worktree with action \"keep\" to \
                     leave the work there, or \"remove\" to discard it.",
                    state.name,
                    state.path.display(),
                    state.base_dir.display(),
                )))
            }
            // Not a repository, a name already taken, a checkout that failed: all things the model
            // or the user can act on.
            Err(e) => Ok(ToolExecResult::failed(format!(
                "could not create the worktree: {e}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExitArgs {
    action: String,
    #[serde(default)]
    discard_changes: bool,
}

pub struct ExitWorktree;

#[async_trait]
impl Tool for ExitWorktree {
    fn meta(&self) -> ToolMeta {
        // High because `remove` deletes a checkout and a branch. `keep` is harmless, but static
        // risk is one value per tool and the approval pipeline reads the arguments.
        ToolMeta {
            name: "exit_worktree".into(),
            source: "builtin",
            risk: ToolRisk::High,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exit_worktree".into(),
            description: "Leave the worktree this session entered with enter_worktree and go back \
                          to the user's working directory. `keep` leaves the checkout and its \
                          branch on disk for the user to review or merge; `remove` deletes both. \
                          Removing is refused while there are uncommitted files or unmerged \
                          commits unless discard_changes says the work is disposable — ask the \
                          user first. Only ever touches a worktree this session created."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["keep", "remove"],
                        "description": "`keep` leaves the worktree and branch on disk; `remove` deletes both"
                    },
                    "discard_changes": {
                        "type": "boolean",
                        "description": "Only with `remove`: confirms that uncommitted files and unmerged commits may be destroyed. Ask the user before setting it."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: ExitArgs = parse_args(args)?;
        let action = match a.action.as_str() {
            "keep" => ExitAction::Keep,
            "remove" => ExitAction::Remove,
            other => {
                return Ok(ToolExecResult::failed(format!(
                    "action must be \"keep\" or \"remove\", not {other:?}"
                )));
            }
        };

        let Some(host) = &ctx.worktree else {
            return Err(ToolError::Unsupported(
                "worktrees need a host to manage them",
            ));
        };

        // The scope gate. `current` only reports a worktree this session entered, so nothing past
        // this point can reach a checkout the user made themselves.
        let Some(state) = host.current().await else {
            return Ok(ToolExecResult::failed(
                "this session is not in a worktree, so there is nothing to exit. Nothing was \
                 changed on disk. This tool only leaves worktrees created by enter_worktree in \
                 this session — it will not touch one you made with `git worktree add`."
                    .to_string(),
            ));
        };

        if action == ExitAction::Remove && !a.discard_changes {
            match host.changes().await {
                // Unknown state is unsafe state. Reporting a confident 0/0 here is exactly how
                // somebody's afternoon gets deleted.
                None => {
                    return Ok(ToolExecResult::failed(format!(
                        "could not determine whether {} holds unsaved work, so it was not \
                         removed. Check it with the user, then either re-run with \
                         discard_changes: true or use action \"keep\".",
                        state.path.display()
                    )));
                }
                Some(changes) if !changes.is_clean() => {
                    return Ok(ToolExecResult::failed(format!(
                        "{} has {} — removing it destroys that work permanently. Ask the user \
                         first: to go ahead re-run with discard_changes: true, to preserve it use \
                         action \"keep\".",
                        state.path.display(),
                        changes.describe()
                    )));
                }
                Some(_) => {}
            }
        }

        // Read before exiting: after this the worktree may be gone, and the counts are worth
        // reporting either way. Failure to read them is not a reason to fail the exit — the safety
        // decision has already been made.
        let changes = host.changes().await;

        match host.exit(action).await {
            Ok(exited) => {
                let branch = match &exited.branch {
                    Some(b) => format!(" on branch {b}"),
                    None => String::new(),
                };
                let body = if exited.removed {
                    let discarded = match changes {
                        Some(c) if !c.is_clean() => format!(" Discarded {}.", c.describe()),
                        _ => String::new(),
                    };
                    format!(
                        "Removed the worktree at {}.{discarded} Tool calls run in {} again.",
                        exited.path.display(),
                        exited.base_dir.display()
                    )
                } else {
                    format!(
                        "Left the worktree. The work is still at {}{branch} for the user to \
                         review or merge. Tool calls run in {} again.",
                        exited.path.display(),
                        exited.base_dir.display()
                    )
                };
                Ok(ToolExecResult::success(body))
            }
            Err(e) => Ok(ToolExecResult::failed(format!(
                "could not leave the worktree at {}: {e}. The session is still running there.",
                state.path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// A host that records what it was asked to do, so the tools' rules can be tested without git.
    #[derive(Default)]
    struct FakeHost {
        state: Mutex<Option<WorktreeState>>,
        changes: Mutex<Option<WorktreeChanges>>,
        entered: Mutex<Vec<Option<String>>>,
        exits: Mutex<Vec<ExitAction>>,
        enter_fails: bool,
        /// What the host says it had to do to make the checkout usable.
        notes: Vec<String>,
    }

    impl FakeHost {
        fn inside(name: &str) -> Self {
            let me = Self::default();
            *me.state.lock().unwrap() = Some(WorktreeState {
                name: name.into(),
                path: PathBuf::from("/data/worktrees/ws/").join(name),
                branch: Some(name.into()),
                base_dir: PathBuf::from("/work"),
                notes: Vec::new(),
            });
            *me.changes.lock().unwrap() = Some(WorktreeChanges {
                changed_files: 0,
                commits: 0,
            });
            me
        }

        fn with_changes(self, changed_files: u32, commits: u32) -> Self {
            *self.changes.lock().unwrap() = Some(WorktreeChanges {
                changed_files,
                commits,
            });
            self
        }

        fn with_unknown_changes(self) -> Self {
            *self.changes.lock().unwrap() = None;
            self
        }
    }

    #[async_trait]
    impl WorktreeHost for FakeHost {
        async fn current(&self) -> Option<WorktreeState> {
            self.state.lock().unwrap().clone()
        }

        async fn enter(&self, name: Option<&str>) -> std::result::Result<WorktreeState, String> {
            self.entered.lock().unwrap().push(name.map(str::to_string));
            if self.enter_fails {
                return Err("not a git repository".into());
            }
            let name = name.unwrap_or("generated").to_string();
            let state = WorktreeState {
                path: PathBuf::from("/data/worktrees/ws").join(&name),
                branch: Some(name.clone()),
                base_dir: PathBuf::from("/work"),
                notes: self.notes.clone(),
                name,
            };
            *self.state.lock().unwrap() = Some(state.clone());
            Ok(state)
        }

        async fn changes(&self) -> Option<WorktreeChanges> {
            *self.changes.lock().unwrap()
        }

        async fn exit(&self, action: ExitAction) -> std::result::Result<ExitedWorktree, String> {
            self.exits.lock().unwrap().push(action);
            let state = self.state.lock().unwrap().take().expect("in a worktree");
            Ok(ExitedWorktree {
                path: state.path,
                branch: state.branch,
                base_dir: state.base_dir,
                removed: action == ExitAction::Remove,
            })
        }
    }

    fn ctx_with(host: Arc<FakeHost>) -> ToolCtx {
        let mut c = test_ctx(Path::new("/work"));
        c.max_result_chars = 100_000;
        c.worktree = Some(host);
        c
    }

    #[tokio::test]
    async fn entering_reports_the_new_directory_and_how_to_leave() {
        let host = Arc::new(FakeHost::default());
        let ctx = ctx_with(host.clone());

        let out = EnterWorktree
            .execute(&ctx, r#"{"name":"login-retry"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        let text = out.model_text();
        assert!(text.contains("login-retry"), "{text}");
        assert!(
            text.contains("exit_worktree"),
            "the way back must be in the result: {text}"
        );
        assert_eq!(
            *host.entered.lock().unwrap(),
            [Some("login-retry".to_string())]
        );
    }

    /// A file the host had to carry over must be **said**: a model that does not know `.env` was
    /// copied cannot know it is there, and a user who does not know it was copied is holding a
    /// second copy of their own secret.
    #[tokio::test]
    async fn what_the_host_had_to_prepare_reaches_the_result() {
        let host = Arc::new(FakeHost {
            notes: vec!["Carried over 2 local files: .zlogic/settings.yaml, .env".into()],
            ..Default::default()
        });
        let out = EnterWorktree
            .execute(&ctx_with(host), r#"{"name":"x"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(
            out.model_text().contains(".zlogic/settings.yaml"),
            "{}",
            out.model_text()
        );
    }

    /// No name is a valid request — the host picks one.
    #[tokio::test]
    async fn a_missing_name_is_left_to_the_host() {
        let host = Arc::new(FakeHost::default());
        let ctx = ctx_with(host.clone());
        assert_eq!(
            EnterWorktree.execute(&ctx, "{}").await.unwrap().status,
            ToolExecStatus::Success
        );
        assert_eq!(*host.entered.lock().unwrap(), [None]);
    }

    /// A name that could escape the worktree directory must be refused **before** anything is
    /// created, which is why validation is here and not in the host.
    #[tokio::test]
    async fn an_unsafe_name_never_reaches_the_host() {
        let host = Arc::new(FakeHost::default());
        let ctx = ctx_with(host.clone());
        for bad in ["../../etc", "/absolute", "trailing/", "has space", "a/../b"] {
            let args = json!({ "name": bad }).to_string();
            let out = EnterWorktree.execute(&ctx, &args).await.unwrap();
            assert_eq!(out.status, ToolExecStatus::Failed, "{bad:?} was accepted");
        }
        assert!(
            host.entered.lock().unwrap().is_empty(),
            "nothing was created"
        );
    }

    /// A blank name is "you did not name it", not an invalid name — refusing it would be a round
    /// wasted on a distinction with no consequence.
    #[tokio::test]
    async fn a_blank_name_means_no_name() {
        let host = Arc::new(FakeHost::default());
        let ctx = ctx_with(host.clone());
        let out = EnterWorktree
            .execute(&ctx, r#"{"name":"   "}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(*host.entered.lock().unwrap(), [None]);
    }

    #[tokio::test]
    async fn a_branch_shaped_name_is_allowed() {
        assert!(validate_worktree_name("feature/login-v2.1").is_ok());
        assert!(validate_worktree_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
    }

    /// "Which worktree am I in" has to have one answer.
    #[tokio::test]
    async fn entering_twice_is_refused() {
        let host = Arc::new(FakeHost::inside("first"));
        let ctx = ctx_with(host.clone());
        let out = EnterWorktree
            .execute(&ctx, r#"{"name":"second"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("first"), "{}", out.model_text());
        assert!(host.entered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_host_failure_reaches_the_model_rather_than_ending_the_turn() {
        let host = Arc::new(FakeHost {
            enter_fails: true,
            ..Default::default()
        });
        let out = EnterWorktree.execute(&ctx_with(host), "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("not a git repository"));
    }

    /// Without a host these tools are unavailable — never a pretend success.
    #[tokio::test]
    async fn both_tools_fail_closed_without_a_host() {
        let ctx = test_ctx(Path::new("/work"));
        assert!(matches!(
            EnterWorktree.execute(&ctx, "{}").await.unwrap_err(),
            ToolError::Unsupported(_)
        ));
        assert!(matches!(
            ExitWorktree
                .execute(&ctx, r#"{"action":"keep"}"#)
                .await
                .unwrap_err(),
            ToolError::Unsupported(_)
        ));
    }

    /// The scope gate: not in a worktree is a no-op that says so, not a filesystem operation.
    #[tokio::test]
    async fn exiting_without_a_worktree_changes_nothing() {
        let host = Arc::new(FakeHost::default());
        let out = ExitWorktree
            .execute(&ctx_with(host.clone()), r#"{"action":"remove"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("not in a worktree"),
            "{}",
            out.model_text()
        );
        assert!(host.exits.lock().unwrap().is_empty(), "nothing was removed");
    }

    #[tokio::test]
    async fn keeping_says_where_the_work_is_and_where_the_session_went() {
        let host = Arc::new(FakeHost::inside("login-retry").with_changes(3, 1));
        let out = ExitWorktree
            .execute(&ctx_with(host.clone()), r#"{"action":"keep"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        let text = out.model_text();
        assert!(text.contains("login-retry"), "{text}");
        assert!(text.contains("/work"), "where tools run now: {text}");
        assert_eq!(*host.exits.lock().unwrap(), [ExitAction::Keep]);
    }

    /// `keep` never asks about changes — preserving work cannot lose any.
    #[tokio::test]
    async fn keeping_is_allowed_with_unsaved_work() {
        let host = Arc::new(FakeHost::inside("wip").with_changes(9, 4));
        let out = ExitWorktree
            .execute(&ctx_with(host), r#"{"action":"keep"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
    }

    #[tokio::test]
    async fn a_clean_worktree_can_be_removed_without_confirmation() {
        let host = Arc::new(FakeHost::inside("scratch"));
        let out = ExitWorktree
            .execute(&ctx_with(host.clone()), r#"{"action":"remove"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(*host.exits.lock().unwrap(), [ExitAction::Remove]);
    }

    /// The refusal has to name what would be lost, or the user cannot answer the question it asks.
    #[tokio::test]
    async fn removing_unsaved_work_is_refused_and_says_what_it_is() {
        let host = Arc::new(FakeHost::inside("wip").with_changes(2, 3));
        let out = ExitWorktree
            .execute(&ctx_with(host.clone()), r#"{"action":"remove"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("2 uncommitted files"), "{text}");
        assert!(text.contains("3 unmerged commits"), "{text}");
        assert!(
            text.contains("discard_changes"),
            "and how to proceed: {text}"
        );
        assert!(host.exits.lock().unwrap().is_empty(), "nothing was removed");
    }

    #[tokio::test]
    async fn removing_unsaved_work_proceeds_once_it_is_confirmed() {
        let host = Arc::new(FakeHost::inside("wip").with_changes(2, 0));
        let out = ExitWorktree
            .execute(
                &ctx_with(host.clone()),
                r#"{"action":"remove","discard_changes":true}"#,
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(
            out.model_text().contains("Discarded 2 uncommitted files"),
            "{}",
            out.model_text()
        );
        assert_eq!(*host.exits.lock().unwrap(), [ExitAction::Remove]);
    }

    /// An unknown state is treated as unsafe. A confident 0/0 here is how work gets deleted.
    #[tokio::test]
    async fn an_undeterminable_state_blocks_removal() {
        let host = Arc::new(FakeHost::inside("wip").with_unknown_changes());
        let out = ExitWorktree
            .execute(&ctx_with(host.clone()), r#"{"action":"remove"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("could not determine"),
            "{}",
            out.model_text()
        );
        assert!(host.exits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_action_is_reported_to_the_model() {
        let host = Arc::new(FakeHost::inside("wip"));
        let out = ExitWorktree
            .execute(&ctx_with(host.clone()), r#"{"action":"delete"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("keep"), "{}", out.model_text());
        assert!(host.exits.lock().unwrap().is_empty());
    }
}
