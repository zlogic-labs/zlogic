//! ```text
//! ```

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use zlogic_config::Dirs;
use zlogic_policy::{CommandRule, Effect, Policy};
use zlogic_protocol::SessionId;
use zlogic_protocol::interaction::GrantScope;

pub const GRANTS_FILE: &str = "grants.yaml";

pub const GRANT_ID_PREFIX: &str = "grant";

pub const MACHINE_TAG_PREFIX: &str = "machine:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEntry {
    pub scope: GrantScope,
    pub id: String,
    pub pattern: String,
    pub allow: bool,
    pub file: PathBuf,
}

pub struct Grants {
    dirs: Dirs,
    machine: String,
}

impl Grants {
    pub fn new(dirs: Dirs) -> Self {
        Self {
            machine: machine_id(&dirs),
            dirs,
        }
    }

    pub fn load(&self, session_id: SessionId, root: &Path) -> (Policy, Vec<String>) {
        let mut merged = Policy::default();
        let mut problems = Vec::new();

        for (path, scope) in [
            (self.global_path(), GrantScope::Global),
            (self.workspace_path(root), GrantScope::Workspace),
            (self.session_path(session_id), GrantScope::Session),
        ] {
            if !path.exists() {
                continue;
            }
            match Policy::from_yaml_file(&path) {
                Ok(loaded) => {
                    let (kept, dropped) = self.filter_foreign(loaded, scope);
                    problems.extend(dropped);
                    merged.commands.extend(kept.commands);
                    merged.paths.extend(kept.paths);
                    merged.exec.extend(kept.exec);
                    merged.scripts.extend(kept.scripts);
                }
                Err(e) => {
                    problems.push(format!("cannot read grants file ({}): {e}", path.display()))
                }
            }
        }
        (merged, problems)
    }

    pub fn record(
        &self,
        scope: GrantScope,
        session_id: SessionId,
        root: &Path,
        rule: CommandRule,
    ) -> Result<PathBuf, String> {
        let path = match scope {
            GrantScope::Global => self.global_path(),
            GrantScope::Workspace => self.workspace_path(root),
            GrantScope::Session => self.session_path(session_id),
            other => {
                return Err(format!(
                    "{other:?} is not a grant that needs to be persisted"
                ));
            }
        };

        let mut policy = match path.exists() {
            true => Policy::from_yaml_file(&path)
                .map_err(|e| format!("cannot read grants file ({}): {e}", path.display()))?,
            false => Policy::default(),
        };
        if policy
            .commands
            .iter()
            .any(|existing| existing.pattern == rule.pattern && existing.effect == rule.effect)
        {
            return Ok(path);
        }
        policy.commands.push(rule);
        policy
            .validate()
            .map_err(|e| format!("the policy is invalid with this grant written in: {e}"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, policy.to_yaml())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    }

    pub fn list(&self, session_id: Option<SessionId>, root: Option<&Path>) -> Vec<GrantEntry> {
        let mut out = Vec::new();
        let mut layers: Vec<(GrantScope, PathBuf)> = vec![(GrantScope::Global, self.global_path())];
        if let Some(root) = root {
            layers.push((GrantScope::Workspace, self.workspace_path(root)));
        }
        if let Some(session) = session_id {
            layers.push((GrantScope::Session, self.session_path(session)));
        }

        for (scope, path) in layers {
            let Ok(policy) = Policy::from_yaml_file(&path) else {
                continue;
            };
            for rule in policy.commands {
                out.push(GrantEntry {
                    scope,
                    id: rule.id,
                    pattern: rule.pattern,
                    allow: rule.effect == Effect::Allow,
                    file: path.clone(),
                });
            }
        }
        out
    }

    pub fn revoke(
        &self,
        id: &str,
        session_id: Option<SessionId>,
        root: Option<&Path>,
    ) -> Result<bool, String> {
        let mut layers: Vec<PathBuf> = vec![self.global_path()];
        if let Some(root) = root {
            layers.push(self.workspace_path(root));
        }
        if let Some(session) = session_id {
            layers.push(self.session_path(session));
        }

        for path in layers {
            if !path.exists() {
                continue;
            }
            let mut policy = Policy::from_yaml_file(&path)
                .map_err(|e| format!("cannot read grants file ({}): {e}", path.display()))?;
            let before = policy.commands.len();
            policy.commands.retain(|rule| rule.id != id);
            if policy.commands.len() == before {
                continue;
            }
            let written = match policy.commands.is_empty() && policy.paths.is_empty() {
                true => std::fs::remove_file(&path).map_err(|e| e.to_string()),
                false => std::fs::write(&path, policy.to_yaml()).map_err(|e| e.to_string()),
            };
            written.map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn protected_paths(&self, session_id: SessionId, root: &Path) -> Vec<PathBuf> {
        vec![
            self.global_path(),
            self.workspace_path(root),
            self.session_path(session_id),
        ]
    }

    fn rule_id(&self, scope: GrantScope, at: DateTime<Utc>) -> String {
        format!(
            "{GRANT_ID_PREFIX}:{}:{}:{MACHINE_TAG_PREFIX}{}",
            scope_tag(scope),
            at.format("%Y-%m-%dT%H:%M:%SZ"),
            self.machine
        )
    }

    fn filter_foreign(&self, policy: Policy, scope: GrantScope) -> (Policy, Vec<String>) {
        if scope != GrantScope::Workspace {
            return (policy, Vec::new());
        }
        let mine = format!("{MACHINE_TAG_PREFIX}{}", self.machine);
        let mut dropped = Vec::new();
        let mut policy = policy;
        policy.commands.retain(|rule| {
            let is_grant = rule.id.starts_with(&format!("{GRANT_ID_PREFIX}:"));
            let keep = !is_grant || rule.id.ends_with(&mine);
            if !keep {
                dropped.push(format!(
                    "project grant {:?} was approved on another machine and was ignored ({})",
                    rule.pattern, rule.id
                ));
            }
            keep
        });
        (policy, dropped)
    }

    fn global_path(&self) -> PathBuf {
        self.dirs.config.join(GRANTS_FILE)
    }

    fn workspace_path(&self, root: &Path) -> PathBuf {
        root.join(".zlogic").join(GRANTS_FILE)
    }

    fn session_path(&self, session_id: SessionId) -> PathBuf {
        self.dirs
            .state
            .join("sessions")
            .join(session_id.to_string())
            .join(GRANTS_FILE)
    }

    pub fn session_dir(&self, session_id: SessionId) -> PathBuf {
        self.dirs
            .state
            .join("sessions")
            .join(session_id.to_string())
    }
}

fn scope_tag(scope: GrantScope) -> &'static str {
    match scope {
        GrantScope::Once => "once",
        GrantScope::Turn => "turn",
        GrantScope::Session => "session",
        GrantScope::Workspace => "workspace",
        GrantScope::Global => "global",
    }
}

fn machine_id(dirs: &Dirs) -> String {
    if let Some(name) = std::env::var_os("HOSTNAME").or_else(|| std::env::var_os("COMPUTERNAME")) {
        let name = name.to_string_lossy().trim().to_string();
        if !name.is_empty() {
            return sanitize(&name);
        }
    }
    sanitize(&dirs.data.to_string_lossy())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

pub fn derive_rule(
    tool: &str,
    args: &str,
    scope: GrantScope,
    at: DateTime<Utc>,
    grants: &Grants,
) -> Option<CommandRule> {
    let command = command_of(tool, args)?;
    Some(CommandRule {
        id: grants.rule_id(scope, at),
        pattern: command,
        effect: Effect::Allow,
        require_static_args: true,
        allow_substitutions: false,
    })
}

pub fn grant_preview(tool: &str, args: &str) -> Option<String> {
    command_of(tool, args)
}

pub fn derive_rule_shape(tool: &str, args: &str) -> bool {
    command_of(tool, args).is_some()
}

fn command_of(tool: &str, args: &str) -> Option<String> {
    if tool != "shell" {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    let command = parsed.get("command")?.as_str()?.trim();
    if command.is_empty() {
        return None;
    }
    let compound = ["&&", "||", ";", "|"]
        .iter()
        .any(|sep| command.contains(sep));
    if compound {
        return None;
    }
    Some(command.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Grants, SessionId, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let grants = Grants::new(dirs);
        (tmp, grants, SessionId::new(), root)
    }

    fn rule(grants: &Grants, command: &str, scope: GrantScope) -> CommandRule {
        derive_rule(
            "shell",
            &serde_json::json!({ "command": command }).to_string(),
            scope,
            Utc::now(),
            grants,
        )
        .expect("shell must yield a rule")
    }

    #[test]
    fn each_durable_scope_writes_its_own_layer() {
        let (tmp, grants, session, root) = setup();

        for (scope, command) in [
            (GrantScope::Global, "git status"),
            (GrantScope::Workspace, "cargo test"),
            (GrantScope::Session, "ls -la"),
        ] {
            let at = grants
                .record(scope, session, &root, rule(&grants, command, scope))
                .unwrap();
            assert!(at.exists(), "the file for {scope:?} was not written");
        }

        assert!(
            grants
                .session_path(session)
                .starts_with(tmp.path().join("state")),
            "{}",
            grants.session_path(session).display()
        );
        assert!(grants.global_path().starts_with(tmp.path().join("config")));
        assert_eq!(
            grants.workspace_path(&root),
            root.join(".zlogic/grants.yaml")
        );

        let (merged, problems) = grants.load(session, &root);
        assert!(problems.is_empty(), "{problems:?}");
        let patterns: Vec<&str> = merged.commands.iter().map(|c| c.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["git status", "cargo test", "ls -la"],
            "global → workspace → session"
        );
    }

    #[test]
    fn what_is_written_round_trips_through_zlogic_policy() {
        let (_tmp, grants, session, root) = setup();
        grants
            .record(
                GrantScope::Session,
                session,
                &root,
                rule(&grants, "cargo build --release", GrantScope::Session),
            )
            .unwrap();

        let text = std::fs::read_to_string(grants.session_path(session)).unwrap();
        assert!(text.starts_with("policy:\n  version: 1\n"), "{text}");
        let reloaded = Policy::from_yaml(&text).unwrap();
        assert_eq!(reloaded.commands[0].pattern, "cargo build --release");
        assert_eq!(reloaded.commands[0].effect, Effect::Allow);
    }

    #[test]
    fn a_rule_carries_where_it_came_from() {
        let (_tmp, grants, _session, _root) = setup();
        let r = rule(&grants, "git status", GrantScope::Workspace);
        assert!(r.id.starts_with("grant:workspace:"), "{}", r.id);
        assert!(r.id.contains("machine:"), "{}", r.id);
        assert!(
            r.id.contains('T') && r.id.contains('Z'),
            "carries a timestamp: {}",
            r.id
        );
    }

    #[test]
    fn approving_the_same_thing_twice_writes_one_rule() {
        let (_tmp, grants, session, root) = setup();
        for _ in 0..3 {
            grants
                .record(
                    GrantScope::Session,
                    session,
                    &root,
                    rule(&grants, "git status", GrantScope::Session),
                )
                .unwrap();
        }
        assert_eq!(grants.load(session, &root).0.commands.len(), 1);
    }

    #[test]
    fn a_project_grant_from_another_machine_is_ignored() {
        let (_tmp, grants, session, root) = setup();
        let mine = rule(&grants, "cargo test", GrantScope::Workspace);
        let mut theirs = mine.clone();
        theirs.id = "grant:workspace:2026-01-01T00:00:00Z:machine:someone-else".into();
        theirs.pattern = "curl evil.test".into();

        let mut policy = Policy::default();
        policy.commands.extend([mine, theirs]);
        std::fs::create_dir_all(root.join(".zlogic")).unwrap();
        std::fs::write(root.join(".zlogic/grants.yaml"), policy.to_yaml()).unwrap();

        let (merged, problems) = grants.load(session, &root);
        let patterns: Vec<&str> = merged.commands.iter().map(|c| c.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["cargo test"],
            "only what this machine approved is kept"
        );
        assert!(problems[0].contains("another machine"), "{problems:?}");
    }

    #[test]
    fn a_hand_written_rule_in_the_grants_file_is_kept() {
        let (_tmp, grants, session, root) = setup();
        let mut policy = Policy::default();
        policy.commands.push(CommandRule {
            id: "mine:allow-fmt".into(),
            pattern: "cargo fmt".into(),
            effect: Effect::Allow,
            require_static_args: true,
            allow_substitutions: false,
        });
        std::fs::create_dir_all(root.join(".zlogic")).unwrap();
        std::fs::write(root.join(".zlogic/grants.yaml"), policy.to_yaml()).unwrap();

        let (merged, problems) = grants.load(session, &root);
        assert_eq!(merged.commands.len(), 1);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_broken_grants_file_is_reported_not_fatal() {
        let (_tmp, grants, session, root) = setup();
        std::fs::create_dir_all(grants.global_path().parent().unwrap()).unwrap();
        std::fs::write(grants.global_path(), "policy: { version: 9 }\n").unwrap();

        let (merged, problems) = grants.load(session, &root);
        assert!(merged.commands.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("cannot read"), "{problems:?}");
    }

    #[test]
    fn the_grant_files_are_protected_from_writes() {
        let (_tmp, grants, session, root) = setup();
        let protected = grants.protected_paths(session, &root);
        assert!(protected.contains(&grants.global_path()));
        assert!(protected.contains(&root.join(".zlogic/grants.yaml")));
        assert!(protected.contains(&grants.session_path(session)));
    }

    #[test]
    fn the_pattern_is_the_command_itself_not_a_widened_form() {
        let (_tmp, grants, _s, _r) = setup();
        let r = rule(&grants, "git status --short", GrantScope::Session);
        assert_eq!(r.pattern, "git status --short");
        assert!(
            !r.pattern.contains('*'),
            "the system does not guess a wider form on the user's behalf: {}",
            r.pattern
        );
        assert!(
            r.require_static_args,
            "a wildcard only accepts static words by default"
        );
        assert!(!r.allow_substitutions, "`$(…)` is not accepted by default");
    }

    #[test]
    fn only_shell_yields_a_rule() {
        let (_tmp, grants, _s, _r) = setup();
        let at = Utc::now();
        assert!(
            derive_rule(
                "web_fetch",
                r#"{"url":"https://x"}"#,
                GrantScope::Session,
                at,
                &grants
            )
            .is_none()
        );
        assert!(
            derive_rule(
                "write_file",
                r#"{"path":"a"}"#,
                GrantScope::Session,
                at,
                &grants
            )
            .is_none()
        );
        assert!(derive_rule("shell", "not json", GrantScope::Session, at, &grants).is_none());
        assert!(
            derive_rule(
                "shell",
                r#"{"command":"  "}"#,
                GrantScope::Session,
                at,
                &grants
            )
            .is_none()
        );
    }

    #[test]
    fn a_compound_command_yields_no_rule() {
        let (_tmp, grants, _s, _r) = setup();
        let at = Utc::now();
        for command in ["a && b", "a || b", "a; b", "a | b"] {
            let args = serde_json::json!({ "command": command }).to_string();
            assert!(
                derive_rule("shell", &args, GrantScope::Session, at, &grants).is_none(),
                "{command} must not yield a rule"
            );
        }
    }
}
