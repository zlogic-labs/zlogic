//! Path canonicalization and zone classification.
//! Every judgement is made on a physically-resolved absolute path: tilde
//! expanded, joined against the tracked cwd, symlinks resolved component by
//! component (so a workspace-internal symlink pointing outside is judged as
//! outside, and `link/../x` is resolved through the link's real parent, not
//! lexically). Paths that cannot be resolved (glob, dynamic word, unknown
//! cwd) stay `Unresolved` — the decision layer must treat unresolved
//! write/delete targets as ask.

use std::path::{Path, PathBuf};

pub use zlogic_paths::physical_resolve;

#[derive(Debug, Clone, PartialEq)]
pub enum Cwd {
    Known(PathBuf),
    /// `cd $VAR`, `cd -`, sourced scripts, popd past the tracked stack…
    /// relative paths are unresolvable from here on.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    Workspace,
    Home,
    System,
    /// Credential-bearing locations. Built-in floor — a policy config must
    /// not be able to remove entries from it.
    Sensitive,
    /// UNC network paths (`\\server\share\...`) — outward-facing.
    Network,
    /// Non-filesystem PowerShell provider paths (`HKLM:`, `Env:` …).
    Provider,
    Other,
    Unresolved,
}

impl std::fmt::Display for Zone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Zone::Workspace => "workspace",
            Zone::Home => "home",
            Zone::System => "system",
            Zone::Sensitive => "sensitive",
            Zone::Network => "network",
            Zone::Provider => "provider",
            Zone::Other => "other",
            Zone::Unresolved => "unresolved",
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPath {
    /// As written in the command (display / audit).
    pub requested: String,
    /// Physically-resolved absolute path; None when unresolvable.
    pub resolved: Option<PathBuf>,
    pub zone: Zone,
}

impl std::fmt::Display for ResolvedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.resolved {
            Some(p) => write!(f, "{} -> {} [{}]", self.requested, p.display(), self.zone),
            None => write!(f, "{} -> <unresolved> [{}]", self.requested, self.zone),
        }
    }
}

/// Home-relative locations whose entire subtree is credential-bearing.
/// Shared with the Windows resolver (compared case-insensitively there).
pub(crate) const SENSITIVE_HOME_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".docker/config.json",
    ".config/gh",
    ".config/gcloud",
];

/// Basename patterns that are sensitive anywhere ( `*` wildcard only).
/// Keep lowercase — the Windows resolver matches case-insensitively.
pub(crate) const SENSITIVE_BASENAMES: &[&str] = &[
    "id_rsa*",
    "id_ed25519*",
    "id_ecdsa*",
    "id_dsa*",
    "*.pem",
    "*.p12",
    "*.pfx",
    ".env",
    ".env.*",
    "credentials",
];

const SENSITIVE_ABSOLUTE: &[&str] = &[
    "/etc/shadow",
    "/etc/sudoers",
    "/private/etc/shadow",
    "/private/etc/sudoers",
];

const SYSTEM_PREFIXES: &[&str] = &[
    "/etc",
    "/private/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/boot",
    "/System",
    "/Library",
    "/opt",
];

pub struct PathResolver {
    pub workspace: PathBuf,
    pub home: PathBuf,
}

impl PathResolver {
    pub fn new(workspace: PathBuf, home: PathBuf) -> Self {
        let cur = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let abs = |p: PathBuf| if p.is_absolute() { p } else { cur.join(p) };
        Self {
            workspace: physical_resolve(&abs(workspace)),
            home: physical_resolve(&abs(home)),
        }
    }

    pub fn resolve(&self, requested: &str, cwd: &Cwd, glob: bool) -> ResolvedPath {
        let expanded = self.expand_tilde(requested);
        if glob {
            return self.unresolved(requested, &expanded);
        }
        let abs = if expanded.is_absolute() {
            expanded
        } else {
            match cwd {
                Cwd::Known(c) => c.join(&expanded),
                Cwd::Unknown => return self.unresolved(requested, &expanded),
            }
        };
        let real = physical_resolve(&abs);
        let zone = self.classify(&real);
        ResolvedPath {
            requested: requested.to_string(),
            resolved: Some(real),
            zone,
        }
    }

    /// A path we cannot resolve at all (dynamic word). Sensitivity is still
    /// checked on the written form — it can only escalate, never clear.
    pub fn dynamic(&self, display: &str) -> ResolvedPath {
        self.unresolved(display, Path::new(display))
    }

    fn unresolved(&self, requested: &str, probe: &Path) -> ResolvedPath {
        let zone = if self.sensitive_by_string(probe) {
            Zone::Sensitive
        } else {
            Zone::Unresolved
        };
        ResolvedPath {
            requested: requested.to_string(),
            resolved: None,
            zone,
        }
    }

    pub fn classify(&self, p: &Path) -> Zone {
        // sensitive floor wins even inside the workspace (a workspace .env
        // is still a secret)
        if self.is_sensitive(p) {
            return Zone::Sensitive;
        }
        if p.starts_with(&self.workspace) {
            return Zone::Workspace;
        }
        if SYSTEM_PREFIXES.iter().any(|s| p.starts_with(s)) {
            return Zone::System;
        }
        if p.starts_with(&self.home) {
            return Zone::Home;
        }
        Zone::Other
    }

    fn is_sensitive(&self, p: &Path) -> bool {
        for sp in SENSITIVE_HOME_SUBPATHS {
            if p.starts_with(self.home.join(sp)) {
                return true;
            }
        }
        for ap in SENSITIVE_ABSOLUTE {
            if p.starts_with(ap) {
                return true;
            }
        }
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if SENSITIVE_BASENAMES.iter().any(|pat| glob_match(pat, name)) {
                return true;
            }
        }
        false
    }

    /// Heuristic sensitivity check for paths that never resolve (globs,
    /// `$HOME/.ssh/...`, unknown cwd): matches sensitive basenames and any
    /// component naming a sensitive home subtree.
    fn sensitive_by_string(&self, p: &Path) -> bool {
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if SENSITIVE_BASENAMES.iter().any(|pat| glob_match(pat, name)) {
                return true;
            }
        }
        p.components().any(|c| {
            c.as_os_str().to_str().is_some_and(|s| {
                SENSITIVE_HOME_SUBPATHS
                    .iter()
                    .any(|sp| sp.split('/').next() == Some(s))
            })
        })
    }

    fn expand_tilde(&self, s: &str) -> PathBuf {
        if s == "~" {
            return self.home.clone();
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return self.home.join(rest);
        }
        PathBuf::from(s)
    }
}

/// Minimal glob: `*` matches any run of characters. No `?`/classes — the
/// sensitive-basename table only needs `*`.
pub fn glob_match(pat: &str, s: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p[0] == b'*' {
            return m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..]));
        }
        !s.is_empty() && p[0] == s[0] && m(&p[1..], &s[1..])
    }
    m(pat.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star() {
        assert!(glob_match("*.pem", "server.pem"));
        assert!(glob_match("id_rsa*", "id_rsa.pub"));
        assert!(glob_match("id_rsa*", "id_rsa"));
        assert!(!glob_match("*.pem", "pem.txt"));
        assert!(glob_match(".env.*", ".env.local"));
        assert!(!glob_match(".env.*", ".env"));
    }

    #[cfg(not(windows))]
    fn abs(p: &str) -> String {
        p.to_string()
    }

    #[cfg(windows)]
    fn abs(p: &str) -> String {
        format!("C:{p}")
    }

    #[test]
    fn tilde_expansion() {
        let r = PathResolver::new(PathBuf::from(abs("/w")), PathBuf::from(abs("/h")));
        let rp = r.resolve(
            "~/x.txt",
            &Cwd::Known(PathBuf::from(abs("/anywhere"))),
            false,
        );
        assert_eq!(
            rp.resolved.as_deref(),
            Some(Path::new(&format!("{}/x.txt", abs("/h"))))
        );
    }

    #[test]
    fn dotdot_lexical_on_nonexisting() {
        let r = PathResolver::new(PathBuf::from("/w"), PathBuf::from("/h"));
        let rp = r.resolve(
            "/no/such/dir/../file",
            &Cwd::Known(PathBuf::from("/")),
            false,
        );
        assert_eq!(rp.resolved.as_deref(), Some(Path::new("/no/such/file")));
    }

    #[test]
    fn unknown_cwd_relative_is_unresolved() {
        let r = PathResolver::new(PathBuf::from("/w"), PathBuf::from("/h"));
        let rp = r.resolve("x.txt", &Cwd::Unknown, false);
        assert!(rp.resolved.is_none());
        assert_eq!(rp.zone, Zone::Unresolved);
    }

    #[test]
    fn sensitive_by_string_on_dynamic() {
        let r = PathResolver::new(PathBuf::from("/w"), PathBuf::from("/h"));
        let rp = r.dynamic("$HOME/.ssh/id_rsa");
        assert_eq!(rp.zone, Zone::Sensitive);
        let rp = r.resolve("~/.ssh/*", &Cwd::Unknown, true);
        assert_eq!(rp.zone, Zone::Sensitive);
    }
}
