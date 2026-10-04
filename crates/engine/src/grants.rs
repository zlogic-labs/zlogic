//! ```text
//! ```

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use zlogic_config::Dirs;
use zlogic_policy::{CommandRule, ComputerRule, ComputerTarget, Effect, Policy};
use zlogic_protocol::SessionId;
use zlogic_protocol::interaction::GrantScope;
use zlogic_tools::ComputerFacts;

pub const GRANTS_FILE: &str = "grants.yaml";

pub const GRANT_ID_PREFIX: &str = "grant";

pub const MACHINE_TAG_PREFIX: &str = "machine:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEntry {
    pub scope: GrantScope,
    pub id: String,
    /// A shell command, or — for a `computer` grant — the window it was approved for. Whatever a
    /// listing shows, it has to be enough for a user to recognise a grant they no longer want, so
    /// the computer form is phrased rather than encoded.
    pub pattern: String,
    pub allow: bool,
    pub file: PathBuf,
}

/// What the user approved, in the shape it is written back out in.
///
/// The two families cannot be one type: a command rule is a pattern the evaluator matches against a
/// command line, while a computer rule is a target the evaluator matches against a window. Keeping
/// them apart is what stops a computer grant from being smuggled through the `commands` list,
/// where it would silently never match.
#[derive(Debug, Clone)]
pub enum GrantRule {
    Command(CommandRule),
    Computer(ComputerRule),
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
                    merged.computer.extend(kept.computer);
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
        rule: GrantRule,
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
        let already = match &rule {
            GrantRule::Command(rule) => policy
                .commands
                .iter()
                .any(|existing| existing.pattern == rule.pattern && existing.effect == rule.effect),
            GrantRule::Computer(rule) => policy.computer.iter().any(|existing| {
                existing.actions == rule.actions
                    && existing.process_contains == rule.process_contains
                    && existing.title_contains == rule.title_contains
                    && existing.effect == rule.effect
            }),
        };
        if already {
            return Ok(path);
        }
        match rule {
            GrantRule::Command(rule) => policy.commands.push(rule),
            GrantRule::Computer(rule) => policy.computer.push(rule),
        }
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
            for rule in policy.computer {
                out.push(GrantEntry {
                    scope,
                    id: rule.id.clone(),
                    pattern: describe_computer_grant(&rule),
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
            let before = policy.commands.len() + policy.computer.len();
            policy.commands.retain(|rule| rule.id != id);
            policy.computer.retain(|rule| rule.id != id);
            if policy.commands.len() + policy.computer.len() == before {
                continue;
            }
            let written = match policy.commands.is_empty()
                && policy.computer.is_empty()
                && policy.paths.is_empty()
            {
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
        let keep = |id: &str| -> bool {
            let is_grant = id.starts_with(&format!("{GRANT_ID_PREFIX}:"));
            is_grant && !id.ends_with(&mine)
        };
        policy.commands.retain(|rule| {
            let drop = keep(&rule.id);
            if drop {
                dropped.push(format!(
                    "project grant {:?} was approved on another machine and was ignored ({})",
                    rule.pattern, rule.id
                ));
            }
            !drop
        });
        policy.computer.retain(|rule| {
            let drop = keep(&rule.id);
            if drop {
                dropped.push(format!(
                    "project computer grant {} was approved on another machine and was ignored ({})",
                    describe_computer_grant(rule),
                    rule.id
                ));
            }
            !drop
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
) -> Option<GrantRule> {
    let command = command_of(tool, args)?;
    Some(GrantRule::Command(CommandRule {
        id: grants.rule_id(scope, at),
        pattern: command,
        effect: Effect::Allow,
        require_static_args: true,
        allow_substitutions: false,
    }))
}

/// A `computer` call, turned into the rule that would have allowed it.
///
/// The shape is *target-scoped, not argument-scoped*, and that is the whole point. `git status`
/// can be granted as a command because the command **is** the thing being approved. A click is
/// not: what the user is approving is "in this window", and the coordinates change every time. So
/// the rule names the action and the window, and nothing about the click itself.
///
/// Which means a grant here is broader than it may look. `actions: [mouse]` against a title
/// allows *every* click in any window whose title contains that string, forever, for that action —
/// so the caller decides whether an action is grantable at all, and `type` is not (see
/// [`grantable_computer_action`]).
pub fn derive_computer_rule(
    tool: &str,
    _args: &str,
    scope: GrantScope,
    at: DateTime<Utc>,
    grants: &Grants,
    facts: &ComputerFacts,
) -> Option<GrantRule> {
    if tool != COMPUTER_TOOL {
        return None;
    }
    if !grantable_computer_action(&facts.action) {
        return None;
    }
    // A rule with no matcher would allow the action against *every* window on the machine, which
    // is the opposite of what approving one click meant. Unidentified targets get no durable
    // scope offered, and this is the belt to that braces.
    let target = ComputerTarget {
        process: facts.process.clone(),
        title: facts.window.clone(),
    };
    if target.process.is_none() && target.title.is_none() {
        return None;
    }
    let (process_contains, title_contains) = match (&target.process, &target.title) {
        (Some(process), _) => (Some(process.clone()), None),
        (None, Some(title)) => (None, Some(title.clone())),
        (None, None) => (None, None),
    };
    Some(GrantRule::Computer(ComputerRule {
        id: grants.rule_id(scope, at),
        actions: vec![facts.action.clone()],
        process_contains,
        title_contains,
        effect: Effect::Allow,
        message: None,
    }))
}

/// Whether an action is one a user can durably approve.
///
/// `type` is excluded, and it is the only exclusion. The rule never sees the text, so a `type`
/// grant is not a grant to send *that* text — it is a grant to type **anything**, into any window
/// whose title matches, for the rest of the session. A user who wanted to allow pasting one
/// address is not asking for that, and the prompt cannot say it in a line. Everything else is
/// either a click or a screenshot, and a click is exactly what a target-scoped rule means.
pub fn grantable_computer_action(action: &str) -> bool {
    !action.is_empty() && action != "type"
}

/// A grant is only offered where the rules can be matched back. `workspace` and `global` are not
/// offered for a computer action even though `record` could write them: a rule that says "click in
/// Notepad" is a promise about this machine's windows, and a project directory or a global config
/// file is the wrong place to keep a promise about the desktop.
pub const COMPUTER_GRANT_SCOPES: [GrantScope; 2] = [GrantScope::Once, GrantScope::Session];

/// The desktop tool's name, spelled out here because the gate has to recognise it before the tool
/// runs and cannot depend on the crate that implements it. The tool side has the same literal; the
/// two drifting apart would mean a tool nobody gates.
pub const COMPUTER_TOOL: &str = "computer_use";

fn describe_computer_grant(rule: &ComputerRule) -> String {
    let actions = rule.actions.join(", ");
    match (&rule.process_contains, &rule.title_contains) {
        (Some(process), _) => format!("computer: {actions} in {process}"),
        (None, Some(title)) => format!("computer: {actions} in a window titled {title:?}"),
        (None, None) => format!("computer: {actions} (no target)"),
    }
}

/// Whether a durable `computer` grant could be derived, without deriving it. The prompt needs this
/// to decide which buttons to show, and it must agree with [`derive_computer_rule`] exactly — a
/// scope that is offered and then refused is the bug this pair exists to prevent.
pub fn derive_computer_rule_shape(tool: &str, _args: &str, facts: &ComputerFacts) -> bool {
    tool == COMPUTER_TOOL
        && grantable_computer_action(&facts.action)
        && (facts.process.is_some() || facts.window.is_some())
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

    fn rule(grants: &Grants, command: &str, scope: GrantScope) -> GrantRule {
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
        let GrantRule::Command(r) = rule(&grants, "git status", GrantScope::Workspace) else {
            panic!("a shell command must yield a command grant");
        };
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
        let GrantRule::Command(mine) = rule(&grants, "cargo test", GrantScope::Workspace) else {
            panic!("a shell command must yield a command grant");
        };
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

    fn computer_facts(action: &str, process: Option<&str>, window: Option<&str>) -> ComputerFacts {
        ComputerFacts {
            action: action.to_string(),
            process: process.map(str::to_string),
            window: window.map(str::to_string),
            ..ComputerFacts::default()
        }
    }

    fn computer_rule(grants: &Grants, facts: &ComputerFacts, scope: GrantScope) -> GrantRule {
        derive_computer_rule(COMPUTER_TOOL, "{}", scope, Utc::now(), grants, facts)
            .expect("a computer grant must be derivable")
    }

    #[test]
    fn a_computer_grant_is_written_and_read_back_as_a_computer_rule() {
        let (_tmp, grants, session, root) = setup();
        let facts = computer_facts("mouse", Some("notepad.exe"), Some("Untitled - Notepad"));
        grants
            .record(
                GrantScope::Session,
                session,
                &root,
                computer_rule(&grants, &facts, GrantScope::Session),
            )
            .unwrap();

        let (merged, problems) = grants.load(session, &root);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(merged.computer.len(), 1, "{:?}", merged.computer);
        let rule = &merged.computer[0];
        assert_eq!(rule.actions, ["mouse"]);
        assert_eq!(rule.process_contains.as_deref(), Some("notepad.exe"));
        assert_eq!(
            rule.title_contains, None,
            "the process is the sharper matcher"
        );
        assert_eq!(rule.effect, Effect::Allow);

        let decision = merged
            .evaluate_computer(
                "mouse",
                &ComputerTarget {
                    process: Some("notepad.exe".into()),
                    title: Some("notes.txt - Notepad".into()),
                },
            )
            .unwrap();
        assert_eq!(
            decision.effect,
            Effect::Allow,
            "a retitled window is still the same application, which is the whole point of matching \
             on the process: {decision:?}"
        );
    }

    #[test]
    fn the_same_computer_grant_twice_writes_one_rule() {
        let (_tmp, grants, session, root) = setup();
        let facts = computer_facts("mouse", Some("notepad.exe"), None);
        for _ in 0..3 {
            grants
                .record(
                    GrantScope::Session,
                    session,
                    &root,
                    computer_rule(&grants, &facts, GrantScope::Session),
                )
                .unwrap();
        }
        assert_eq!(grants.load(session, &root).0.computer.len(), 1);
    }

    #[test]
    fn typing_is_not_grantable_but_a_click_is() {
        let (_tmp, grants, _s, _r) = setup();
        let target = Some("vault.exe");
        assert!(
            !grantable_computer_action("type"),
            "a type grant is a grant to type anything, into any window that matches, for the rest \
             of the session — which is not what allowing one paste of an address means"
        );
        for action in [
            "mouse",
            "scroll",
            "key",
            "screenshot",
            "inspect",
            "state",
            "wait",
        ] {
            assert!(grantable_computer_action(action), "{action}");
        }
        assert!(!grantable_computer_action(""));
        assert!(
            derive_computer_rule(
                COMPUTER_TOOL,
                "{}",
                GrantScope::Session,
                Utc::now(),
                &grants,
                &computer_facts("type", target, None),
            )
            .is_none()
        );
    }

    #[test]
    fn an_unidentified_target_gets_no_durable_rule_at_all() {
        let (_tmp, grants, _s, _r) = setup();
        // A rule with no matcher would allow the action against every window on the machine, which
        // is the opposite of what approving one click on a canvas meant.
        let facts = computer_facts("mouse", None, None);
        assert!(!derive_computer_rule_shape(COMPUTER_TOOL, "{}", &facts));
        assert!(
            derive_computer_rule(
                COMPUTER_TOOL,
                "{}",
                GrantScope::Session,
                Utc::now(),
                &grants,
                &facts,
            )
            .is_none()
        );
    }

    #[test]
    fn a_window_title_is_enough_when_the_process_could_not_be_read() {
        let (_tmp, grants, _s, _r) = setup();
        let GrantRule::Computer(rule) = derive_computer_rule(
            COMPUTER_TOOL,
            "{}",
            GrantScope::Session,
            Utc::now(),
            &grants,
            &computer_facts("mouse", None, Some("Unlock")),
        )
        .expect("a title is a matcher") else {
            panic!("a computer call must yield a computer grant");
        };
        assert_eq!(rule.process_contains, None);
        assert_eq!(rule.title_contains.as_deref(), Some("Unlock"));
    }

    #[test]
    fn what_is_offered_and_what_is_derivable_agree() {
        // A scope the prompt offers and the recorder then refuses is the bug this pair exists to
        // prevent: the user picks "this session", nothing is written, and the next call asks again.
        let cases = [
            computer_facts("mouse", Some("notepad.exe"), None),
            computer_facts("mouse", None, Some("Unlock")),
            computer_facts("type", Some("notepad.exe"), None),
            computer_facts("mouse", None, None),
        ];
        for facts in cases {
            let offered = derive_computer_rule_shape(COMPUTER_TOOL, "{}", &facts);
            let derived = derive_computer_rule(
                COMPUTER_TOOL,
                "{}",
                GrantScope::Session,
                Utc::now(),
                &crate::grants::Grants::new(zlogic_config::Dirs::under(
                    std::env::temp_dir().join("zlogic-grant-shape"),
                )),
                &facts,
            )
            .is_some();
            assert_eq!(
                offered, derived,
                "{facts:?}: the button and the writer must agree"
            );
        }
    }

    #[test]
    fn a_computer_grant_from_another_machine_is_ignored_like_a_command_one() {
        let (_tmp, grants, session, root) = setup();
        let facts = computer_facts("mouse", Some("notepad.exe"), None);
        let GrantRule::Computer(mine) = computer_rule(&grants, &facts, GrantScope::Workspace)
        else {
            panic!("a computer call must yield a computer grant");
        };
        let mut theirs = mine.clone();
        theirs.id = "grant:workspace:2026-01-01T00:00:00Z:machine:someone-else".into();
        theirs.process_contains = Some("attacker.exe".into());

        let mut policy = Policy::default();
        policy.computer.extend([mine, theirs]);
        std::fs::create_dir_all(root.join(".zlogic")).unwrap();
        std::fs::write(root.join(".zlogic/grants.yaml"), policy.to_yaml()).unwrap();

        let (merged, problems) = grants.load(session, &root);
        assert_eq!(merged.computer.len(), 1);
        assert_eq!(
            merged.computer[0].process_contains.as_deref(),
            Some("notepad.exe"),
            "only what this machine approved is kept"
        );
        assert!(problems[0].contains("another machine"), "{problems:?}");
    }

    #[test]
    fn a_computer_grant_can_be_listed_and_revoked() {
        let (_tmp, grants, session, root) = setup();
        let facts = computer_facts("mouse", Some("notepad.exe"), None);
        grants
            .record(
                GrantScope::Session,
                session,
                &root,
                computer_rule(&grants, &facts, GrantScope::Session),
            )
            .unwrap();

        let listed = grants.list(Some(session), Some(&root));
        let entry = listed
            .iter()
            .find(|entry| entry.pattern.starts_with("computer:"))
            .expect("a computer grant is listed like any other");
        assert!(entry.pattern.contains("notepad.exe"), "{}", entry.pattern);
        assert!(entry.allow);

        assert!(
            grants
                .revoke(&entry.id, Some(session), Some(&root))
                .unwrap()
        );
        assert!(
            grants.load(session, &root).0.computer.is_empty(),
            "and a revoked grant stops applying"
        );
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
        let GrantRule::Command(r) = rule(&grants, "git status --short", GrantScope::Session) else {
            panic!("a shell command must yield a command grant");
        };
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
