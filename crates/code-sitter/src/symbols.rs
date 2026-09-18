//! Stage 2: enclosing-symbol lookup + hit classification via tree-sitter.
//! Version-robust on purpose: we do NOT use `Query`/`QueryCursor` (whose
//! iterator became a StreamingIterator in tree-sitter 0.24/0.25). We just
//! locate the node at the hit position and walk parents. Only stable Node API
//! is used: `descendant_for_point_range`, `kind`, `parent`,
//! `child_by_field_name`, `utf8_text`, `id`.

use std::path::Path;

use tree_sitter::{Node, Parser, Point, Tree};

use crate::lang::Lang;
use crate::{HitKind, Symbol};

/// Files larger than this are not parsed for annotation — the grep hit still
/// shows, just without a symbol. Guards against multi-second parses of huge
/// files whose symbols nobody needs.
const MAX_PARSE_BYTES: u64 = 1_000_000;
/// An absurdly long line near the top marks generated/minified code — skip.
const MAX_EARLY_LINE_BYTES: usize = 5_000;

/// One definition found by an outline pass, in document order.
#[derive(Debug, Clone, PartialEq)]
pub struct DefEntry {
    pub symbol: Symbol,
    /// Nesting depth: 0 = top-level.
    pub depth: usize,
    /// 1-based inclusive line span of the whole definition.
    pub start_line: usize,
    pub end_line: usize,
}

pub struct FileSymbols {
    source: String,
    tree: Tree,
    lang: Lang,
}

impl FileSymbols {
    /// Parse `path`. Returns None if the language is unhandled, the file is
    /// unreadable, too large / minified, or parsing fails — callers then just
    /// get no annotation.
    pub fn parse(path: &Path) -> Option<FileSymbols> {
        let lang = Lang::from_path(path)?;
        let meta = std::fs::metadata(path).ok()?;
        if meta.len() > MAX_PARSE_BYTES {
            return None;
        }
        let source = std::fs::read_to_string(path).ok()?;
        if source
            .lines()
            .take(10)
            .any(|l| l.len() > MAX_EARLY_LINE_BYTES)
        {
            return None; // minified bundle / generated single-liner
        }
        let mut parser = Parser::new();
        parser.set_language(&lang.language()).ok()?;
        let tree = parser.parse(source.as_bytes(), None)?;
        Some(FileSymbols { source, tree, lang })
    }

    /// Innermost named definition containing the hit at (`row`, `col`), both
    /// 0-based; `col` is a BYTE offset within the line (tree-sitter's unit).
    /// `Symbol.qualified` carries the full dotted chain of enclosing defs,
    /// e.g. `C.speak.fireAndForget`.
    pub fn enclosing(&self, row: usize, col: usize) -> Option<Symbol> {
        let point = Point { row, column: col };
        let mut chain: Vec<Symbol> = Vec::new(); // innermost first
        let mut node = self
            .tree
            .root_node()
            .descendant_for_point_range(point, point);
        while let Some(n) = node {
            if let Some(sym) = self.def_of(n) {
                chain.push(sym);
            }
            node = n.parent();
        }
        let innermost = chain.first()?.clone();
        let qualified = chain
            .iter()
            .rev()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(".");
        Some(Symbol {
            qualified,
            ..innermost
        })
    }

    /// Classify WHAT the hit at (`row`, `col`) is: the name of a definition, a
    /// call site, inside a comment / string literal, or plain code.
    pub fn classify(&self, row: usize, col: usize) -> HitKind {
        let point = Point { row, column: col };
        let Some(node) = self
            .tree
            .root_node()
            .descendant_for_point_range(point, point)
        else {
            return HitKind::Code;
        };

        // Comment / string literal? Climb, but stop at an interpolation hole —
        // code inside `${…}` / f-string braces is code, not string.
        let mut n = Some(node);
        while let Some(x) = n {
            match x.kind() {
                "template_substitution" | "interpolation" => break,
                k if k.contains("comment") => return HitKind::Comment,
                "string" | "template_string" | "string_fragment" | "string_content" => {
                    return HitKind::Str;
                }
                _ => {}
            }
            n = x.parent();
        }

        let Some(parent) = node.parent() else {
            return HitKind::Code;
        };

        // Definition name? (the hit token IS the name/key of a def node)
        if self.def_of(parent).is_some() {
            let name_node = parent
                .child_by_field_name("name")
                .or_else(|| parent.child_by_field_name("key"));
            if name_node.map(|nn| nn.id() == node.id()).unwrap_or(false) {
                return HitKind::Def;
            }
        }

        // Call site? `foo(…)` directly, or `obj.foo(…)` via TS member_expression
        // / Python attribute.
        let is_callee = |call: Node, target: Node| {
            matches!(call.kind(), "call_expression" | "call")
                && call
                    .child_by_field_name("function")
                    .map(|f| f.id() == target.id())
                    .unwrap_or(false)
        };
        if is_callee(parent, node) {
            return HitKind::Call;
        }
        // One `if let` chain rather than three nested blocks — edition 2024 allows it, and the
        // condition reads as the single question it is: is this the method part of `obj.foo(…)`.
        if matches!(parent.kind(), "member_expression" | "attribute")
            && let Some(gp) = parent.parent()
            && is_callee(gp, parent)
        {
            return HitKind::Call;
        }

        HitKind::Code
    }

    fn text(&self, node: Node) -> Option<String> {
        node.utf8_text(self.source.as_bytes())
            .ok()
            .map(|s| s.to_string())
    }

    fn named(&self, node: Node, field: &str, kind: &str) -> Option<Symbol> {
        let name = self.text(node.child_by_field_name(field)?)?;
        Some(Symbol {
            kind: kind.to_string(),
            qualified: name.clone(),
            name,
        })
    }

    /// Normalize an object-literal key node to a readable name:
    ///   `compute`        -> "compute"          (property_identifier)
    ///   `"compute"`      -> "compute"          (string: quotes stripped)
    ///   `[COMPUTE_KEY]`  -> "[COMPUTE_KEY]"    (computed: kept bracketed)
    ///   `#compute`       -> "#compute"         (private)
    fn key_name(&self, key: Node) -> Option<String> {
        let raw = self.text(key)?;
        Some(match key.kind() {
            "string" => strip_quotes(&raw),
            _ => raw, // ident / number / computed_property_name / private
        })
    }

    /// Map a node to the definition it represents, if it is one.
    /// (`Symbol.qualified` here is just the local name; `enclosing` rewrites it
    /// to the full chain.)
    fn def_of(&self, node: Node) -> Option<Symbol> {
        match self.lang {
            Lang::Ts | Lang::Tsx => self.def_of_ts(node),
            Lang::Py => match node.kind() {
                "function_definition" => self.named(node, "name", "fn"),
                "class_definition" => self.named(node, "name", "class"),
                _ => None,
            },
            Lang::Rust => self.def_of_rust(node),
            Lang::Java => self.def_of_java(node),
            Lang::Go => self.def_of_go(node),
            Lang::C | Lang::Cpp => self.def_of_c_family(node),
            Lang::Ruby => self.def_of_ruby(node),
            Lang::Php => self.def_of_php(node),
            Lang::CSharp => self.def_of_csharp(node),
            Lang::Zig => self.def_of_zig(node),
            Lang::Lua => self.def_of_lua(node),
        }
    }

    fn def_of_go(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_declaration" | "method_declaration" => self.named(node, "name", "fn"),
            "type_declaration" => {
                let spec = (0..node.child_count())
                    .find_map(|i| node.child(i).filter(|c| c.kind() == "type_spec"))?;
                let name = self.text(spec.child_by_field_name("name")?)?;
                let kind = match spec
                    .child_by_field_name("type")
                    .map(|t| t.kind())
                    .unwrap_or_default()
                {
                    "struct_type" => "struct",
                    "interface_type" => "interface",
                    _ => "type",
                };
                Some(Symbol {
                    kind: kind.to_string(),
                    qualified: name.clone(),
                    name,
                })
            }
            _ => None,
        }
    }

    fn def_of_c_family(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_definition" => {
                let name = self.declarator_name(node.child_by_field_name("declarator")?)?;
                Some(Symbol {
                    kind: "fn".to_string(),
                    qualified: name.clone(),
                    name,
                })
            }
            "struct_specifier" => self.named(node, "name", "struct"),
            "union_specifier" => self.named(node, "name", "union"),
            "enum_specifier" => self.named(node, "name", "enum"),
            "class_specifier" => self.named(node, "name", "class"),
            "namespace_definition" => self.named(node, "name", "module"),
            _ => None,
        }
    }

    /// Follows declarator nesting down to the identifier that names the function:
    /// `function_declarator` / `pointer_declarator` / `parenthesized_declarator`
    /// each delegate through their `declarator` field. When a grammar leaves that
    /// field out (e.g. C++ `void pay() {}` in a class body), falls back to the
    /// first plain identifier of the subtree, skipping parameter machinery.
    fn declarator_name(&self, node: Node) -> Option<String> {
        match node.kind() {
            "identifier" | "field_identifier" => self.text(node),
            "function_declarator"
            | "pointer_declarator"
            | "parenthesized_declarator"
            | "array_declarator"
            | "reference_declarator" => {
                if let Some(inner) = node.child_by_field_name("declarator") {
                    return self.declarator_name(inner);
                }
                self.first_plain_identifier(node)
            }
            _ => None,
        }
    }

    /// First plain name identifier in the subtree, not descending into parameter
    /// lists or initialisers (those names are parameters, not the function's).
    fn first_plain_identifier(&self, node: Node) -> Option<String> {
        if matches!(node.kind(), "identifier" | "field_identifier") {
            return self.text(node);
        }
        if matches!(
            node.kind(),
            "parameter_list"
                | "parameter_declaration"
                | "default_parameter"
                | "optional_parameter"
                | "variadic_parameter"
                | "initializer_list"
                | "field_initializer"
                | "argument_list"
                | "arguments"
        ) {
            return None;
        }
        for idx in 0..node.child_count() {
            if let Some(child) = node.child(idx)
                && let Some(name) = self.first_plain_identifier(child)
            {
                return Some(name);
            }
        }
        None
    }

    fn def_of_ruby(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "method" => self.named(node, "name", "method"),
            "singleton_method" => self.named(node, "name", "method"),
            "class" => self.named(node, "name", "class"),
            "module" => self.named(node, "name", "module"),
            _ => None,
        }
    }

    fn def_of_php(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_definition" => self.named(node, "name", "fn"),
            "method_declaration" => self.named(node, "name", "method"),
            "class_declaration" => self.named(node, "name", "class"),
            "interface_declaration" => self.named(node, "name", "interface"),
            "trait_declaration" => self.named(node, "name", "trait"),
            "enum_declaration" => self.named(node, "name", "enum"),
            _ => None,
        }
    }

    fn def_of_csharp(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "class_declaration"
            | "struct_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "enum_declaration" => self.named(node, "name", "class"),
            "method_declaration" => self.named(node, "name", "method"),
            "constructor_declaration" => self.named(node, "name", "constructor"),
            _ => None,
        }
    }

    fn def_of_zig(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_declaration" => self.named(node, "name", "fn"),
            "variable_declaration" => {
                let kind = self
                    .first_descendant_kind(
                        node,
                        &[
                            "struct_declaration",
                            "enum_declaration",
                            "union_declaration",
                        ],
                    )
                    .map(|n| match n.kind() {
                        "enum_declaration" => "enum",
                        "union_declaration" => "union",
                        _ => "struct",
                    })?;
                let name = self.first_identifier_text(node)?;
                Some(Symbol {
                    kind: kind.to_string(),
                    qualified: name.clone(),
                    name,
                })
            }
            _ => None,
        }
    }

    /// First descendant whose kind is in `kinds`, document order.
    fn first_descendant_kind<'a>(&self, node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
        if kinds.contains(&node.kind()) {
            return Some(node);
        }
        for idx in 0..node.child_count() {
            if let Some(child) = node.child(idx)
                && let Some(found) = self.first_descendant_kind(child, kinds)
            {
                return Some(found);
            }
        }
        None
    }

    /// Text of the first identifier/field_identifier in the subtree (document order).
    fn first_identifier_text(&self, node: Node) -> Option<String> {
        if matches!(node.kind(), "identifier" | "field_identifier") {
            return self.text(node);
        }
        for idx in 0..node.child_count() {
            if let Some(child) = node.child(idx)
                && let Some(name) = self.first_identifier_text(child)
            {
                return Some(name);
            }
        }
        None
    }

    fn def_of_lua(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_declaration" => self.named(node, "name", "fn"),
            _ => None,
        }
    }

    /// Rust definition nodes. `impl` blocks carry no `name` field — the type
    /// being implemented (`impl Type`) is the closest thing to a name, and
    /// nesting methods under it gives outlines like `Sessions.set_effort`.
    fn def_of_rust(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_item" => self.named(node, "name", "fn"),
            "struct_item" => self.named(node, "name", "struct"),
            "enum_item" => self.named(node, "name", "enum"),
            "union_item" => self.named(node, "name", "union"),
            "trait_item" => self.named(node, "name", "trait"),
            "type_item" => self.named(node, "name", "type"),
            "const_item" | "static_item" => self.named(node, "name", "const"),
            "macro_definition" => self.named(node, "name", "macro"),
            "mod_item" => self.named(node, "name", "module"),
            "impl_item" => {
                let name = self.text(node.child_by_field_name("type")?)?;
                Some(Symbol {
                    kind: "impl".to_string(),
                    qualified: name.clone(),
                    name,
                })
            }
            _ => None,
        }
    }

    /// Java definition nodes.
    fn def_of_java(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => self.named(node, "name", "class"),
            "method_declaration" => self.named(node, "name", "method"),
            "constructor_declaration" => self.named(node, "name", "constructor"),
            "compact_constructor_declaration" => self.named(node, "name", "constructor"),
            _ => None,
        }
    }

    /// Definitions deeper than this are skipped from an outline — closures
    /// inside methods are noise for navigation. Their descendants are only
    /// deeper still, so skipping their subtrees is safe.
    const MAX_OUTLINE_DEPTH: usize = 2;

    /// Enumerate the file's definitions in document order (pre-order, so every
    /// definition precedes the definitions nested inside it), with nesting
    /// depth and 1-based line spans. Powers `read_file`'s structural view of a
    /// big file: show the outline, let the model pick `start:end` ranges.
    pub fn defs(&self) -> Vec<DefEntry> {
        let mut out = Vec::new();
        self.collect_defs(self.tree.root_node(), &mut Vec::new(), &mut out);
        out
    }

    fn collect_defs(&self, node: Node, chain: &mut Vec<Symbol>, out: &mut Vec<DefEntry>) {
        let is_def = self.def_of(node);
        if let Some(sym) = is_def {
            let depth = chain.len();
            if depth <= Self::MAX_OUTLINE_DEPTH {
                let mut parts: Vec<&str> = chain.iter().map(|s| s.name.as_str()).collect();
                parts.push(sym.name.as_str());
                out.push(DefEntry {
                    symbol: Symbol {
                        kind: sym.kind.clone(),
                        name: sym.name.clone(),
                        qualified: parts.join("."),
                    },
                    depth,
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                });
            }
            chain.push(sym);
            self.collect_children(node, chain, out);
            chain.pop();
        } else {
            self.collect_children(node, chain, out);
        }
    }

    fn collect_children(&self, node: Node, chain: &mut Vec<Symbol>, out: &mut Vec<DefEntry>) {
        for idx in 0..node.child_count() {
            if let Some(child) = node.child(idx) {
                self.collect_defs(child, chain, out);
            }
        }
    }

    /// Every distinct tree-sitter node kind in the file, sorted. Diagnostic
    /// tooling: when wiring a new language's `def_of`, list what a sample file
    /// actually parses into before guessing node names.
    pub fn node_kinds(&self) -> Vec<String> {
        let mut kinds = std::collections::BTreeSet::new();
        self.collect_kinds(self.tree.root_node(), &mut kinds);
        kinds.into_iter().collect()
    }

    fn collect_kinds(&self, node: Node, kinds: &mut std::collections::BTreeSet<String>) {
        kinds.insert(node.kind().to_string());
        for idx in 0..node.child_count() {
            if let Some(child) = node.child(idx) {
                self.collect_kinds(child, kinds);
            }
        }
    }

    fn def_of_ts(&self, node: Node) -> Option<Symbol> {
        match node.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                self.named(node, "name", "fn")
            }
            "method_definition" | "method_signature" => self.named(node, "name", "method"),
            "class_declaration" | "abstract_class_declaration" => self.named(node, "name", "class"),
            // A named binding whose value is a function/arrow IS the definition
            // — matched directly (not via the arrow child) so a hit on the
            // DECLARATION line (e.g. `const fireAndForget = () => …`) resolves
            // to the binding, not the enclosing method. Anonymous callables
            // (no such binding parent) fall through and we keep climbing.
            "variable_declarator" if self.value_is_fn(node, "value") => {
                self.named(node, "name", "fn")
            }
            "public_field_definition" if self.value_is_fn(node, "value") => {
                self.named(node, "name", "method")
            }
            // Object-literal `{ compute: () => {} }`: `pair`'s key field is
            // `key` (NOT `name`); may be ident, quoted string, or `[computed]`.
            "pair" if self.value_is_fn(node, "value") => {
                let name = self.key_name(node.child_by_field_name("key")?)?;
                Some(Symbol {
                    kind: "method".to_string(),
                    qualified: name.clone(),
                    name,
                })
            }
            "assignment_expression" if self.value_is_fn(node, "right") => {
                let name = self.text(node.child_by_field_name("left")?)?;
                Some(Symbol {
                    kind: "fn".to_string(),
                    qualified: name.clone(),
                    name,
                })
            }
            _ => None,
        }
    }

    /// True if `node`'s `field` child is a function/arrow value.
    fn value_is_fn(&self, node: Node, field: &str) -> bool {
        node.child_by_field_name(field)
            .map(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function" | "function_expression"
                )
            })
            .unwrap_or(false)
    }
}

/// Strip a matching pair of surrounding quotes (`"`, `'`, or backtick) from a
/// string-literal key. Quotes are ASCII (single byte), so byte slicing stays on
/// char boundaries.
fn strip_quotes(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (first, last) = (b[0], b[b.len() - 1]);
        if last == first && (first == b'"' || first == b'\'' || first == b'`') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}
