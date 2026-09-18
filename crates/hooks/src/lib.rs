//! Zlogic-native lifecycle hooks.
//! Hooks are deliberately a small, typed boundary. Commands receive one JSON
//! document on stdin and may return one tagged action on stdout. Project hooks
//! are only loaded for roots explicitly trusted by the global configuration.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    #[serde(rename = "input.submit.before")]
    InputSubmitBefore,
    #[serde(rename = "tool.execute.before")]
    ToolExecuteBefore,
    #[serde(rename = "tool.execute.after")]
    ToolExecuteAfter,
    #[serde(rename = "tool.execute.failed")]
    ToolExecuteFailed,
    #[serde(rename = "permission.requested")]
    PermissionRequested,
    #[serde(rename = "permission.denied")]
    PermissionDenied,
    #[serde(rename = "turn.finish.before")]
    TurnFinishBefore,
    #[serde(rename = "turn.failed")]
    TurnFailed,
    #[serde(rename = "context.compact.before")]
    ContextCompactBefore,
    #[serde(rename = "context.compact.after")]
    ContextCompactAfter,
    #[serde(rename = "notification.emitted")]
    NotificationEmitted,
    #[serde(rename = "mcp.elicitation.requested")]
    McpElicitationRequested,
    #[serde(rename = "mcp.elicitation.resolved")]
    McpElicitationResolved,
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "session.ending")]
    SessionEnding,
    #[serde(rename = "agent.start.before")]
    AgentStartBefore,
    #[serde(rename = "agent.finish.before")]
    AgentFinishBefore,
    #[serde(rename = "task.create.before")]
    TaskCreateBefore,
    #[serde(rename = "task.complete.before")]
    TaskCompleteBefore,
    #[serde(rename = "workspace.prepare.before")]
    WorkspacePrepareBefore,
    #[serde(rename = "instructions.loaded")]
    InstructionsLoaded,
    #[serde(rename = "config.apply.before")]
    ConfigApplyBefore,
    #[serde(rename = "worktree.create.before")]
    WorktreeCreateBefore,
    #[serde(rename = "worktree.create.after")]
    WorktreeCreateAfter,
    #[serde(rename = "worktree.remove.before")]
    WorktreeRemoveBefore,
    #[serde(rename = "worktree.remove.after")]
    WorktreeRemoveAfter,
    #[serde(rename = "cwd.changed")]
    CwdChanged,
    #[serde(rename = "file.changed")]
    FileChanged,
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = serde_json::to_value(self).map_err(|_| fmt::Error)?;
        f.write_str(value.as_str().ok_or(fmt::Error)?)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookRequest {
    pub event: HookEvent,
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub payload: Value,
    #[serde(skip)]
    pub matchers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow(Option<String>),
    Deny(Option<String>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    pub blocked: Option<String>,
    pub input: Option<Value>,
    pub contexts: Vec<String>,
    pub permission: Option<PermissionDecision>,
    pub warnings: Vec<String>,
}

#[async_trait]
pub trait HookRunner: Send + Sync {
    async fn run(&self, request: &HookRequest, cancel: &CancellationToken) -> HookOutcome;
}

#[derive(Debug, Default)]
pub struct NoopHookRunner;

#[async_trait]
impl HookRunner for NoopHookRunner {
    async fn run(&self, _: &HookRequest, _: &CancellationToken) -> HookOutcome {
        HookOutcome::default()
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GlobalConfig {
    #[serde(default)]
    trusted_projects: Vec<PathBuf>,
    #[serde(default)]
    hooks: BTreeMap<HookEvent, Vec<RuleConfig>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    #[serde(default)]
    hooks: BTreeMap<HookEvent, Vec<RuleConfig>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfig {
    #[serde(default, rename = "match")]
    matchers: BTreeMap<String, String>,
    run: CommandConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandConfig {
    command: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug)]
struct Rule {
    matchers: BTreeMap<String, Regex>,
    command: String,
    timeout: Duration,
}

#[derive(Debug, Default)]
pub struct CommandHookRunner {
    rules: BTreeMap<HookEvent, Vec<Rule>>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid hook config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("invalid matcher `{matcher}` for {event}: {source}")]
    Matcher {
        event: HookEvent,
        matcher: String,
        source: regex::Error,
    },
}

impl CommandHookRunner {
    /// Loads global hooks, then trusted project hooks from `<root>/.zlogic/hooks.yaml`.
    pub fn load(global_path: &Path, project_root: &Path) -> Result<Arc<Self>, LoadError> {
        let global = read_optional::<GlobalConfig>(global_path)?;
        let canonical_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let trusted = global
            .trusted_projects
            .iter()
            .any(|root| root.canonicalize().unwrap_or_else(|_| root.clone()) == canonical_root);
        let mut configured = global.hooks;
        let project_path = project_root.join(".zlogic").join("hooks.yaml");
        if trusted {
            let project = read_optional::<ProjectConfig>(&project_path)?;
            for (event, mut rules) in project.hooks {
                configured.entry(event).or_default().append(&mut rules);
            }
        }

        let mut compiled = BTreeMap::new();
        for (event, rules) in configured {
            let mut target = Vec::with_capacity(rules.len());
            for rule in rules {
                let mut matchers = BTreeMap::new();
                for (key, value) in rule.matchers {
                    let regex = Regex::new(&value).map_err(|source| LoadError::Matcher {
                        event,
                        matcher: value,
                        source,
                    })?;
                    matchers.insert(key, regex);
                }
                target.push(Rule {
                    matchers,
                    command: rule.run.command,
                    timeout: Duration::from_secs(rule.run.timeout_secs),
                });
            }
            compiled.insert(event, target);
        }
        Ok(Arc::new(Self { rules: compiled }))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.values().all(Vec::is_empty)
    }
}

fn read_optional<T>(path: &Path) -> Result<T, LoadError>
where
    T: serde::de::DeserializeOwned + Default,
{
    match std::fs::read_to_string(path) {
        Ok(text) => serde_yaml_ng::from_str(&text).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(source) => Err(LoadError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum CommandResponse {
    Continue,
    Block { reason: String },
    Modify { input: Value },
    AddContext { content: String },
    Allow { reason: Option<String> },
    Deny { reason: Option<String> },
}

#[async_trait]
impl HookRunner for CommandHookRunner {
    async fn run(&self, request: &HookRequest, cancel: &CancellationToken) -> HookOutcome {
        let mut outcome = HookOutcome::default();
        let Some(rules) = self.rules.get(&request.event) else {
            return outcome;
        };
        let mut current = request.clone();
        for rule in rules {
            if !rule.matchers.iter().all(|(key, regex)| {
                current
                    .matchers
                    .get(key)
                    .is_some_and(|value| regex.is_match(value))
            }) {
                continue;
            }
            current.payload = outcome
                .input
                .clone()
                .unwrap_or_else(|| request.payload.clone());
            match run_command(rule, &current, cancel).await {
                Ok(Some(CommandResponse::Continue)) | Ok(None) => {}
                Ok(Some(CommandResponse::Block { reason })) => {
                    outcome.blocked = Some(reason);
                    break;
                }
                Ok(Some(CommandResponse::Modify { input })) => outcome.input = Some(input),
                Ok(Some(CommandResponse::AddContext { content })) => outcome.contexts.push(content),
                Ok(Some(CommandResponse::Allow { reason })) => {
                    outcome.permission = Some(PermissionDecision::Allow(reason))
                }
                Ok(Some(CommandResponse::Deny { reason })) => {
                    outcome.permission = Some(PermissionDecision::Deny(reason))
                }
                Err(error) => outcome.warnings.push(error),
            }
        }
        outcome
    }
}

async fn run_command(
    rule: &Rule,
    request: &HookRequest,
    cancel: &CancellationToken,
) -> Result<Option<CommandResponse>, String> {
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("powershell");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &rule.command])
            // Headless: hooks run from a GUI host and must not pop a console window.
            .creation_flags(0x0800_0000);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", &rule.command]);
        command
    };
    command
        .current_dir(&request.cwd)
        .env("ZLOGIC_PROJECT_DIR", &request.cwd)
        .env("ZLOGIC_SESSION_ID", &request.session_id)
        .env("ZLOGIC_HOOK_EVENT", request.event.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("hook {} could not start: {error}", request.event))?;
    if let Some(mut stdin) = child.stdin.take() {
        let mut body = serde_json::to_vec(request)
            .map_err(|error| format!("hook {} request failed: {error}", request.event))?;
        body.push(b'\n');
        stdin
            .write_all(&body)
            .await
            .map_err(|error| format!("hook {} stdin failed: {error}", request.event))?;
    }
    let output = tokio::select! {
        () = cancel.cancelled() => return Err(format!("hook {} cancelled", request.event)),
        result = tokio::time::timeout(rule.timeout, child.wait_with_output()) => {
            result
                .map_err(|_| format!("hook {} timed out after {}s", request.event, rule.timeout.as_secs()))?
                .map_err(|error| format!("hook {} failed: {error}", request.event))?
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.code() == Some(2) {
        let reason = if stderr.is_empty() { stdout } else { stderr };
        return Ok(Some(CommandResponse::Block {
            reason: if reason.is_empty() {
                format!("hook {} blocked the operation", request.event)
            } else {
                reason
            },
        }));
    }
    if !output.status.success() {
        return Err(format!(
            "hook {} exited with {}{}",
            request.event,
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ));
    }
    if stdout.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&stdout)
        .map(Some)
        .map_err(|error| format!("hook {} returned invalid JSON: {error}", request.event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_zlogic_native() {
        assert_eq!(
            serde_json::to_string(&HookEvent::ToolExecuteBefore).unwrap(),
            "\"tool.execute.before\""
        );
        assert_eq!(
            serde_json::from_str::<HookEvent>("\"mcp.elicitation.resolved\"").unwrap(),
            HookEvent::McpElicitationResolved
        );
    }

    #[test]
    fn project_hooks_require_global_trust() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".zlogic")).unwrap();
        std::fs::write(
            temp.path().join(".zlogic/hooks.yaml"),
            "hooks:\n  tool.execute.before:\n    - run:\n        command: exit 2\n",
        )
        .unwrap();
        let runner =
            CommandHookRunner::load(&temp.path().join("global.yaml"), temp.path()).unwrap();
        assert!(runner.is_empty());
    }

    #[tokio::test]
    async fn exit_two_blocks_and_matchers_select_rules() {
        // The runner hands the command to `powershell` on Windows and to `/bin/sh` elsewhere, so the
        // fixture writes to stderr in the syntax of whichever shell will receive it.
        let command = if cfg!(windows) {
            r#"[Console]::Error.WriteLine("blocked"); exit 2"#
        } else {
            "printf blocked >&2; exit 2"
        };
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("hooks.yaml");
        std::fs::write(
            &config,
            format!(
                "hooks:\n  tool.execute.before:\n    - match:\n        tool: '^shell$'\n      run:\n        command: '{command}'\n"
            ),
        )
        .unwrap();
        let runner = CommandHookRunner::load(&config, temp.path()).unwrap();
        let mut matchers = BTreeMap::new();
        matchers.insert("tool".into(), "shell".into());
        let outcome = runner
            .run(
                &HookRequest {
                    event: HookEvent::ToolExecuteBefore,
                    session_id: "session".into(),
                    cwd: temp.path().to_path_buf(),
                    payload: serde_json::json!({"command": "zlogic ok"}),
                    matchers,
                },
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.blocked.as_deref(), Some("blocked"));
    }
}
