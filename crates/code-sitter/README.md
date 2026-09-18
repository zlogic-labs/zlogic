# code-sitter

Code-aware grep, in Rust. Search like ripgrep, then tag every hit with the
symbol that encloses it. Two stages:

1. **Search** (`search.rs`): ripgrep-style search via `grep-regex` +
   `grep-searcher` + `ignore::WalkBuilder` → raw hits with the match's byte
   column. Binary files (NUL byte) are skipped; hidden files and `.gitignore`
   follow ripgrep defaults.
2. **Annotate** (`symbols.rs`): parse **only the files that had hits** with
   tree-sitter and tag each hit with its **enclosing symbol** (qualified) and
   a **hit kind** — `def` / `call` / `comment` / `string`.

```
[call] src/checkout/total.ts:15: (in method Checkout.computeTotal) subtotal = applyCoupon(subtotal, c);
[def]  src/checkout/discount.ts:3: (in fn applyCoupon) export function applyCoupon(subtotal, coupon) {
```

## Design

- **Stateless**: no persistent index, no staleness, no incremental bookkeeping, no per-language
  resolver. Parsing happens on demand over the handful of hit files (capped at 1MB / non-minified),
  then is thrown away.
- **Syntax only**: enclosing `function`/`class` and hit-kind are pure syntax — no type inference —
  so it is as correct in Python/JS as in TS.
- **Graceful degrade**: files whose language we don't handle (or that fail to parse / are huge /
  minified) simply come back without annotation → plain grep output, unchanged.

## Build & run

**Requires network** on first build (fetches `grep-*`, `ignore`, `tree-sitter`
grammars from crates.io). If the build complains about tree-sitter core vs
grammar versions, `cargo update` or nudge the versions in `Cargo.toml`.

```bash
cd crates/code-sitter
cargo build --release
cargo test

# search + annotate (enclosing symbol, hit kind)
./target/release/code-sitter 'applyCoupon' .

# glob filter, case-sensitive, capped, with context
./target/release/code-sitter 'TODO' src --include '*.ts' --case-sensitive --max 50 --context 2

# just the matching file paths / plain grep (no tree-sitter pass)
./target/release/code-sitter 'applyCoupon' . --files-only
./target/release/code-sitter 'applyCoupon' . --no-annotate
```

Flags: `--include GLOB` `--case-sensitive` `--max N` `--context N`
`--files-only` `--hidden` `--no-annotate` `--include-ignored`.

## Library API

```rust
use code_sitter::{search, Options};

let res = search(std::path::Path::new("."), "applyCoupon", &Options::default())?;
// res.hits: Vec<Hit { path, line, col, text, symbol, hit_kind, context_before/after }>
//   symbol:   Option<Symbol { kind, name, qualified }>   e.g. "Checkout.computeTotal"
//   hit_kind: Option<HitKind::{Def,Call,Comment,Str,Code}>
// res.truncated / res.skipped_files: honest bookkeeping (surface to the agent)
```

Extras:
- `Options.cancel: Option<Arc<AtomicBool>>` — cooperative cancellation (set the
  flag; the search returns `SearchError::Cancelled`). Map an abort signal to it.
- `Options.context_lines`, `files_only`, `include_hidden` — grep parity knobs.

### Annotating an EXISTING grep's hits

Let your grep search; hand hits to `annotate` for symbol + kind:

```rust
// (path, 1-based line, 0-based byte column — pass rg --column minus 1; 0 if unknown)
let anns = code_sitter::annotate(&[(path, 15, 24)]);
```

CLI equivalent (one process per batch): `code-sitter --annotate`, stdin
`path\tline[\tcol]` per hit → stdout `symkind\tqualified\thitkind` per line
(all-empty line = no annotation).

## Adding a language

Tiny, three touch-points — no `.tsg`, no resolver:

1. `lang.rs` — map extensions → `Lang` variant, return its tree-sitter grammar.
2. `symbols.rs` — list that language's definition node kinds in `def_of` (and,
   if its call/string/comment node kinds differ, extend `classify`).
3. add the grammar crate to `Cargo.toml`.

## Scope / non-goals

Single-hop, syntax-only annotation. **No** cross-file resolution, call graph,
or find-references — by design.
