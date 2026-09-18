//! Windows path resolution and zoning. LEXICAL only — this must work when
//! the policy engine runs on any OS (tests included), so no filesystem
//! calls. On an actual Windows host, junction/symlink/8.3-shortname
//! resolution should be layered on via fs canonicalization, which is not
//! implemented yet; until then those are an accepted gap of this static layer.
//! Windows-specific traps handled here:
//! - case-insensitive comparison everywhere (NTFS default);
//! - both `/` and `\` are separators;
//! - PER-DRIVE current directories: `C:x` is relative to drive C's own cwd,
//!   which is independent state from the shell's current drive;
//! - UNC paths (`\\server\share\...`) → `Zone::Network`;
//! - non-filesystem provider drives (`HKLM:`, `Env:` — multi-letter) →
//!   `Zone::Provider`, never treated as file paths;
//! - Win32 strips trailing dots/spaces from each component (`secret.env.`
//!   opens `secret.env`) — normalize the same way or basename floors miss;
//! - device names (`NUL` &c.) exist in every directory.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::path::{ResolvedPath, SENSITIVE_BASENAMES, SENSITIVE_HOME_SUBPATHS, Zone, glob_match};

#[derive(Debug, Clone, PartialEq)]
pub enum WinRoot {
    Drive(char), // uppercase
    Unc { server: String, share: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct WinAbs {
    pub root: WinRoot,
    pub comps: Vec<String>,
}

impl WinAbs {
    pub fn display(&self) -> String {
        match &self.root {
            WinRoot::Drive(d) => format!("{}:\\{}", d, self.comps.join("\\")),
            WinRoot::Unc { server, share } => {
                format!("\\\\{}\\{}\\{}", server, share, self.comps.join("\\"))
            }
        }
    }

    fn same_root(&self, other: &WinAbs) -> bool {
        match (&self.root, &other.root) {
            (WinRoot::Drive(a), WinRoot::Drive(b)) => a.eq_ignore_ascii_case(b),
            (
                WinRoot::Unc {
                    server: s1,
                    share: h1,
                },
                WinRoot::Unc {
                    server: s2,
                    share: h2,
                },
            ) => s1.eq_ignore_ascii_case(s2) && h1.eq_ignore_ascii_case(h2),
            _ => false,
        }
    }

    pub fn starts_with(&self, anchor: &WinAbs) -> bool {
        if !self.same_root(anchor) {
            return false;
        }
        self.comps.len() >= anchor.comps.len()
            && anchor
                .comps
                .iter()
                .zip(&self.comps)
                .all(|(a, s)| a.eq_ignore_ascii_case(s))
    }

    pub fn drive(&self) -> Option<char> {
        match self.root {
            WinRoot::Drive(d) => Some(d),
            WinRoot::Unc { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WinPathForm {
    Absolute(WinAbs),
    /// `C:x` — relative to drive C's OWN tracked cwd.
    DriveRelative {
        drive: char,
        rel: Vec<String>,
    },
    /// `\x` — root of the CURRENT drive.
    RootRelative(Vec<String>),
    Relative(Vec<String>),
    /// `HKLM:\...`, `Env:...` — PowerShell provider, not a file path.
    Provider(String),
}

/// Win32 strips trailing dots and spaces from each component.
fn clean_comp(c: &str) -> String {
    if c == "." || c == ".." {
        return c.to_string();
    }
    c.trim_end_matches(['.', ' ']).to_string()
}

fn split_comps(s: &str) -> Vec<String> {
    s.split(['\\', '/'])
        .map(clean_comp)
        .filter(|c| !c.is_empty() && c != ".")
        .collect()
}

fn apply(mut base: Vec<String>, rel: &[String]) -> Vec<String> {
    for c in rel {
        if c == ".." {
            base.pop(); // past the root stays at the root
        } else {
            base.push(c.clone());
        }
    }
    base
}

pub fn parse_path(s: &str) -> WinPathForm {
    let t = s.replace('/', "\\");
    if let Some(rest) = t.strip_prefix("\\\\") {
        let mut it = rest.splitn(3, '\\');
        let server = it.next().unwrap_or("").to_string();
        let share = it.next().unwrap_or("").to_string();
        let tail = it.next().unwrap_or("");
        return WinPathForm::Absolute(WinAbs {
            root: WinRoot::Unc { server, share },
            comps: apply(Vec::new(), &split_comps(tail)),
        });
    }
    if let Some(colon) = t.find(':') {
        let prefix = &t[..colon];
        if colon == 1 && prefix.chars().all(|c| c.is_ascii_alphabetic()) {
            let drive = prefix.chars().next().unwrap().to_ascii_uppercase();
            let rest = &t[colon + 1..];
            if rest.starts_with('\\') {
                return WinPathForm::Absolute(WinAbs {
                    root: WinRoot::Drive(drive),
                    comps: apply(Vec::new(), &split_comps(rest)),
                });
            }
            return WinPathForm::DriveRelative {
                drive,
                rel: split_comps(rest),
            };
        }
        if colon > 1 && prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return WinPathForm::Provider(s.to_string());
        }
    }
    if t.starts_with('\\') {
        return WinPathForm::RootRelative(split_comps(&t));
    }
    WinPathForm::Relative(split_comps(&t))
}

/// Per-drive current directories + the current drive. `cd D:\x` (no `/d`)
/// changes drive D's cwd WITHOUT switching to it; bare `D:` switches to
/// whatever D's cwd currently is.
#[derive(Debug, Clone)]
pub struct WinCwd {
    pub current: Option<WinAbs>,
    drives: HashMap<char, WinAbs>,
}

impl WinCwd {
    pub fn known(abs: WinAbs) -> Self {
        let mut cwd = WinCwd {
            current: None,
            drives: HashMap::new(),
        };
        cwd.set_current(abs);
        cwd
    }

    pub fn unknown() -> Self {
        WinCwd {
            current: None,
            drives: HashMap::new(),
        }
    }

    pub fn set_current(&mut self, abs: WinAbs) {
        if let Some(d) = abs.drive() {
            self.drives.insert(d, abs.clone());
        }
        self.current = Some(abs);
    }

    pub fn set_drive(&mut self, d: char, abs: WinAbs) {
        self.drives.insert(d.to_ascii_uppercase(), abs);
    }

    pub fn drive_cwd(&self, d: char) -> Option<WinAbs> {
        let d = d.to_ascii_uppercase();
        if let Some(cur) = &self.current {
            if cur.drive() == Some(d) {
                return Some(cur.clone());
            }
        }
        self.drives.get(&d).cloned()
    }

    /// Everything untracked from here on (dynamic cd target &c.).
    pub fn poison(&mut self) {
        self.current = None;
        self.drives.clear();
    }
}

pub(crate) enum AbsResult {
    Abs(WinAbs),
    Provider,
    Unresolvable,
}

/// Windows-only sensitive additions on top of the shared home lists.
const WIN_SENSITIVE_HOME: &[&str] = &[
    "appdata/roaming/microsoft/credentials",
    "appdata/local/microsoft/credentials",
    ".azure",
];
/// System-drive subtrees holding the SAM / security hives.
const WIN_SENSITIVE_SYSTEM: &[&[&str]] = &[&["windows", "system32", "config"]];
const WIN_SYSTEM_TOPDIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
];
const DEVICE_NAMES: &[&str] = &[
    "nul", "con", "prn", "aux", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

pub struct WinResolver {
    pub workspace: Option<WinAbs>,
    pub home: Option<WinAbs>,
}

impl WinResolver {
    pub fn new(workspace: &str, home: &str) -> Self {
        let anchor = |s: &str| match parse_path(s) {
            WinPathForm::Absolute(a) => Some(a),
            _ => None,
        };
        WinResolver {
            workspace: anchor(workspace),
            home: anchor(home),
        }
    }

    pub(crate) fn to_abs(&self, requested: &str, cwd: &WinCwd) -> AbsResult {
        // ~ / ~\x — PowerShell home shorthand (cmd passes it literally, but
        // resolving it here only escalates)
        if requested == "~" || requested.starts_with("~/") || requested.starts_with("~\\") {
            let Some(h) = &self.home else {
                return AbsResult::Unresolvable;
            };
            let rest = requested[1..].trim_start_matches(['/', '\\']);
            return AbsResult::Abs(WinAbs {
                root: h.root.clone(),
                comps: apply(h.comps.clone(), &split_comps(rest)),
            });
        }
        match parse_path(requested) {
            WinPathForm::Provider(_) => AbsResult::Provider,
            WinPathForm::Absolute(a) => AbsResult::Abs(a),
            WinPathForm::DriveRelative { drive, rel } => match cwd.drive_cwd(drive) {
                Some(b) => AbsResult::Abs(WinAbs {
                    root: b.root.clone(),
                    comps: apply(b.comps, &rel),
                }),
                None => AbsResult::Unresolvable,
            },
            WinPathForm::RootRelative(rel) => match &cwd.current {
                Some(c) => AbsResult::Abs(WinAbs {
                    root: c.root.clone(),
                    comps: apply(Vec::new(), &rel),
                }),
                None => AbsResult::Unresolvable,
            },
            WinPathForm::Relative(rel) => match &cwd.current {
                Some(c) => AbsResult::Abs(WinAbs {
                    root: c.root.clone(),
                    comps: apply(c.comps.clone(), &rel),
                }),
                None => AbsResult::Unresolvable,
            },
        }
    }

    pub fn resolve(&self, requested: &str, cwd: &WinCwd, glob: bool) -> ResolvedPath {
        if glob {
            return self.unresolved(requested);
        }
        match self.to_abs(requested, cwd) {
            AbsResult::Provider => ResolvedPath {
                requested: requested.to_string(),
                resolved: None,
                zone: Zone::Provider,
            },
            AbsResult::Unresolvable => self.unresolved(requested),
            AbsResult::Abs(abs) => {
                let zone = self.classify(&abs);
                ResolvedPath {
                    requested: requested.to_string(),
                    resolved: Some(PathBuf::from(abs.display())),
                    zone,
                }
            }
        }
    }

    pub fn dynamic(&self, display: &str) -> ResolvedPath {
        self.unresolved(display)
    }

    fn unresolved(&self, requested: &str) -> ResolvedPath {
        let zone = if sensitive_by_string(requested) {
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

    pub fn classify(&self, p: &WinAbs) -> Zone {
        if self.is_sensitive(p) {
            return Zone::Sensitive;
        }
        if let Some(ws) = &self.workspace {
            if p.starts_with(ws) {
                return Zone::Workspace;
            }
        }
        if matches!(p.root, WinRoot::Unc { .. }) {
            return Zone::Network;
        }
        if let Some(first) = p.comps.first() {
            if WIN_SYSTEM_TOPDIRS
                .iter()
                .any(|d| first.eq_ignore_ascii_case(d))
            {
                return Zone::System;
            }
        }
        if let Some(h) = &self.home {
            if p.starts_with(h) {
                return Zone::Home;
            }
        }
        Zone::Other
    }

    fn is_sensitive(&self, p: &WinAbs) -> bool {
        if let Some(h) = &self.home {
            for sp in SENSITIVE_HOME_SUBPATHS.iter().chain(WIN_SENSITIVE_HOME) {
                let mut anchor = h.clone();
                anchor.comps.extend(sp.split('/').map(str::to_string));
                if p.starts_with(&anchor) {
                    return true;
                }
            }
        }
        for sys in WIN_SENSITIVE_SYSTEM {
            if p.comps.len() >= sys.len()
                && sys
                    .iter()
                    .zip(&p.comps)
                    .all(|(a, c)| c.eq_ignore_ascii_case(a))
            {
                return true;
            }
        }
        if let Some(name) = p.comps.last() {
            let lower = name.to_lowercase();
            if SENSITIVE_BASENAMES
                .iter()
                .any(|pat| glob_match(pat, &lower))
            {
                return true;
            }
        }
        false
    }

    /// `NUL` (any directory, any casing) is a sink — writes to it carry no
    /// meaning, callers skip the op entirely.
    pub fn is_nul(requested: &str) -> bool {
        let base = requested.rsplit(['\\', '/']).next().unwrap_or(requested);
        base.eq_ignore_ascii_case("nul")
    }

    pub fn is_device(requested: &str) -> bool {
        let base = requested.rsplit(['\\', '/']).next().unwrap_or(requested);
        DEVICE_NAMES.iter().any(|d| base.eq_ignore_ascii_case(d))
    }
}

/// Escalate-only heuristic for strings that never resolve (`%X%\.ssh\k`).
fn sensitive_by_string(s: &str) -> bool {
    let comps: Vec<String> = s.split(['\\', '/']).map(|c| c.to_lowercase()).collect();
    if let Some(name) = comps.last() {
        if SENSITIVE_BASENAMES.iter().any(|pat| glob_match(pat, name)) {
            return true;
        }
    }
    comps.iter().any(|c| {
        SENSITIVE_HOME_SUBPATHS
            .iter()
            .chain(WIN_SENSITIVE_HOME)
            .any(|sp| sp.split('/').next() == Some(c.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd_at(s: &str) -> WinCwd {
        match parse_path(s) {
            WinPathForm::Absolute(a) => WinCwd::known(a),
            _ => panic!("test cwd must be absolute"),
        }
    }

    #[test]
    fn parse_forms() {
        assert!(matches!(parse_path("C:\\x\\y"), WinPathForm::Absolute(_)));
        assert!(matches!(parse_path("c:/x"), WinPathForm::Absolute(_)));
        assert!(matches!(
            parse_path("D:file"),
            WinPathForm::DriveRelative { drive: 'D', .. }
        ));
        assert!(matches!(parse_path("\\x"), WinPathForm::RootRelative(_)));
        assert!(matches!(parse_path("x\\y"), WinPathForm::Relative(_)));
        assert!(matches!(
            parse_path("\\\\srv\\share\\f"),
            WinPathForm::Absolute(_)
        ));
        assert!(matches!(
            parse_path("HKLM:\\Software"),
            WinPathForm::Provider(_)
        ));
    }

    #[test]
    fn per_drive_cwd() {
        let r = WinResolver::new("C:\\ws", "C:\\Users\\u");
        let mut cwd = cwd_at("C:\\ws");
        // D: untracked → D:file unresolvable
        let rp = r.resolve("D:file", &cwd, false);
        assert!(rp.resolved.is_none());
        cwd.set_drive(
            'D',
            match parse_path("D:\\data") {
                WinPathForm::Absolute(a) => a,
                _ => unreachable!(),
            },
        );
        let rp = r.resolve("D:file", &cwd, false);
        assert_eq!(
            rp.resolved.as_deref().unwrap().to_str().unwrap(),
            "D:\\data\\file"
        );
        // current drive unchanged
        let rp = r.resolve("x", &cwd, false);
        assert_eq!(
            rp.resolved.as_deref().unwrap().to_str().unwrap(),
            "C:\\ws\\x"
        );
    }

    #[test]
    fn case_insensitive_zones() {
        let r = WinResolver::new("C:\\ws", "C:\\Users\\u");
        let cwd = cwd_at("C:\\ws");
        assert_eq!(
            r.resolve("c:\\WS\\a.txt", &cwd, false).zone,
            Zone::Workspace
        );
        assert_eq!(
            r.resolve("C:\\Users\\u\\.SSH\\id_rsa", &cwd, false).zone,
            Zone::Sensitive
        );
        assert_eq!(
            r.resolve("C:\\WINDOWS\\a.dll", &cwd, false).zone,
            Zone::System
        );
    }

    #[test]
    fn trailing_dots_stripped() {
        let r = WinResolver::new("C:\\ws", "C:\\Users\\u");
        let cwd = cwd_at("C:\\ws");
        // Win32 opens `.env` for the name `.env.` — the floor must too
        assert_eq!(
            r.resolve("C:\\ws\\.env.", &cwd, false).zone,
            Zone::Sensitive
        );
    }

    #[test]
    fn unc_is_network() {
        let r = WinResolver::new("C:\\ws", "C:\\Users\\u");
        let cwd = cwd_at("C:\\ws");
        assert_eq!(
            r.resolve("\\\\srv\\share\\doc.txt", &cwd, false).zone,
            Zone::Network
        );
    }

    #[test]
    fn devices() {
        assert!(WinResolver::is_nul("NUL"));
        assert!(WinResolver::is_nul("nul"));
        assert!(WinResolver::is_device("COM3"));
        assert!(!WinResolver::is_device("nul.txt"));
    }

    #[test]
    fn dynamic_sensitive_heuristic() {
        let r = WinResolver::new("C:\\ws", "C:\\Users\\u");
        assert_eq!(
            r.dynamic("%USERPROFILE%\\.ssh\\id_rsa").zone,
            Zone::Sensitive
        );
    }
}
