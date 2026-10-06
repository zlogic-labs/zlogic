use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The shell used by the built-in `shell` tool.
/// `Auto` is resolved once during engine bootstrap. Keeping the user's preference separate from
/// the resolved executable means the tool definition and the process launcher can share one
/// concrete backend without re-running detection or guessing from command text.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellPreference {
    #[default]
    Auto,
    GitBash,
    Ps7,
    Powershell,
    Cmd,
    Bash,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Auto,
    Bypass,
}

/// Which layer a variable is declared in. Precedence runs in this order: a name declared higher
/// wins outright, and a higher layer that switches a name **off** masks the lower one rather than
/// falling back to it — that is the only way to say "do not use the global one here".
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvScope {
    Global,
    Workspace,
    Session,
}

/// Where a variable lives: one of the three layers, or zlogic itself.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvSource {
    Builtin,
    Global,
    Workspace,
    Session,
}

/// The `ZLOGIC_` prefix is zlogic's own. A user variable under it would be indistinguishable from
/// a built-in in the shell, so it is refused rather than shadowed.
pub const ENV_BUILTIN_PREFIX: &str = "ZLOGIC_";

/// One variable as a configuration file spells it: `FOO: bar` for the common case,
/// `FOO: { value: bar, enabled: false }` when it is switched off.
///
/// Untagged rather than a struct with optional fields because `enabled` has to be *absent* by
/// default, and `Option<bool>` would make `enabled: null` mean "on" — a second spelling of the
/// same thing, which is how a file ends up meaning two things at once.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvVar {
    Value(String),
    Detailed(EnvVarDetail),
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvVarDetail {
    pub value: String,
    pub enabled: bool,
}

impl Default for EnvVarDetail {
    fn default() -> Self {
        Self {
            value: String::new(),
            enabled: true,
        }
    }
}

impl EnvVar {
    pub fn detail(&self) -> EnvVarDetail {
        match self {
            EnvVar::Value(value) => EnvVarDetail {
                value: value.clone(),
                enabled: true,
            },
            EnvVar::Detailed(detail) => detail.clone(),
        }
    }
}

impl From<EnvVarDetail> for EnvVar {
    fn from(detail: EnvVarDetail) -> Self {
        EnvVar::Detailed(detail)
    }
}

/// The `env:` block. Present at all three layers with the same shape, so one loader reads any of
/// them and the desktop's table is the same component wherever it is shown.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvConfig {
    /// The layer's own master switch. Off means this layer contributes nothing at all, whatever
    /// its variables say — the difference between "I have no variables" and "I do not want any".
    pub enabled: bool,
    pub variables: BTreeMap<String, EnvVar>,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            variables: BTreeMap::new(),
        }
    }
}

/// The refusals that need no policy crate: shape and the reserved prefix.
///
/// The dangerous-name judgement lives in the engine, which is the one layer that can see both this
/// and the command analyzer's list — `policy` deliberately depends on nothing but `zlogic-paths`,
/// so the shared list cannot be imported from here without inverting the dependency.
pub fn name_syntax_reason(name: &str) -> std::result::Result<(), String> {
    if !is_env_name(name) {
        return Err("not a valid environment variable name".into());
    }
    if name.starts_with(ENV_BUILTIN_PREFIX) {
        return Err(format!(
            "`{ENV_BUILTIN_PREFIX}` is reserved for zlogic's own variables"
        ));
    }
    Ok(())
}

/// A name-shape denylist, matching the one the shell applies to the inherited environment. Values
/// are never scanned: a value that looks random is very often a legitimate build hash, whereas
/// `*_TOKEN` is unambiguous.
pub fn looks_like_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || matches!(upper.as_str(), "API_KEY" | "TOKEN" | "SECRET" | "PASSWORD")
}

/// Shell env name syntax: ASCII alphanumerics and `_`, not starting with a digit.
pub fn is_env_name(key: &str) -> bool {
    !key.is_empty()
        && !key.contains(['=', '\0'])
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !key.as_bytes()[0].is_ascii_digit()
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub auto_title: AutoTitle,
    pub approval_mode: ApprovalMode,
    /// Plan mode: a turn is restricted to the tools that declare `ToolRisk::Read` and gets a
    /// system-prompt section saying so. The restriction is applied at materialisation, so a
    /// mutating tool is not offered to the model rather than refused once called. `false` is the
    /// ordinary posture; the desktop's Plan switch writes it here, and the engine reads it when a
    /// turn starts.
    pub plan_mode: bool,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoTitle {
    pub enabled: bool,
    pub max_chars: usize,
    pub source_chars: usize,
}

impl Default for AutoTitle {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: 30,
            source_chars: 600,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub compact_ratio: f32,
    pub tail_turns: u32,
    pub overflow_retries: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_ratio: 0.8,
            tail_turns: 1,
            overflow_retries: 1,
        }
    }
}

impl ContextConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(0.1..=0.95).contains(&self.compact_ratio) {
            return Err(format!(
                "context.compact_ratio must be between 0.1 and 0.95, got {}",
                self.compact_ratio
            ));
        }
        if self.tail_turns == 0 {
            return Err("context.tail_turns must be at least 1, otherwise compaction would also consume the current tool chain".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// Shell backend for the `shell` tool. `Auto` prefers, on Windows:
    /// Git Bash → PowerShell 7 → Windows PowerShell → cmd. Other platforms use bash.
    pub default_shell: ShellPreference,
    pub max_result_chars: usize,
    /// Blanket ceiling on **any** tool call, on top of whatever the tool itself allows. 0 = off.
    /// Overrides `shell`'s per-class budgets when set, which is why it is an emergency stop
    /// rather than the normal way to bound a run.
    pub timeout_secs: u64,
    /// Wall-clock ceiling on one `grep` / `glob` / `list_dir` call. 0 = off.
    ///
    /// Separate from [`Self::timeout_secs`] because a search is the one tool call that can run for
    /// hours without producing anything to show for it: a pattern that matches nothing reads every
    /// file in the tree, and `include_ignored` turns a Rust repository's `target/` from a skipped
    /// directory into tens of thousands of files. This bound is checked *inside* the walk, so
    /// expiry returns the hits already found plus a note that the tree was not fully read —
    /// rather than an error, and rather than a search that keeps burning disk after the caller has
    /// given up on it.
    pub search_timeout_secs: u64,
    pub shell: ShellConfig,
    pub web_search: WebSearchConfig,
    pub computer: ComputerConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            default_shell: ShellPreference::Auto,
            max_result_chars: 30_000,
            timeout_secs: 0,
            search_timeout_secs: DEFAULT_SEARCH_TIMEOUT_SECS,
            shell: ShellConfig::default(),
            web_search: WebSearchConfig::default(),
            computer: ComputerConfig::default(),
        }
    }
}

/// Generous enough that no ordinary search notices it, low enough that a runaway one is minutes
/// rather than hours. A search that legitimately needs longer wants a narrower `path` or
/// `include`, which returns a better answer than the same search with more time.
pub const DEFAULT_SEARCH_TIMEOUT_SECS: u64 = 60;

/// How long the `shell` tool may run, per kind of command.
///
/// The caller never picks these: it says whether it needs the result (`wait`) and the tool matches
/// the command to a class, because a model asked for a number before it knows how long the work
/// takes guesses badly in both directions. Per workspace, `<project>/.zlogic/settings.yaml` may
/// override any single field on top of this global default.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Whether the shell reads its startup files before running a command.
    ///
    /// On: bash is launched as a login shell, so `/etc/profile` and `~/.bash_profile` (and
    /// whatever that sources) run; PowerShell runs its `$PROFILE`; cmd runs its AutoRun commands.
    /// Off: `--noprofile --norc`, `-NoProfile`, `/d`.
    ///
    /// On by default, because the alternative is a `PATH` nobody can extend — the user spends an
    /// afternoon wiring a toolchain into `~/.bash_profile` and the agent cannot see it, so it has
    /// to be declared as a variable instead. The cost is that a startup file can `set -e`, install
    /// an `EXIT` trap or block on a `read`, and all of that then applies to a command the model
    /// wrote. A profile that is safe interactively is not automatically safe non-interactively,
    /// so `set -e`, prompts and readline tweaks belong behind an interactivity check.
    pub read_profile: bool,
    /// A command whose answer is a fact about the repository: status, a diff, a listing.
    pub quick_secs: u64,
    /// A test run. Generous on purpose — a suite killed at the quick budget has told nobody
    /// anything, and re-running it is how one short timeout becomes four.
    pub test_secs: u64,
    /// A compile, a type check, an install.
    pub build_secs: u64,
    /// Ceiling for `wait: true`, which says the result is required and so opts out of its
    /// class's budget. Not "no deadline": a command blocked on a port nobody opened would
    /// otherwise hold the turn until the user gives up on it.
    pub wait_secs: u64,
    /// No output at all for this long is a stall rather than slowness, and is reported as one.
    /// Independent of the budgets above on purpose, because the two call for opposite responses.
    pub stall_secs: u64,
    /// How often a running call reports that it is still running. Display cadence only — it
    /// changes what the console shows, never what the tool does — so it is deliberately not
    /// offered in the settings UI.
    pub progress_secs: u64,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            read_profile: true,
            quick_secs: 60,
            test_secs: 600,
            build_secs: 1_200,
            wait_secs: 3_600,
            stall_secs: 600,
            progress_secs: 30,
        }
    }
}

impl ShellConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (field, value) in [
            ("quick_secs", self.quick_secs),
            ("test_secs", self.test_secs),
            ("build_secs", self.build_secs),
            ("wait_secs", self.wait_secs),
            ("stall_secs", self.stall_secs),
            ("progress_secs", self.progress_secs),
        ] {
            if value == 0 {
                return Err(format!(
                    "tools.shell.{field} must be at least 1 second, or the command is killed \
                     before it can start"
                ));
            }
        }
        // Not an error — a stall limit above a class budget simply never fires for that class —
        // but it means a number the user set is doing nothing, and silence about that is how a
        // setting gets abandoned as broken.
        if self.stall_secs > self.wait_secs {
            return Err(format!(
                "tools.shell.stall_secs ({}) is above tools.shell.wait_secs ({}), so a waiting \
                 call would never be reported as stuck",
                self.stall_secs, self.wait_secs
            ));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub provider: Option<String>,
    pub exa_url: Option<String>,
    pub parallel_url: Option<String>,
    pub timeout_secs: u64,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: None,
            exa_url: None,
            parallel_url: None,
            timeout_secs: 25,
        }
    }
}

impl WebSearchConfig {
    pub fn provider_id(&self) -> Option<String> {
        self.provider
            .as_ref()
            .map(|p| p.trim().to_ascii_lowercase())
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = self.provider_id()
            && !matches!(p.as_str(), "exa" | "parallel")
        {
            return Err(format!(
                "tools.web_search.provider must be exa or parallel, got {:?}",
                self.provider.as_deref().unwrap_or_default()
            ));
        }
        Ok(())
    }
}

/// The `computer` tool: letting the model look at and drive the desktop.
///
/// Off by default and not for the usual reason: every field here is a knob that hands an agent
/// control of the machine, so the enabled flag is a decision the user has to make out loud rather
/// than a default that happens to be conservative.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComputerConfig {
    pub enabled: bool,
    /// Long edge of the copy handed to the model, in pixels. Bounds the image cost of a turn:
    /// every action returns a fresh screenshot unless `observe` says otherwise.
    pub max_edge: u32,
    /// `png` keeps text sharp; `jpeg` is smaller and cheaper per image. PNG is the default because
    /// a compressed screenshot of a dialog is a screenshot of mush.
    pub format: String,
    /// Default budget for a `wait` that does not name one.
    pub wait_ms: u64,
    /// Ceiling on screenshots in one turn, so a model that has lost the thread fails on a named
    /// counter instead of on the context window.
    pub max_screens_per_turn: u32,
    /// Whether an action that moves something comes back with a screenshot. On by default
    /// because a mis-click is only visible if the result shows the screen it caused.
    pub observe_after_action: bool,
}

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_edge: 1280,
            format: "png".into(),
            wait_ms: 2000,
            max_screens_per_turn: 20,
            observe_after_action: true,
        }
    }
}

impl ComputerConfig {
    /// `png` or `jpeg`, normalized. The tools crate maps this onto an encoder; the protocol crate
    /// deliberately does not depend on `image` for one enum.
    pub fn image_format(&self) -> &'static str {
        match self.format.trim().to_ascii_lowercase().as_str() {
            "jpeg" | "jpg" => "jpeg",
            _ => "png",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(320..=2000).contains(&self.max_edge) {
            return Err(format!(
                "tools.computer.max_edge must be between 320 and 2000, got {}",
                self.max_edge
            ));
        }
        if !matches!(
            self.format.trim().to_ascii_lowercase().as_str(),
            "png" | "jpeg" | "jpg"
        ) {
            return Err(format!(
                "tools.computer.format must be png or jpeg, got {:?}",
                self.format
            ));
        }
        if self.wait_ms == 0 {
            return Err("tools.computer.wait_ms must be at least 1ms".into());
        }
        if self.max_screens_per_turn == 0 {
            return Err("tools.computer.max_screens_per_turn must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    pub dir: String,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            dir: "../{workspace}-worktrees".into(),
        }
    }
}

impl WorktreeConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.dir.trim().is_empty() {
            return Err("worktree.dir must not be empty".into());
        }
        Ok(())
    }
}

/// How many restore points to keep, and for how long. The three caps are independent and the
/// first one reached wins, because each catches something the others cannot: days keep a busy
/// repository from growing without bound, the count keeps a long day of small edits from keeping a
/// hundred trees, and the size is the only one that notices a repository holding a 2 GB file.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointsConfig {
    /// Off by default and only ever turned on deliberately: a snapshot is a copy of the user's
    /// code on their disk, so the decision to keep one belongs to them, not to the installer.
    pub enabled: bool,
    pub retention_days: u32,
    pub max_snapshots: u32,
    pub max_size_gb: u32,
    /// A file above this is left out of a snapshot rather than copied. 0 = no limit.
    pub max_file_mb: u64,
    /// The most files one snapshot may hold. 0 = no limit. Reaching it marks the snapshot
    /// partial, and a partial snapshot cannot be restored.
    pub max_files: u32,
}

impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: 7,
            max_snapshots: 500,
            max_size_gb: 2,
            max_file_mb: 256,
            max_files: 200_000,
        }
    }
}

impl CheckpointsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.retention_days == 0 {
            return Err("checkpoints.retention_days must be at least 1".into());
        }
        if self.max_snapshots == 0 {
            return Err("checkpoints.max_snapshots must be at least 1".into());
        }
        Ok(())
    }
}

/// How long the things that only accumulate are kept.
///
/// The three numbers are independent on purpose: a cache day and a log day cost nothing to keep
/// and are worthless once stale, while `session_days` is the only one that deletes something the
/// user wrote. It is called out separately in the settings UI for that reason.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// MCP tool lists and the remote object store's read-through copy. Both are regenerable, and
    /// the cache is written under a per-day folder so this is a directory removal.
    pub cache_days: u32,
    /// Log files, which are already filed under a per-day folder.
    pub logs_days: u32,
    /// How long a chat session survives without being touched. Older ones are **deleted** — their
    /// entries, their objects and their temp files — not archived; archive is what the user does
    /// when they want to keep something. 1 is the floor: a shorter window than a day would delete
    /// the session someone is in the middle of reading over lunch.
    pub session_days: u32,
    /// Off leaves every sweep unrun, session deletion included. The engine still checkpoints its
    /// WAL, because that is a few seconds of copying rather than a deletion.
    pub enabled: bool,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            cache_days: 7,
            logs_days: 7,
            session_days: 7,
            enabled: true,
        }
    }
}

impl RetentionConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.cache_days == 0 {
            return Err("retention.cache_days must be at least 1".into());
        }
        if self.logs_days == 0 {
            return Err("retention.logs_days must be at least 1".into());
        }
        if self.session_days == 0 {
            return Err("retention.session_days must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSettings {
    pub proxy: String,
    pub no_proxy: Vec<String>,
}

impl NetworkSettings {
    pub fn proxy_url(&self) -> Option<&str> {
        let trimmed = self.proxy.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    pub fn no_proxy_list(&self) -> String {
        self.no_proxy
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn validate(&self) -> Result<(), String> {
        let Some(url) = self.proxy_url() else {
            return Ok(());
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(format!(
                "network.proxy must include a scheme, e.g. http://127.0.0.1:7890 (got {url:?})"
            ));
        };
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ) {
            return Err(format!(
                "network.proxy scheme must be http, https or socks5, got {scheme:?}"
            ));
        }
        let host = rest.trim_end_matches('/');
        if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
            return Err(format!(
                "network.proxy must be host[:port] with no path, got {url:?}"
            ));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// `error` / `warn` / `info` / `debug` / `trace`
    pub level: String,
    pub to_file: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            to_file: true,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CostConfig {
    pub display_currency: String,
    pub rates: Vec<ExchangeRate>,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            display_currency: "USD".into(),
            rates: Vec::new(),
        }
    }
}

impl CostConfig {
    pub fn display(&self) -> String {
        let trimmed = self.display_currency.trim();
        if trimmed.is_empty() {
            "USD".into()
        } else {
            trimmed.to_ascii_uppercase()
        }
    }

    pub fn convert(&self, amount: f64, from: &str) -> Option<f64> {
        self.convert_to(amount, from, &self.display())
    }

    pub fn convert_to(&self, amount: f64, from: &str, to: &str) -> Option<f64> {
        let to = normalize_currency(to);
        let from = from.trim().to_ascii_uppercase();
        if from == to {
            return Some(amount);
        }
        for rate in &self.rates {
            let (rf, rt) = (rate.from(), rate.to());
            if rate.rate <= 0.0 || !rate.rate.is_finite() {
                continue;
            }
            if rf == from && rt == to {
                return Some(amount * rate.rate);
            }
            if rf == to && rt == from {
                return Some(amount / rate.rate);
            }
        }
        None
    }

    pub fn validate(&self) -> Result<(), String> {
        if !is_currency_code(&self.display()) {
            return Err(format!(
                "cost.display_currency must be a currency code (e.g. USD / CNY), got {:?}",
                self.display_currency
            ));
        }
        for rate in &self.rates {
            rate.validate()?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_rounds: u32,
    pub max_depth: u32,
    pub max_parallel_tools: usize,
    pub task_wait_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_rounds: 150,
            max_depth: 1,
            max_parallel_tools: 16,
            task_wait_secs: 60,
        }
    }
}

impl LimitsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_rounds == 0 {
            return Err(
                "limits.max_rounds must be at least 1, otherwise no round could ever run".into(),
            );
        }
        if self.max_depth == 0 {
            return Err("limits.max_depth must be at least 1 (don't enable the tool if you don't want sub-agents)".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_turn: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_session: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_task: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub window: Option<BudgetWindow>,
    pub on_exceeded: BudgetAction,
}

impl BudgetConfig {
    pub fn is_set(&self) -> bool {
        self.per_turn.is_some()
            || self.per_session.is_some()
            || self.per_task.is_some()
            || self.window.is_some()
    }

    pub fn task_limit(&self) -> Option<f64> {
        self.per_task.or(self.per_turn)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("per_turn", self.per_turn),
            ("per_session", self.per_session),
            ("per_task", self.per_task),
        ] {
            if let Some(amount) = value
                && !(amount.is_finite() && amount > 0.0)
            {
                return Err(format!(
                    "budget.{name} must be a positive number (omit the key for unlimited), got {amount}"
                ));
            }
        }
        if let Some(window) = &self.window {
            window.validate()?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetWindow {
    pub amount: f64,
    pub hours: u32,
}

impl BudgetWindow {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.amount.is_finite() && self.amount > 0.0) {
            return Err(format!(
                "budget.window.amount must be a positive number, got {}",
                self.amount
            ));
        }
        if self.hours == 0 {
            return Err("budget.window.hours must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetAction {
    #[default]
    Ask,
    Stop,
    Warn,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeRate {
    pub from: String,
    pub to: String,
    pub rate: f64,
}

impl ExchangeRate {
    pub fn from(&self) -> String {
        self.from.trim().to_ascii_uppercase()
    }

    pub fn to(&self) -> String {
        self.to.trim().to_ascii_uppercase()
    }

    pub fn validate(&self) -> Result<(), String> {
        let (from, to) = (self.from(), self.to());
        if !is_currency_code(&from) || !is_currency_code(&to) {
            return Err(format!(
                "exchange-rate currencies must be currency codes (e.g. USD / CNY), got {:?} → {:?}",
                self.from, self.to
            ));
        }
        if from == to {
            return Err(format!("the exchange rate {from} → {to} is meaningless"));
        }
        if !(self.rate.is_finite() && self.rate > 0.0) {
            return Err(format!(
                "the exchange rate {from} → {to} must be a positive number, got {}",
                self.rate
            ));
        }
        Ok(())
    }
}

fn is_currency_code(code: &str) -> bool {
    let mut chars = code.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && code.len() >= 2
        && code.len() <= 12
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn normalize_currency(code: &str) -> String {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        "USD".into()
    } else {
        trimmed.to_ascii_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_defaults_are_ordered_so_a_stall_can_actually_fire() {
        let defaults = ShellConfig::default();
        assert!(
            defaults.validate().is_ok(),
            "the shipped defaults are invalid"
        );
        assert!(
            defaults.stall_secs <= defaults.wait_secs,
            "a default stall limit above the wait ceiling would never be reached"
        );
    }

    #[test]
    fn a_zero_budget_is_refused_rather_than_read_as_unlimited() {
        let error = ShellConfig {
            test_secs: 0,
            ..Default::default()
        }
        .validate()
        .expect_err("a zero test budget must not be accepted");
        assert!(error.contains("tools.shell.test_secs"), "{error}");
    }

    #[test]
    fn a_stall_limit_above_the_wait_ceiling_is_named_as_the_dead_setting_it_is() {
        let error = ShellConfig {
            stall_secs: 7_200,
            wait_secs: 3_600,
            ..Default::default()
        }
        .validate()
        .expect_err("a stall limit the wait ceiling pre-empts must not be accepted");
        assert!(error.contains("stall_secs"), "{error}");
        assert!(error.contains("wait_secs"), "{error}");
    }

    #[test]
    fn a_proxy_needs_a_scheme_and_a_bare_host_and_port() {
        let ok = |proxy: &str| {
            NetworkSettings {
                proxy: proxy.into(),
                ..Default::default()
            }
            .validate()
        };
        assert!(ok("").is_ok());
        assert!(ok("  ").is_ok());
        assert!(ok("http://127.0.0.1:7890/").is_ok());
        assert!(ok("socks5://127.0.0.1:7891").is_ok());
        assert!(
            ok("127.0.0.1:7890").is_err(),
            "a missing scheme must not be guessed on the user's behalf"
        );
        assert!(ok("ftp://127.0.0.1:7890").is_err());
        assert!(ok("http://127.0.0.1:7890/pac").is_err());
        assert!(ok("http://127.0.0.1:78 90").is_err());
    }

    #[test]
    fn the_no_proxy_list_is_trimmed_and_joined() {
        let settings = NetworkSettings {
            proxy: "http://127.0.0.1:7890".into(),
            no_proxy: vec![" localhost ".into(), String::new(), "127.0.0.1".into()],
        };
        assert_eq!(settings.no_proxy_list(), "localhost,127.0.0.1");
    }

    #[test]
    fn defaults_are_sane() {
        let c = ContextConfig::default();
        assert!(c.validate().is_ok());
        assert!(
            c.compact_ratio < 1.0,
            "compacting only once 100% is reached is already too late"
        );
        assert!(c.tail_turns >= 1);
    }

    #[test]
    fn ratio_out_of_range_is_rejected() {
        for r in [0.0, 0.05, 1.0, 1.5, -1.0] {
            let c = ContextConfig {
                compact_ratio: r,
                ..Default::default()
            };
            assert!(c.validate().is_err(), "{r} must be rejected");
        }
    }

    #[test]
    fn zero_tail_turns_is_rejected() {
        let c = ContextConfig {
            tail_turns: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn approval_mode_defaults_to_auto() {
        assert_eq!(
            SessionConfig::default().approval_mode,
            ApprovalMode::Auto,
            "a full grant must never be on by default"
        );
    }

    #[test]
    fn sections_round_trip_through_yaml() {
        let s = SessionConfig::default();
        let y = serde_yaml_ng::to_string(&s).unwrap();
        assert_eq!(serde_yaml_ng::from_str::<SessionConfig>(&y).unwrap(), s);
    }

    #[test]
    fn default_shell_is_configurable_and_defaults_to_auto() {
        assert_eq!(ToolsConfig::default().default_shell, ShellPreference::Auto);
        let tools: ToolsConfig = serde_yaml_ng::from_str("default_shell: ps7\n").unwrap();
        assert_eq!(tools.default_shell, ShellPreference::Ps7);
        assert!(serde_yaml_ng::from_str::<ToolsConfig>("default_shell: zsh\n").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let y = "compact_ratio: 0.5\ntail_turn: 3\n"; // missing the s
        assert!(serde_yaml_ng::from_str::<ContextConfig>(y).is_err());
    }

    #[test]
    fn one_rate_serves_both_directions() {
        let cost = CostConfig {
            display_currency: "USD".into(),
            rates: vec![ExchangeRate {
                from: "USD".into(),
                to: "CNY".into(),
                rate: 7.0,
            }],
        };
        assert_eq!(cost.convert(1.0, "USD"), Some(1.0));
        assert_eq!(cost.convert(7.0, "CNY"), Some(1.0));

        let cost = CostConfig {
            display_currency: "CNY".into(),
            ..cost
        };
        assert_eq!(cost.convert(1.0, "USD"), Some(7.0));
        assert_eq!(cost.convert(7.0, "CNY"), Some(7.0));
    }

    #[test]
    fn currency_codes_are_matched_case_insensitively() {
        let cost = CostConfig {
            display_currency: " usd ".into(),
            rates: vec![ExchangeRate {
                from: "usd".into(),
                to: " cny".into(),
                rate: 7.0,
            }],
        };
        assert_eq!(cost.display(), "USD");
        assert_eq!(cost.convert(7.0, "cny"), Some(1.0));
    }

    #[test]
    fn a_missing_rate_cannot_be_converted() {
        let cost = CostConfig::default();
        assert_eq!(cost.convert(700.0, "CNY"), None);
        assert_eq!(
            cost.convert(1.0, "USD"),
            Some(1.0),
            "the same currency needs no rate"
        );
    }

    #[test]
    fn conversion_does_not_chain_through_a_third_currency() {
        let cost = CostConfig {
            display_currency: "JPY".into(),
            rates: vec![
                ExchangeRate {
                    from: "USD".into(),
                    to: "CNY".into(),
                    rate: 7.0,
                },
                ExchangeRate {
                    from: "CNY".into(),
                    to: "JPY".into(),
                    rate: 20.0,
                },
            ],
        };
        assert_eq!(cost.convert(1.0, "CNY"), Some(20.0));
        assert_eq!(cost.convert(1.0, "USD"), None, "USD→CNY→JPY is not derived");
    }

    #[test]
    fn a_nonsense_rate_is_rejected_and_never_used() {
        for rate in [0.0, -7.0, f64::NAN, f64::INFINITY] {
            let one = ExchangeRate {
                from: "USD".into(),
                to: "CNY".into(),
                rate,
            };
            assert!(one.validate().is_err(), "{rate} must be rejected");
            let cost = CostConfig {
                display_currency: "USD".into(),
                rates: vec![one],
            };
            assert_eq!(
                cost.convert(7.0, "CNY"),
                None,
                "{rate} must never be used for conversion"
            );
        }
    }

    #[test]
    fn a_self_referential_rate_is_rejected() {
        assert!(
            ExchangeRate {
                from: "USD".into(),
                to: "usd".into(),
                rate: 1.0,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn currency_codes_are_checked_loosely_not_against_a_list() {
        for ok in ["USD", "CNY", "USDT", "MY-CREDITS"] {
            let cost = CostConfig {
                display_currency: ok.into(),
                rates: Vec::new(),
            };
            assert!(cost.validate().is_ok(), "{ok} must be accepted");
        }
        for bad in ["", " ", "1", "U S D", "¥"] {
            let cost = CostConfig {
                display_currency: bad.into(),
                rates: Vec::new(),
            };
            if bad.trim().is_empty() {
                assert!(cost.validate().is_ok());
            } else {
                assert!(cost.validate().is_err(), "{bad:?} must be rejected");
            }
        }
    }

    #[test]
    fn the_default_needs_no_rates_at_all() {
        let cost = CostConfig::default();
        assert_eq!(
            cost.display(),
            "USD",
            "the built-in catalog prices are in USD"
        );
        assert!(cost.rates.is_empty());
        assert!(cost.validate().is_ok());
    }
}
