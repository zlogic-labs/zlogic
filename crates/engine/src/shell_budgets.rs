//! ```yaml
//! tools:
//!   shell:
//!     test_secs: 1800
//!     stall_secs: 1200
//! ```
//!
//! Per-workspace half of the global `tools.shell` block. Only the keys a workspace names are
//! overridden, so a project that cares about a slow suite can raise that one number without
//! restating the other five.

use std::path::Path;

use serde::Deserialize;
use zlogic_protocol::settings::ShellConfig;

/// The enclosing document. Unknown sections are ignored rather than rejected: this file is shared
/// with settings this module does not read, and a `locale` key written by another feature must not
/// make the shell budgets look broken.
#[derive(Debug, Default, Deserialize)]
struct SettingsDocument {
    tools: Option<ToolsSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ToolsSection {
    shell: Option<ShellOverrides>,
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

/// Reads `<root>/.zlogic/settings.yaml`. A workspace that names no shell key yields `None` and no
/// warning, which is the common case and must stay silent.
pub fn load(root: &Path) -> (Option<ShellOverrides>, Vec<String>) {
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
                    "cannot read {}: {e} (shell budgets not applied)",
                    path.display()
                )],
            );
        }
    };
    let document: SettingsDocument = match serde_yaml_ng::from_str(&text) {
        Ok(document) => document,
        Err(e) => {
            return (None, vec![format!("cannot parse {}: {e}", path.display())]);
        }
    };
    // A file that parses but names no shell key leaves the global numbers alone, which is the
    // common case and must stay silent.
    (
        document
            .tools
            .and_then(|tools| tools.shell)
            .filter(|overrides| !overrides.is_empty()),
        Vec::new(),
    )
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
        assert!(overrides.is_none());
        assert!(warnings.is_empty());
    }

    #[test]
    fn one_key_overrides_and_the_five_others_stay_global() {
        let dir = root_with("tools:\n  shell:\n    test_secs: 1800\n");
        let (overrides, warnings) = load(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let base = ShellConfig {
            quick_secs: 90,
            test_secs: 600,
            build_secs: 1_500,
            wait_secs: 2_400,
            stall_secs: 900,
            progress_secs: 45,
        };
        let merged = overrides.expect("one key is an override").apply(&base);
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
        let merged = load(dir.path()).0.unwrap().apply(&ShellConfig::default());
        assert_eq!(merged.quick_secs, 10);
        assert!(merged.validate().is_ok());
    }

    #[test]
    fn a_settings_file_with_no_shell_keys_is_not_an_override() {
        let dir = root_with("locale: zh-CN\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.is_none());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_misspelled_key_is_reported_rather_than_silently_dropped() {
        let dir = root_with("tools:\n  shell:\n    test_sec: 1800\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("test_sec"), "{}", warnings[0]);
    }

    #[test]
    fn unreadable_yaml_names_the_file_instead_of_falling_back_silently() {
        let dir = root_with("tools: [unclosed\n");
        let (overrides, warnings) = load(dir.path());
        assert!(overrides.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("settings.yaml"), "{}", warnings[0]);
    }
}
