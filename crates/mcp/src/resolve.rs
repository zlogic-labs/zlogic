//! From a definition to launch parameters — and to the key that decides which connection serves it.
//! # The pool key is the resolved parameters, and a declared scope overrides it
//! For a server to behave differently per workspace, that difference has to reach it somehow — and
//! the only channels are the ones in the definition: command, args, env and cwd for stdio, url and
//! headers for HTTP. So by default the key is a hash of those, after expansion:
//! - args mention `${workspaceRoot}` → each workspace expands to different parameters → each gets
//!   its own connection, without anyone declaring that.
//! - nothing in the parameters varies (a search API, GitHub, a database) → every workspace expands
//!   to the same parameters → one connection serves all of them, again without declaring it.
//! `cwd` defaulting to the **workspace root** rather than to zlogic's own directory is what makes the
//! first case a rule instead of a heuristic: a stdio server that reads its process directory is
//! isolated per workspace because the directory is part of the key. Pinning `cwd` in the definition
//! is how a user says "this one is directory-independent, share it".
//! [`Binding`] overrides all of that, and exists because derivation gets stateful servers wrong: a
//! browser is per-workspace *because* of its directory and per-session *because* of elicitation, and
//! neither is what "one browser for the whole app" means. `global` therefore also decides the launch
//! directory — `<state>/mcp/<server>`, ours rather than the caller's — and builds its key from the
//! definition's literal text, since a key that varied with the workspace would not be global. A
//! `${workspaceRoot}` left in a global server's parameters is a contradiction: the key ignores it and
//! the launch value takes whichever caller's workspace connected first.
//! # Secrets are resolved for the launch and excluded from the key
//! A credential placeholder contributes its **literal text** (`${env:GITHUB_TOKEN}`) to the key, not
//! the token it resolves to. Otherwise rotating a token would silently split the pool — a second
//! connection to the same server, with the old one still running. The tradeoff is deliberate and has
//! one edge: a non-secret `${env:…}` that genuinely changes behaviour does not split the pool
//! either. Anything that must split belongs in the args, where it is visible.
//! # What is *not* decidable, and the one field that admits it
//! A server whose parameters are identical everywhere but which keeps state inside its tools — a
//! browser holding a page — cannot be detected: MCP has no "I am stateful" capability. That is what
//! [`crate::def::Binding::Session`] is for, and it is the only hand-written scope in the system.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zlogic_credential::{CredentialStore, SystemCredentialStore};

use zlogic_protocol::SessionId;

use crate::def::{Binding, Fields, HttpAuthDef, ServerDef, TransportDef};
use crate::{McpError, Result};

/// A lookup that may fail to find anything. Injected so tests never touch the real environment or
/// the developer's keychain.
pub type Lookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Turns a definition into launch parameters.
/// Cheap to build — one is made per call rather than cached, so a token stored a moment ago is
/// picked up without anything having to be invalidated.
pub struct Resolver {
    workspace_root: PathBuf,
    /// `<state>/mcp` — the parent of the per-server directory a [`Binding::Global`] stdio server is
    /// launched in. See [`Resolver::shared_dir`].
    shared_root: PathBuf,
    env: Lookup,
    secret: Lookup,
}

impl Resolver {
    /// The real environment and the OS keychain.
    pub fn system(workspace_root: impl Into<PathBuf>, shared_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            shared_root: shared_root.into(),
            env: Arc::new(|k| std::env::var(k).ok()),
            secret: Arc::new(|name| SystemCredentialStore.resolve(&format!("keyring:{name}"))),
        }
    }

    /// Both lookups injected. Tests, and any host that keeps credentials somewhere else.
    pub fn with_lookups(
        workspace_root: impl Into<PathBuf>,
        shared_root: impl Into<PathBuf>,
        env: Lookup,
        secret: Lookup,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            shared_root: shared_root.into(),
            env,
            secret,
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Where a [`Binding::Global`] stdio server runs, given no `cwd` of its own.
    /// A directory of ours rather than the caller's workspace, because the whole point of the scope
    /// is that the connection outlives any one workspace: a cwd borrowed from whichever workspace
    /// happened to call first would make the process's idea of "here" depend on a race.
    pub fn shared_dir(&self, server_id: &str) -> PathBuf {
        self.shared_root
            .join(crate::def::sanitize_segment(server_id))
    }

    /// Expands every template in the definition and computes the pool key.
    /// `session` is only read when the definition asks for it ([`Binding::Session`]); passing it
    /// always is correct and has no effect otherwise.
    pub fn resolve(&self, def: &ServerDef, session: Option<SessionId>) -> Result<Resolved> {
        let mut key = Fields::new();
        // A globally shared connection must hash the same in every workspace, so its key is built
        // from the definition's *literal* text: `${workspaceRoot}` stays a placeholder instead of
        // becoming a path, and a rotated credential stays a placeholder too. The value that is
        // launched with is still resolved, so a missing one fails here rather than at spawn.
        let literal = def.binding == Binding::Global;
        let transport = match &def.transport {
            TransportDef::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                key.field("kind", "stdio");
                let command = self.field(command, literal)?;
                key.field("command", &command.1);

                let mut out_args = Vec::with_capacity(args.len());
                for a in args {
                    let a = self.field(a, literal)?;
                    key.field("arg", &a.1);
                    out_args.push(a.0);
                }

                let mut out_env = BTreeMap::new();
                // A BTreeMap iterates in key order, which is what makes the key stable: an
                // environment serialised in hash-map order would hash differently run to run and
                // every launch would look like a new server.
                for (k, v) in env {
                    let v = self.field(v, literal)?;
                    key.field("env", &format!("{k}={}", v.1));
                    out_env.insert(k.clone(), v.0);
                }

                let cwd = match cwd {
                    Some(raw) => {
                        let (value, _) = self.field(raw, literal)?;
                        let p = PathBuf::from(&value);
                        if p.is_absolute() {
                            p
                        } else {
                            self.workspace_root.join(p)
                        }
                    }
                    None if literal => self.shared_dir(&def.id),
                    None => self.workspace_root.clone(),
                };
                key.field("cwd", &cwd.to_string_lossy());
                if def.binding == Binding::Workspace {
                    // Parameters that do not vary still have to split, or the scope is a lie.
                    key.field("workspace", &self.workspace_root.to_string_lossy());
                }

                ResolvedTransport::Stdio {
                    command: command.0,
                    args: out_args,
                    env: out_env,
                    cwd,
                }
            }
            TransportDef::Http { url, headers, auth } => {
                key.field("kind", "http");
                let url = self.field(url, literal)?;
                key.field("url", &url.1);

                let mut out = BTreeMap::new();
                for (k, v) in headers {
                    let v = self.field(v, literal)?;
                    key.field("header", &format!("{k}={}", v.1));
                    out.insert(k.clone(), v.0);
                }
                if let Some(HttpAuthDef::Bearer { credential }) = auth {
                    let literal_ref = credential.to_string();
                    let value = match credential {
                        zlogic_credential::CredentialRef::Env(name) => (self.env)(name),
                        zlogic_credential::CredentialRef::Keyring(name) => (self.secret)(name),
                    }
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| McpError::Template {
                        placeholder: literal_ref.clone(),
                        reason: "credential is not set".into(),
                    })?;
                    key.field("auth", &format!("bearer:{literal_ref}"));
                    out.insert("Authorization".into(), format!("Bearer {value}"));
                }
                let oauth = matches!(auth, Some(HttpAuthDef::OAuth { .. }));
                if oauth {
                    key.field("auth", "oauth");
                }
                ResolvedTransport::Http {
                    url: url.0,
                    headers: out,
                    oauth,
                }
            }
        };

        Ok(Resolved {
            transport,
            key: PoolKey {
                params: key.finish(),
                session: match def.binding {
                    Binding::Session => session,
                    Binding::Params | Binding::Workspace | Binding::Global => None,
                },
            },
        })
    }

    /// One string, two forms: the value to launch with, and the form that enters the key.
    /// `literal_key` keeps the definition's own text as the key form, which is what a
    /// [`Binding::Global`] server needs — its key may not vary with the caller's workspace.
    fn field(&self, raw: &str, literal_key: bool) -> Result<(String, String)> {
        if !literal_key {
            let expanded = self.expand(raw)?;
            return Ok((expanded.value, expanded.key));
        }
        Ok((self.expand(raw)?.value, raw.to_string()))
    }

    /// One string, expanded twice: the value to launch with and the form that enters the key.
    fn expand(&self, raw: &str) -> Result<Expanded> {
        let mut value = String::with_capacity(raw.len());
        let mut key = String::with_capacity(raw.len());
        let mut rest = raw;

        while let Some(start) = rest.find("${") {
            value.push_str(&rest[..start]);
            key.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                // An unclosed `${` is literal text, not an error: it may well be what the server
                // wants on its command line.
                value.push_str(&rest[start..]);
                key.push_str(&rest[start..]);
                return Ok(Expanded { value, key });
            };
            let name = &after[..end];
            let sub = self.substitute(name)?;
            value.push_str(&sub.value);
            key.push_str(&sub.key);
            rest = &after[end + 1..];
        }
        value.push_str(rest);
        key.push_str(rest);
        Ok(Expanded { value, key })
    }

    fn substitute(&self, name: &str) -> Result<Expanded> {
        let literal = format!("${{{name}}}");
        // The workspace root expands in **both** forms. That is the point: it is exactly what should
        // split the pool.
        if matches!(
            name,
            "workspaceRoot" | "workspaceFolder" | "projectDir" | "ZLOGIC_PROJECT_DIR"
        ) {
            let root = self.workspace_root.to_string_lossy().to_string();
            return Ok(Expanded {
                key: root.clone(),
                value: root,
            });
        }

        if let Some(entry) = name.strip_prefix("keyring:") {
            // `keyring:mcp/github/token` → the flat entry `mcp_github_token`. One rule, no mapping
            // table: keyring backends want a flat string, and the slashes are only there to read
            // like a path.
            let flat = entry.replace('/', "_");
            let secret = (self.secret)(&flat)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| McpError::Template {
                    placeholder: literal.clone(),
                    reason: format!("no keyring entry `{flat}` under the `zlogic` service"),
                })?;
            return Ok(Expanded {
                value: secret,
                key: literal,
            });
        }

        // `${env:VAR}`, and the bare `${VAR}` that `.mcp.json` files in the wild are written with.
        // `:-default` is supported on both, because a definition that can fall back is one that
        // does not have to fail.
        let spec = name.strip_prefix("env:").unwrap_or(name);
        let (var, default) = match spec.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (spec, None),
        };
        if var.is_empty() {
            return Err(McpError::Template {
                placeholder: literal,
                reason: "names no variable".into(),
            });
        }
        match (self.env)(var)
            .filter(|v| !v.is_empty())
            .or_else(|| default.map(str::to_string))
        {
            Some(v) => Ok(Expanded {
                value: v,
                key: literal,
            }),
            None => Err(McpError::Template {
                placeholder: literal,
                reason: format!("environment variable `{var}` is not set"),
            }),
        }
    }
}

struct Expanded {
    /// What the server is launched with.
    value: String,
    /// What enters the pool key.
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        /// Layered **on top of** the inherited environment, not instead of it: a server launched
        /// without `PATH` or `HOME` fails in ways that look nothing like a configuration mistake.
        env: BTreeMap<String, String>,
        cwd: PathBuf,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        oauth: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub transport: ResolvedTransport,
    pub key: PoolKey,
}

/// Which connection serves a call.
/// Two definitions that resolve to the same parameters share one connection even across sessions and
/// workspaces — that is the intent, not an accident, and it is what [`Binding::Params`] asks for.
/// A definition that declares a wider scope gets a key that varies on the thing it named instead:
/// [`Binding::Workspace`] on the workspace root, [`Binding::Global`] on nothing at all. The number of
/// distinct keys stays bounded — the closed set of template variables, or one per server — so there
/// is no path to unbounded growth.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolKey {
    params: String,
    /// Only ever set for [`Binding::Session`].
    session: Option<SessionId>,
}

impl PoolKey {
    pub fn params(&self) -> &str {
        &self.params
    }

    pub fn session(&self) -> Option<SessionId> {
        self.session
    }

    /// Forces a live tool connection to belong to one zlogic session.
    /// Server-to-client requests such as elicitation do not carry the originating zlogic session.
    /// A session-bound connection therefore provides the routing boundary; catalogue discovery
    /// connections remain shareable because they never execute a tool.
    pub fn bind_session(&mut self, session: SessionId) {
        self.session = Some(session);
    }
}

impl Resolved {
    pub fn bind_session(&mut self, session: SessionId) {
        self.key.bind_session(session);
    }
}

impl std::fmt::Display for PoolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.session {
            Some(s) => write!(f, "{}@{s}", self.params),
            None => f.write_str(&self.params),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{Origin, parse_value};
    use serde_json::{Value, json};

    fn def(v: Value) -> ServerDef {
        let mut p = parse_value(&v, "x", Origin::Global, None);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        p.servers.remove(0)
    }

    fn resolver(root: &str, env: &[(&str, &str)], secrets: &[(&str, &str)]) -> Resolver {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let secrets: Vec<(String, String)> = secrets
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Resolver::with_lookups(
            root,
            "/shared",
            Arc::new(move |k| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            Arc::new(move |k| secrets.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
        )
    }

    #[test]
    fn bearer_auth_is_resolved_at_call_time_and_excluded_from_the_pool_key() {
        let def = def(json!({
            "url": "https://mcp.example.test",
            "auth": {
                "type": "bearer",
                "credential": "keyring:mcp_github_token"
            }
        }));
        let resolved = resolver("/w", &[], &[("mcp_github_token", "gh-secret")])
            .resolve(&def, None)
            .unwrap();
        let ResolvedTransport::Http { headers, .. } = resolved.transport else {
            panic!("expected http")
        };
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer gh-secret");
        assert!(!resolved.key.params().contains("gh-secret"));
        let rotated = resolver("/w", &[], &[("mcp_github_token", "new-secret")])
            .resolve(&def, None)
            .unwrap();
        assert_eq!(resolved.key, rotated.key);
    }

    #[test]
    fn stdio_defaults_its_directory_to_the_workspace_root() {
        let r = resolver("/w/one", &[], &[]);
        let out = r.resolve(&def(json!({ "command": "srv" })), None).unwrap();
        match out.transport {
            ResolvedTransport::Stdio { cwd, .. } => assert_eq!(cwd, PathBuf::from("/w/one")),
            other => panic!("{other:?}"),
        }
    }

    /// The central claim of the module: sharing follows from the parameters.
    #[test]
    fn a_server_that_never_mentions_the_workspace_is_one_connection_everywhere() {
        let d = def(json!({ "command": "gh-mcp", "cwd": "/opt/gh" }));
        let a = resolver("/w/one", &[], &[]).resolve(&d, None).unwrap();
        let b = resolver("/w/two", &[], &[]).resolve(&d, None).unwrap();
        assert_eq!(a.key, b.key, "identical parameters must share a connection");
    }

    #[test]
    fn a_server_whose_args_name_the_workspace_splits_by_itself() {
        let d = def(json!({ "command": "fs", "args": ["${workspaceRoot}"] }));
        let a = resolver("/w/one", &[], &[]).resolve(&d, None).unwrap();
        let b = resolver("/w/two", &[], &[]).resolve(&d, None).unwrap();
        assert_ne!(a.key, b.key);
    }

    /// The rule that makes directory dependence decidable rather than guessed at.
    #[test]
    fn the_default_directory_makes_a_plain_stdio_server_per_workspace() {
        let d = def(json!({ "command": "srv" }));
        let a = resolver("/w/one", &[], &[]).resolve(&d, None).unwrap();
        let b = resolver("/w/two", &[], &[]).resolve(&d, None).unwrap();
        assert_ne!(a.key, b.key, "cwd is part of the parameters");
    }

    /// Rotating a token must not leave two connections to the same server.
    #[test]
    fn a_rotated_credential_does_not_split_the_pool() {
        let d = def(json!({ "url": "https://h/mcp",
                            "headers": { "Authorization": "Bearer ${env:TOKEN}" } }));
        let old = resolver("/w", &[("TOKEN", "aaa")], &[])
            .resolve(&d, None)
            .unwrap();
        let new = resolver("/w", &[("TOKEN", "bbb")], &[])
            .resolve(&d, None)
            .unwrap();
        assert_eq!(old.key, new.key);
        // But the launch itself uses the current value.
        match new.transport {
            ResolvedTransport::Http { headers, .. } => {
                assert_eq!(headers["Authorization"], "Bearer bbb");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The one hand-written scope, and it only takes effect where it is written.
    #[test]
    fn session_binding_keys_by_session_and_nothing_else_does() {
        let plain = def(json!({ "command": "browser" }));
        let bound = def(json!({ "command": "browser", "binding": "session" }));
        let (s1, s2) = (SessionId::new(), SessionId::new());
        let r = resolver("/w", &[], &[]);

        assert_eq!(
            r.resolve(&plain, Some(s1)).unwrap().key,
            r.resolve(&plain, Some(s2)).unwrap().key
        );
        assert_ne!(
            r.resolve(&bound, Some(s1)).unwrap().key,
            r.resolve(&bound, Some(s2)).unwrap().key
        );
        assert_eq!(r.resolve(&bound, Some(s1)).unwrap().key.session(), Some(s1));
    }

    #[test]
    fn a_keyring_reference_flattens_to_one_entry_name() {
        let d = def(json!({ "url": "https://h/mcp",
                            "headers": { "Authorization": "${keyring:mcp/gh/token}" } }));
        let r = resolver("/w", &[], &[("mcp_gh_token", "s3cret")]);
        match r.resolve(&d, None).unwrap().transport {
            ResolvedTransport::Http { headers, .. } => {
                assert_eq!(headers["Authorization"], "s3cret")
            }
            other => panic!("{other:?}"),
        }
    }

    /// `${VAR}` without a scheme is what `.mcp.json` files in the wild are written with.
    #[test]
    fn a_bare_variable_is_an_environment_variable() {
        let d = def(json!({ "command": "srv", "args": ["--home=${HOME_DIR}"] }));
        let r = resolver("/w", &[("HOME_DIR", "/h")], &[]);
        match r.resolve(&d, None).unwrap().transport {
            ResolvedTransport::Stdio { args, .. } => assert_eq!(args, ["--home=/h"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_default_keeps_an_unset_variable_from_failing() {
        let d = def(json!({ "command": "srv", "args": ["${LOG:-info}", "${env:MODE:-fast}"] }));
        let r = resolver("/w", &[], &[]);
        match r.resolve(&d, None).unwrap().transport {
            ResolvedTransport::Stdio { args, .. } => assert_eq!(args, ["info", "fast"]),
            other => panic!("{other:?}"),
        }
    }

    /// A missing credential is a definite, named failure — not an empty string passed to a server
    /// that will answer 401 half a second later.
    #[test]
    fn a_missing_variable_names_itself() {
        let d = def(json!({ "command": "srv", "env": { "T": "${env:NOPE}" } }));
        match resolver("/w", &[], &[]).resolve(&d, None) {
            Err(McpError::Template {
                placeholder,
                reason,
            }) => {
                assert_eq!(placeholder, "${env:NOPE}");
                assert!(reason.contains("NOPE"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_blank_variable_counts_as_unset() {
        let d = def(json!({ "command": "srv", "env": { "T": "${env:BLANK}" } }));
        assert!(
            resolver("/w", &[("BLANK", "")], &[])
                .resolve(&d, None)
                .is_err()
        );
    }

    #[test]
    fn several_placeholders_in_one_string_all_expand() {
        let d = def(json!({ "command": "srv", "args": ["${A}/${B}/x"] }));
        let r = resolver("/w", &[("A", "1"), ("B", "2")], &[]);
        match r.resolve(&d, None).unwrap().transport {
            ResolvedTransport::Stdio { args, .. } => assert_eq!(args, ["1/2/x"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unclosed_placeholder_is_literal_text() {
        let d = def(json!({ "command": "srv", "args": ["${oops"] }));
        match resolver("/w", &[], &[])
            .resolve(&d, None)
            .unwrap()
            .transport
        {
            ResolvedTransport::Stdio { args, .. } => assert_eq!(args, ["${oops"]),
            other => panic!("{other:?}"),
        }
    }

    /// A relative `cwd` is relative to the workspace, which is the only root a definition can mean.
    #[test]
    fn a_relative_directory_resolves_against_the_workspace() {
        let d = def(json!({ "command": "srv", "cwd": "sub/dir" }));
        match resolver("/w", &[], &[])
            .resolve(&d, None)
            .unwrap()
            .transport
        {
            ResolvedTransport::Stdio { cwd, .. } => assert_eq!(cwd, PathBuf::from("/w/sub/dir")),
            other => panic!("{other:?}"),
        }
    }

    /// Environment order must not reach the key: the same definition has to hash the same twice.
    #[test]
    fn the_key_is_stable_across_calls() {
        let d = def(json!({ "command": "srv",
                            "env": { "Z": "1", "A": "2", "M": "3" } }));
        let r = resolver("/w", &[], &[]);
        assert_eq!(
            r.resolve(&d, None).unwrap().key,
            r.resolve(&d, None).unwrap().key
        );
    }

    /// Two servers differing only in one arg must not collide.
    #[test]
    fn different_parameters_are_different_keys() {
        let r = resolver("/w", &[], &[]);
        let a = r
            .resolve(&def(json!({ "command": "srv", "args": ["--a"] })), None)
            .unwrap();
        let b = r
            .resolve(&def(json!({ "command": "srv", "args": ["--b"] })), None)
            .unwrap();
        assert_ne!(a.key, b.key);
    }

    /// The id is not part of the key: two names for the same launch are the same connection.
    #[test]
    fn the_server_id_does_not_enter_the_key() {
        let r = resolver("/w", &[], &[]);
        let mut a = def(json!({ "command": "srv" }));
        let mut b = a.clone();
        a.id = "one".into();
        b.id = "two".into();
        assert_eq!(
            r.resolve(&a, None).unwrap().key,
            r.resolve(&b, None).unwrap().key
        );
    }

    /// The claim the whole `global` scope rests on: one key, whatever workspace and session asks.
    #[test]
    fn a_global_server_resolves_to_one_key_everywhere() {
        let d = def(
            json!({ "command": "npx", "args": ["@playwright/mcp@latest", "--root=${workspaceRoot}"], "binding": "global" }),
        );
        let r = resolver("/w", &[], &[]);
        let (s1, s2) = (SessionId::new(), SessionId::new());

        let a = r.resolve(&d, Some(s1)).unwrap();
        let b = resolver("/other", &[], &[]).resolve(&d, Some(s2)).unwrap();
        assert_eq!(
            a.key, b.key,
            "a global connection cannot vary with its caller"
        );
    }

    /// It is launched in a directory of ours, and that directory is the same one every time: a cwd
    /// borrowed from the first caller's workspace would make "here" depend on a race.
    #[test]
    fn a_global_stdio_server_runs_in_a_directory_of_ours() {
        let mut d = def(json!({ "command": "srv", "binding": "global" }));
        d.id = "browser".into();
        let shared = PathBuf::from("/shared").join("browser");
        for root in ["/w/one", "/w/two"] {
            let out = resolver(root, &[], &[]).resolve(&d, None).unwrap();
            let ResolvedTransport::Stdio { cwd, .. } = out.transport else {
                panic!("expected stdio")
            };
            assert_eq!(cwd, shared, "root was {root}");
        }
    }

    /// An id from a hand-edited file still cannot escape the shared directory.
    #[test]
    fn a_global_directory_name_is_sanitised() {
        let mut d = def(json!({ "command": "srv", "binding": "global" }));
        d.id = "../../etc".into();
        let out = resolver("/w", &[], &[]).resolve(&d, None).unwrap();
        let ResolvedTransport::Stdio { cwd, .. } = out.transport else {
            panic!("expected stdio")
        };
        assert_eq!(
            cwd.parent(),
            Some(Path::new("/shared")),
            "stayed put: {cwd:?}"
        );
        assert!(
            !cwd.to_string_lossy().contains(".."),
            "no traversal in {cwd:?}"
        );
    }

    /// A `cwd` the definition pins still wins — the scope decides sharing, not where the process runs.
    #[test]
    fn a_global_server_keeps_a_directory_it_declares() {
        let d = def(json!({ "command": "srv", "cwd": "/opt/browser", "binding": "global" }));
        let out = resolver("/w", &[], &[]).resolve(&d, None).unwrap();
        let ResolvedTransport::Stdio { cwd, .. } = out.transport else {
            panic!("expected stdio")
        };
        assert_eq!(cwd, PathBuf::from("/opt/browser"));
    }

    /// The point of the scope: parameters that would share everything now split per workspace.
    #[test]
    fn a_workspace_scope_splits_what_the_parameters_would_share() {
        let d = def(json!({ "command": "srv", "cwd": "/opt/gh", "binding": "workspace" }));
        let r = resolver("/w", &[], &[]);
        assert_ne!(
            r.resolve(&d, None).unwrap().key,
            resolver("/w/two", &[], &[]).resolve(&d, None).unwrap().key
        );
    }

    /// A placeholder that expands differently per workspace must not split a global key — that is
    /// the difference between the literal form and the expanded one.
    #[test]
    fn a_global_key_ignores_a_placeholder_that_varies() {
        let d =
            def(json!({ "command": "srv", "env": { "T": "${env:TOKEN}" }, "binding": "global" }));
        let old = resolver("/w", &[("TOKEN", "aaa")], &[])
            .resolve(&d, None)
            .unwrap();
        let new = resolver("/w", &[("TOKEN", "bbb")], &[])
            .resolve(&d, None)
            .unwrap();
        assert_eq!(old.key, new.key);
        // …while the launch still gets the current value.
        let ResolvedTransport::Stdio { env, .. } = new.transport else {
            panic!("expected stdio")
        };
        assert_eq!(env["T"], "bbb");
    }
}
