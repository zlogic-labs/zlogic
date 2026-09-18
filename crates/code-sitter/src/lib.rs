//! code-sitter — code-aware grep.
//! Two-stage, stateless (no persistent index, no staleness, no per-language
//! resolver):
//!   1. `search` runs a ripgrep-style search (grep-regex + grep-searcher +
//!      ignore::WalkBuilder) and collects raw `path:line:text` hits.
//!   2. when `annotate` is on, ONLY the files that had hits are parsed with
//!      tree-sitter to tag each hit with its enclosing symbol (qualified, e.g.
//!      `(in method Checkout.computeTotal)`) and a hit kind
//!      (definition / call / comment / string).
//! Because stage 2 is pure syntax over the handful of hit files, it is cheap
//! and — crucially — accurate even in dynamic languages (Python/JS): finding
//! the enclosing `function`/`class` needs no type inference. Files whose
//! language we don't handle simply come back without annotation (graceful
//! degrade to plain grep output).

mod lang;
mod search;
mod symbols;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use search::SearchError;
pub use symbols::DefEntry;

/// The enclosing definition a hit sits inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Coarse kind label of the innermost definition: "fn" | "method" | "class".
    pub kind: String,
    /// Declared name of the innermost definition.
    pub name: String,
    /// Dotted chain of enclosing definitions, outermost first — e.g.
    /// `Checkout.computeTotal` or `C.speak.fireAndForget`. Equals `name` for
    /// top-level definitions.
    pub qualified: String,
}

/// What a hit IS, syntactically. Lets a caller (or the agent) separate "the
/// definition of X" from "a call of X" from "X mentioned in a comment".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// The hit token is the name of a definition.
    Def,
    /// The hit token is used as a callee (`foo()`, `obj.foo()`).
    Call,
    /// The hit lies inside a comment.
    Comment,
    /// The hit lies inside a string literal (not an interpolation hole).
    Str,
    /// Anything else.
    Code,
}

impl HitKind {
    /// Short display label; `None` for plain code (no tag shown).
    pub fn label(self) -> Option<&'static str> {
        match self {
            HitKind::Def => Some("def"),
            HitKind::Call => Some("call"),
            HitKind::Comment => Some("comment"),
            HitKind::Str => Some("string"),
            HitKind::Code => None,
        }
    }
}

/// A context line accompanying a hit (grep -C style).
#[derive(Debug, Clone)]
pub struct ContextLine {
    /// 1-based line number.
    pub line: u64,
    pub text: String,
}

/// One match line, optionally annotated.
#[derive(Debug, Clone)]
pub struct Hit {
    pub path: PathBuf,
    /// 1-based line number.
    pub line: u64,
    /// 0-based byte column of the match start within the line.
    pub col: usize,
    /// Match line text (trailing newline stripped).
    pub text: String,
    /// Enclosing symbol, when annotation is on and resolvable.
    pub symbol: Option<Symbol>,
    /// Hit classification; None when annotation is off / language unhandled.
    pub hit_kind: Option<HitKind>,
    /// Context lines (only when `Options.context_lines > 0`).
    pub context_before: Vec<ContextLine>,
    pub context_after: Vec<ContextLine>,
}

/// Search results plus honest bookkeeping — library consumers get these as
/// data (NOT stderr prints) so they can surface them to the agent/user.
#[derive(Debug)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    /// Files that errored mid-read and were skipped (potential false negatives).
    pub skipped_files: usize,
    /// True when `max_total` cut results short — narrow the search to see more.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Optional file glob filter, e.g. "*.ts" (gitignore glob semantics).
    pub include: Option<String>,
    pub case_sensitive: bool,
    /// Global cap on total hits.
    pub max_total: usize,
    /// Also search .gitignored paths (node_modules, build output).
    pub include_ignored: bool,
    /// Also search hidden (dot) files/dirs. Default false — matches ripgrep
    /// and the TS grep tool.
    pub include_hidden: bool,
    /// Directory basenames pruned before matches consume `max_total`.
    pub exclude_dirs: Vec<String>,
    /// File basenames skipped before matches consume `max_total`.
    pub exclude_files: Vec<String>,
    /// Run the tree-sitter annotation pass (enclosing symbol + hit kind) over
    /// hit files.
    pub annotate: bool,
    /// Lines of context before AND after each match (grep -C). 0 = off.
    pub context_lines: usize,
    /// Return one hit per matching file (its first match); skips annotation.
    pub files_only: bool,
    /// Cooperative cancellation: set the flag to true and the search returns
    /// `SearchError::Cancelled` at the next checkpoint.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include: None,
            case_sensitive: false,
            max_total: 100,
            include_ignored: false,
            include_hidden: false,
            exclude_dirs: Vec::new(),
            exclude_files: Vec::new(),
            annotate: true,
            context_lines: 0,
            files_only: false,
            cancel: None,
        }
    }
}

/// Run a code-aware search under `root`.
pub fn search(
    root: &std::path::Path,
    pattern: &str,
    opts: &Options,
) -> Result<SearchResult, SearchError> {
    // Stage 1 — raw hits.
    let run = search::run(root, pattern, opts)?;
    let do_annotate = opts.annotate && !opts.files_only;

    let mut hits: Vec<Hit> = Vec::with_capacity(run.hits.len());
    if !do_annotate {
        hits.extend(run.hits.into_iter().map(|r| to_hit(r, None, None)));
        return Ok(SearchResult {
            hits,
            skipped_files: run.skipped_files,
            truncated: run.truncated,
        });
    }

    // Stage 2 — group hits by file (preserving first-seen order) so each hit
    // file is parsed at most once.
    let mut groups: Vec<(PathBuf, Vec<search::RawHit>)> = Vec::new();
    let mut index_of: HashMap<PathBuf, usize> = HashMap::new();
    for r in run.hits {
        match index_of.get(&r.path) {
            Some(&i) => groups[i].1.push(r),
            None => {
                index_of.insert(r.path.clone(), groups.len());
                groups.push((r.path.clone(), vec![r]));
            }
        }
    }

    for (path, group) in groups {
        // Parse the file once (best-effort); None if language unknown / too
        // large / minified / parse fails.
        let file_syms = symbols::FileSymbols::parse(&path);
        for r in group {
            let (symbol, kind) = match file_syms.as_ref() {
                Some(fs) => {
                    let row = r.line.saturating_sub(1) as usize;
                    (fs.enclosing(row, r.col), Some(fs.classify(row, r.col)))
                }
                None => (None, None),
            };
            hits.push(to_hit(r, symbol, kind));
        }
    }

    Ok(SearchResult {
        hits,
        skipped_files: run.skipped_files,
        truncated: run.truncated,
    })
}

fn to_hit(r: search::RawHit, symbol: Option<Symbol>, hit_kind: Option<HitKind>) -> Hit {
    let ctx = |v: Vec<(u64, String)>| {
        v.into_iter()
            .map(|(line, text)| ContextLine { line, text })
            .collect()
    };
    Hit {
        path: r.path,
        line: r.line,
        col: r.col,
        text: r.text,
        symbol,
        hit_kind,
        context_before: ctx(r.before),
        context_after: ctx(r.after),
    }
}

/// One hit to annotate: `(path, 1-based line, 0-based byte column)`.
/// CONTRACT: `line` MUST be **1-based** (as every grep emits); `col` is the
/// 0-based BYTE column of the match within the line. Pass `col = 0` if the
/// external grep can't supply a column — annotation still works for most cases
/// but may misattribute a hit that shares a line with an outer construct (e.g.
/// an arrow-fn declaration line). Feeding a real column (rg `--column`, minus 1)
/// makes it exact.
pub type Pair = (PathBuf, u64, u32);

/// Annotation for one externally-supplied hit.
#[derive(Debug, Clone)]
pub struct Annotation {
    pub symbol: Option<Symbol>,
    /// None when the file's language is unhandled / parse failed.
    pub hit_kind: Option<HitKind>,
}

/// Batch-resolve annotations for `(path, line, col)` triples, preserving input
/// order. Groups by file so each file is parsed at most once.
/// Integration entrypoint for an EXISTING grep: let grep do the search, then
/// hand its hits here to add the enclosing symbol + hit kind. Files whose
/// language is unknown / that fail to parse yield an empty `Annotation`
/// (caller shows the plain grep line).
pub fn annotate(pairs: &[Pair]) -> Vec<Annotation> {
    let mut result: Vec<Annotation> = vec![
        Annotation {
            symbol: None,
            hit_kind: None
        };
        pairs.len()
    ];
    let mut by_file: HashMap<&PathBuf, Vec<usize>> = HashMap::new();
    for (i, (p, _, _)) in pairs.iter().enumerate() {
        by_file.entry(p).or_default().push(i);
    }
    for (path, idxs) in by_file {
        if let Some(fs) = symbols::FileSymbols::parse(path) {
            for i in idxs {
                let (_, line, col) = &pairs[i];
                let row = line.saturating_sub(1) as usize;
                let col = *col as usize;
                result[i] = Annotation {
                    symbol: fs.enclosing(row, col),
                    hit_kind: Some(fs.classify(row, col)),
                };
            }
        }
    }
    result
}

/// Definition outline of `path`: every definition the tree-sitter pass knows,
/// in document order, with nesting depth and 1-based line spans — what a caller
/// needs to offer "here is the file's structure, pick a range".
/// Returns `None` when the language is unhandled, the file is unreadable, too
/// large / minified, or parsing fails: callers then fall back to reading the
/// file whole (their existing behaviour). A known-language file with no
/// definitions comes back as `Some(vec![])`.
pub fn outline(path: &PathBuf) -> Option<Vec<DefEntry>> {
    symbols::FileSymbols::parse(path).map(|fs| fs.defs())
}

/// Every distinct tree-sitter node kind of `path`, sorted. Diagnostic surface
/// for wiring a new language: point it at a sample file and see what the
/// grammar actually names its nodes before writing `def_of` arms.
pub fn node_kinds(path: &PathBuf) -> Option<Vec<String>> {
    symbols::FileSymbols::parse(path).map(|fs| fs.node_kinds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// Fresh unique temp dir for one test (the search root).
    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "code-sitter-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scratch(name: &str, content: &str) -> PathBuf {
        let dir = scratch_dir();
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        dir
    }

    fn sym(kind: &str, name: &str, qualified: &str) -> Symbol {
        Symbol {
            kind: kind.into(),
            name: name.into(),
            qualified: qualified.into(),
        }
    }

    fn first_symbol(name: &str, content: &str, pattern: &str) -> Option<Symbol> {
        let dir = scratch(name, content);
        let res = search(&dir, pattern, &Options::default()).unwrap();
        res.hits.into_iter().next().and_then(|h| h.symbol)
    }

    #[test]
    fn ts_function_declaration() {
        let s = first_symbol("a.ts", "function foo() {\n  return bar();\n}\n", "bar");
        assert_eq!(s, Some(sym("fn", "foo", "foo")));
    }

    #[test]
    fn ts_method_definition_qualified() {
        let s = first_symbol("a.ts", "class C {\n  greet() {\n    hi();\n  }\n}\n", "hi");
        assert_eq!(s, Some(sym("method", "greet", "C.greet")));
    }

    /// Regression: a hit on the arrow-fn DECLARATION line must resolve to the
    /// arrow (`fireAndForget`), not the enclosing method (`speak`). Only works
    /// because the match column is carried through.
    #[test]
    fn ts_arrow_declaration_line_uses_column() {
        let src = "class C {\n  speak() {\n    const fireAndForget = () => run();\n  }\n}\n";
        let s = first_symbol("b.ts", src, "fireAndForget");
        assert_eq!(s, Some(sym("fn", "fireAndForget", "C.speak.fireAndForget")));
    }

    /// Regression: object-literal arrow binding resolves via the `key` field.
    #[test]
    fn ts_object_pair_uses_key_field() {
        let src = "const o = {\n  compute: () => {\n    return 1;\n  },\n};\n";
        let s = first_symbol("c.ts", src, "return 1");
        assert_eq!(s, Some(sym("method", "compute", "compute")));
    }

    #[test]
    fn py_function() {
        let s = first_symbol("d.py", "def foo():\n    return bar()\n", "bar");
        assert_eq!(s, Some(sym("fn", "foo", "foo")));
    }

    #[test]
    fn rust_outline_uses_impl_type_as_container() {
        let src = "struct Wallet {}\n\nimpl Wallet {\n    fn pay(&self) {}\n}\n\nfn helper() {}\n";
        let dir = scratch("w.rs", src);
        let defs = outline(&dir.join("w.rs")).unwrap();
        let flat: Vec<(usize, &str, &str)> = defs
            .iter()
            .map(|d| (d.depth, d.symbol.kind.as_str(), d.symbol.qualified.as_str()))
            .collect();
        assert_eq!(
            flat,
            vec![
                (0, "struct", "Wallet"),
                (0, "impl", "Wallet"),
                (1, "fn", "Wallet.pay"),
                (0, "fn", "helper"),
            ]
        );
        let pay = defs
            .iter()
            .find(|d| d.symbol.qualified == "Wallet.pay")
            .unwrap();
        assert_eq!((pay.start_line, pay.end_line), (4, 4));
    }

    #[test]
    fn java_outline_qualifies_methods_and_constructors() {
        let src = "public class Cart {\n  Cart() {}\n  void add(int n) {}\n}\n";
        let dir = scratch("Cart.java", src);
        let defs = outline(&dir.join("Cart.java")).unwrap();
        let flat: Vec<(usize, &str, &str)> = defs
            .iter()
            .map(|d| (d.depth, d.symbol.kind.as_str(), d.symbol.qualified.as_str()))
            .collect();
        assert_eq!(
            flat,
            vec![
                (0, "class", "Cart"),
                (1, "constructor", "Cart.Cart"),
                (1, "method", "Cart.add"),
            ]
        );
    }

    /// Runs an outline over an in-memory sample and asserts it contains the
    /// given `(depth, kind, qualified)` rows (order-insensitive; other defs are
    /// fine). Grammar node shapes differ enough between languages that only
    /// "does the expected definition show up" is asserted, plus its nesting.
    fn assert_outline_has(file: &str, content: &str, expected: &[(usize, &str, &str)]) {
        let dir = scratch(file, content);
        let defs = outline(&dir.join(file)).unwrap();
        let flat: Vec<(usize, String, String)> = defs
            .iter()
            .map(|d| (d.depth, d.symbol.kind.clone(), d.symbol.qualified.clone()))
            .collect();
        for (depth, kind, qualified) in expected {
            assert!(
                flat.contains(&(*depth, kind.to_string(), qualified.to_string())),
                "outline of {file} must contain ({depth}, {kind}, {qualified}); got {flat:?}"
            );
        }
    }

    #[test]
    fn go_outline_lists_functions_types_and_methods() {
        assert_outline_has(
            "g.go",
            "package p\n\nfunc Add(a, b int) int { return a + b }\n\
             type Wallet struct{ ID int }\n\nfunc (w *Wallet) Pay(amount int) {}\n",
            &[(0, "fn", "Add"), (0, "struct", "Wallet"), (0, "fn", "Pay")],
        );
    }

    #[test]
    fn c_outline_extracts_names_out_of_declarators() {
        assert_outline_has(
            "lib.c",
            "int add(int a, int b) { return a + b; }\n\
             struct Wallet { int id; };\n\
             enum Color { RED, BLUE };\n",
            &[
                (0, "fn", "add"),
                (0, "struct", "Wallet"),
                (0, "enum", "Color"),
            ],
        );
    }

    #[test]
    fn cpp_outline_qualifies_inside_namespaces_and_classes() {
        assert_outline_has(
            "app.cpp",
            "namespace app {\nclass Wallet {\n public:\n  void pay() {}\n};\n\
             struct Box { int x; };\nint add(int a, int b) { return a + b; }\n}\n",
            &[
                (0, "module", "app"),
                (1, "class", "app.Wallet"),
                (2, "fn", "app.Wallet.pay"),
                (1, "fn", "app.add"),
            ],
        );
    }

    #[test]
    fn ruby_outline_qualifies_modules_classes_and_methods() {
        assert_outline_has(
            "app.rb",
            "module App\n  class Wallet\n    def pay(amount)\n    end\n  end\n\
             \n  def helper\n  end\nend\n",
            &[
                (0, "module", "App"),
                (1, "class", "App.Wallet"),
                (2, "method", "App.Wallet.pay"),
                (1, "method", "App.helper"),
            ],
        );
    }

    #[test]
    fn php_outline_lists_functions_classes_and_members() {
        assert_outline_has(
            "app.php",
            "<?php\nnamespace App;\n\nfunction helper() { return 1; }\n\
             class Wallet {\n  public function pay(int $x) {}\n}\n\
             interface Payer {}\ntrait Payable {}\n",
            &[
                (0, "fn", "helper"),
                (0, "class", "Wallet"),
                (1, "method", "Wallet.pay"),
                (0, "interface", "Payer"),
                (0, "trait", "Payable"),
            ],
        );
    }

    #[test]
    fn csharp_outline_qualifies_members_inside_classes() {
        assert_outline_has(
            "App.cs",
            "namespace App;\n\npublic class Wallet {\n  public Wallet() {}\n  \
             public void Pay(int x) {}\n}\n\npublic interface Payer { }\n",
            &[
                (0, "class", "Wallet"),
                (1, "constructor", "Wallet.Wallet"),
                (1, "method", "Wallet.Pay"),
                (0, "class", "Payer"),
            ],
        );
    }

    #[test]
    fn zig_outline_lists_functions_and_container_types() {
        assert_outline_has(
            "lib.zig",
            "const std = @import(\"std\");\n\nfn helper() usize { return 1; }\n\
             pub const Wallet = struct { id: u32 };\npub const Color = enum { red, blue };\n",
            &[
                (0, "fn", "helper"),
                (0, "struct", "Wallet"),
                (0, "enum", "Color"),
            ],
        );
    }

    #[test]
    fn lua_outline_lists_functions() {
        assert_outline_has(
            "m.lua",
            "local function helper(a) return a end\nfunction M.run() end\n",
            &[(0, "fn", "helper")],
        );
    }

    /// Regression: a missing root is an error, not empty results.
    #[test]
    fn missing_root_errors() {
        let dir = std::env::temp_dir().join("code-sitter-nope-xyz-does-not-exist");
        assert!(search(&dir, "x", &Options::default()).is_err());
    }

    /// Hit-kind labeling: comment / def / call / string.
    #[test]
    fn hit_kinds() {
        let src =
            "// call foo in comment\nfunction foo() {\n  return foo();\n}\nconst s = \"foo\";\n";
        let dir = scratch("k.ts", src);
        let res = search(&dir, "foo", &Options::default()).unwrap();
        let kinds: Vec<HitKind> = res.hits.iter().map(|h| h.hit_kind.unwrap()).collect();
        assert_eq!(
            kinds,
            vec![HitKind::Comment, HitKind::Def, HitKind::Call, HitKind::Str]
        );
    }

    #[test]
    fn context_lines_attach_to_hit() {
        let dir = scratch("ctx.txt", "alpha\nneedle here\nomega\n");
        let opts = Options {
            context_lines: 1,
            ..Options::default()
        };
        let res = search(&dir, "needle", &opts).unwrap();
        assert_eq!(res.hits.len(), 1);
        let h = &res.hits[0];
        assert_eq!(h.context_before.len(), 1);
        assert_eq!(h.context_before[0].text, "alpha");
        assert_eq!(h.context_after.len(), 1);
        assert_eq!(h.context_after[0].text, "omega");
    }

    #[test]
    fn files_only_one_hit_per_file() {
        let dir = scratch_dir();
        std::fs::write(dir.join("x.txt"), "hit\nhit\n").unwrap();
        std::fs::write(dir.join("y.txt"), "hit\nhit\n").unwrap();
        let opts = Options {
            files_only: true,
            ..Options::default()
        };
        let res = search(&dir, "hit", &opts).unwrap();
        assert_eq!(res.hits.len(), 2);
        assert!(res.hits.iter().all(|h| h.symbol.is_none()));
    }

    #[test]
    fn truncation_reported() {
        let dir = scratch("t.txt", "m\nm\nm\n");
        let opts = Options {
            max_total: 2,
            ..Options::default()
        };
        let res = search(&dir, "m", &opts).unwrap();
        assert_eq!(res.hits.len(), 2);
        assert!(res.truncated);
    }

    /// Binary files (NUL byte) must not produce garbage hits.
    #[test]
    fn outline_lists_definitions_in_document_order_with_spans() {
        let src = "function top() {\n  return 1;\n}\n\
                   export class C {\n  greet() {\n    const tick = () => 1;\n    return tick();\n  }\n}\n\
                   const last = () => 2;\n";
        let dir = scratch("o.ts", src);
        let defs = outline(&dir.join("o.ts")).unwrap();
        let rows: Vec<(usize, usize, &str)> = defs
            .iter()
            .map(|d| (d.depth, d.start_line, d.symbol.qualified.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (0, 1, "top"),
                (0, 4, "C"),
                (1, 5, "C.greet"),
                (2, 6, "C.greet.tick"),
                (0, 10, "last"),
            ]
        );
        let greet = &defs[2];
        assert_eq!((greet.start_line, greet.end_line), (5, 8));
    }

    #[test]
    fn outline_returns_none_for_unhandled_languages() {
        let dir = scratch("plain.txt", "hello\nworld\n");
        assert!(outline(&dir.join("plain.txt")).is_none());
    }

    #[test]
    fn binary_files_skipped() {
        let dir = scratch_dir();
        std::fs::write(dir.join("blob.bin"), b"\x00foo").unwrap();
        let res = search(&dir, "foo", &Options::default()).unwrap();
        assert_eq!(res.hits.len(), 0);
    }
}
