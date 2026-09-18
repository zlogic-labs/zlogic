//! ```text
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zlogic_config::Dirs;
use zlogic_plugins::{DISABLED_MARKER, PluginDirs};
use zlogic_protocol::WorkspaceId;

use crate::extensions::{Kind as ExtensionKind, Origin as ExtensionOrigin, state::States};

/// Discovery reads frontmatter and loading snapshots the body into the object store. A multi-MB
/// procedure should do neither on the request path.
const MAX_SKILL_BYTES: u64 = 256 * 1024;

const MAX_SKILLS: usize = 100;

const MAX_DESCRIPTION_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    User,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub origin: SkillOrigin,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Skills {
    pub found: Vec<SkillDef>,
    pub problems: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    when_to_use: Option<String>,
}

pub fn discover(dirs: &Dirs, root: &Path, workspace: Option<WorkspaceId>) -> Skills {
    let mut by_name: BTreeMap<String, SkillDef> = BTreeMap::new();
    let mut problems = Vec::new();

    let sources = [
        (dirs.data.join("skills"), SkillOrigin::User),
        (root.join(".zlogic").join("skills"), SkillOrigin::Workspace),
        (root.join(".agents").join("skills"), SkillOrigin::Workspace),
    ];
    for (dir, origin) in sources {
        for def in read_dir_of_skills(&dir, origin, &mut problems) {
            if let Some(previous) = by_name.insert(def.name.clone(), def.clone()) {
                problems.push(format!(
                    "skill {} exists twice: using {}, ignoring {}",
                    def.name,
                    def.path.display(),
                    previous.path.display()
                ));
            }
        }
    }

    // Enabled plugins are another skill source. Namespacing is mandatory: installing a plugin must
    // never silently replace a user's standalone skill with the same frontmatter name.
    let plugins = zlogic_plugins::load(&PluginDirs::under(dirs), Some(root));
    let extension_states = States::load_for(dirs, workspace);
    problems.extend(plugins.problems.iter().map(ToString::to_string));
    for plugin in &plugins.plugins {
        let extension_origin = ExtensionOrigin::Plugin {
            plugin: plugin.id.clone(),
            workspace: plugin.from_workspace,
        };
        if !extension_states
            .verdict(
                ExtensionKind::Plugin,
                &plugin.id,
                &extension_origin,
                plugin.enabled,
            )
            .enabled
        {
            continue;
        }
        let origin = if plugin.from_workspace {
            SkillOrigin::Workspace
        } else {
            SkillOrigin::User
        };
        for mut def in read_dir_of_skills(&plugin.root.join("skills"), origin, &mut problems) {
            def.name = format!("{}:{}", plugin.id, def.name);
            by_name.insert(def.name.clone(), def);
        }
    }

    let mut found: Vec<SkillDef> = by_name.into_values().collect();
    if found.len() > MAX_SKILLS {
        problems.push(format!(
            "found {} skills; only listing the first {MAX_SKILLS} (more than that and the \
             directory itself becomes noise)",
            found.len()
        ));
        found.truncate(MAX_SKILLS);
    }
    Skills { found, problems }
}

fn read_dir_of_skills(
    dir: &Path,
    origin: SkillOrigin,
    problems: &mut Vec<String>,
) -> Vec<SkillDef> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(DISABLED_MARKER).exists() {
            continue;
        }
        let file = path.join("SKILL.md");
        if !file.is_file() {
            continue;
        }
        let fallback_name = entry.file_name().to_string_lossy().to_string();
        match read_skill(&file, &fallback_name, origin) {
            Ok(def) => out.push(def),
            Err(why) => problems.push(format!("failed to load {}: {why}", file.display())),
        }
    }
    out
}

pub fn inspect_skill_dir(root: &Path, origin: SkillOrigin) -> Result<SkillDef, String> {
    let fallback = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("skill directory name is not valid UTF-8")?;
    read_skill(&root.join("SKILL.md"), fallback, origin)
}

fn read_skill(file: &Path, fallback_name: &str, origin: SkillOrigin) -> Result<SkillDef, String> {
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size > MAX_SKILL_BYTES {
        return Err(format!(
            "SKILL.md is {size} bytes, over {MAX_SKILL_BYTES} — frontmatter should not be this large"
        ));
    }
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let front = frontmatter(text)
        .ok_or("the file does not start with a `---`-delimited YAML frontmatter")?;
    let parsed: Frontmatter =
        serde_yaml_ng::from_str(front).map_err(|e| format!("failed to parse frontmatter: {e}"))?;

    let description = parsed
        .description
        .or(parsed.when_to_use)
        .map(|d| trim_to(d.trim(), MAX_DESCRIPTION_CHARS))
        .filter(|d| !d.is_empty())
        .ok_or(
            "no description in the frontmatter (the model uses it to decide when to load the \
        skill)",
        )?;
    reject_invisible(&description, "description")?;

    let name = parsed
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    reject_invisible(&name, "name")?;

    Ok(SkillDef {
        name,
        description,
        path: file.to_path_buf(),
        origin,
    })
}

/// Reject text a model could smuggle past a human reader: control characters and the invisible
/// formatting characters of the Unicode standard.
///
/// Shared with the closed half, which validates template metadata the same way.
pub fn reject_invisible(value: &str, field: &str) -> Result<(), String> {
    let bad = value.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{00ad}'
                    | '\u{034f}'
                    | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    });
    if bad {
        Err(format!(
            "the {field} in the frontmatter contains invisible control characters"
        ))
    } else {
        Ok(())
    }
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---")?
        .trim_start_matches(['\r'])
        .strip_prefix('\n')?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end..]
        .trim_start_matches(['\r', '\n'])
        .strip_prefix("---")
        .unwrap_or("")
        .trim_start_matches(['\r', '\n']);
    Some((front, body))
}

fn frontmatter(text: &str) -> Option<&str> {
    split_frontmatter(text).map(|(front, _)| front)
}

fn trim_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    fn dirs_under(base: &Path) -> Dirs {
        Dirs::under(base)
    }

    fn write_plugin_skill(dirs: &Dirs, plugin: &str, skill: &str) {
        let root = PluginDirs::under(dirs).global.join(plugin);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("plugin.json"),
            format!(r#"{{"name":"{plugin}"}}"#),
        )
        .unwrap();
        write_skill(
            &root.join("skills"),
            skill,
            "---\ndescription: plugin skill\n---\n",
        );
    }

    #[test]
    fn a_skill_is_a_name_a_description_and_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let root = tmp.path().join("repo");
        write_skill(
            &dirs.data.join("skills"),
            "pdf-forms",
            "---\nname: pdf-forms\ndescription: Fill and flatten PDF forms\n---\n\n# body\nlots of text\n",
        );

        let skills = discover(&dirs, &root, None);
        assert_eq!(skills.problems, Vec::<String>::new());
        assert_eq!(skills.found.len(), 1);
        let s = &skills.found[0];
        assert_eq!(s.name, "pdf-forms");
        assert_eq!(s.description, "Fill and flatten PDF forms");
        assert!(s.path.ends_with("pdf-forms/SKILL.md"));
        assert_eq!(s.origin, SkillOrigin::User);
    }

    #[test]
    fn a_skill_without_a_description_is_refused_and_explained() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        write_skill(
            &dirs.data.join("skills"),
            "mute",
            "---\nname: mute\n---\nbody\n",
        );

        let skills = discover(&dirs, &tmp.path().join("repo"), None);
        assert!(skills.found.is_empty());
        assert!(
            skills.problems[0].contains("description"),
            "{:?}",
            skills.problems
        );
    }

    #[test]
    fn when_to_use_stands_in_for_a_missing_description() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        write_skill(
            &dirs.data.join("skills"),
            "x",
            "---\nname: x\nwhen_to_use: after a release\n---\nbody\n",
        );
        let skills = discover(&dirs, &tmp.path().join("repo"), None);
        assert_eq!(skills.found[0].description, "after a release");
    }

    #[test]
    fn the_directory_name_stands_in_for_a_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        write_skill(
            &dirs.data.join("skills"),
            "release-notes",
            "---\ndescription: d\n---\n",
        );
        let skills = discover(&dirs, &tmp.path().join("repo"), None);
        assert_eq!(skills.found[0].name, "release-notes");
    }

    #[test]
    fn a_workspace_skill_overrides_the_users_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let root = tmp.path().join("repo");
        write_skill(
            &dirs.data.join("skills"),
            "review",
            "---\ndescription: mine\n---\n",
        );
        write_skill(
            &root.join(".zlogic").join("skills"),
            "review",
            "---\ndescription: theirs\n---\n",
        );

        let skills = discover(&dirs, &root, None);
        assert_eq!(skills.found.len(), 1);
        assert_eq!(skills.found[0].description, "theirs", "the repo wins");
        assert_eq!(skills.found[0].origin, SkillOrigin::Workspace);
        assert!(
            skills.problems[0].contains("twice"),
            "{:?}",
            skills.problems
        );
    }

    #[test]
    fn the_agents_skill_location_is_read_too() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let root = tmp.path().join("repo");
        write_skill(
            &root.join(".agents").join("skills"),
            "portable",
            "---\ndescription: d\n---\n",
        );
        assert_eq!(discover(&dirs, &root, None).found.len(), 1);
    }

    #[test]
    fn a_personally_disabled_plugin_contributes_no_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let root = tmp.path().join("repo");
        let workspace = WorkspaceId::new();
        write_plugin_skill(&dirs, "acme", "review");

        assert_eq!(
            discover(&dirs, &root, Some(workspace)).found[0].name,
            "acme:review"
        );

        crate::extensions::state::set_personal(
            &dirs,
            workspace,
            ExtensionKind::Plugin,
            "acme",
            Some(false),
        )
        .unwrap();
        assert!(
            discover(&dirs, &root, Some(workspace)).found.is_empty(),
            "the same plugin switch must gate its skills as well as its MCP servers"
        );
        assert_eq!(
            discover(&dirs, &root, Some(WorkspaceId::new())).found.len(),
            1,
            "a personal override must remain scoped to its workspace"
        );
    }

    #[test]
    fn nothing_installed_is_not_a_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = discover(&dirs_under(tmp.path()), &tmp.path().join("repo"), None);
        assert_eq!(skills, Skills::default());
    }

    #[test]
    fn the_order_is_by_name_not_by_readdir() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        for name in ["zulu", "alpha", "mike"] {
            write_skill(
                &dirs.data.join("skills"),
                name,
                "---\ndescription: d\n---\n",
            );
        }
        let names: Vec<String> = discover(&dirs, &tmp.path().join("repo"), None)
            .found
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, ["alpha", "mike", "zulu"]);
    }

    #[test]
    fn only_the_leading_block_is_parsed_as_yaml() {
        let text = "---\nname: a\ndescription: d\n---\n\n## Notes\nkey: not yaml\n  - weird\n";
        assert_eq!(frontmatter(text).unwrap(), "name: a\ndescription: d");
    }

    #[test]
    fn a_file_without_frontmatter_is_refused_with_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        write_skill(&dirs.data.join("skills"), "bare", "# just markdown\n");
        let skills = discover(&dirs, &tmp.path().join("repo"), None);
        assert!(skills.found.is_empty());
        assert!(
            skills.problems[0].contains("frontmatter"),
            "{:?}",
            skills.problems
        );
    }

    #[test]
    fn a_long_description_is_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        write_skill(
            &dirs.data.join("skills"),
            "long",
            &format!("---\ndescription: {long}\n---\n"),
        );
        let d = &discover(&dirs, &tmp.path().join("repo"), None).found[0].description;
        assert_eq!(
            d.chars().count(),
            MAX_DESCRIPTION_CHARS + 1,
            "truncated with a single ellipsis"
        );
        assert!(d.ends_with('…'));
    }
}

pub struct SkillLibrary {
    dirs: Dirs,
}

pub struct SessionSkills {
    dirs: Dirs,
    root: PathBuf,
    workspace: WorkspaceId,
}

impl SkillLibrary {
    pub fn new(dirs: Dirs) -> Self {
        Self { dirs }
    }

    pub fn host(&self, workspace: WorkspaceId, root: impl Into<PathBuf>) -> Arc<SessionSkills> {
        Arc::new(SessionSkills {
            dirs: self.dirs.clone(),
            root: root.into(),
            workspace,
        })
    }
}

const UNSUPPORTED_FIELDS: &[(&str, &str)] = &[
    ("allowed_tools", "allowed_tools (pre-authorisation)"),
    ("context", "context: fork (running in a sub-agent)"),
    ("model", "model / effort overrides"),
    ("effort", "model / effort overrides"),
];

#[async_trait]
impl zlogic_tools::SkillHost for SessionSkills {
    async fn available(&self) -> Vec<String> {
        discover(&self.dirs, &self.root, Some(self.workspace))
            .found
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    async fn load(&self, name: &str) -> std::result::Result<zlogic_tools::LoadedSkill, String> {
        let found = discover(&self.dirs, &self.root, Some(self.workspace))
            .found
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("skill {name} is no longer in the library"))?;

        let text = std::fs::read_to_string(&found.path)
            .map_err(|e| format!("cannot read {}: {e}", found.path.display()))?;
        let body = body_after_frontmatter(&text);
        Ok(zlogic_tools::LoadedSkill {
            name: found.name,
            revision: format!("{:x}", Sha256::digest(text.as_bytes())),
            raw_body: body.to_string(),
            path: found.path.display().to_string(),
            unsupported: unsupported_fields(&text),
        })
    }
}

fn body_after_frontmatter(text: &str) -> &str {
    split_frontmatter(text)
        .map(|(_, body)| body)
        .unwrap_or(text)
}

fn unsupported_fields(text: &str) -> Vec<String> {
    let Some(front) = frontmatter(text) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for (key, label) in UNSUPPORTED_FIELDS {
        let declared = front
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{key}:")));
        if declared && !out.iter().any(|existing| existing == label) {
            out.push((*label).to_string());
        }
    }
    out
}
