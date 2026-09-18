use std::fs;
use std::path::{Component, Path, PathBuf};

pub fn normalise(path: &Path) -> String {
    let cleaned = fs::canonicalize(path).unwrap_or_else(|_| lexical(path));
    strip_verbatim(&cleaned.to_string_lossy())
}

pub fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        return rest.to_owned();
    }
    path.to_owned()
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        out
    }
}

/// Resolve an absolute path physically, component by component: the existing
/// prefix goes through the filesystem (symlinks resolved), the non-existing
/// tail is appended with lexical `.`/`..` handling. `..` is applied AFTER
/// resolving the prefix, so `link/../x` pops the link target's real parent.
pub fn physical_resolve(abs: &Path) -> PathBuf {
    let mut cur = PathBuf::new();
    let mut exists = true;
    for comp in abs.components() {
        match comp {
            Component::RootDir => cur.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::Prefix(pre) => cur.push(pre.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                cur.pop();
            }
            Component::Normal(n) => {
                cur.push(n);
                if exists {
                    match fs::canonicalize(&cur) {
                        Ok(c) => cur = c,
                        Err(_) => exists = false,
                    }
                }
            }
        }
    }
    cur
}

/// Normalize a floor path to the form the op resolvers emit, so the protected
/// floors' `starts_with` comparisons actually match.
/// On Windows, [`physical_resolve`] (via `fs::canonicalize`) yields verbatim
/// `\\?\C:\...` paths, while the Windows resolver and the shell analyzers build
/// display-form `C:\...` targets. A verbatim path never `starts_with`-matches a
/// display target, so without this the protected write/delete floors silently
/// do not apply on Windows — an agent could rewrite its own `policy.yaml`
/// without a human. Everything else is the identity.
#[cfg(windows)]
pub fn floor_normalize(path: &Path) -> PathBuf {
    use std::path::Prefix;

    let mut comps = path.components();
    if let Some(Component::Prefix(prefix)) = comps.next() {
        if let Prefix::VerbatimDisk(drive) = prefix.kind() {
            // `drive` is the ASCII code of the drive letter (u8). PathBuf::push
            // with an absolute component replaces the drive, so start from
            // `C:\` and append only the normal tail components.
            let mut out = PathBuf::from(format!("{}:\\", drive as char));
            for comp in comps.skip_while(|comp| matches!(comp, Component::RootDir)) {
                out.push(comp.as_os_str());
            }
            return out;
        }
    }
    path.to_path_buf()
}

/// Non-Windows floors already compare like for like ([`physical_resolve`] and
/// the resolver produce the same plain form).
#[cfg(not(windows))]
pub fn floor_normalize(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_windows_verbatim_prefix() {
        assert_eq!(strip_verbatim(r"\\?\C:\proj\zlogic"), r"C:\proj\zlogic");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\proj"),
            r"\\server\share\proj"
        );
    }

    #[test]
    fn leaves_every_other_path_alone() {
        assert_eq!(strip_verbatim("/Users/me/proj"), "/Users/me/proj");
        assert_eq!(strip_verbatim(r"C:\proj"), r"C:\proj");
        assert_eq!(strip_verbatim(r"\\server\share"), r"\\server\share");
    }

    #[test]
    fn normalise_roundtrips_an_existing_directory() {
        let dir = tempfile_dir();
        assert_eq!(normalise(&dir), dir.to_string_lossy());
    }

    #[test]
    fn normalise_falls_back_to_lexical() {
        let missing = std::env::temp_dir().join("zlogic-paths-definitely-missing-dir");
        let normalized = normalise(&missing);
        assert_eq!(normalized, missing.to_string_lossy());
        let with_dot = missing.join(".").join("sub");
        assert_eq!(normalise(&with_dot), missing.join("sub").to_string_lossy());
    }

    #[test]
    fn physical_resolve_applies_dotdot_lexically_on_nonexisting() {
        assert_eq!(
            physical_resolve(Path::new("/no/such/dir/../file")),
            Path::new("/no/such/file")
        );
    }

    #[test]
    fn physical_resolve_resolves_existing_symlinks() {
        let base = tempfile_dir();
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let link = base.join("link");
        make_symlink(&outside, &link);
        let probe = link.join("../x");
        assert_eq!(
            physical_resolve(&probe),
            physical_resolve(&base.join("x")),
            "link/../x must pop the link target's real parent, not the link's"
        );
    }

    #[cfg(windows)]
    #[test]
    fn floor_normalize_strips_verbatim_for_comparison() {
        let verbatim = Path::new(r"\\?\C:\Users\me\.zlogic\policy.yaml");
        let display = Path::new(r"C:\Users\me\.zlogic\policy.yaml");
        let normalized = floor_normalize(verbatim);
        assert_eq!(normalized, display);
        assert!(display.starts_with(&normalized));
        assert_eq!(floor_normalize(display), display);
    }

    #[cfg(unix)]
    #[test]
    fn floor_normalize_is_identity() {
        let p = Path::new("/ws/.zlogic/policy.yaml");
        assert_eq!(floor_normalize(p), p);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zlogic-paths-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn make_symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn make_symlink(target: &Path, link: &Path) {
        if std::os::windows::fs::symlink_dir(target, link).is_err() {
            return;
        }
    }
}
