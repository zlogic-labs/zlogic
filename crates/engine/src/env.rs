//! Environment variables for the `shell` tool.
//!
//! # What this is for
//!
//! A shell call runs with its environment rebuilt from scratch (see `child_env` in
//! `tools::exec::shell`). The shell does read the user's startup files by default
//! (`tools.shell.read_profile`), so the PATH they wired up is already inherited; this module is
//! for everything that is *not* in them — a value that must be the same every command, a per-project
//! setting, a secret the machine has no other copy of.
//!
//! The refusals below exist because a *variable* is a narrower version of the power a profile has.
//! A profile runs once, in the user's own hands; a workspace's `settings.yaml` arrives with
//! `git clone` and then defines the environment of every command anyone runs in that repository.
//!
//! # Three layers
//!
//! `global` (`<config>/config.yaml`) → `workspace` (`<root>/.zlogic/settings.yaml`) → `session`
//! (`state.db`), lowest to highest. A name declared higher wins outright; a higher layer that
//! switches a name **off** masks the lower one rather than falling back to it, which is the only
//! way to say "do not use the global one here".
//!
//! # How a value reaches a process
//!
//! Only through the child's environment. **The command text is never rewritten**: `$FOO` is
//! expanded by the shell, so an approval prompt still shows the command the model wrote and the
//! policy analyzer still sees the command it was written to judge. One consequence worth stating
//! out loud: the variables are *not* put into zlogic's own process, so a `*_API_KEY` declared here
//! does not become a model-provider key. That lookup has its own chain — `zlogic key list`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use zlogic_core::SharedStore;
use zlogic_protocol::error::ApiError;
use zlogic_protocol::query::{
    ApiResult, EnvEffective, EnvEntry, EnvGetReq, EnvLayer, EnvSetReq, EnvView,
};
use zlogic_protocol::settings::{
    ENV_BUILTIN_PREFIX, EnvConfig, EnvScope, EnvSource, EnvVar, EnvVarDetail, name_syntax_reason,
};
use zlogic_protocol::{SessionId, WorkspaceId};
use zlogic_tools::{EnvFacts, EnvProvider, EnvVariable};

use crate::service::EnvService;

/// Why this layer may not contribute this name.
///
/// The dangerous-name list is the one the command analyzer already uses to refuse a `PATH=x cmd`
/// prefix. A configured variable is a *standing* assignment rather than a one-command one, so a
/// name refused there has to be refused here — from one list, in the engine, which is the only
/// layer that can see both this and `policy` (that crate depends on nothing but `zlogic-paths`,
/// so the list cannot be shared from `protocol` without inverting the dependency).
pub fn reject_reason(name: &str, scope: EnvScope) -> std::result::Result<(), String> {
    name_syntax_reason(name)?;
    if zlogic_policy::analyze::env_name_is_dangerous(name) {
        return Err("this name can change what a later command executes".into());
    }
    if scope == EnvScope::Workspace && zlogic_protocol::settings::looks_like_credential(name) {
        return Err(
            "a workspace variable must not be named like a credential — it travels with the \
             repository, so `git clone` would deliver it"
                .into(),
        );
    }
    Ok(())
}

/// The names this layer cannot contribute, with the reason.
///
/// A layer reports rather than drops: a silently missing variable is a debugging session, a
/// reported one is a line in the transcript the user can act on.
pub fn rejections(config: &EnvConfig, scope: EnvScope) -> Vec<(String, String)> {
    config
        .variables
        .keys()
        .filter_map(|name| Some((name.clone(), reject_reason(name, scope).err()?)))
        .collect()
}

/// The global layer, republished on every config load.
///
/// A `std::sync::RwLock` rather than the engine's async one because [`EnvProvider::variables`] is
/// synchronous — it is called from inside a tool, on the async runtime's own thread, where taking a
/// tokio lock would have to block. Same shape as the bypass flag and the checkpoint policy, which
/// are pushed out of the config snapshot for the same reason.
#[derive(Debug, Default)]
pub struct EnvGlobal {
    current: RwLock<EnvConfig>,
}

impl EnvGlobal {
    pub fn snapshot(&self) -> EnvConfig {
        // A poisoned lock means another thread panicked while writing a config it had already
        // validated. The value is still a whole `EnvConfig`, so there is nothing to rebuild.
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn set(&self, config: EnvConfig) {
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = config;
    }
}

/// The variables zlogic itself supplies.
///
/// No git branch: a session can change it mid-turn, and a value frozen into a process environment
/// at spawn time would be a snapshot that lies from the next commit onwards. A shell can ask git
/// itself, and it will be right.
pub fn builtin_variables(facts: &EnvFacts<'_>) -> Vec<EnvVariable> {
    let cache = crate::retention::session_cache_dir(facts.root, Some(facts.session_id));
    let deliverables =
        zlogic_core::turndir::deliverables_dir(facts.root, facts.session_id, facts.turn_id);
    [
        ("WORKSPACE_ROOT", facts.root.to_string_lossy().into_owned()),
        ("CWD", facts.exec_cwd.to_string_lossy().into_owned()),
        ("SESSION_ID", facts.session_id.to_string()),
        ("TURN_ID", facts.turn_id.to_string()),
        ("CACHE_DIR", cache.to_string_lossy().into_owned()),
        // What a generated file should be written to. The path is also in the prompt's environment
        // block, where it is not truncated; this copy is the one a script interpolates, so a
        // script never has to be told where to put its output.
        (
            "DELIVERABLES_DIR",
            deliverables.to_string_lossy().into_owned(),
        ),
        ("OS", std::env::consts::OS.to_owned()),
        ("ARCH", std::env::consts::ARCH.to_owned()),
        ("VERSION", env!("CARGO_PKG_VERSION").to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| EnvVariable {
        name: format!("{ENV_BUILTIN_PREFIX}{name}"),
        value,
        source: EnvSource::Builtin,
    })
    .collect()
}

/// Folds the layers into the effective set, lowest first.
///
/// A disabled entry is recorded as a tombstone rather than skipped, because skipping it would let
/// the lower layer's value show through — which is the opposite of what "switch this one off here"
/// has to mean. Refused names do not even get a tombstone: a workspace entry the rules reject must
/// not be able to switch off a global one it was never allowed to touch.
pub fn merge_layers(layers: Vec<EnvConfig>) -> Vec<EnvVariable> {
    let mut merged: BTreeMap<String, Option<EnvVariable>> = BTreeMap::new();
    for (index, layer) in layers.into_iter().enumerate() {
        if !layer.enabled {
            continue;
        }
        let source = match index {
            0 => EnvSource::Global,
            1 => EnvSource::Workspace,
            _ => EnvSource::Session,
        };
        for (name, var) in layer.variables {
            if reject_reason(&name, scope_of(source)).is_err() {
                continue;
            }
            let detail = var.detail();
            let variable = EnvVariable {
                name: name.clone(),
                value: detail.value,
                source,
            };
            merged.insert(name, detail.enabled.then_some(variable));
        }
    }
    merged.into_values().flatten().collect()
}

fn scope_of(source: EnvSource) -> EnvScope {
    match source {
        EnvSource::Global => EnvScope::Global,
        EnvSource::Workspace => EnvScope::Workspace,
        _ => EnvScope::Session,
    }
}

// ═══════════════════════════════ the login PATH probe ═══════════════════════════════

/// The PATH a login shell computes, read at most once per process.
///
/// Independent of `tools.shell.read_profile`, on purpose. A `cmd` or PowerShell session has no
/// bash profile to read, so the PATH a user built in `~/.bash_profile` would be missing from it;
/// and a session whose own shell already read a profile loses nothing by also being handed this.
/// One throwaway login shell is asked for its `PATH` and nothing else, and the answer goes into a
/// file rather than stdout — a profile's banner prints to stdout, and parsing that would mean
/// guessing which line is the answer.
///
/// Failure is silent and falls back to the inherited PATH. There is no useful error to report:
/// "your `.bash_profile` is broken" is not something to say mid-command, and the command still runs.
#[derive(Default)]
pub struct RcPath {
    cached: Mutex<Option<Option<String>>>,
}

impl RcPath {
    /// The probe's entries, with the inherited ones kept behind them.
    ///
    /// The profile's own order is what the user meant, and an inherited entry it dropped is kept
    /// at the back rather than discarded: a desktop app's PATH can legitimately carry something a
    /// login profile does not know about, and losing it breaks a command that used to work.
    pub fn entries(&self) -> Option<String> {
        let mut guard = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(probe_login_path().map(|found| {
                let mut entries: Vec<PathBuf> = std::env::split_paths(&found)
                    .filter(|entry| !entry.as_os_str().is_empty())
                    .collect();
                if let Some(inherited) = std::env::var_os("PATH") {
                    for entry in std::env::split_paths(&inherited) {
                        if !entries.contains(&entry) {
                            entries.push(entry);
                        }
                    }
                }
                std::env::join_paths(entries)
                    .map(|joined| joined.to_string_lossy().into_owned())
                    .unwrap_or(found)
            }));
        }
        guard.clone().flatten()
    }
}

fn probe_login_path() -> Option<String> {
    let file = std::env::temp_dir().join(format!(
        "zlogic-rc-path-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default()
    ));
    let ran = probe_with(&file);
    let read = std::fs::read_to_string(&file).ok();
    let _ = std::fs::remove_file(&file);
    ran?;
    read.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Runs the probe on a detached thread so a profile that blocks on `read` costs a timeout rather
/// than a hung turn. `std::process::Command` has no timeout of its own, and killing a login shell
/// that is already inside someone else's `.bash_profile` is not a thing to do from a library.
fn probe_with(file: &Path) -> Option<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let file = file.to_path_buf();
    std::thread::spawn(move || {
        let ran = run_probe(&file);
        let _ = tx.send(ran);
    });
    rx.recv_timeout(Duration::from_secs(3)).ok()?
}

#[cfg(windows)]
fn run_probe(file: &Path) -> Option<()> {
    let file = file.to_str()?;
    // `cygpath -w` because Git Bash reports `$PATH` as `/c/Users/...`, and a native Windows
    // program handed that finds nothing. Without cygpath the answer would be worse than no answer
    // at all, so the probe declines rather than guessing at a conversion.
    probe_bash(&["-l", "-c", "cygpath -wp \"$PATH\" > \"$1\"", "zlogic", file])
}

#[cfg(not(windows))]
fn run_probe(file: &Path) -> Option<()> {
    let file = file.to_str()?;
    probe_bash(&["-l", "-c", "printf %s \"$PATH\" > \"$1\"", "zlogic", file])
}

fn probe_bash(args: &[&str]) -> Option<()> {
    use std::process::{Command, Stdio};
    let program = login_shell_program()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // This runs inside a GUI host, and a console child from one puts a window on screen.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // Not `output()`: a login shell that prints a banner would fill the pipe, and nothing here
    // reads it. Null keeps the child from blocking on a full buffer.
    command.status().ok()?.success().then_some(())
}

/// The shell the login-PATH probe runs in.
///
/// On Windows that is Git for Windows' *real* bash rather than the `bin\bash.exe` launcher, for
/// the same reason the `shell` backend resolves it that way: the launcher's re-execution drops
/// the console the parent was given and asks for a new one, and the new one is a window.
#[cfg(windows)]
fn login_shell_program() -> Option<std::path::PathBuf> {
    zlogic_tools::exec::shell::git_bash_program()
        .or_else(|| zlogic_tools::executable_on_path("bash"))
}

#[cfg(not(windows))]
fn login_shell_program() -> Option<std::path::PathBuf> {
    zlogic_tools::executable_on_path("bash")
}

// ═══════════════════════════════════ the provider ═══════════════════════════════════

/// Resolves the effective set for one tool call.
pub struct EnvResolver {
    global: Arc<EnvGlobal>,
    store: SharedStore,
    rc_path: Arc<RcPath>,
}

impl EnvResolver {
    pub fn new(global: Arc<EnvGlobal>, store: SharedStore, rc_path: Arc<RcPath>) -> Self {
        Self {
            global,
            store,
            rc_path,
        }
    }

    /// The three layers, lowest first. `root` / `session_id` are `None` when the caller named no
    /// context for them, which is the settings page with nothing open.
    pub fn layers(
        &self,
        root: Option<&Path>,
        session_id: Option<SessionId>,
    ) -> (Vec<EnvConfig>, Vec<String>) {
        let mut warnings = Vec::new();
        let global = self.global.snapshot();
        let workspace = match root {
            Some(root) => crate::shell_budgets::load_env(root, &mut warnings),
            None => EnvConfig::default(),
        };
        (
            vec![global, workspace, self.session_layer(session_id)],
            warnings,
        )
    }

    fn session_layer(&self, session_id: Option<SessionId>) -> EnvConfig {
        let Some(id) = session_id else {
            return EnvConfig::default();
        };
        // A session with no row cannot have variables, and a task session is created and read
        // inside one turn. Not worth a warning.
        let vars = self
            .store
            .with_named("env.session", |db| db.session_env().list(id))
            .unwrap_or_default();
        EnvConfig {
            enabled: true,
            variables: vars
                .into_iter()
                .map(|var| {
                    (
                        var.name,
                        EnvVar::Detailed(EnvVarDetail {
                            value: var.value,
                            enabled: var.enabled,
                        }),
                    )
                })
                .collect(),
        }
    }
}

impl EnvProvider for EnvResolver {
    fn variables(&self, facts: &EnvFacts<'_>) -> Vec<EnvVariable> {
        let (layers, _) = self.layers(Some(facts.root), Some(facts.session_id));
        let mut out = builtin_variables(facts);
        out.extend(merge_layers(layers));
        // Last, and marked `Builtin`: this is a whole replacement rather than an override, so the
        // merge order only has to be the last writer. `child_env` then folds it in front of the
        // inherited entries, and zlogic's own managed directories in front of that.
        if let Some(entries) = self.rc_path.entries() {
            out.push(EnvVariable {
                name: "PATH".into(),
                value: entries,
                source: EnvSource::Builtin,
            });
        }
        out
    }
}

// ═══════════════════════════════════ the service ═══════════════════════════════════

/// Reads and writes the three layers for the desktop's table.
pub struct EnvLayers {
    global: Arc<EnvGlobal>,
    store: SharedStore,
    config: Arc<crate::config::Config>,
    rc_path: Arc<RcPath>,
}

impl EnvLayers {
    pub fn new(
        global: Arc<EnvGlobal>,
        store: SharedStore,
        config: Arc<crate::config::Config>,
        rc_path: Arc<RcPath>,
    ) -> Self {
        Self {
            global,
            store,
            config,
            rc_path,
        }
    }

    fn root_of(&self, workspace_id: Option<WorkspaceId>) -> Option<PathBuf> {
        let id = workspace_id?;
        self.store
            .with_named("env.root_of", |db| db.workspaces().find(id))
            .ok()
            .flatten()
            .map(|workspace| PathBuf::from(workspace.path))
    }

    fn entries_of(config: &EnvConfig, scope: EnvScope) -> Vec<EnvEntry> {
        config
            .variables
            .iter()
            .map(|(name, var)| {
                let detail = var.detail();
                EnvEntry {
                    name: name.clone(),
                    value: detail.value,
                    enabled: detail.enabled,
                    rejected: reject_reason(name, scope).err(),
                }
            })
            .collect()
    }

    fn session_layer(&self, session_id: Option<SessionId>) -> EnvConfig {
        let Some(id) = session_id else {
            return EnvConfig::default();
        };
        let vars = self
            .store
            .with_named("env.session", |db| db.session_env().list(id))
            .unwrap_or_default();
        EnvConfig {
            enabled: true,
            variables: vars
                .into_iter()
                .map(|var| {
                    (
                        var.name,
                        EnvVar::Detailed(EnvVarDetail {
                            value: var.value,
                            enabled: var.enabled,
                        }),
                    )
                })
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl EnvService for EnvLayers {
    async fn get(&self, req: EnvGetReq) -> ApiResult<EnvView> {
        let root = self.root_of(req.workspace_id);
        let mut warnings = Vec::new();
        let workspace = match &root {
            Some(root) => crate::shell_budgets::load_env(root, &mut warnings),
            None => EnvConfig::default(),
        };
        let session = self.session_layer(req.session_id);
        let global = self.global.snapshot();

        let layers = vec![
            EnvLayer {
                scope: EnvScope::Global,
                entries: Self::entries_of(&global, EnvScope::Global),
                location: self.config.global_path(),
                available: true,
                enabled: global.enabled,
            },
            EnvLayer {
                scope: EnvScope::Workspace,
                entries: Self::entries_of(&workspace, EnvScope::Workspace),
                location: root
                    .as_ref()
                    .map(|root| workspace_settings_path(root).to_string_lossy().into_owned())
                    .unwrap_or_default(),
                available: root.is_some(),
                enabled: workspace.enabled,
            },
            EnvLayer {
                scope: EnvScope::Session,
                entries: Self::entries_of(&session, EnvScope::Session),
                location: req.session_id.map(|id| id.to_string()).unwrap_or_default(),
                available: req.session_id.is_some(),
                enabled: true,
            },
        ];

        let mut effective: Vec<EnvEffective> = merge_layers(vec![global, workspace, session])
            .into_iter()
            .map(|var| EnvEffective {
                name: var.name,
                value: var.value,
                source: var.source,
            })
            .collect();
        // The login PATH is a built-in like the `ZLOGIC_*` set, and a user staring at an empty
        // table would otherwise conclude their variables are not reaching the shell.
        if let Some(entries) = self.rc_path.entries() {
            effective.insert(
                0,
                EnvEffective {
                    name: "PATH".into(),
                    value: entries,
                    source: EnvSource::Builtin,
                },
            );
        }

        Ok(EnvView { layers, effective })
    }

    async fn set(&self, req: EnvSetReq) -> ApiResult<EnvView> {
        for name in req.variables.keys() {
            if let Err(reason) = reject_reason(name, req.scope) {
                return Err(ApiError::invalid_code(
                    "env_name_rejected",
                    format!("{name}: {reason}"),
                ));
            }
        }
        let enabled = req.enabled.unwrap_or(true);
        let config = EnvConfig {
            enabled,
            variables: req
                .variables
                .into_iter()
                .map(|(name, detail)| (name, EnvVar::Detailed(detail)))
                .collect(),
        };

        match req.scope {
            EnvScope::Global => {
                self.config.set_env(config).await.map_err(|error| {
                    ApiError::invalid_code("env_write_failed", error.to_string())
                })?;
            }
            EnvScope::Workspace => {
                let root = self.root_of(req.workspace_id).ok_or_else(|| {
                    ApiError::invalid_code("env_scope_unavailable", "no workspace is open")
                })?;
                write_workspace_env(&root, &config)
                    .map_err(|error| ApiError::invalid_code("env_write_failed", error))?;
            }
            EnvScope::Session => {
                let session_id = req.session_id.ok_or_else(|| {
                    ApiError::invalid_code("env_scope_unavailable", "no session is open")
                })?;
                let vars: Vec<zlogic_store::SessionEnvVar> = config
                    .variables
                    .into_iter()
                    .map(|(name, var)| {
                        let detail = var.detail();
                        zlogic_store::SessionEnvVar {
                            name,
                            value: detail.value,
                            enabled: detail.enabled,
                        }
                    })
                    .collect();
                self.store
                    .with_named("env.set", |db| db.session_env().replace(session_id, &vars))
                    .map_err(|error| {
                        ApiError::invalid_code("env_write_failed", error.to_string())
                    })?;
            }
        }

        self.get(EnvGetReq {
            workspace_id: req.workspace_id,
            session_id: req.session_id,
        })
        .await
    }
}

fn workspace_settings_path(root: &Path) -> PathBuf {
    root.join(".zlogic").join("settings.yaml")
}

/// Writes the workspace layer into the project's own settings file.
///
/// The same file the shell budgets live in, and the same patch helper the config writer uses, so
/// the two features cannot end up with two different ideas of how that file is edited. A workspace
/// with no `tools.shell` keys is the common case, and patching a key into a file that does not
/// exist yet has to create the file rather than fail.
fn write_workspace_env(root: &Path, config: &EnvConfig) -> std::result::Result<(), String> {
    use zlogic_config::write::{YamlPatch, patch_yaml_file, value_or_default};
    let path = workspace_settings_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut patch = YamlPatch::new();
    patch.insert(
        "env".into(),
        value_or_default(config).map_err(|e| e.to_string())?,
    );
    patch_yaml_file(&path, &patch).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::settings::EnvVar;

    fn layer(pairs: &[(&str, &str)]) -> EnvConfig {
        EnvConfig {
            enabled: true,
            variables: pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), EnvVar::Value((*value).to_string())))
                .collect(),
        }
    }

    fn off(name: &str, value: &str) -> (String, EnvVar) {
        (
            name.to_string(),
            EnvVar::Detailed(EnvVarDetail {
                value: value.to_string(),
                enabled: false,
            }),
        )
    }

    fn names(layers: Vec<EnvConfig>) -> Vec<(String, String, EnvSource)> {
        merge_layers(layers)
            .into_iter()
            .map(|var| (var.name, var.value, var.source))
            .collect()
    }

    #[test]
    fn a_higher_layer_replaces_a_lower_one_and_keeps_the_rest() {
        let merged = names(vec![layer(&[("A", "1"), ("B", "1")]), layer(&[("B", "2")])]);
        assert_eq!(
            merged,
            vec![
                ("A".into(), "1".into(), EnvSource::Global),
                ("B".into(), "2".into(), EnvSource::Workspace),
            ]
        );
    }

    /// The one behaviour a plain `extend` gets wrong: switching a name off at a higher layer has to
    /// *mask* the lower value, or there is no way to say "not the global one here".
    #[test]
    fn switching_a_name_off_higher_up_masks_the_lower_value() {
        let mut session = layer(&[]);
        session
            .variables
            .insert(off("A", "ignored").0, off("A", "ignored").1);
        let merged = names(vec![layer(&[("A", "from-global"), ("B", "kept")]), session]);
        assert_eq!(
            merged,
            vec![("B".into(), "kept".into(), EnvSource::Global)],
            "a masked name must not fall back to the layer below"
        );
    }

    #[test]
    fn a_layer_with_its_switch_off_contributes_nothing() {
        let mut global = layer(&[("A", "1")]);
        global.enabled = false;
        assert!(names(vec![global, layer(&[])]).is_empty());
    }

    #[test]
    fn a_name_on_the_dangerous_list_cannot_be_declared_anywhere() {
        // The list is refused at every layer, global included: a configured `PATH` would be a
        // standing assignment, and the login-shell probe is the only sanctioned source of one.
        let merged = names(vec![
            layer(&[("PATH", "/from/global")]),
            layer(&[("PATH", "/from/workspace")]),
        ]);
        assert!(merged.is_empty(), "PATH is not declarable: {merged:?}");
    }

    /// A refusal must not leave a tombstone either. A workspace entry that is not allowed to
    /// contribute must not be able to switch off the global value of the same name — that would be
    /// a way to disable a variable without being able to set it.
    #[test]
    fn a_refused_workspace_entry_cannot_mask_the_global_one() {
        // `SERVICE_TOKEN` is refused at the workspace layer only (it travels with the repository),
        // so this is the one case where the two layers can name the same thing.
        let merged = names(vec![
            layer(&[("SERVICE_TOKEN", "from-global")]),
            layer(&[("SERVICE_TOKEN", "from-workspace")]),
        ]);
        assert_eq!(
            merged,
            vec![(
                "SERVICE_TOKEN".into(),
                "from-global".into(),
                EnvSource::Global
            )],
            "the refused workspace entry must not mask what it cannot replace"
        );
    }

    #[test]
    fn the_dangerous_names_and_the_reserved_prefix_are_refused_everywhere() {
        for scope in [EnvScope::Global, EnvScope::Workspace, EnvScope::Session] {
            for name in [
                "PATH",
                "BASH_ENV",
                "LD_PRELOAD",
                "DYLD_INSERT_LIBRARIES",
                "ZLOGIC_OS",
            ] {
                assert!(
                    reject_reason(name, scope).is_err(),
                    "{name} must be refused at {scope:?}"
                );
            }
            for name in ["lowercase_ok", "WITH_1", "_leading"] {
                assert!(
                    reject_reason(name, scope).is_ok(),
                    "{name} must be accepted"
                );
            }
        }
    }

    #[test]
    fn a_credential_shaped_name_is_refused_only_where_it_travels_with_the_repository() {
        let name = "SERVICE_TOKEN";
        assert!(reject_reason(name, EnvScope::Workspace).is_err());
        assert!(reject_reason(name, EnvScope::Global).is_ok());
        assert!(reject_reason(name, EnvScope::Session).is_ok());
    }

    #[test]
    fn a_misspelled_shape_is_refused_rather_than_normalised() {
        assert!(reject_reason("has-dash", EnvScope::Global).is_err());
        assert!(reject_reason("1LEADING_DIGIT", EnvScope::Global).is_err());
        assert!(reject_reason("", EnvScope::Global).is_err());
    }
}
