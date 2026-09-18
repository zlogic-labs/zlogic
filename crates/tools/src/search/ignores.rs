//! What the search tools skip, and why.
//! # Three layers, in order of authority
//! 1. **The project's `.gitignore`.** Whatever the user declared as not-source. Handled by the
//!    `ignore` crate, which is the same implementation ripgrep uses — nesting, `!` re-inclusion
//!    and directory precedence all behave the way the user expects from `git`.
//! 2. **[`NEVER_SOURCE_DIRS`].** Dependency trees and tool caches, applied *on top of*
//!    `.gitignore`. A project that forgot to ignore `node_modules` should still get a usable
//!    search rather than ten thousand hits inside its dependencies.
//! 3. **[`BUILD_OUTPUT_DIRS`].** Ambiguous names — `build`, `dist`, `target`, `bin` — that are
//!    generated output in one project and real source in the next. Excluded **only** when the tree
//!    has no `.gitignore` anywhere above the search root. With one present, the project has
//!    already declared which of these are output; a name it did *not* list is source, and
//!    guessing over that declaration would hide real code.
//! # Why this list is not the one the ignore-walker uses
//! A tracked `node_modules` is still noise for search even though the project committed it, and
//! `.idea/` matters here even though it is small: search asks *what is worth reading*, not *what
//! is worth storing*.

use std::path::Path;

/// Dependency trees and tool caches: never hand-written, always reconstructible.
/// Deliberately excludes `vendor` / `Pods` / `third_party`, which are frequently committed on
/// purpose — hiding them unconditionally would hide real, searchable code.
pub(crate) const NEVER_SOURCE_DIRS: &[&str] = &[
    ".git",
    // JS / TS
    "node_modules",
    "bower_components",
    ".pnpm-store",
    ".yarn",
    // Python
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    // Tool caches and editor metadata
    ".gradle",
    ".terraform",
    ".serverless",
    ".aws-sam",
    ".turbo",
    ".parcel-cache",
    ".cache",
    ".direnv",
    ".idea",
    ".vscode",
];

/// Build-output names that are also plausible source directory names.
/// Only excluded when there is no `.gitignore` to defer to — see the module docs.
pub(crate) const BUILD_OUTPUT_DIRS: &[&str] = &[
    "target",
    "build",
    "out",
    "bin",
    "obj",
    "_build",
    "dist",
    "coverage",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".angular",
    ".output",
    ".build",
    "DerivedData",
];

/// Individual files that are never worth matching.
const NEVER_SOURCE_FILES: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Does a `.gitignore` exist at `root` or anywhere above it?
/// Walked upward because a monorepo package usually has none of its own and relies on the one at
/// the repository root — checking only `root` would wrongly conclude "no signal" and start hiding
/// `dist/`.
pub(crate) fn has_gitignore_above(root: &Path) -> bool {
    let mut dir = Some(root);
    while let Some(d) = dir {
        if d.join(".gitignore").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// The built-in exclusions, resolved for one search root.
#[derive(Debug, Clone)]
pub(crate) struct Ignores {
    /// False when the caller asked to search everything; only `.git` still goes.
    respect: bool,
    /// Whether the ambiguous build-output names are in play — see [`BUILD_OUTPUT_DIRS`].
    build_dirs: bool,
}

impl Ignores {
    /// `respect = false` reduces this to "`.git` only", which is what an explicit
    /// "search the ignored files too" means: the object database is never useful to read, but
    /// everything else the user asked for is.
    pub(crate) fn resolve(root: &Path, respect: bool) -> Self {
        Self {
            respect,
            build_dirs: respect && !has_gitignore_above(root),
        }
    }

    /// Whether a path component names a directory to skip.
    pub(crate) fn skips_dir(&self, name: &str) -> bool {
        if name == ".git" {
            return true;
        }
        if !self.respect {
            return false;
        }
        NEVER_SOURCE_DIRS.contains(&name) || (self.build_dirs && BUILD_OUTPUT_DIRS.contains(&name))
    }

    pub(crate) fn skips_file(&self, name: &str) -> bool {
        self.respect && NEVER_SOURCE_FILES.contains(&name)
    }

    pub(crate) fn excluded_dir_names(&self) -> Vec<String> {
        let mut names = vec![".git".to_string()];
        if self.respect {
            names.extend(NEVER_SOURCE_DIRS.iter().map(|name| (*name).to_string()));
            if self.build_dirs {
                names.extend(BUILD_OUTPUT_DIRS.iter().map(|name| (*name).to_string()));
            }
        }
        names.sort();
        names.dedup();
        names
    }

    pub(crate) fn excluded_file_names(&self) -> Vec<String> {
        if self.respect {
            NEVER_SOURCE_FILES
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Whether any component of `path` is a skipped directory.
    /// For results that arrive already-walked (a `grep` hit) rather than from a walk this can
    /// steer. Checking the whole path is what makes it equivalent to having pruned the walk.
    #[cfg(test)]
    pub(crate) fn skips_path(&self, path: &Path) -> bool {
        let mut parts = path.components().rev();
        let file = parts.next();
        if let Some(std::path::Component::Normal(name)) = file
            && self.skips_file(&name.to_string_lossy())
        {
            return true;
        }
        parts.any(|c| match c {
            std::path::Component::Normal(name) => self.skips_dir(&name.to_string_lossy()),
            _ => false,
        })
    }

    /// A walker over `root` with `.gitignore` and the built-in exclusions applied.
    pub(crate) fn walker(&self, root: &Path) -> ignore::WalkBuilder {
        let mut b = ignore::WalkBuilder::new(root);
        b.standard_filters(self.respect)
            // A `.gitignore` is honoured wherever it is, repository or not: the user wrote it
            // down, and "is there a `.git` next to it" is not a statement about their intent.
            // This is the one place these tools are deliberately looser than ripgrep — and than
            // `grep`, which inherits ripgrep's stricter rule from `code-sitter`. It only shows up in
            // a directory that has a `.gitignore` and no repository, and it also keeps
            // [`Self::resolve`] coherent: deferring to a declaration we then ignored would leave
            // `target/` searched in exactly the case the deferral was meant to handle.
            .require_git(false)
            // Dotfiles are hidden by default (ripgrep's behaviour), but `.git` is pruned by
            // `filter_entry` too, so asking for hidden files never drags the object database in.
            .hidden(self.respect)
            // Symlinks are not followed: a link pointing at a parent makes the walk unbounded,
            // and a link out of the tree makes the results claim files that are not there.
            .follow_links(false);
        let rules = self.clone();
        b.filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                !rules.skips_dir(&name)
            } else {
                !rules.skips_file(&name)
            }
        });
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_trees_are_skipped_even_without_a_gitignore_entry() {
        let ig = Ignores {
            respect: true,
            build_dirs: false,
        };
        assert!(ig.skips_dir("node_modules"));
        assert!(ig.skips_dir(".venv"));
        assert!(ig.skips_dir(".git"));
        assert!(!ig.skips_dir("src"));
    }

    /// Committed dependency trees exist and are real code; they must stay searchable.
    #[test]
    fn deliberately_committed_vendor_trees_are_not_skipped() {
        let ig = Ignores {
            respect: true,
            build_dirs: true,
        };
        assert!(!ig.skips_dir("vendor"));
        assert!(!ig.skips_dir("Pods"));
        assert!(!ig.skips_dir("third_party"));
    }

    /// The whole point of the two-tier split: with a `.gitignore` present the project decides.
    #[test]
    fn ambiguous_build_names_defer_to_a_gitignore_when_there_is_one() {
        let with = Ignores {
            respect: true,
            build_dirs: false,
        };
        assert!(
            !with.skips_dir("build"),
            "the project's .gitignore already said what is output"
        );
        assert!(!with.skips_dir("bin"));

        let without = Ignores {
            respect: true,
            build_dirs: true,
        };
        assert!(
            without.skips_dir("build"),
            "no signal to defer to, so assume it is output"
        );
        assert!(without.skips_dir("target"));
    }

    /// "Search the ignored files too" means everything except the object database.
    #[test]
    fn opting_out_keeps_only_dot_git_excluded() {
        let ig = Ignores {
            respect: false,
            build_dirs: false,
        };
        assert!(ig.skips_dir(".git"), ".git is never worth reading");
        assert!(!ig.skips_dir("node_modules"));
        assert!(!ig.skips_dir("build"));
        assert!(!ig.skips_file(".DS_Store"));
    }

    #[test]
    fn a_path_is_skipped_when_any_directory_in_it_is() {
        let ig = Ignores {
            respect: true,
            build_dirs: false,
        };
        assert!(ig.skips_path(Path::new("node_modules/pkg/index.js")));
        assert!(ig.skips_path(Path::new("a/b/__pycache__/m.pyc")));
        assert!(ig.skips_path(Path::new("src/.DS_Store")));
        assert!(!ig.skips_path(Path::new("src/lib/index.js")));
        // A *file* named like an excluded directory is still a file.
        assert!(!ig.skips_path(Path::new("src/node_modules")));
    }

    #[test]
    fn a_gitignore_is_found_above_the_search_root() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("packages/app/src");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(!has_gitignore_above(&deep));

        std::fs::write(d.path().join(".gitignore"), "dist/\n").unwrap();
        assert!(
            has_gitignore_above(&deep),
            "a monorepo package relies on the root's .gitignore"
        );
    }
}
