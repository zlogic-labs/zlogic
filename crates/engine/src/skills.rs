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

use crate::extensions::{Kind as ExtensionKind, Origin, Origin as ExtensionOrigin, state::States};

/// Discovery reads frontmatter and loading snapshots the body into the object store. A multi-MB
/// procedure should do neither on the request path.
const MAX_SKILL_BYTES: u64 = 256 * 1024;

const MAX_SKILLS: usize = 100;

const MAX_DESCRIPTION_CHARS: usize = 300;

/// The skills compiled into the binary rather than installed.
///
/// Exactly one ships, and it ships for a reason particular to itself: `zlogic-guide` **is** this
/// program's manual, so what a user reads has to describe the build they are holding. Freezing it
/// here is what makes that true. Everything else is maintained in its own repository and reaches
/// users through the install path, where changing it does not wait on an app release — a workflow
/// bundled in the binary would drift from the copy its own documentation points at.
///
/// A bundled skill has no directory, so no disable marker can sit in one: its switch is a key in
/// the extension state file, and it ships **off** until someone flips it in Runtime → Skills.
struct BuiltinSkill {
    name: &'static str,
    text: &'static str,
    enabled_by_default: bool,
}

const GUIDE: BuiltinSkill = BuiltinSkill {
    name: "zlogic-guide",
    text: include_str!("../defaults/skills/zlogic-guide/SKILL.md"),
    enabled_by_default: false,
};

/// A second entry, under test only. The bundled path is otherwise carried entirely by the guide,
/// whose text changes whenever the product does — so every assertion about *how a bundled skill
/// behaves* would be an assertion about a document. This one never changes.
#[cfg(test)]
const EXAMPLE: BuiltinSkill = BuiltinSkill {
    name: "bundled-example",
    text: concat!(
        "---\n",
        "name: bundled-example\n",
        "description: a skill that had to be compiled in\n",
        "descriptions:\n",
        "  zh-CN: 只能编译进来的 skill\n",
        "  en-US: a skill that had to be compiled in\n",
        "---\n\n",
        "the body a bundled skill would carry\n",
    ),
    enabled_by_default: false,
};

#[cfg(not(test))]
const BUILTIN_SKILLS: &[BuiltinSkill] = &[GUIDE];

#[cfg(test)]
const BUILTIN_SKILLS: &[BuiltinSkill] = &[GUIDE, EXAMPLE];

/// The name the bundled-path tests refer to, so renaming the test entry does not mean rewriting
/// each assertion. `prompt`'s tests read it from there.
#[cfg(test)]
pub(crate) const TEST_BUILTIN: &str = "bundled-example";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    Builtin,
    User,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    /// Client-facing only: a UI that knows the reader's language picks from it (the graphical
    /// clients' built-in skill cards do). It is deliberately **not** part of the skill format and
    /// never reaches the model, which reads `description` and the frontmatter-free body.
    pub descriptions: BTreeMap<String, String>,
    pub path: PathBuf,
    pub origin: SkillOrigin,
    /// Whether this skill may be listed to the model and loaded. An installed skill is on unless
    /// its folder carries the disable marker; a built-in is off until someone switches it on.
    pub enabled: bool,
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
    #[serde(default)]
    descriptions: BTreeMap<String, String>,
}

/// The compiled-in skills alone, for callers with no workspace to scan: a machine-wide settings
/// page has no root, and the guide is the one skill that still has to be visible there.
pub fn builtin_skills(dirs: &Dirs, workspace: Option<WorkspaceId>) -> Skills {
    let mut by_name: BTreeMap<String, SkillDef> = BTreeMap::new();
    let mut problems = Vec::new();
    seed_builtin(
        &States::load_for(dirs, workspace),
        &mut by_name,
        &mut problems,
    );
    Skills {
        found: by_name.into_values().collect(),
        problems,
    }
}

pub fn discover(dirs: &Dirs, root: &Path, workspace: Option<WorkspaceId>) -> Skills {
    let mut by_name: BTreeMap<String, SkillDef> = BTreeMap::new();
    let mut problems = Vec::new();

    seed_builtin(
        &States::load_for(dirs, workspace),
        &mut by_name,
        &mut problems,
    );

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
        for dir in &plugin.skill_dirs {
            for mut def in read_plugin_skills(dir, &plugin.id, origin, &mut problems) {
                def.name = format!("{}:{}", plugin.id, def.name);
                by_name.insert(def.name.clone(), def);
            }
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

/// Reads the skills a manifest pointed at.
///
/// One of those directories can be the plugin root itself — `skills: ["./"]` says exactly that — so
/// a `SKILL.md` sitting directly in the directory counts as one skill there, named after the plugin
/// when the frontmatter does not name it.
fn read_plugin_skills(
    dir: &Path,
    fallback_name: &str,
    origin: SkillOrigin,
    problems: &mut Vec<String>,
) -> Vec<SkillDef> {
    let file = dir.join("SKILL.md");
    if file.is_file() {
        return match read_skill(&file, fallback_name, origin) {
            Ok(def) => vec![def],
            Err(why) => {
                problems.push(format!("failed to load {}: {why}", file.display()));
                Vec::new()
            }
        };
    }
    read_dir_of_skills(dir, origin, problems)
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
    parse_skill(&text, fallback_name, origin, file.to_path_buf())
}

/// Seeded first so a user or workspace skill of the same name still wins: overriding the guide is
/// a legitimate thing to want, and the conflict is reported like any other.
///
/// A built-in carries no folder, so the switch that turns one off is the state file's skill map
/// rather than a marker on disk. `Origin::Global` is what a built-in is — it never came from the
/// repository — which is also what lets a machine-wide switch reach it.
fn seed_builtin(
    states: &States,
    by_name: &mut BTreeMap<String, SkillDef>,
    problems: &mut Vec<String>,
) {
    for skill in BUILTIN_SKILLS {
        match parse_skill(
            skill.text,
            skill.name,
            SkillOrigin::Builtin,
            builtin_path(skill.name),
        ) {
            Ok(mut def) => {
                def.enabled = states
                    .verdict(
                        ExtensionKind::Skill,
                        &def.name,
                        &Origin::Global,
                        Some(skill.enabled_by_default),
                    )
                    .enabled;
                by_name.insert(def.name.clone(), def);
            }
            Err(why) => problems.push(format!("built-in skill {} is malformed: {why}", skill.name)),
        }
    }
}

/// The synthetic path a built-in reports. It names where the definition came from without
/// pretending there is a file to edit — the same convention the built-in templates use.
fn builtin_path(name: &str) -> PathBuf {
    PathBuf::from(format!("<builtin>/skills/{name}/SKILL.md"))
}

fn parse_skill(
    text: &str,
    fallback_name: &str,
    origin: SkillOrigin,
    path: PathBuf,
) -> Result<SkillDef, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let front = frontmatter(text)
        .ok_or("the file does not start with a `---`-delimited YAML frontmatter")?;
    let parsed: Frontmatter =
        serde_yaml_ng::from_str(front).map_err(|e| format!("failed to parse frontmatter: {e}"))?;

    let description = parsed
        .description
        .map(|d| trim_to(&fold_whitespace(d.trim()), MAX_DESCRIPTION_CHARS))
        .filter(|d| !d.is_empty())
        .ok_or(
            "no `description` in the frontmatter — the skill format is `name` plus `description`, \
        and the model uses the description to decide when to load it",
        )?;
    reject_invisible(&description, "description")?;

    let descriptions: BTreeMap<String, String> = parsed
        .descriptions
        .into_iter()
        .filter_map(|(locale, text)| {
            let text = trim_to(&fold_whitespace(text.trim()), MAX_DESCRIPTION_CHARS);
            (!text.is_empty()).then_some((locale, text))
        })
        .collect();
    for (locale, text) in &descriptions {
        reject_invisible(text, &format!("description for {locale}"))?;
    }

    let name = parsed
        .name
        .map(|n| fold_whitespace(n.trim()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    reject_invisible(&name, "name")?;

    Ok(SkillDef {
        name,
        description,
        descriptions,
        path,
        origin,
        // A file that got this far was not skipped by its disable marker, so it is on. Only the
        // built-ins carry a switch that lives somewhere other than the folder.
        enabled: true,
    })
}

/// Collapses a run of whitespace into single spaces.
///
/// Descriptions are routinely written as a YAML block scalar, so the author's line breaks arrive as
/// part of the text. The model reads the description as one line, and folding it here is what keeps
/// [`reject_invisible`] about smuggling rather than about formatting.
fn fold_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
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
    use zlogic_tools::SkillHost;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    fn dirs_under(base: &Path) -> Dirs {
        Dirs::under(base)
    }

    /// A built-in ships switched off, so a test that wants to load one turns it on the way a
    /// user would — one boolean in the state file.
    fn with_builtins_on(base: &Path) -> Dirs {
        let dirs = dirs_under(base);
        for skill in BUILTIN_SKILLS {
            crate::extensions::state::set_global(
                &dirs,
                ExtensionKind::Skill,
                skill.name,
                Some(true),
            )
            .unwrap();
        }
        dirs
    }

    /// Look a skill up by name. `found[0]` is not a skill under test: built-ins are seeded first,
    /// so the first entry is whatever they are called today.
    fn by_name<'a>(skills: &'a Skills, name: &str) -> &'a SkillDef {
        skills
            .found
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} was not discovered"))
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

    /// The skills that came off disk. Tests below are about what a directory yields, so the
    /// built-in guide — which is in every catalog — is filtered out rather than counted.
    fn installed(skills: Skills) -> Skills {
        Skills {
            found: skills
                .found
                .into_iter()
                .filter(|s| s.origin != SkillOrigin::Builtin)
                .collect(),
            problems: skills.problems,
        }
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

        let skills = installed(discover(&dirs, &root, None));
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

        let skills = installed(discover(&dirs, &tmp.path().join("repo"), None));
        assert!(skills.found.is_empty());
        assert!(
            skills.problems[0].contains("description"),
            "{:?}",
            skills.problems
        );
    }

    /// The format is `name` plus `description`. Another ecosystem's spelling of the second field is
    /// not read, so a skill carrying only that one is skipped and the report says which key is
    /// missing.
    #[test]
    fn a_when_to_use_field_is_not_a_description() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        write_skill(
            &dirs.data.join("skills"),
            "x",
            "---\nname: x\nwhen_to_use: after a release\n---\nbody\n",
        );
        let skills = installed(discover(&dirs, &tmp.path().join("repo"), None));
        assert!(skills.found.is_empty());
        assert!(
            skills.problems[0].contains("description"),
            "{:?}",
            skills.problems
        );
    }

    #[test]
    fn a_locale_description_companions_the_one_the_model_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        write_skill(
            &dirs.data.join("skills"),
            "x",
            "---\nname: x\ndescription: the model reads this\ndescriptions:\n  zh-CN: 界面读这一句\n---\nbody\n",
        );
        let skills = discover(&dirs, &tmp.path().join("repo"), None);
        assert_eq!(by_name(&skills, "x").description, "the model reads this");
        assert_eq!(
            by_name(&skills, "x")
                .descriptions
                .get("zh-CN")
                .map(String::as_str),
            Some("界面读这一句")
        );
    }

    #[test]
    fn a_bundled_skill_carries_both_languages() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = builtin_skills(&dirs_under(tmp.path()), None);
        let bundled = skills
            .found
            .iter()
            .find(|s| s.name == TEST_BUILTIN)
            .expect("the test entry is compiled in");
        assert!(
            bundled.descriptions.contains_key("zh-CN"),
            "{:?}",
            bundled.descriptions
        );
        assert!(
            bundled.descriptions.contains_key("en-US"),
            "{:?}",
            bundled.descriptions
        );
    }

    #[test]
    fn every_built_in_skill_parses_and_says_when_to_load_it() {
        let tmp = tempfile::tempdir().unwrap();
        for skill in BUILTIN_SKILLS {
            let found = builtin_skills(&dirs_under(tmp.path()), None);
            assert!(
                found.problems.is_empty(),
                "{}: {:?}",
                skill.name,
                found.problems
            );
            let def = found
                .found
                .iter()
                .find(|s| s.name == skill.name)
                .unwrap_or_else(|| panic!("{} is not listed", skill.name));
            assert!(
                !def.description.is_empty(),
                "{} has no description, so the model never loads it",
                skill.name
            );
            assert!(
                unsupported_fields(skill.text).is_empty(),
                "{} declares fields this build cannot honour: {:?}",
                skill.name,
                unsupported_fields(skill.text)
            );
        }
    }

    #[tokio::test]
    async fn loading_a_bundled_skill_returns_its_own_body() {
        let tmp = tempfile::tempdir().unwrap();
        let host = SessionSkills {
            dirs: with_builtins_on(tmp.path()),
            root: tmp.path().join("repo"),
            workspace: WorkspaceId::new(),
        };
        let loaded = host.load(TEST_BUILTIN).await.unwrap();
        assert!(
            loaded
                .raw_body
                .contains("the body a bundled skill would carry"),
            "{}",
            loaded.raw_body
        );
        let guide = host.load("zlogic-guide").await.unwrap();
        assert_ne!(
            guide.raw_body, loaded.raw_body,
            "each bundled skill loads its own text, not one shared body"
        );
    }

    /// Bundled skills carry a `descriptions:` map so the clients can show a translated card. That
    /// is the client's business: the model reads `description` and a frontmatter-free body.
    #[tokio::test]
    async fn the_client_only_translations_never_reach_the_model() {
        let tmp = tempfile::tempdir().unwrap();
        let host = SessionSkills {
            dirs: with_builtins_on(tmp.path()),
            root: tmp.path().join("repo"),
            workspace: WorkspaceId::new(),
        };
        for skill in BUILTIN_SKILLS {
            let loaded = host.load(skill.name).await.unwrap();
            assert!(
                !loaded.raw_body.starts_with("---"),
                "{}: frontmatter is not model-facing",
                skill.name
            );
            for leaked in ["descriptions:", "zh-CN:"] {
                assert!(
                    !loaded.raw_body.contains(leaked),
                    "{}: {leaked} reached the model",
                    skill.name
                );
            }
        }
        assert!(
            builtin_skills(&host.dirs, Some(host.workspace))
                .found
                .iter()
                .find(|s| s.name == TEST_BUILTIN)
                .unwrap()
                .descriptions
                .contains_key("zh-CN"),
            "the clients still get their translated card"
        );
    }

    /// A bundled skill nobody asked for spends a user's context the moment the model can see it,
    /// so it ships off: listed for the switch to offer, never offered to the model, not loadable.
    #[tokio::test]
    async fn a_bundled_skill_is_off_until_it_is_switched_on() {
        let tmp = tempfile::tempdir().unwrap();
        let off = SessionSkills {
            dirs: dirs_under(tmp.path()),
            root: tmp.path().join("repo"),
            workspace: WorkspaceId::new(),
        };
        assert!(
            off.available().await.is_empty(),
            "the model must not be told about a skill that is switched off"
        );
        assert!(
            off.load(TEST_BUILTIN).await.is_err(),
            "and loading it by name has to fail too"
        );

        let on = SessionSkills {
            dirs: with_builtins_on(tmp.path()),
            root: tmp.path().join("repo"),
            workspace: off.workspace,
        };
        assert!(
            on.available().await.contains(&TEST_BUILTIN.to_string()),
            "{:?}",
            on.available().await
        );
        assert!(on.load(TEST_BUILTIN).await.is_ok());
    }

    /// One workspace turning it on must not turn it on everywhere, and the other way round: the
    /// same three layers a server or a plugin lives on.
    #[test]
    fn a_bundled_skills_switch_follows_the_state_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let elsewhere = WorkspaceId::new();
        let here = WorkspaceId::new();
        crate::extensions::state::set_personal(
            &dirs,
            here,
            ExtensionKind::Skill,
            TEST_BUILTIN,
            Some(true),
        )
        .unwrap();

        assert!(
            by_name(&discover(&dirs, tmp.path(), Some(here)), TEST_BUILTIN).enabled,
            "switched on for this workspace"
        );
        assert!(
            !by_name(&discover(&dirs, tmp.path(), Some(elsewhere)), TEST_BUILTIN).enabled,
            "another workspace did not ask for it"
        );
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
        assert_eq!(by_name(&skills, "release-notes").name, "release-notes");
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

        let skills = installed(discover(&dirs, &root, None));
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
        assert_eq!(installed(discover(&dirs, &root, None)).found.len(), 1);
    }

    #[test]
    fn a_personally_disabled_plugin_contributes_no_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_under(tmp.path());
        let root = tmp.path().join("repo");
        let workspace = WorkspaceId::new();
        write_plugin_skill(&dirs, "acme", "review");

        assert_eq!(
            installed(discover(&dirs, &root, Some(workspace))).found[0].name,
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
            installed(discover(&dirs, &root, Some(workspace)))
                .found
                .is_empty(),
            "the same plugin switch must gate its skills as well as its MCP servers"
        );
        assert_eq!(
            installed(discover(&dirs, &root, Some(WorkspaceId::new())))
                .found
                .len(),
            1,
            "a personal override must remain scoped to its workspace"
        );
    }

    #[test]
    fn nothing_installed_is_not_a_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = installed(discover(
            &dirs_under(tmp.path()),
            &tmp.path().join("repo"),
            None,
        ));
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
        let names: Vec<String> = installed(discover(&dirs, &tmp.path().join("repo"), None))
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
        let skills = installed(discover(&dirs, &tmp.path().join("repo"), None));
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
        let skills = discover(&dirs, &tmp.path().join("repo"), None);
        let d = &by_name(&skills, "long").description;
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
            .filter(|s| s.enabled)
            .map(|s| s.name)
            .collect()
    }

    async fn load(&self, name: &str) -> std::result::Result<zlogic_tools::LoadedSkill, String> {
        let found = discover(&self.dirs, &self.root, Some(self.workspace))
            .found
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("skill {name} is no longer in the library"))?;
        if !found.enabled {
            return Err(format!("skill {name} is switched off"));
        }

        // A built-in's `path` is a label, not a file, so its body comes from the binary.
        let text = match found.origin {
            SkillOrigin::Builtin => BUILTIN_SKILLS
                .iter()
                .find(|s| s.name == found.name)
                .map(|s| s.text.to_string())
                .ok_or_else(|| format!("built-in skill {} is not compiled in", found.name))?,
            _ => std::fs::read_to_string(&found.path)
                .map_err(|e| format!("cannot read {}: {e}", found.path.display()))?,
        };
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

/// Everything the model gets, which is the body alone.
///
/// The frontmatter is cut here, not merely ignored while parsing: it carries the client-only
/// `descriptions` map, and the standard format the guide documents is `name` plus `description`.
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
