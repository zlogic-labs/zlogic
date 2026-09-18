//! Language detection + tree-sitter grammar wiring.
//! Kept deliberately tiny: add a language by (1) mapping its extensions here,
//! (2) returning its grammar, (3) listing its definition node kinds in
//! `symbols.rs`. No `.tsg`, no resolver — just enclosing-symbol lookup.

use std::path::Path;
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ts,
    Tsx,
    Py,
    Rust,
    Java,
    Go,
    C,
    Cpp,
    Ruby,
    Php,
    CSharp,
    Zig,
    Lua,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "ts" | "mts" | "cts" => Some(Lang::Ts),
            // TSX grammar is a superset that also parses JS/JSX fine for our
            // enclosing-symbol purpose.
            "tsx" | "jsx" | "js" | "mjs" | "cjs" => Some(Lang::Tsx),
            "py" | "pyi" => Some(Lang::Py),
            "rs" => Some(Lang::Rust),
            "java" => Some(Lang::Java),
            "go" => Some(Lang::Go),
            "c" | "h" => Some(Lang::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Lang::Cpp),
            "rb" => Some(Lang::Ruby),
            "php" => Some(Lang::Php),
            "cs" => Some(Lang::CSharp),
            "zig" => Some(Lang::Zig),
            "lua" => Some(Lang::Lua),
            _ => None,
        }
    }

    pub fn language(self) -> Language {
        // LanguageFn -> Language. If a pinned grammar predates LanguageFn,
        // swap these for the older `tree_sitter_python::language()` form.
        match self {
            Lang::Ts => Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            Lang::Tsx => Language::new(tree_sitter_typescript::LANGUAGE_TSX),
            Lang::Py => Language::new(tree_sitter_python::LANGUAGE),
            Lang::Rust => Language::new(tree_sitter_rust::LANGUAGE),
            Lang::Java => Language::new(tree_sitter_java::LANGUAGE),
            Lang::Go => Language::new(tree_sitter_go::LANGUAGE),
            Lang::C => Language::new(tree_sitter_c::LANGUAGE),
            Lang::Cpp => Language::new(tree_sitter_cpp::LANGUAGE),
            Lang::Ruby => Language::new(tree_sitter_ruby::LANGUAGE),
            Lang::Php => Language::new(tree_sitter_php::LANGUAGE_PHP),
            Lang::CSharp => Language::new(tree_sitter_c_sharp::LANGUAGE),
            Lang::Zig => Language::new(tree_sitter_zig::LANGUAGE),
            Lang::Lua => Language::new(tree_sitter_lua::LANGUAGE),
        }
    }
}
