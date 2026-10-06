//! ```yaml
//! tools:
//!   shell:
//!     test_secs: 1800
//!     stall_secs: 1200
//!   search:
//!     timeout_secs: 300
//! ```
//!
//! Per-workspace half of the global tool budgets. Only the keys a workspace names are overridden,
//! so a project that cares about a slow suite can raise that one number without restating the
//! other five.

use std::path::Path;

use serde::Deserialize;
use zlogic_protocol::settings::{EnvConfig, ShellConfig};

/// The enclosing document. Unknown sections are ignored rather than rejected: this file is shared
/// with settings this module does not read, and a `locale` key written by another feature must not
/// make the shell budgets look broken.
#[derive(Debug, Default, Deserialize)]
struct SettingsDocument {
    tools: Option<ToolsSection>,
    env: Option<EnvConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ToolsSection {
    shell: Option<ShellOverrides>,
    search: Option<SearchOverrides>,
}

/// The search budget a workspace names. A nested block rather than a bare
/// `tools.search_timeout_secs` so it reads like `tools.shell` and has somewhere to put the next
/// search limit without changing this file's shape again.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchOverrides {
    pub timeout_secs: Option<u64>,
}

impl SearchOverrides {
    pub fn is_empty(&self) -> bool {
        self.timeout_secs.is_none()
    }

    /// The global configuration with this workspace's numbers laid over it.
    pub fn apply(&self, base: &zlogic_tools::SearchBudgets) -> zlogic_tools::SearchBudgets {
        let mut out = *base;
        if let Some(v) = self.timeout_secs {
            out.timeout = std::time::Duration::from_secs(v);
        }
        out
    }
}

/// Six optional overrides.
///
/// `deny_unknown_fields` here, unlike on the document: a misspelled `test_sec` would otherwise be
/// silently ignored and the suite would be killed at the default ten minutes while the config
/// looked exactly right.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellOverrides {
    pub quick_secs: Option<u64>,
    pub test_secs: Option<u64>,
    pub build_secs: Option<u64>,
    pub wait_secs: Option<u64>,
    pub stall_secs: Option<u64>,
    pub progress_secs: Option<u64>,
}

impl ShellOverrides {
    pub fn is_empty(&self) -> bool {
        self.quick_secs.is_none()
            && self.test_secs.is_none()
            && self.build_secs.is_none()
            && self.wait_secs.is_none()
            && self.stall_secs.is_none()
            && self.progress_secs.is_none()
    }

    /// The global configuration with this workspace's numbers laid over it.
    pub fn apply(&self, base: &ShellConfig) -> ShellConfig {
        let mut out = base.clone();
        macro_rules! take {
            ($($field:ident),+ $(,)?) => {
                $(if let Some(v) = self.$field { out.$field = v; })+
            };
        }
        take!(
            quick_secs,
            test_secs,
            build_secs,
            wait_secs,
            stall_secs,
            progress_secs,
        );
        out
    }
}

/// Reads `<root>/.zlogic/settings.yaml`. A workspace that names no shell or search key yields
/// `None` for both and no warning, which is the common case and must stay silent.
pub fn load(root: &Path) -> (WorkspaceOverrides, Vec<String>) {
    let (document, warnings) = read_document(root, "tool budgets not applied");
    let mut out = WorkspaceOverrides::default();
    // A file that parses but names no budget key leaves the global numbers alone, which is the
    // common case and must stay silent.
    if let Some(tools) = document.and_then(|document| document.tools) {
        out.shell = tools.shell.filter(|o| !o.is_empty());
        out.search = tools.search.filter(|o| !o.is_empty());
    }
    (out, warnings)
}

/// What a workspace named, per tool family. Default is "named nothing".
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkspaceOverrides {
    pub shell: Option<ShellOverrides>,
    pub search: Option<SearchOverrides>,
}

/// The same file's `env:` block — the workspace layer of the shell's variables.
///
/// `deny_unknown_fields` on [`EnvConfig`] does the real work here: a misspelled key inside `env:`
/// invalidates the block rather than being dropped, which is the same trade the shell overrides
/// make and for the same reason. The *file* around it stays permissive, so a bad `env:` does not
/// also cost the project its shell budgets — the caller decides what to do with the warning.
pub fn load_env(root: &Path, warnings: &mut Vec<String>) -> EnvConfig {
    let (document, mut read_warnings) = read_document(root, "workspace variables not applied");
    warnings.append(&mut read_warnings);
    document
        .and_then(|document| document.env)
        .unwrap_or_default()
}

fn read_document(root: &Path, consequence: &str) -> (Option<SettingsDocument>, Vec<String>) {
    let path = root.join(".zlogic").join("settings.yaml");
    if !path.exists() {
        return (None, Vec::new());
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            return (
                None,
                vec![format!(
                    "cannot read {}: {e} ({consequence})",
                    path.display()
                )],
            );
        }
    };
    match serde_yaml_ng::from_str::<SettingsDocument>(&text) {
        Ok(document) => (Some(document), Vec::new()),
        Err(e) => (None, vec![format!("cannot parse {}: {e}", path.display())]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".zlogic")).unwrap();
        std::fs::write(dir.path().join(".zlogic/settings.yaml"), body).unwrap();
        dir
    }

    #[test]
    fn no_settings_file_means_no_override_and_no_complaint() {
        let dir = tempfile::tempdir().unwrap();
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.shell.is_none(), "{overrides:?}");
        assert!(warnings.is_empty());
    }

    #[test]
    fn one_key_overrides_and_the_five_others_stay_global() {
        let dir = root_with("tools:\n  shell:\n    test_secs: 1800\n");
        let (overrides, warnings) = load(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let base = ShellConfig {
            read_profile: true,
            quick_secs: 90,
            test_secs: 600,
            build_secs: 1_500,
            wait_secs: 2_400,
            stall_secs: 900,
            progress_secs: 45,
        };
        let merged = overrides
            .shell
            .expect("one key is an override")
            .apply(&base);
        assert_eq!(merged.test_secs, 1_800, "the named key is not applied");
        assert_eq!(merged.quick_secs, 90, "an unnamed key must stay global");
        assert_eq!(merged.build_secs, 1_500);
        assert_eq!(merged.wait_secs, 2_400);
        assert_eq!(merged.stall_secs, 900);
        assert_eq!(merged.progress_secs, 45);
    }

    #[test]
    fn a_workspace_can_lower_a_budget_as_well_as_raise_one() {
        let dir = root_with("tools:\n  shell:\n    quick_secs: 10\n");
        let merged = load(dir.path())
            .0
            .shell
            .expect("a named key is an override")
            .apply(&ShellConfig::default());
        assert_eq!(merged.quick_secs, 10);
        assert!(merged.validate().is_ok());
    }

    #[test]
    fn a_settings_file_with_no_shell_keys_is_not_an_override() {
        let dir = root_with("locale: zh-CN\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.shell.is_none(), "{overrides:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_misspelled_key_is_reported_rather_than_silently_dropped() {
        let dir = root_with("tools:\n  shell:\n    test_sec: 1800\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.shell.is_none(), "{overrides:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("test_sec"), "{}", warnings[0]);
    }

    #[test]
    fn unreadable_yaml_names_the_file_instead_of_falling_back_silently() {
        let dir = root_with("tools: [unclosed\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.shell.is_none(), "{overrides:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("settings.yaml"), "{}", warnings[0]);
    }

    #[test]
    fn a_search_budget_overrides_the_global_one() {
        let dir = root_with("tools:\n  search:\n    timeout_secs: 300\n");
        let (overrides, warnings) = load(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let base = zlogic_tools::SearchBudgets {
            timeout: std::time::Duration::from_secs(60),
        };
        let merged = overrides
            .search
            .expect("a named key is an override")
            .apply(&base);
        assert_eq!(merged.timeout, std::time::Duration::from_secs(300));
    }

    /// The two blocks are independent: a workspace that only cares about search time must not have
    /// to restate — or lose — the shell budgets, and vice versa.
    #[test]
    fn the_two_blocks_are_read_independently() {
        let dir = root_with("tools:\n  search:\n    timeout_secs: 300\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.search.is_some());
        assert!(overrides.shell.is_none(), "{overrides:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_misspelled_search_key_is_reported_rather_than_silently_dropped() {
        let dir = root_with("tools:\n  search:\n    timeout_sec: 300\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.search.is_none(), "{overrides:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("timeout_sec"), "{}", warnings[0]);
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    fn root_with(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".zlogic")).unwrap();
        std::fs::write(dir.path().join(".zlogic/settings.yaml"), body).unwrap();
        dir
    }

    fn loaded(body: &str) -> (EnvConfig, Vec<String>) {
        let dir = root_with(body);
        let mut warnings = Vec::new();
        let env = load_env(dir.path(), &mut warnings);
        (env, warnings)
    }

    #[test]
    fn a_workspace_env_block_reads_both_spellings() {
        let (env, warnings) = loaded(
            "env:\n  variables:\n    A: one\n    B:\n      value: two\n      enabled: false\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(env.variables["A"].detail().value, "one");
        assert!(
            env.variables["A"].detail().enabled,
            "the shorthand means on"
        );
        let b = env.variables["B"].detail();
        assert_eq!((b.value, b.enabled), ("two".into(), false));
    }

    /// The file is shared with the shell budgets, so a bad `env:` block must not cost the project
    /// its other settings — and a bad `env:` block must not be silently half-applied either.
    #[test]
    fn a_misspelled_key_invalidates_the_block_and_says_so() {
        let (env, warnings) = loaded("env:\n  variables:\n    A:\n      valu: one\n");
        assert!(env.variables.is_empty(), "a bad block contributes nothing");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("settings.yaml"), "{}", warnings[0]);
    }

    #[test]
    fn a_project_with_no_env_block_is_silent() {
        let (env, warnings) = loaded("tools:\n  shell:\n    test_secs: 1800\n");
        assert!(env.variables.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
        // The shell budgets in the same file must still be readable.
        let dir = root_with("tools:\n  shell:\n    test_secs: 1800\n");
        assert!(load(dir.path()).0.shell.is_some());
    }

    #[test]
    fn a_layer_absent_entirely_is_the_default_and_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut warnings = Vec::new();
        let env = load_env(dir.path(), &mut warnings);
        assert!(env.variables.is_empty());
        assert!(warnings.is_empty());
    }
}
