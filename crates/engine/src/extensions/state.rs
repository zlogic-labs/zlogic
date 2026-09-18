//! ```text
//! ```
//! ```text
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zlogic_config::Dirs;
use zlogic_mcp::Origin;
use zlogic_protocol::WorkspaceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mcp,
    Plugin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decided {
    Personal,
    Global,
    Shipped,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub enabled: bool,
    pub decided_by: Decided,
}

#[derive(Debug, Default)]
pub struct States {
    global: GlobalFile,
    workspace: WorkspaceFile,
}

impl States {
    pub fn load(dirs: &Dirs, workspace: WorkspaceId) -> Self {
        Self::load_for(dirs, Some(workspace))
    }

    /// Load extension switches when a caller may only have a workspace path.
    /// Global state still applies without a registered workspace id; only the personal override
    /// layer is absent. This is used by read-only extension catalog inspection of an arbitrary
    /// folder, while turns and session-bound skill loading always pass their real workspace id.
    pub fn load_for(dirs: &Dirs, workspace: Option<WorkspaceId>) -> Self {
        Self {
            global: read_or_default(&global_path(dirs)),
            workspace: workspace
                .map(|workspace| read_or_default(&workspace_path(dirs, workspace)))
                .unwrap_or_default(),
        }
    }

    pub fn verdict(&self, kind: Kind, id: &str, origin: &Origin, shipped: Option<bool>) -> Verdict {
        if let Some(&enabled) = self.workspace.map(kind).get(id) {
            return Verdict {
                enabled,
                decided_by: Decided::Personal,
            };
        }
        if !origin.is_from_workspace()
            && let Some(&enabled) = self.global.map(kind).get(id)
        {
            return Verdict {
                enabled,
                decided_by: Decided::Global,
            };
        }
        match shipped {
            Some(enabled) => Verdict {
                enabled,
                decided_by: Decided::Shipped,
            },
            None => Verdict {
                enabled: true,
                decided_by: Decided::Default,
            },
        }
    }

    pub fn personal(&self, kind: Kind, id: &str) -> Option<bool> {
        self.workspace.map(kind).get(id).copied()
    }

    pub fn global(&self, kind: Kind, id: &str) -> Option<bool> {
        self.global.map(kind).get(id).copied()
    }

    pub fn trusted_fingerprint(&self, id: &str) -> Option<&str> {
        self.workspace.trusted.get(id).map(String::as_str)
    }
}

pub fn set_global(dirs: &Dirs, kind: Kind, id: &str, enabled: Option<bool>) -> std::io::Result<()> {
    update(&global_path(dirs), |file: &mut GlobalFile| {
        put(file.map_mut(kind), id, enabled);
    })
}

pub fn set_personal(
    dirs: &Dirs,
    workspace: WorkspaceId,
    kind: Kind,
    id: &str,
    enabled: Option<bool>,
) -> std::io::Result<()> {
    update(
        &workspace_path(dirs, workspace),
        |file: &mut WorkspaceFile| {
            put(file.map_mut(kind), id, enabled);
        },
    )
}

pub fn set_trusted(
    dirs: &Dirs,
    workspace: WorkspaceId,
    id: &str,
    fingerprint: Option<&str>,
) -> std::io::Result<()> {
    update(
        &workspace_path(dirs, workspace),
        |file: &mut WorkspaceFile| match fingerprint {
            Some(fp) => {
                file.trusted.insert(id.to_string(), fp.to_string());
            }
            None => {
                file.trusted.remove(id);
            }
        },
    )
}

pub fn forget(
    dirs: &Dirs,
    workspace: Option<WorkspaceId>,
    kind: Kind,
    id: &str,
) -> std::io::Result<()> {
    update(&global_path(dirs), |file: &mut GlobalFile| {
        file.map_mut(kind).remove(id);
    })?;
    if let Some(workspace) = workspace {
        update(
            &workspace_path(dirs, workspace),
            |file: &mut WorkspaceFile| {
                file.map_mut(kind).remove(id);
                file.trusted.remove(id);
            },
        )?;
    }
    Ok(())
}

pub fn global_path(dirs: &Dirs) -> PathBuf {
    dirs.data.join("extensions").join("state.json")
}

pub fn workspace_path(dirs: &Dirs, workspace: WorkspaceId) -> PathBuf {
    dirs.workspace(&workspace.to_string())
        .join("extensions.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct GlobalFile {
    mcp: BTreeMap<String, bool>,
    plugins: BTreeMap<String, bool>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WorkspaceFile {
    mcp: BTreeMap<String, bool>,
    plugins: BTreeMap<String, bool>,
    trusted: BTreeMap<String, String>,
}

impl GlobalFile {
    fn map(&self, kind: Kind) -> &BTreeMap<String, bool> {
        match kind {
            Kind::Mcp => &self.mcp,
            Kind::Plugin => &self.plugins,
        }
    }
    fn map_mut(&mut self, kind: Kind) -> &mut BTreeMap<String, bool> {
        match kind {
            Kind::Mcp => &mut self.mcp,
            Kind::Plugin => &mut self.plugins,
        }
    }
}

impl WorkspaceFile {
    fn map(&self, kind: Kind) -> &BTreeMap<String, bool> {
        match kind {
            Kind::Mcp => &self.mcp,
            Kind::Plugin => &self.plugins,
        }
    }
    fn map_mut(&mut self, kind: Kind) -> &mut BTreeMap<String, bool> {
        match kind {
            Kind::Mcp => &mut self.mcp,
            Kind::Plugin => &mut self.plugins,
        }
    }
}

fn put(map: &mut BTreeMap<String, bool>, id: &str, value: Option<bool>) {
    match value {
        Some(v) => {
            map.insert(id.to_string(), v);
        }
        None => {
            map.remove(id);
        }
    }
}

fn read_or_default<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> T {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return T::default();
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "zlogic::engine", path = %path.display(), "could not read extension state file, treating as empty: {e}");
            T::default()
        }
    }
}

fn update<T: Default + Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
    change: impl FnOnce(&mut T),
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file: T = read_or_default(path);
    change(&mut file);
    let body = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, Dirs) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        (tmp, dirs)
    }

    #[test]
    fn nothing_configured_means_enabled() {
        let (_t, dirs) = dirs();
        let states = States::load(&dirs, WorkspaceId::new());
        let v = states.verdict(Kind::Mcp, "gh", &Origin::Global, None);
        assert!(v.enabled);
        assert_eq!(v.decided_by, Decided::Default);
    }

    #[test]
    fn the_definitions_own_field_is_the_next_layer_out() {
        let (_t, dirs) = dirs();
        let states = States::load(&dirs, WorkspaceId::new());
        let v = states.verdict(Kind::Mcp, "gh", &Origin::Workspace, Some(false));
        assert!(!v.enabled);
        assert_eq!(v.decided_by, Decided::Shipped);
    }

    #[test]
    fn the_global_switch_wins_over_the_shipped_default() {
        let (_t, dirs) = dirs();
        set_global(&dirs, Kind::Mcp, "gh", Some(false)).unwrap();
        let states = States::load(&dirs, WorkspaceId::new());
        let v = states.verdict(Kind::Mcp, "gh", &Origin::Global, Some(true));
        assert!(!v.enabled);
        assert_eq!(v.decided_by, Decided::Global);
    }

    #[test]
    fn the_global_switch_does_not_reach_into_the_repository() {
        let (_t, dirs) = dirs();
        set_global(&dirs, Kind::Mcp, "gh", Some(false)).unwrap();
        let states = States::load(&dirs, WorkspaceId::new());

        let repo = states.verdict(Kind::Mcp, "gh", &Origin::Workspace, None);
        assert!(
            repo.enabled,
            "a same-named definition inside the repository must not be switched off by someone else's switch"
        );
        assert_eq!(repo.decided_by, Decided::Default);

        let from_repo_plugin = states.verdict(
            Kind::Mcp,
            "gh",
            &Origin::Plugin {
                plugin: "p".into(),
                workspace: true,
            },
            None,
        );
        assert!(from_repo_plugin.enabled);
    }

    #[test]
    fn a_personal_override_wins_over_everything() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_global(&dirs, Kind::Mcp, "gh", Some(false)).unwrap();
        set_personal(&dirs, ws, Kind::Mcp, "gh", Some(true)).unwrap();

        let states = States::load(&dirs, ws);
        let v = states.verdict(Kind::Mcp, "gh", &Origin::Global, Some(false));
        assert!(
            v.enabled,
            "this machine, in this project, it is wanted on purpose"
        );
        assert_eq!(v.decided_by, Decided::Personal);
    }

    #[test]
    fn clearing_an_override_returns_to_inheritance_and_is_not_the_same_as_off() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_global(&dirs, Kind::Mcp, "gh", Some(true)).unwrap();

        set_personal(&dirs, ws, Kind::Mcp, "gh", Some(false)).unwrap();
        let states = States::load(&dirs, ws);
        assert_eq!(states.personal(Kind::Mcp, "gh"), Some(false));
        assert!(
            !states
                .verdict(Kind::Mcp, "gh", &Origin::Global, None)
                .enabled
        );

        set_personal(&dirs, ws, Kind::Mcp, "gh", None).unwrap();
        let states = States::load(&dirs, ws);
        assert_eq!(
            states.personal(Kind::Mcp, "gh"),
            None,
            "remove the key, do not write false"
        );
        let v = states.verdict(Kind::Mcp, "gh", &Origin::Global, None);
        assert!(v.enabled);
        assert_eq!(v.decided_by, Decided::Global, "back to the layer above");
    }

    #[test]
    fn a_personal_override_belongs_to_one_workspace() {
        let (_t, dirs) = dirs();
        let (a, b) = (WorkspaceId::new(), WorkspaceId::new());
        set_personal(&dirs, a, Kind::Mcp, "gh", Some(false)).unwrap();

        assert!(
            !States::load(&dirs, a)
                .verdict(Kind::Mcp, "gh", &Origin::Global, None)
                .enabled
        );
        assert!(
            States::load(&dirs, b)
                .verdict(Kind::Mcp, "gh", &Origin::Global, None)
                .enabled
        );
    }

    #[test]
    fn plugins_and_servers_have_separate_id_spaces() {
        let (_t, dirs) = dirs();
        set_global(&dirs, Kind::Plugin, "same-name", Some(false)).unwrap();
        let states = States::load(&dirs, WorkspaceId::new());

        assert!(
            !states
                .verdict(Kind::Plugin, "same-name", &Origin::Global, None)
                .enabled
        );
        assert!(
            states
                .verdict(Kind::Mcp, "same-name", &Origin::Global, None)
                .enabled
        );
    }

    #[test]
    fn trust_is_recorded_against_the_content_hash() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_trusted(&dirs, ws, "repo-srv", Some("abc123")).unwrap();

        let states = States::load(&dirs, ws);
        assert_eq!(states.trusted_fingerprint("repo-srv"), Some("abc123"));
        assert_ne!(states.trusted_fingerprint("repo-srv"), Some("def456"));
        assert_eq!(states.trusted_fingerprint("someone-else"), None);
    }

    #[test]
    fn trust_can_be_withdrawn() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_trusted(&dirs, ws, "s", Some("abc")).unwrap();
        set_trusted(&dirs, ws, "s", None).unwrap();
        assert_eq!(States::load(&dirs, ws).trusted_fingerprint("s"), None);
    }

    #[test]
    fn forgetting_an_extension_clears_every_key_it_left_behind() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_global(&dirs, Kind::Mcp, "gone", Some(false)).unwrap();
        set_personal(&dirs, ws, Kind::Mcp, "gone", Some(false)).unwrap();
        set_trusted(&dirs, ws, "gone", Some("abc")).unwrap();

        forget(&dirs, Some(ws), Kind::Mcp, "gone").unwrap();

        let states = States::load(&dirs, ws);
        assert_eq!(states.global(Kind::Mcp, "gone"), None);
        assert_eq!(states.personal(Kind::Mcp, "gone"), None);
        assert_eq!(states.trusted_fingerprint("gone"), None);
        assert!(
            states
                .verdict(Kind::Mcp, "gone", &Origin::Global, None)
                .enabled,
            "back to a pristine state"
        );
    }

    #[test]
    fn the_two_files_stay_separate() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_personal(&dirs, ws, Kind::Mcp, "gh", Some(false)).unwrap();

        let global = std::fs::read_to_string(global_path(&dirs)).unwrap_or_default();
        assert!(
            !global.contains("gh"),
            "personal preferences must not land in the global file: {global}"
        );
        assert!(workspace_path(&dirs, ws).is_file());
    }

    #[test]
    fn a_corrupt_state_file_reads_as_empty() {
        let (_t, dirs) = dirs();
        let path = global_path(&dirs);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let states = States::load(&dirs, WorkspaceId::new());
        assert!(
            states
                .verdict(Kind::Mcp, "gh", &Origin::Global, None)
                .enabled
        );
        assert_eq!(states.global(Kind::Mcp, "gh"), None);

        set_global(&dirs, Kind::Mcp, "gh", Some(false)).unwrap();
        assert_eq!(
            States::load(&dirs, WorkspaceId::new()).global(Kind::Mcp, "gh"),
            Some(false)
        );
    }

    #[test]
    fn writing_one_key_keeps_the_others() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        set_personal(&dirs, ws, Kind::Mcp, "a", Some(false)).unwrap();
        set_personal(&dirs, ws, Kind::Mcp, "b", Some(true)).unwrap();
        set_trusted(&dirs, ws, "c", Some("fp")).unwrap();

        let states = States::load(&dirs, ws);
        assert_eq!(states.personal(Kind::Mcp, "a"), Some(false));
        assert_eq!(states.personal(Kind::Mcp, "b"), Some(true));
        assert_eq!(states.trusted_fingerprint("c"), Some("fp"));
    }

    #[test]
    fn writing_leaves_no_temporary_file_behind() {
        let (_t, dirs) = dirs();
        set_global(&dirs, Kind::Mcp, "gh", Some(true)).unwrap();
        let dir = global_path(&dirs).parent().unwrap().to_path_buf();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
