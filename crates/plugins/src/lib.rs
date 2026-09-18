//! # zlogic-plugins
//! A plugin is a **directory with a manifest**. Nothing is executed to discover one, and nothing is
//! installed by discovering one: loading a plugin means reading files.
//! ```text
//! <data>/extensions/plugins/<name>/     ← the user installed it
//! <root>/.zlogic/extensions/plugins/<name>/   ← it came with the repository
//!     plugin.json | plugin.yaml | .claude-plugin/plugin.json   ← the manifest
//!     .mcp.json                                                ← or servers in their own file
//! ```
//! # What a plugin contributes
//! Today plugins contribute MCP servers and skills. Agent files are discovered for the management
//! catalogue, but are not executable until the dispatcher has a real plugin-agent profile loader.
//! Contributed servers are namespaced `<plugin>.<server>`, so two plugins can each ship a server
//! called `search` and a plugin can never shadow a server the user configured themselves.
//! # Why the plugin root is expanded *after* parsing
//! A manifest refers to its own directory as `${pluginRoot}` (or `${CLAUDE_PLUGIN_ROOT}`, which is
//! what plugins written for Claude Code use). The obvious implementation — substitute in the file's
//! text before parsing it — is wrong on Windows: a path containing `\` is not a valid JSON string
//! escape, so a textual substitution produces a file that no longer parses, or worse, one that parses
//! differently. So the substitution happens on the parsed definition, where a path is just a value.
//! # A plugin is data on disk, and it is not trusted by being present
//! A plugin inside a repository arrived with a `git clone`, and its manifest can name any command on
//! the machine. This crate therefore only ever *reports* what it found, carrying
//! [`zlogic_mcp::Origin::Plugin { workspace: true }`] so a caller can tell repository-supplied
//! definitions from the user's own installs. Deciding whether to start any of it is not made here.

use std::path::{Path, PathBuf};

use zlogic_mcp::Origin;
use zlogic_mcp::def::{self, Problem, ServerDef, TransportDef};

/// Manifest file names, in the order they are looked for.
/// `.claude-plugin/plugin.json` is first because a plugin written for Claude Code should work as-is;
/// finding it before our own names means a plugin that ships both is read as its author intended.
const MANIFEST_NAMES: &[&str] = &[
    ".claude-plugin/plugin.json",
    "plugin.json",
    "plugin.yaml",
    "plugin.yml",
    "zlogic-plugin.json",
    "zlogic-plugin.yaml",
];

/// Files a plugin may declare its servers in, instead of inlining them in the manifest.
const MCP_FILE_NAMES: &[&str] = &[".mcp.json", "mcp.json"];
pub const DISABLED_MARKER: &str = ".zlogic-disabled";

/// Where plugins are looked for.
#[derive(Debug, Clone)]
pub struct PluginDirs {
    /// `<data>/extensions/plugins` — the user's own installs.
    pub global: PathBuf,
}

impl PluginDirs {
    pub fn under(dirs: &zlogic_config::Dirs) -> Self {
        Self {
            global: dirs.data.join("extensions").join("plugins"),
        }
    }

    /// `<root>/.zlogic/extensions/plugins` — inside the repository, shared with whoever clones it.
    pub fn in_workspace(root: &Path) -> PathBuf {
        root.join(".zlogic").join("extensions").join("plugins")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDef {
    /// The directory name. Identity comes from the location, as it does for MCP definitions — the
    /// manifest's `name` is a label.
    pub id: String,
    pub label: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// `None` is enabled. A plugin the user placed there is one they want.
    pub enabled: Option<bool>,
    pub root: PathBuf,
    pub manifest: PathBuf,
    /// Whether it came from inside the repository.
    pub from_workspace: bool,
    /// Contributed servers, already namespaced and with `${pluginRoot}` expanded.
    pub servers: Vec<ServerDef>,
}

impl PluginDef {
    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    /// Directories this plugin ships that zlogic has nowhere to put yet.
    /// Reported rather than ignored in silence: a plugin whose skills do nothing should be
    /// explainable to the person who installed it.
    pub fn unused_directories(&self) -> Vec<&'static str> {
        ["agents", "commands", "hooks"]
            .into_iter()
            .filter(|d| self.root.join(d).is_dir())
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct Loaded {
    pub plugins: Vec<PluginDef>,
    /// Manifests that could not be read, and servers inside them that could not be parsed.
    pub problems: Vec<Problem>,
}

impl Loaded {
    /// The MCP servers of every **enabled** plugin, ready to hand to
    /// [`zlogic_mcp::Catalog::load`](zlogic_mcp::Catalog::load).
    pub fn mcp_servers(&self) -> Vec<ServerDef> {
        self.plugins
            .iter()
            .filter(|p| p.is_enabled())
            .flat_map(|p| p.servers.iter().cloned())
            .collect()
    }

    pub fn enabled(&self) -> impl Iterator<Item = &PluginDef> {
        self.plugins.iter().filter(|p| p.is_enabled())
    }
}

/// Loads every plugin that applies to a workspace.
/// The workspace's own directory is read **after** the global one, so a repository shipping a plugin
/// with the same directory name as an installed one wins — the same precedence the MCP catalogue
/// uses, for the same reason.
pub fn load(dirs: &PluginDirs, workspace_root: Option<&Path>) -> Loaded {
    let mut out = Loaded::default();
    let mut seen: Vec<usize> = Vec::new();

    let mut absorb = |dir: &Path, from_workspace: bool, out: &mut Loaded| {
        for plugin_dir in plugin_dirs_in(dir) {
            match load_one(&plugin_dir, from_workspace) {
                Ok(Some(mut plugin)) => {
                    out.problems.extend(std::mem::take(&mut plugin.0));
                    // Same directory name from a later root replaces the earlier one.
                    if let Some(i) = out.plugins.iter().position(|p| p.id == plugin.1.id) {
                        out.plugins[i] = plugin.1;
                    } else {
                        seen.push(out.plugins.len());
                        out.plugins.push(plugin.1);
                    }
                }
                // A directory with no manifest is not a plugin and not an error: `plugins/` may well
                // contain a stray download or a half-removed install.
                Ok(None) => {}
                Err(problem) => out.problems.push(problem),
            }
        }
    };

    absorb(&dirs.global, false, &mut out);
    if let Some(root) = workspace_root {
        absorb(&PluginDirs::in_workspace(root), true, &mut out);
    }
    out
}

/// Immediate subdirectories that could be plugins.
fn plugin_dirs_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| !p.join(DISABLED_MARKER).exists())
        .filter(|p| {
            // Hidden directories are housekeeping (`.git`, `.DS_Store` siblings, download temporaries),
            // never a plugin somebody meant to install.
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        })
        .collect();
    dirs.sort();
    dirs
}

/// Reads one plugin directory. `Ok(None)` = there is no manifest, so this is not a plugin.
pub fn inspect(
    root: &Path,
    from_workspace: bool,
) -> Result<Option<(Vec<Problem>, PluginDef)>, Problem> {
    load_one(root, from_workspace)
}

fn load_one(
    root: &Path,
    from_workspace: bool,
) -> Result<Option<(Vec<Problem>, PluginDef)>, Problem> {
    let Some(manifest) = MANIFEST_NAMES
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
    else {
        return Ok(None);
    };
    let id = root
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| Problem::new(None, Some(root.to_path_buf()), "unreadable directory name"))?;

    let raw = std::fs::read_to_string(&manifest)
        .map_err(|e| Problem::new(Some(id.clone()), Some(manifest.clone()), e.to_string()))?;
    let value = parse_manifest(&raw, &manifest)
        .map_err(|reason| Problem::new(Some(id.clone()), Some(manifest.clone()), reason))?;

    let mut problems = Vec::new();
    let mut servers = Vec::new();
    let origin = Origin::Plugin {
        plugin: id.clone(),
        workspace: from_workspace,
    };

    // Servers declared in their own file. Same format as a standalone `.mcp.json`, so a plugin is
    // partly just a directory holding one.
    for name in MCP_FILE_NAMES {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        match def::parse_file(&path, origin.clone()) {
            Ok(parsed) => {
                problems.extend(parsed.problems);
                servers.extend(parsed.servers);
            }
            Err(e) => problems.push(Problem::new(Some(id.clone()), Some(path), e.to_string())),
        }
        break;
    }

    // Servers inlined in the manifest. `parse_value` accepts `mcp.servers`, `mcpServers` and
    // `servers`, so this covers every spelling a manifest might use.
    if value.get("mcp").is_some() || value.get("mcpServers").is_some() {
        let parsed = def::parse_value(&value, &id, origin, Some(manifest.clone()));
        problems.extend(parsed.problems);
        servers.extend(parsed.servers);
    }

    for server in &mut servers {
        // Namespaced before anything else sees it: the id ends up in every tool name, and a plugin
        // must not be able to shadow a server the user configured.
        server.id = format!("{id}.{}", server.id);
        expand_plugin_root(server, root);
    }

    let plugin = PluginDef {
        id,
        label: string_of(&value, "displayName").or_else(|| string_of(&value, "name")),
        version: string_of(&value, "version"),
        description: string_of(&value, "description"),
        enabled: value.get("enabled").and_then(|v| v.as_bool()),
        root: root.to_path_buf(),
        manifest,
        from_workspace,
        servers,
    };
    Ok(Some((problems, plugin)))
}

fn parse_manifest(raw: &str, path: &Path) -> Result<serde_json::Value, String> {
    let is_json = path.extension().is_some_and(|e| e == "json");
    if is_json {
        serde_json::from_str(raw).map_err(|e| e.to_string())
    } else {
        // YAML is parsed into the same `Value`, so there is exactly one manifest reader.
        serde_yaml_ng::from_str(raw).map_err(|e| e.to_string())
    }
}

fn string_of(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Substitutes the plugin's own directory into its transport parameters.
/// Done on the parsed definition rather than the manifest's text — see the module docs for why that
/// matters on Windows.
fn expand_plugin_root(server: &mut ServerDef, root: &Path) {
    let root = root.to_string_lossy().to_string();
    let sub = |s: &mut String| {
        for name in [
            "${pluginRoot}",
            "${CLAUDE_PLUGIN_ROOT}",
            "${ZLOGIC_PLUGIN_ROOT}",
        ] {
            if s.contains(name) {
                *s = s.replace(name, &root);
            }
        }
    };
    match &mut server.transport {
        TransportDef::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            sub(command);
            args.iter_mut().for_each(&sub);
            env.values_mut().for_each(&sub);
            match cwd {
                Some(c) => sub(c),
                // A plugin's server runs in the workspace by default, like any other stdio server —
                // its *files* are in the plugin directory, its *work* is on the user's project.
                None => {}
            }
        }
        TransportDef::Http { url, headers, .. } => {
            sub(url);
            headers.values_mut().for_each(&sub);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        _tmp: tempfile::TempDir,
        dirs: PluginDirs,
        workspace: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = PluginDirs {
            global: tmp.path().join("data/extensions/plugins"),
        };
        let workspace = tmp.path().join("work");
        std::fs::create_dir_all(&dirs.global).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        Fixture {
            _tmp: tmp,
            dirs,
            workspace,
        }
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn write_json(path: &Path, value: serde_json::Value) {
        write(path, &serde_json::to_string_pretty(&value).unwrap());
    }

    #[test]
    fn a_manifest_with_inline_servers_contributes_them_namespaced() {
        let f = fixture();
        write_json(
            &f.dirs.global.join("acme/plugin.json"),
            json!({
                "name": "Acme tools",
                "version": "1.2.0",
                "mcpServers": { "search": { "command": "acme-search" } }
            }),
        );

        let loaded = load(&f.dirs, Some(&f.workspace));
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.plugins.len(), 1);
        let plugin = &loaded.plugins[0];
        assert_eq!(plugin.id, "acme", "the directory name is the identity");
        assert_eq!(plugin.label.as_deref(), Some("Acme tools"));
        assert_eq!(plugin.version.as_deref(), Some("1.2.0"));

        let servers = loaded.mcp_servers();
        assert_eq!(
            servers[0].id, "acme.search",
            "a plugin cannot shadow the user's own server"
        );
        assert_eq!(
            servers[0].origin,
            Origin::Plugin {
                plugin: "acme".into(),
                workspace: false
            }
        );
    }

    /// A plugin written for Claude Code should work as it is.
    #[test]
    fn a_claude_plugin_manifest_is_read() {
        let f = fixture();
        write_json(
            &f.dirs.global.join("compat/.claude-plugin/plugin.json"),
            json!({ "name": "Compat", "mcpServers": { "s": { "command": "x" } } }),
        );
        let loaded = load(&f.dirs, None);
        assert_eq!(loaded.plugins.len(), 1);
        assert_eq!(loaded.mcp_servers()[0].id, "compat.s");
    }

    #[test]
    fn a_yaml_manifest_is_read_by_the_same_parser() {
        let f = fixture();
        write(
            &f.dirs.global.join("y/plugin.yaml"),
            "name: Yaml plugin\nmcpServers:\n  s:\n    command: run\n    args: [--flag]\n",
        );
        let loaded = load(&f.dirs, None);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.plugins[0].label.as_deref(), Some("Yaml plugin"));
        match &loaded.mcp_servers()[0].transport {
            TransportDef::Stdio { command, args, .. } => {
                assert_eq!(command, "run");
                assert_eq!(args, &["--flag"]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Servers in their own file: a plugin is partly just a directory holding a `.mcp.json`.
    #[test]
    fn servers_may_live_in_their_own_file() {
        let f = fixture();
        write_json(&f.dirs.global.join("p/plugin.json"), json!({ "name": "P" }));
        write_json(
            &f.dirs.global.join("p/.mcp.json"),
            json!({ "mcpServers": { "fs": { "command": "fs" } } }),
        );
        let loaded = load(&f.dirs, None);
        assert_eq!(loaded.mcp_servers()[0].id, "p.fs");
    }

    /// The substitution has to survive a path that is not valid inside a JSON string, which is what
    /// makes doing it before parsing wrong.
    #[test]
    fn the_plugin_root_is_substituted_into_parameters() {
        let f = fixture();
        let root = f.dirs.global.join("local");
        write_json(
            &root.join("plugin.json"),
            json!({
                "name": "Local",
                "mcpServers": { "s": {
                    "command": "node",
                    "args": ["${CLAUDE_PLUGIN_ROOT}/server.js"],
                    "env": { "DATA": "${pluginRoot}/data" }
                } }
            }),
        );

        let loaded = load(&f.dirs, None);
        match &loaded.mcp_servers()[0].transport {
            TransportDef::Stdio { args, env, cwd, .. } => {
                assert_eq!(args[0], format!("{}/server.js", root.display()));
                assert_eq!(env["DATA"], format!("{}/data", root.display()));
                assert!(
                    cwd.is_none(),
                    "a plugin's server still works in the user's project"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_disabled_plugin_contributes_nothing_but_is_still_listed() {
        let f = fixture();
        write_json(
            &f.dirs.global.join("off/plugin.json"),
            json!({ "name": "Off", "enabled": false, "mcpServers": { "s": { "command": "x" } } }),
        );
        let loaded = load(&f.dirs, None);
        assert_eq!(
            loaded.plugins.len(),
            1,
            "the user has to be able to see it to turn it on"
        );
        assert!(loaded.mcp_servers().is_empty());
        assert_eq!(loaded.enabled().count(), 0);
    }

    /// A repository shipping a plugin of the same name as an installed one wins, the same way the MCP
    /// catalogue resolves it.
    #[test]
    fn a_workspace_plugin_replaces_a_global_one_of_the_same_name() {
        let f = fixture();
        write_json(
            &f.dirs.global.join("tool/plugin.json"),
            json!({ "name": "global", "mcpServers": { "s": { "command": "global" } } }),
        );
        write_json(
            &PluginDirs::in_workspace(&f.workspace).join("tool/plugin.json"),
            json!({ "name": "repo", "mcpServers": { "s": { "command": "repo" } } }),
        );

        let loaded = load(&f.dirs, Some(&f.workspace));
        assert_eq!(loaded.plugins.len(), 1);
        assert!(loaded.plugins[0].from_workspace);
        let servers = loaded.mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].origin,
            Origin::Plugin {
                plugin: "tool".into(),
                workspace: true
            },
            "a caller has to be able to tell repository-supplied definitions apart"
        );
        assert!(servers[0].origin.is_from_workspace());
    }

    /// Nothing installed, and nothing to report about it.
    #[test]
    fn no_plugins_is_not_a_problem() {
        let f = fixture();
        let loaded = load(&f.dirs, Some(&f.workspace));
        assert!(loaded.plugins.is_empty());
        assert!(loaded.problems.is_empty());
        assert!(loaded.mcp_servers().is_empty());
    }

    /// `plugins/` accumulates downloads and half-removed installs; those are not plugins and not
    /// errors either.
    #[test]
    fn a_directory_without_a_manifest_is_skipped_silently() {
        let f = fixture();
        std::fs::create_dir_all(f.dirs.global.join("just-a-folder")).unwrap();
        std::fs::create_dir_all(f.dirs.global.join(".tmp-download")).unwrap();
        let loaded = load(&f.dirs, None);
        assert!(loaded.plugins.is_empty());
        assert!(loaded.problems.is_empty());
    }

    #[test]
    fn a_broken_manifest_is_reported_by_name_and_the_others_still_load() {
        let f = fixture();
        write(&f.dirs.global.join("broken/plugin.json"), "{ not json");
        write_json(
            &f.dirs.global.join("fine/plugin.json"),
            json!({ "mcpServers": { "s": { "command": "x" } } }),
        );

        let loaded = load(&f.dirs, None);
        assert_eq!(loaded.plugins.len(), 1);
        assert_eq!(loaded.plugins[0].id, "fine");
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].server.as_deref(), Some("broken"));
    }

    /// One malformed server must not cost the plugin its other servers.
    #[test]
    fn a_malformed_server_inside_a_good_manifest_is_reported_on_its_own() {
        let f = fixture();
        write_json(
            &f.dirs.global.join("p/plugin.json"),
            json!({ "mcpServers": { "good": { "command": "x" }, "bad": {} } }),
        );
        let loaded = load(&f.dirs, None);
        assert_eq!(loaded.mcp_servers().len(), 1);
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].server.as_deref(), Some("bad"));
    }

    /// Directories without a runtime consumer have to be explainable to whoever installed them.
    #[test]
    fn directories_zlogic_has_nowhere_to_put_are_reported_rather_than_ignored() {
        let f = fixture();
        let root = f.dirs.global.join("rich");
        write_json(&root.join("plugin.json"), json!({ "name": "Rich" }));
        std::fs::create_dir_all(root.join("skills")).unwrap();
        std::fs::create_dir_all(root.join("agents")).unwrap();

        let loaded = load(&f.dirs, None);
        // Skills are now consumed by the engine's unified skill catalogue.
        assert_eq!(loaded.plugins[0].unused_directories(), ["agents"]);
    }

    #[test]
    fn a_manifest_with_no_servers_is_a_plugin_with_no_servers() {
        let f = fixture();
        write_json(
            &f.dirs.global.join("empty/plugin.json"),
            json!({ "name": "Empty" }),
        );
        let loaded = load(&f.dirs, None);
        assert_eq!(loaded.plugins.len(), 1);
        assert!(
            loaded.problems.is_empty(),
            "declaring nothing is allowed: {:?}",
            loaded.problems
        );
        assert!(loaded.mcp_servers().is_empty());
    }

    #[test]
    fn the_directories_are_derived_from_the_data_root() {
        let dirs = PluginDirs::under(&zlogic_config::Dirs::under("/base"));
        assert_eq!(dirs.global, PathBuf::from("/base/data/extensions/plugins"));
        assert_eq!(
            PluginDirs::in_workspace(Path::new("/w")),
            PathBuf::from("/w/.zlogic/extensions/plugins")
        );
    }
}
