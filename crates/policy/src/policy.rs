//! Decision layer for normalized command operations.
//! Rules are deliberately data-only and std-only. Configuration loaders
//! (YAML/JSON, merged project policy, grants and audit persistence) can live
//! above this crate and compile their input into [`Policy`].

use std::path::{Path, PathBuf};

use crate::command_rule::{self, CommandRule};
use crate::path::physical_resolve;
use crate::{Access, Dialect, Op, Zone, decompose};
use zlogic_paths::floor_normalize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRule {
    pub id: String,
    pub head: String,
    /// Exact argv after argv[0]. `*` matches one argument.
    pub args: Option<Vec<String>>,
    /// Prefix of argv after argv[0]. `*` matches one argument.
    pub args_prefix: Option<Vec<String>>,
    pub effect: Effect,
    /// Dynamic argv must not receive a prefix/exact allow by default.
    #[serde(default = "default_true")]
    pub require_static_args: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub id: String,
    pub zone: Option<Zone>,
    /// Glob matched against the normalized absolute path. `*` does not cross
    /// a separator; `**` does. `?` matches one non-separator character.
    pub glob: Option<String>,
    pub access: Vec<Access>,
    pub effect: Effect,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptRule {
    pub id: String,
    pub zone: Option<Zone>,
    pub glob: Option<String>,
    pub effect: Effect,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Must be Ask or Deny. A default-Allow policy defeats fail-closed
    /// handling when a newly introduced Op has no rule yet.
    pub default: Effect,
    /// User-tier command rules (see [`crate::command_rule`]). Matched per
    /// simple command via [`evaluate_command`] (POSIX only) BEFORE op-level
    /// evaluation; when every command in the input matches, the decision is
    /// terminal and op-level uncertainty floors are skipped — only the
    /// protected_delete / protected_write floor still applies. Callers going
    /// through [`Policy::evaluate_ops`] directly never see these.
    #[serde(default)]
    pub commands: Vec<CommandRule>,
    #[serde(default)]
    pub exec: Vec<ExecRule>,
    #[serde(default)]
    pub paths: Vec<PathRule>,
    #[serde(default)]
    pub scripts: Vec<ScriptRule>,
    /// Concrete directories that must survive deletion. A Delete is denied
    /// when its target is the protected path, is inside it, or is an ancestor
    /// that would recursively remove it.
    #[serde(default)]
    pub protected_delete: Vec<PathBuf>,
    /// Policy-integrity floor: files whose write (or delete) can never be
    /// auto-approved by ANY rule, command rules included — forced Ask. Meant
    /// for the policy file itself: "user rules always pass" is only sound if
    /// rules are actually user-written, so an agent must not be able to edit
    /// the policy and then benefit from the edit without a human approving.
    #[serde(default)]
    pub protected_write: Vec<PathBuf>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            default: Effect::Ask,
            commands: Vec::new(),
            exec: Vec::new(),
            paths: Vec::new(),
            scripts: Vec::new(),
            protected_delete: Vec::new(),
            protected_write: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpDecision {
    pub effect: Effect,
    pub matched_rule_ids: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub effect: Effect,
    pub ops: Vec<Op>,
    pub op_decisions: Vec<OpDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(pub String);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

impl Policy {
    /// Parse the documented `policy: { version: 1, ... }` YAML format.
    pub fn from_yaml(input: &str) -> Result<Self, PolicyError> {
        let document: PolicyDocument = serde_yaml_ng::from_str(input)
            .map_err(|e| PolicyError(format!("invalid policy YAML: {e}")))?;
        if document.policy.version != 1 {
            return Err(PolicyError(format!(
                "unsupported policy version {} (expected 1)",
                document.policy.version
            )));
        }
        let policy = document.policy.policy;
        policy.validate()?;
        Ok(policy)
    }

    pub fn from_yaml_file(path: &Path) -> Result<Self, PolicyError> {
        let input = std::fs::read_to_string(path)
            .map_err(|e| PolicyError(format!("failed to read {}: {e}", path.display())))?;
        Self::from_yaml(&input)
    }

    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.default == Effect::Allow {
            return Err(PolicyError("policy default must be ask or deny".into()));
        }
        for r in &self.exec {
            if r.id.is_empty() || r.head.is_empty() {
                return Err(PolicyError(
                    "exec rule id and head must not be empty".into(),
                ));
            }
            if r.args.is_some() && r.args_prefix.is_some() {
                return Err(PolicyError(format!(
                    "exec rule '{}' cannot set both args and args_prefix",
                    r.id
                )));
            }
            if r.effect == Effect::Allow && is_priv_escalation_head(&r.head) {
                return Err(PolicyError(format!(
                    "exec rule '{}': {} cannot be allowed by an exec rule; use a command rule \
                     with an explicit pattern instead",
                    r.id, r.head
                )));
            }
        }
        for r in &self.commands {
            if r.id.is_empty() {
                return Err(PolicyError("command rule id must not be empty".into()));
            }
            if let Err(e) = command_rule::compile_pattern(&r.pattern) {
                return Err(PolicyError(format!("command rule '{}': {e}", r.id)));
            }
        }
        for r in &self.paths {
            validate_path_selector(&r.id, r.zone, r.glob.as_deref())?;
            if r.access.is_empty() {
                return Err(PolicyError(format!(
                    "path rule '{}' has no access values",
                    r.id
                )));
            }
            if r.effect == Effect::Allow && r.zone == Some(Zone::Sensitive) {
                return Err(PolicyError(format!(
                    "path rule '{}' cannot allow the sensitive zone",
                    r.id
                )));
            }
            if r.effect == Effect::Allow && r.zone == Some(Zone::Unresolved) {
                return Err(PolicyError(format!(
                    "path rule '{}' cannot allow the unresolved zone",
                    r.id
                )));
            }
        }
        for r in &self.scripts {
            validate_path_selector(&r.id, r.zone, r.glob.as_deref())?;
            if r.effect == Effect::Allow && r.zone == Some(Zone::Sensitive) {
                return Err(PolicyError(format!(
                    "script rule '{}' cannot allow the sensitive zone",
                    r.id
                )));
            }
            if r.effect == Effect::Allow && r.zone == Some(Zone::Unresolved) {
                return Err(PolicyError(format!(
                    "script rule '{}' cannot allow the unresolved zone",
                    r.id
                )));
            }
        }
        for (field, list) in [
            ("protected_delete", &self.protected_delete),
            ("protected_write", &self.protected_write),
        ] {
            for p in list {
                if !p.is_absolute() {
                    return Err(PolicyError(format!(
                        "{field} path '{}' must be absolute",
                        p.display()
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn evaluate_ops(&self, ops: Vec<Op>) -> Result<Decision, PolicyError> {
        self.validate()?;
        let op_decisions: Vec<_> = ops.iter().map(|op| self.evaluate_op(op)).collect();
        // Deny > Ask > Allow. Empty decompositions never silently allow.
        let effect = op_decisions
            .iter()
            .map(|d| d.effect)
            .max()
            .unwrap_or(Effect::Ask);
        Ok(Decision {
            effect,
            ops,
            op_decisions,
        })
    }

    /// The non-negotiable per-op floor: protected_delete → Deny,
    /// protected_write → Ask. Applied in BOTH evaluation paths — op-level
    /// rules and terminal command-rule matches — so no rule of any tier can
    /// clear it.
    fn protected_floor(&self, op: &Op) -> Option<OpDecision> {
        let Op::Path { access, path } = op else {
            return None;
        };
        // Targets can arrive verbatim (`\\?\C:\...` from physical_resolve-based
        // resolvers) or display-form (`C:\...` from the Windows resolver/shell
        // analyzers); floor entries are normalized to display form at creation.
        // Normalize here too so the starts_with comparisons always see one form.
        let target = floor_normalize(path.resolved.as_ref()?);
        if *access == Access::Delete
            && self
                .protected_delete
                .iter()
                .any(|p| target.starts_with(p) || p.starts_with(&target))
        {
            return Some(OpDecision {
                effect: Effect::Deny,
                matched_rule_ids: vec!["builtin:protected-delete".into()],
                reason: "delete target would remove a protected directory".into(),
            });
        }
        let hits_protected_write = match access {
            Access::Write => self.protected_write.iter().any(|p| target.starts_with(p)),
            // deleting the file, its directory, or an ancestor removes it too
            Access::Delete => self
                .protected_write
                .iter()
                .any(|p| target.starts_with(p) || p.starts_with(&target)),
            Access::Read => false,
        };
        if hits_protected_write {
            return Some(OpDecision {
                effect: Effect::Ask,
                matched_rule_ids: vec!["builtin:protected-write".into()],
                reason: "target is a protected policy file; rules cannot auto-approve writing it"
                    .into(),
            });
        }
        None
    }

    fn evaluate_op(&self, op: &Op) -> OpDecision {
        let floor = self.protected_floor(op);
        if let Some(f) = &floor {
            if f.effect == Effect::Deny {
                return f.clone();
            }
        }
        let mut matches: Vec<(Effect, &str)> = Vec::new();
        match op {
            Op::Exec {
                head,
                argv,
                dynamic_args,
            } => {
                let args = argv.get(1..).unwrap_or_default();
                for r in &self.exec {
                    if r.head != "*" && !r.head.eq_ignore_ascii_case(head) {
                        continue;
                    }
                    // dynamic argv must not receive a prefix/exact ALLOW;
                    // tightening rules (deny/ask) always participate
                    if *dynamic_args && r.require_static_args && r.effect == Effect::Allow {
                        continue;
                    }
                    if r.args.as_ref().is_some_and(|p| !args_exact(p, args)) {
                        continue;
                    }
                    if r.args_prefix
                        .as_ref()
                        .is_some_and(|p| !args_prefix(p, args))
                    {
                        continue;
                    }
                    matches.push((r.effect, &r.id));
                }
            }
            Op::Path { access, path } => {
                for r in &self.paths {
                    if !r.access.contains(access) || !path_matches(r.zone, r.glob.as_deref(), path)
                    {
                        continue;
                    }
                    matches.push((r.effect, &r.id));
                }
            }
            Op::Script { path, .. } => {
                for r in &self.scripts {
                    if path_matches(r.zone, r.glob.as_deref(), path) {
                        matches.push((r.effect, &r.id));
                    }
                }
            }
            // Cwd changes use the path rules as read-like traversal checks.
            Op::CwdChange { path } => {
                for r in &self.paths {
                    if r.access.contains(&Access::Read)
                        && path_matches(r.zone, r.glob.as_deref(), path)
                    {
                        matches.push((r.effect, &r.id));
                    }
                }
            }
            Op::Unknown { .. } => {}
        }
        let mut decision = decision_from_matches(matches, self.default);
        if let Some(reason) = safety_floor(op) {
            if decision.effect < Effect::Ask {
                decision.effect = Effect::Ask;
                decision.matched_rule_ids.clear();
            }
            if decision.effect == Effect::Ask {
                decision.reason = reason.into();
            }
        }
        if let Some(f) = floor {
            if f.effect > decision.effect {
                return f;
            }
        }
        decision
    }

    /// Match user command rules against every simple command in the input.
    /// `Ok(decision)` is terminal; `Err(ops)` hands the ops back for normal
    /// op-level evaluation (no rule matched some command, input didn't parse,
    /// or the policy has no command rules).
    fn apply_command_rules(&self, command: &str, ops: Vec<Op>) -> Result<Decision, Vec<Op>> {
        if self.commands.is_empty() {
            return Err(ops);
        }
        let Some(cmds) = command_rule::parse_input(command) else {
            return Err(ops);
        };
        if cmds.is_empty() {
            return Err(ops);
        }
        // A deny match on ANY command is terminal regardless of the rest.
        for cmd in &cmds {
            for r in &self.commands {
                if r.effect == Effect::Deny && command_rule::rule_matches(r, cmd) {
                    return Ok(self.command_rule_decision(Effect::Deny, vec![r.id.clone()], ops));
                }
            }
        }
        // Otherwise every command must match at least one rule; the result is
        // the strictest effect any command settled on.
        let mut effect = Effect::Allow;
        let mut ids: Vec<String> = Vec::new();
        for cmd in &cmds {
            let matched: Vec<&CommandRule> = self
                .commands
                .iter()
                .filter(|r| command_rule::rule_matches(r, cmd))
                .collect();
            let Some(cmd_effect) = matched.iter().map(|r| r.effect).max() else {
                return Err(ops); // an unmatched command → op-level evaluation
            };
            effect = effect.max(cmd_effect);
            for r in matched {
                if r.effect == cmd_effect && !ids.contains(&r.id) {
                    ids.push(r.id.clone());
                }
            }
        }
        Ok(self.command_rule_decision(effect, ids, ops))
    }

    /// Terminal decision from a command-rule match: every op is stamped with
    /// the rule verdict, except ops the protected floor raises above it.
    fn command_rule_decision(
        &self,
        effect: Effect,
        rule_ids: Vec<String>,
        ops: Vec<Op>,
    ) -> Decision {
        let reason = format!(
            "command rule(s) matched: {} — op-level analysis skipped",
            rule_ids.join(", ")
        );
        let op_decisions: Vec<OpDecision> = ops
            .iter()
            .map(|op| match self.protected_floor(op) {
                Some(f) if f.effect > effect => f,
                _ => OpDecision {
                    effect,
                    matched_rule_ids: rule_ids.clone(),
                    reason: reason.clone(),
                },
            })
            .collect();
        let overall = op_decisions
            .iter()
            .map(|d| d.effect)
            .max()
            .unwrap_or(effect);
        Decision {
            effect: overall,
            ops,
            op_decisions,
        }
    }

    /// Render this policy in the supported YAML schema. This keeps generation
    /// std-only; parsing arbitrary YAML remains the configuration layer's job.
    pub fn to_yaml(&self) -> String {
        let mut out = String::from("policy:\n  version: 1\n");
        out.push_str(&format!("  default: {}\n", effect_yaml(self.default)));
        if !self.protected_delete.is_empty() {
            out.push_str("  protected_delete:\n");
            for p in &self.protected_delete {
                out.push_str(&format!("    - {}\n", yaml_quote(&p.to_string_lossy())));
            }
        }
        if !self.protected_write.is_empty() {
            out.push_str("  protected_write:\n");
            for p in &self.protected_write {
                out.push_str(&format!("    - {}\n", yaml_quote(&p.to_string_lossy())));
            }
        }
        if !self.commands.is_empty() {
            out.push_str("  commands:\n");
            for r in &self.commands {
                out.push_str(&format!("    - id: {}\n", yaml_quote(&r.id)));
                out.push_str(&format!("      pattern: {}\n", yaml_quote(&r.pattern)));
                out.push_str(&format!("      effect: {}\n", effect_yaml(r.effect)));
                out.push_str(&format!(
                    "      require_static_args: {}\n",
                    r.require_static_args
                ));
                out.push_str(&format!(
                    "      allow_substitutions: {}\n",
                    r.allow_substitutions
                ));
            }
        }
        if !self.exec.is_empty() {
            out.push_str("  exec:\n");
        }
        for r in &self.exec {
            out.push_str(&format!("    - id: {}\n", yaml_quote(&r.id)));
            out.push_str(&format!("      head: {}\n", yaml_quote(&r.head)));
            if let Some(args) = &r.args {
                write_yaml_list(&mut out, "args", args);
            }
            if let Some(args) = &r.args_prefix {
                write_yaml_list(&mut out, "args_prefix", args);
            }
            out.push_str(&format!("      effect: {}\n", effect_yaml(r.effect)));
            out.push_str(&format!(
                "      require_static_args: {}\n",
                r.require_static_args
            ));
        }
        if !self.paths.is_empty() {
            out.push_str("  paths:\n");
        }
        for r in &self.paths {
            out.push_str(&format!("    - id: {}\n", yaml_quote(&r.id)));
            if let Some(zone) = r.zone {
                out.push_str(&format!("      zone: {zone}\n"));
            }
            if let Some(glob) = &r.glob {
                out.push_str(&format!("      glob: {}\n", yaml_quote(glob)));
            }
            out.push_str("      access:\n");
            for access in &r.access {
                out.push_str(&format!("        - {access}\n"));
            }
            out.push_str(&format!("      effect: {}\n", effect_yaml(r.effect)));
        }
        if !self.scripts.is_empty() {
            out.push_str("  scripts:\n");
        }
        for r in &self.scripts {
            out.push_str(&format!("    - id: {}\n", yaml_quote(&r.id)));
            if let Some(zone) = r.zone {
                out.push_str(&format!("      zone: {zone}\n"));
            }
            if let Some(glob) = &r.glob {
                out.push_str(&format!("      glob: {}\n", yaml_quote(glob)));
            }
            out.push_str(&format!("      effect: {}\n", effect_yaml(r.effect)));
        }
        out
    }
}

/// The `policy.yaml` document.
/// **Unknown top-level keys are ignored on purpose**, unlike everywhere else in this crate. The
/// file is shared: it is the host's per-project "runtime rules" file, and access control is only
/// one of the sections in it (zlogic also reads a `budget:` section from it, which this crate has no
/// business knowing about — it has no cost model).
/// The strictness that matters is kept: [`VersionedPolicy`] and [`Policy`] still reject unknown
/// keys, so a typo *inside* a rule fails loudly. Only the document level is lenient, and only
/// because the alternative is either this crate learning about every section a host might add, or
/// the host parsing the same file with a second lenient struct anyway.
#[derive(serde::Deserialize)]
struct PolicyDocument {
    policy: VersionedPolicy,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedPolicy {
    version: u32,
    #[serde(flatten)]
    policy: Policy,
}

const fn default_true() -> bool {
    true
}

/// Generate the recommended workspace policy using Linux/XDG application
/// directories. Workspace reads/writes/deletes and scripts are allowed, while
/// `.git`, `~/.<app_name>` and the four XDG app directories cannot be
/// deleted, and the app's policy files cannot be auto-approved for writing.
pub fn default_workspace_policy(workspace: &Path, home: &Path, app_name: &str) -> Policy {
    let workspace = physical_resolve(workspace);
    let home = physical_resolve(home);
    let env_or = |name: &str, fallback: PathBuf| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or(fallback)
    };
    let data = env_or("XDG_DATA_HOME", home.join(".local/share")).join(app_name);
    let config = env_or("XDG_CONFIG_HOME", home.join(".config")).join(app_name);
    let cache = env_or("XDG_CACHE_HOME", home.join(".cache")).join(app_name);
    let state = env_or("XDG_STATE_HOME", home.join(".local/state")).join(app_name);
    let global_policy = config.join("policy.yaml");
    let project_policy = workspace.join(format!(".{app_name}")).join("policy.yaml");

    Policy {
        default: Effect::Ask,
        commands: Vec::new(),
        exec: vec![ExecRule {
            id: "allow-static-exec".into(),
            head: "*".into(),
            args: None,
            args_prefix: None,
            effect: Effect::Allow,
            require_static_args: true,
        }],
        paths: vec![PathRule {
            id: "allow-workspace-all".into(),
            zone: Some(Zone::Workspace),
            glob: None,
            access: vec![Access::Read, Access::Write, Access::Delete],
            effect: Effect::Allow,
        }],
        scripts: vec![ScriptRule {
            id: "allow-workspace-scripts".into(),
            zone: Some(Zone::Workspace),
            glob: None,
            effect: Effect::Allow,
        }],
        protected_delete: vec![
            workspace.join(".git"),
            home.join(format!(".{app_name}")),
            data,
            config,
            cache,
            state,
        ]
        .into_iter()
        .map(|p| floor_normalize(&physical_resolve(&p)))
        .collect(),
        protected_write: vec![global_policy, project_policy]
            .into_iter()
            .map(|p| floor_normalize(&physical_resolve(&p)))
            .collect(),
    }
}

fn effect_yaml(effect: Effect) -> &'static str {
    match effect {
        Effect::Allow => "allow",
        Effect::Ask => "ask",
        Effect::Deny => "deny",
    }
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn write_yaml_list(out: &mut String, name: &str, values: &[String]) {
    out.push_str(&format!("      {name}:\n"));
    for value in values {
        out.push_str(&format!("        - {}\n", yaml_quote(value)));
    }
}

/// Decompose and decide a command in one call. This is the entry point that
/// applies user command rules (POSIX only) — a full match short-circuits
/// op-level evaluation, subject only to the protected_delete/protected_write
/// floor.
pub fn evaluate_command(
    policy: &Policy,
    dialect: Dialect,
    command: &str,
    cwd: &str,
    workspace: &str,
    home: &str,
) -> Result<Decision, PolicyError> {
    policy.validate()?;
    let ops = decompose(dialect, command, cwd, workspace, home);
    let ops = if dialect == Dialect::Posix {
        match policy.apply_command_rules(command, ops) {
            Ok(decision) => return Ok(decision),
            Err(ops) => ops,
        }
    } else {
        ops
    };
    policy.evaluate_ops(ops)
}

fn validate_path_selector(
    id: &str,
    zone: Option<Zone>,
    glob: Option<&str>,
) -> Result<(), PolicyError> {
    if id.is_empty() {
        return Err(PolicyError("rule id must not be empty".into()));
    }
    if zone.is_none() && glob.is_none() {
        return Err(PolicyError(format!("rule '{id}' needs a zone or glob")));
    }
    Ok(())
}

fn is_priv_escalation_head(head: &str) -> bool {
    ["sudo", "doas", "su"]
        .iter()
        .any(|h| head.eq_ignore_ascii_case(h))
}

fn safety_floor(op: &Op) -> Option<&'static str> {
    match op {
        Op::Unknown { .. } => Some("unknown operation always requires approval"),
        Op::Exec { head, .. } if is_priv_escalation_head(head) => {
            Some("privilege escalation always requires approval")
        }
        Op::Path {
            access: Access::Write | Access::Delete,
            path,
        } if path.zone == Zone::Unresolved => {
            Some("unresolved write/delete target requires approval")
        }
        Op::Path { path, .. } | Op::Script { path, .. } | Op::CwdChange { path }
            if path.zone == Zone::Sensitive =>
        {
            Some("sensitive path requires approval")
        }
        Op::Script { path, .. } if path.resolved.is_none() => {
            Some("unresolved script requires approval")
        }
        _ => None,
    }
}

fn decision_from_matches(matches: Vec<(Effect, &str)>, default: Effect) -> OpDecision {
    let effect = matches.iter().map(|(e, _)| *e).max().unwrap_or(default);
    let matched_rule_ids = matches
        .into_iter()
        .filter(|(e, _)| *e == effect)
        .map(|(_, id)| id.to_string())
        .collect::<Vec<_>>();
    let reason = if matched_rule_ids.is_empty() {
        format!("no rule matched; using default {effect:?}")
    } else {
        format!(
            "matched {} rule(s): {}",
            matched_rule_ids.len(),
            matched_rule_ids.join(", ")
        )
    };
    OpDecision {
        effect,
        matched_rule_ids,
        reason,
    }
}

fn token_matches(pattern: &str, value: &str) -> bool {
    pattern == "*" || pattern == value
}

fn args_exact(pattern: &[String], args: &[String]) -> bool {
    pattern.len() == args.len() && pattern.iter().zip(args).all(|(p, a)| token_matches(p, a))
}

fn args_prefix(pattern: &[String], args: &[String]) -> bool {
    pattern.len() <= args.len() && pattern.iter().zip(args).all(|(p, a)| token_matches(p, a))
}

fn path_matches(zone: Option<Zone>, pattern: Option<&str>, path: &crate::ResolvedPath) -> bool {
    if zone.is_some_and(|z| z != path.zone) {
        return false;
    }
    let Some(pattern) = pattern else { return true };
    let Some(resolved) = &path.resolved else {
        return false;
    };
    let value = resolved.to_string_lossy().replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    let windows = value.as_bytes().get(1) == Some(&b':');
    if windows {
        glob_matches(&pattern.to_lowercase(), &value.to_lowercase())
    } else {
        glob_matches(&pattern, &value)
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    fn walk(p: &[u8], v: &[u8]) -> bool {
        if p.is_empty() {
            return v.is_empty();
        }
        if p.starts_with(b"**") {
            let rest = &p[2..];
            return (0..=v.len()).any(|n| walk(rest, &v[n..]));
        }
        if p[0] == b'*' {
            let max = v.iter().position(|c| *c == b'/').unwrap_or(v.len());
            return (0..=max).any(|n| walk(&p[1..], &v[n..]));
        }
        if p[0] == b'?' {
            return !v.is_empty() && v[0] != b'/' && walk(&p[1..], &v[1..]);
        }
        !v.is_empty() && p[0] == v[0] && walk(&p[1..], &v[1..])
    }
    walk(pattern.as_bytes(), value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    fn abs(p: &str) -> String {
        p.to_string()
    }

    #[cfg(windows)]
    fn abs(p: &str) -> String {
        format!("C:{p}")
    }

    fn allow_workspace_policy() -> Policy {
        Policy {
            default: Effect::Ask,
            exec: vec![ExecRule {
                id: "allow-rm".into(),
                head: "rm".into(),
                args: None,
                args_prefix: None,
                effect: Effect::Allow,
                require_static_args: true,
            }],
            paths: vec![PathRule {
                id: "allow-workspace-delete".into(),
                zone: Some(Zone::Workspace),
                glob: None,
                access: vec![Access::Delete],
                effect: Effect::Allow,
            }],
            scripts: Vec::new(),
            ..Policy::default()
        }
    }

    #[test]
    fn all_ops_must_allow() {
        let d = evaluate_command(
            &allow_workspace_policy(),
            Dialect::Posix,
            "rm build.log",
            "/ws",
            "/ws",
            "/home/u",
        )
        .unwrap();
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn deny_beats_allow_independent_of_order() {
        let mut p = allow_workspace_policy();
        p.paths.push(PathRule {
            id: "deny-log-delete".into(),
            zone: None,
            glob: Some(format!("{}/*.log", abs("/ws"))).into(),
            access: vec![Access::Delete],
            effect: Effect::Deny,
        });
        let d = evaluate_command(
            &p,
            Dialect::Posix,
            "rm build.log",
            &abs("/ws"),
            &abs("/ws"),
            &abs("/home/u"),
        )
        .unwrap();
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.op_decisions[1].matched_rule_ids, vec!["deny-log-delete"]);
    }

    #[test]
    fn uncertainty_and_sensitive_floor_cannot_be_allowed() {
        let p = allow_workspace_policy();
        let d =
            evaluate_command(&p, Dialect::Posix, "rm $TARGET", "/ws", "/ws", "/home/u").unwrap();
        assert_eq!(d.effect, Effect::Ask);

        let d = evaluate_command(
            &allow_workspace_policy(),
            Dialect::Posix,
            "rm /home/u/.ssh/id_rsa",
            "/ws",
            "/ws",
            "/home/u",
        )
        .unwrap();
        assert_eq!(d.effect, Effect::Ask);
    }

    #[test]
    fn explicit_deny_beats_sensitive_floor() {
        let mut p = allow_workspace_policy();
        p.paths.push(PathRule {
            id: "deny-secrets".into(),
            zone: Some(Zone::Sensitive),
            glob: None,
            access: vec![Access::Delete],
            effect: Effect::Deny,
        });
        let d = evaluate_command(
            &p,
            Dialect::Posix,
            "rm /home/u/.ssh/id_rsa",
            "/ws",
            "/ws",
            "/home/u",
        )
        .unwrap();
        assert_eq!(d.effect, Effect::Deny);
    }

    #[test]
    fn dynamic_args_do_not_match_static_exec_allow() {
        let d = evaluate_command(
            &allow_workspace_policy(),
            Dialect::Posix,
            "rm $TARGET",
            "/ws",
            "/ws",
            "/home/u",
        )
        .unwrap();
        assert_eq!(d.op_decisions[0].effect, Effect::Ask);
    }

    #[test]
    fn invalid_default_allow_is_rejected() {
        let p = Policy {
            default: Effect::Allow,
            ..Policy::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn explicit_unresolved_allow_is_rejected() {
        let mut p = Policy::default();
        p.paths.push(PathRule {
            id: "unsafe".into(),
            zone: Some(Zone::Unresolved),
            glob: None,
            access: vec![Access::Write],
            effect: Effect::Allow,
        });
        assert!(p.validate().is_err());
    }

    #[test]
    fn generated_policy_allows_workspace_but_protects_git_and_xdg() {
        let p = default_workspace_policy(Path::new(&abs("/ws")), Path::new(&abs("/home/u")), "cli");
        let allowed = evaluate_command(
            &p,
            Dialect::Posix,
            "rm build",
            &abs("/ws"),
            &abs("/ws"),
            &abs("/home/u"),
        )
        .unwrap();
        assert_eq!(allowed.effect, Effect::Allow);

        let xdg = format!(
            "rm -rf {}",
            p.protected_delete[3]
                .display()
                .to_string()
                .replace('\\', "/")
        );
        for command in ["rm -rf .git", "rm -rf /ws", &xdg] {
            let denied = evaluate_command(
                &p,
                Dialect::Posix,
                command,
                &abs("/ws"),
                &abs("/ws"),
                &abs("/home/u"),
            )
            .unwrap();
            assert_eq!(denied.effect, Effect::Deny, "{command}");
        }
        let yaml = p.to_yaml();
        assert!(yaml.contains("protected_delete:"));
        let loaded = Policy::from_yaml(&yaml).unwrap();
        assert_eq!(loaded.protected_delete, p.protected_delete);
    }

    #[test]
    fn to_yaml_round_trips_even_with_empty_sections() {
        let mut p = Policy::default();
        p.commands.push(CommandRule {
            id: "grant:session:x".into(),
            pattern: "cargo test".into(),
            effect: Effect::Allow,
            require_static_args: true,
            allow_substitutions: false,
        });

        let text = p.to_yaml();
        let back = Policy::from_yaml(&text).expect(&text);
        assert_eq!(back.commands.len(), 1);
        assert_eq!(back.commands[0].pattern, "cargo test");
        assert!(back.exec.is_empty() && back.paths.is_empty() && back.scripts.is_empty());

        let empty = Policy::default();
        assert!(Policy::from_yaml(&empty.to_yaml()).is_ok());
    }

    #[test]
    fn yaml_rejects_unknown_fields_and_versions() {
        assert!(Policy::from_yaml("policy: { version: 2, default: ask }").is_err());
        assert!(Policy::from_yaml("policy: { version: 1, default: ask, typo: true }").is_err());
    }

    // ───────────────────── command rules (user tier) ─────────────────────

    fn command_rule(pattern: &str, effect: Effect) -> CommandRule {
        CommandRule {
            id: format!("cmd:{pattern}"),
            pattern: pattern.into(),
            effect,
            require_static_args: true,
            allow_substitutions: false,
        }
    }

    fn eval(p: &Policy, cmd: &str) -> Decision {
        evaluate_command(
            p,
            Dialect::Posix,
            cmd,
            &abs("/ws"),
            &abs("/ws"),
            &abs("/home/u"),
        )
        .unwrap()
    }

    #[test]
    fn command_rule_skips_priv_escalation_floor() {
        let mut p = Policy::default();
        p.commands.push(command_rule("sudo **", Effect::Allow));
        // without the rule: floored to Ask
        assert_eq!(
            eval(&Policy::default(), "sudo systemctl restart nginx").effect,
            Effect::Ask
        );
        // with the rule: terminal Allow, uncertainty floors skipped
        let d = eval(&p, "sudo systemctl restart nginx");
        assert_eq!(d.effect, Effect::Allow);
        assert!(
            d.op_decisions
                .iter()
                .all(|od| od.matched_rule_ids == vec!["cmd:sudo **"])
        );
    }

    #[test]
    fn command_rule_skips_unknown_op_floor() {
        // `bash -c $CMD` decomposes to Exec + Op::Unknown (dynamic payload) —
        // only an exact user pattern can clear it.
        let mut p = Policy::default();
        p.commands.push(command_rule("bash -c $CMD", Effect::Allow));
        assert_eq!(eval(&p, "bash -c \"$CMD\"").effect, Effect::Allow);
        // a different variable is a different command — no match, floor asks
        assert_eq!(eval(&p, "bash -c \"$OTHER\"").effect, Effect::Ask);
    }

    #[test]
    fn command_rule_wildcard_does_not_carry_substitutions() {
        let mut p = Policy::default();
        p.commands.push(command_rule("cat *", Effect::Allow));
        assert_eq!(eval(&p, "cat notes.txt").effect, Effect::Allow);
        // $(...) inside the matched command falls through to op analysis,
        // which surfaces the inner delete + asks
        let d = eval(&p, "cat $(rm -rf /ws/x)");
        assert_ne!(d.effect, Effect::Allow);
        assert!(d.ops.iter().any(|op| matches!(
            op,
            Op::Path {
                access: Access::Delete,
                ..
            }
        )));
    }

    #[test]
    fn compound_input_requires_every_command_to_match() {
        let mut p = Policy::default();
        p.commands.push(command_rule("cat *", Effect::Allow));
        assert_eq!(eval(&p, "cat a.txt && curl evil.com").effect, Effect::Ask);
        p.commands.push(command_rule("curl **", Effect::Allow));
        assert_eq!(eval(&p, "cat a.txt && curl evil.com").effect, Effect::Allow);
        // subshells flatten into their inner commands
        assert_eq!(eval(&p, "(cat a.txt)").effect, Effect::Allow);
    }

    #[test]
    fn command_rule_deny_is_terminal_and_matches_dynamic() {
        let mut p = Policy::default();
        p.commands.push(command_rule("git **", Effect::Allow));
        p.commands
            .push(command_rule("git push --force **", Effect::Deny));
        assert_eq!(eval(&p, "git status").effect, Effect::Allow);
        let d = eval(&p, "git push --force origin $BRANCH");
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(
            d.op_decisions[0].matched_rule_ids,
            vec!["cmd:git push --force **"]
        );
        // ask rules are terminal too and win over allow for the same command
        p.commands.push(command_rule("git stash **", Effect::Ask));
        assert_eq!(eval(&p, "git stash drop").effect, Effect::Ask);
    }

    #[test]
    fn protected_write_floor_survives_command_rules() {
        let mut p =
            default_workspace_policy(Path::new(&abs("/ws")), Path::new(&abs("/home/u")), "cli");
        p.commands.push(command_rule("tee **", Effect::Allow));
        p.commands.push(command_rule("rm **", Effect::Allow));
        let policy_file = p.protected_write[0]
            .display()
            .to_string()
            .replace('\\', "/");

        let d = eval(&p, &format!("tee {policy_file}"));
        assert_eq!(d.effect, Effect::Ask, "policy file write must stay Ask");
        assert!(
            d.op_decisions
                .iter()
                .any(|od| od.matched_rule_ids == vec!["builtin:protected-write"])
        );
        // protected_delete keeps its Deny through a matching command rule
        assert_eq!(eval(&p, "rm -rf .git").effect, Effect::Deny);
        // and an unrelated tee is genuinely terminal-allowed
        assert_eq!(eval(&p, "tee /ws/out.log").effect, Effect::Allow);
    }

    #[test]
    fn protected_write_floor_applies_without_command_rules() {
        let p = default_workspace_policy(Path::new(&abs("/ws")), Path::new(&abs("/home/u")), "cli");
        let project_policy = p.protected_write[1]
            .display()
            .to_string()
            .replace('\\', "/");
        // allow-workspace-all would allow this write; the floor raises it
        let d = eval(&p, &format!("touch {project_policy}"));
        assert_eq!(d.effect, Effect::Ask);
        // deleting an ancestor directory of the policy file also asks
        let d = eval(&p, "rm -rf /ws/.cli");
        assert_eq!(d.effect, Effect::Ask);
    }

    #[test]
    fn command_rules_ignored_for_windows_dialects() {
        let mut p = Policy::default();
        p.commands.push(command_rule("del **", Effect::Allow));
        let d = evaluate_command(
            &p,
            Dialect::Cmd,
            "del x.txt",
            "C:\\ws",
            "C:\\ws",
            "C:\\Users\\u",
        )
        .unwrap();
        assert_ne!(d.effect, Effect::Allow);
    }

    #[test]
    fn validate_rejects_bad_command_rules() {
        let mut p = Policy::default();
        p.commands.push(command_rule("a && b", Effect::Allow));
        assert!(p.validate().is_err());
        let mut p = Policy::default();
        p.commands.push(CommandRule {
            id: String::new(),
            pattern: "ls".into(),
            effect: Effect::Allow,
            require_static_args: true,
            allow_substitutions: false,
        });
        assert!(p.validate().is_err());
    }

    // ───────────────────── tightened op-level semantics ─────────────────────

    #[test]
    fn exec_deny_is_not_skipped_for_dynamic_args() {
        let mut p = allow_workspace_policy();
        p.exec.push(ExecRule {
            id: "deny-force-push".into(),
            head: "git".into(),
            args: None,
            args_prefix: Some(vec!["push".into(), "--force".into()]),
            effect: Effect::Deny,
            require_static_args: true,
        });
        let d = eval(&p, "git push --force origin $BRANCH");
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.op_decisions[0].matched_rule_ids, vec!["deny-force-push"]);
    }

    #[test]
    fn doas_and_su_hit_the_priv_escalation_floor() {
        let p = default_workspace_policy(Path::new(&abs("/ws")), Path::new(&abs("/home/u")), "cli");
        assert_eq!(eval(&p, "doas reboot").effect, Effect::Ask);
        assert_eq!(eval(&p, "su -c 'ls'").effect, Effect::Ask);
        // still Ask, not Deny: the user can approve interactively
        assert_eq!(eval(&p, "sudo ls").effect, Effect::Ask);
    }

    #[test]
    fn priv_escalation_heads_cannot_be_exec_allowed() {
        for head in ["sudo", "doas", "su", "SU"] {
            let mut p = Policy::default();
            p.exec.push(ExecRule {
                id: "bad".into(),
                head: head.into(),
                args: None,
                args_prefix: None,
                effect: Effect::Allow,
                require_static_args: true,
            });
            assert!(p.validate().is_err(), "{head}");
        }
    }

    #[test]
    fn yaml_roundtrip_with_commands_and_protected_write() {
        let mut p =
            default_workspace_policy(Path::new(&abs("/ws")), Path::new(&abs("/home/u")), "cli");
        p.commands.push(CommandRule {
            id: "allow-bash-dynamic".into(),
            pattern: "bash -c $CMD".into(),
            effect: Effect::Allow,
            require_static_args: false,
            allow_substitutions: true,
        });
        let yaml = p.to_yaml();
        let loaded = Policy::from_yaml(&yaml).unwrap();
        assert_eq!(loaded.protected_write, p.protected_write);
        assert_eq!(loaded.commands.len(), 1);
        let r = &loaded.commands[0];
        assert_eq!(
            (
                r.id.as_str(),
                r.pattern.as_str(),
                r.effect,
                r.require_static_args,
                r.allow_substitutions
            ),
            (
                "allow-bash-dynamic",
                "bash -c $CMD",
                Effect::Allow,
                false,
                true
            )
        );
    }
}
