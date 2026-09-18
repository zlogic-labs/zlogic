//! The tool registry.
//! # The allowlist applies at **materialisation**, not at call time
//! A sub-agent's tool restriction is not a check performed when a tool is invoked: at
//! [`ToolRegistry::materialize`] time the out-of-set tools are filtered out, so they never
//! appear in that agent's `LlmRequest.tools` at all. The model cannot see them, so it cannot
//! call them.
//! "Visible but refused" is worse: the model retries, wasting rounds and filling the context
//! with rejections.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::{
    Result, Tool, ToolCtx, ToolExecResult, ToolExposure, ToolMeta, ToolPrompt, ToolRisk, parse_args,
};

/// A source of tools. Registering one is how the tool set is extended — builtin, skill, MCP,
/// sub-agent, a2a all implement this.
/// `discover` is **synchronous**: loading definitions must not make network round trips. MCP
/// connects lazily during materialisation, which is where degradation is handled.
pub trait ToolSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn discover(&self) -> Vec<Arc<dyn Tool>>;
}

/// The built-in tools, gathered from every category module.
pub struct BuiltinSource;

impl ToolSource for BuiltinSource {
    fn name(&self) -> &'static str {
        "builtin"
    }
    fn discover(&self) -> Vec<Arc<dyn Tool>> {
        // One line per domain module. A new domain is added here and nowhere else — the registry
        // never needs to know a tool's name.
        let mut v = crate::file::all();
        v.extend(crate::compute::all());
        v.extend(crate::archive::all());
        v.extend(crate::search::all());
        v.extend(crate::exec::all());
        v.extend(crate::net::all());
        v.extend(crate::session::all());
        v.extend(crate::agent::all());
        v.extend(crate::task::all());
        v
    }
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Built-in tools only.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.add_source(&BuiltinSource);
        r
    }

    /// A later registration **replaces** an earlier tool of the same name.
    /// Order is precedence, so a user's skill or MCP server can substitute for a built-in
    /// implementation. Erroring on a name collision only ever forces the user to rename — and
    /// what they rename is usually the name the model has already learned.
    pub fn add(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.meta().name.clone(), tool);
    }

    pub fn add_source(&mut self, source: &dyn ToolSource) {
        for t in source.discover() {
            self.add(t);
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Removes a tool from the set.
    /// Bootstrap uses this when a configured built-in cannot be materialised: leaving the generic
    /// default registered would make the model see and call a different backend than the user
    /// selected.
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Names of tools that can actually be materialised for this run.
    /// Management surfaces intentionally use [`Self::names`] so optional tools remain selectable
    /// before their runtime extension is installed. Model-facing context must use this projection:
    /// describing an unavailable tool in the system prompt makes the model call a schema it was
    /// never given.
    pub fn available_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|(_, tool)| tool.available())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Tool-owned prompt guidance for the effective visible set, in stable name order.
    /// `deferred` rides along because only the registry knows a tool's exposure, and it decides
    /// whether the contract belongs in the system prompt or in the `load_tool` result.
    pub fn prompt_guidance(&self, visible_names: &[String]) -> Vec<ToolPrompt> {
        let visible = visible_names.iter().collect::<BTreeSet<_>>();
        self.tools
            .iter()
            .filter(|(name, tool)| visible.contains(name) && tool.available())
            .filter_map(|(name, tool)| {
                tool.prompt_spec().map(|spec| ToolPrompt {
                    name: name.clone(),
                    spec,
                    deferred: tool.exposure() == ToolExposure::Deferred,
                })
            })
            .collect()
    }

    /// Whether any of these tools would normally have to be approved.
    /// Asked of the tools themselves rather than matched against a list of names: a hand-kept list
    /// silently falls behind every tool that is added, and what it gates is a **warning** (god mode
    /// suppressing every confirmation prompt), so falling behind means the warning goes missing
    /// exactly when a new mutating tool arrives.
    /// Keyed on [`ToolRisk`], **not** on [`Tool::affects_workspace`]. The two are deliberately
    /// different: a tool can declare `affects_workspace() == false` because it touches no file of
    /// the workspace, while being `ToolRisk::High` because what it does outside the workspace is
    /// exactly what a user wants to be asked about. Approval is the question here, so risk is the
    /// answer.
    /// An `mcp__` tool counts regardless of the risk it declares — the declaration comes from
    /// someone else's process, and a server calling its own writes read-only is not a claim to trust.
    pub fn any_effectful(&self, names: &[String]) -> bool {
        names.iter().any(|name| {
            name.starts_with("mcp__")
                || self
                    .tools
                    .get(name)
                    .is_some_and(|tool| tool.meta().risk != ToolRisk::Read)
        })
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn meta(&self, name: &str) -> Option<ToolMeta> {
        self.tools.get(name).map(|t| t.meta())
    }

    /// Materialises the tool set for this turn, applying the allowlist.
    /// `allow = None` means everything. A name in the allowlist that is not registered lands in
    /// `missing`; callers may inspect it, but no per-turn notice is raised — a persistently
    /// missing tool would otherwise be re-reported on every request.
    pub fn materialize(&self, allow: Option<&[String]>) -> Materialized {
        self.materialize_for(allow, true)
    }

    /// Materialises tools for either a root run or a sub-agent.
    pub fn materialize_for(&self, allow: Option<&[String]>, root: bool) -> Materialized {
        let (tools, missing) = match allow {
            None => (
                self.tools
                    .values()
                    .filter(|tool| tool.available() && (root || !tool.root_only()))
                    .cloned()
                    .collect(),
                Vec::new(),
            ),
            Some(allow) => {
                let mut tools = Vec::new();
                let mut missing = Vec::new();
                for name in allow {
                    match self.tools.get(name) {
                        Some(t) if t.available() && (root || !t.root_only()) => {
                            tools.push(t.clone())
                        }
                        Some(_) => {}
                        None => missing.push(name.clone()),
                    }
                }
                (tools, missing)
            }
        };
        Materialized::new(tools, missing)
    }
}

pub struct Materialized {
    /// Every allowlisted tool, including deferred ones. Catalog and diagnostics use this list;
    /// [`definitions`](Self::definitions) and [`get`](Self::get) apply visibility.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Named in the allowlist but absent from the registry.
    pub missing: Vec<String>,
    deferred: BTreeMap<String, DeferredEntry>,
    loaded: Arc<Mutex<BTreeSet<String>>>,
    loader: Option<Arc<dyn Tool>>,
}

impl Materialized {
    fn new(tools: Vec<Arc<dyn Tool>>, missing: Vec<String>) -> Self {
        let deferred = tools
            .iter()
            .filter(|tool| tool.exposure() == ToolExposure::Deferred)
            .map(|tool| {
                (
                    tool.meta().name,
                    DeferredEntry {
                        summary: catalog_summary(&tool.definition().description),
                        contract: tool.prompt_spec().map(|spec| {
                            format!("- `{}`: {}", tool.meta().name, indent_contract(&spec))
                        }),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let loaded = Arc::new(Mutex::new(BTreeSet::new()));
        let loader = (!deferred.is_empty()).then(|| {
            Arc::new(LoadTool {
                deferred: deferred.clone(),
                loaded: loaded.clone(),
            }) as Arc<dyn Tool>
        });
        Self {
            tools,
            missing,
            deferred,
            loaded,
            loader,
        }
    }

    /// Seeds the loaded set from session state.
    /// `load_tool` writes the names it made available as session entries; the next turn reads them
    /// back here so the tools stay loaded instead of being re-requested. Names the registry does
    /// not recognise as deferred (a tool removed between turns, an allowlist change) are ignored —
    /// they can simply be loaded again if the model wants them.
    pub fn with_loaded(self, names: impl IntoIterator<Item = String>) -> Self {
        {
            let mut loaded = self
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for name in names {
                if self.deferred.contains_key(&name) {
                    loaded.insert(name);
                }
            }
        }
        self
    }

    /// Projects to `LlmRequest.tools`.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let loaded = self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut definitions = self
            .tools
            .iter()
            .filter(|tool| {
                tool.exposure() == ToolExposure::Eager || loaded.contains(&tool.meta().name)
            })
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        if let Some(loader) = &self.loader {
            definitions.push(loader.definition());
        }
        definitions
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        if name == "load_tool" {
            return self.loader.as_ref();
        }
        let tool = self.tools.iter().find(|tool| tool.meta().name == name)?;
        if tool.exposure() == ToolExposure::Eager
            || self
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(name)
        {
            Some(tool)
        } else {
            None
        }
    }

    pub fn visible_names(&self) -> Vec<String> {
        self.definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    }

    /// A small system-prompt catalogue. Full descriptions and parameter schemas stay out until
    /// `load_tool` is called.
    /// Already-loaded tools are omitted: their definition is in the request's `tools` already, and
    /// a catalogue line telling the model to load something that is sitting right in front of it
    /// would invite a redundant round-trip.
    pub fn deferred_prompt(&self) -> Option<String> {
        let loaded = self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lines = self
            .deferred
            .iter()
            .filter(|(name, _)| !loaded.contains(*name))
            .map(|(name, entry)| format!("- `{name}`: {}", entry.summary))
            .collect::<Vec<_>>();
        (!lines.is_empty()).then(|| {
            format!(
                "Deferred tools (load their full definitions with `load_tool` only when needed in this turn):\n{}",
                lines.join("\n")
            )
        })
    }
}

/// How long a catalogue entry may be before it is cut.
const CATALOG_CHARS: usize = 200;

/// A deferred tool's one-line catalogue entry.
/// The entry's whole job is to let the model decide whether to `load_tool`, which makes a cut in the
/// middle of the deciding sentence the worst possible outcome — and that is what a bare
/// `take(200)` produced for every description longer than the limit. So the cut lands on the last
/// sentence boundary that fits, and only falls back to a hard cut when there is no sentence end.
fn catalog_summary(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= CATALOG_CHARS {
        return first.to_string();
    }
    let head: String = first.chars().take(CATALOG_CHARS).collect();
    match head.rfind(". ") {
        // Keep the period: a sentence that ends is the point.
        Some(end) => head[..=end].trim_end().to_string(),
        None => format!("{}…", head.trim_end()),
    }
}

/// The spec's contract half, indented for the `load_tool` result.
fn indent_contract(spec: &crate::ToolPromptSpec) -> String {
    spec.contract_lines()
        .into_iter()
        .enumerate()
        .map(|(index, line)| match index {
            0 => line,
            _ => format!("\n  {line}"),
        })
        .collect()
}

/// A deferred tool as the catalogue and `load_tool` see it.
#[derive(Clone)]
struct DeferredEntry {
    /// The line shown in the system prompt.
    summary: String,
    /// The tool-owned input contract, handed over when the tool is loaded — see [`ToolPromptSpec`].
    contract: Option<String>,
}

struct LoadTool {
    deferred: BTreeMap<String, DeferredEntry>,
    loaded: Arc<Mutex<BTreeSet<String>>>,
}

#[derive(Deserialize)]
struct LoadArgs {
    names: Vec<String>,
}

#[async_trait]
impl Tool for LoadTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "load_tool".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "load_tool".into(),
            description: "Load full definitions for deferred tools named in the system prompt. A loaded definition becomes available immediately and stays loaded for the rest of the session — you do not need to load it again in a later turn.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "uniqueItems": true
                    }
                },
                "required": ["names"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: LoadArgs = parse_args(args)?;
        if args.names.is_empty() {
            return Ok(ToolExecResult::failed(
                "`names` must contain at least one deferred tool",
            ));
        }
        let unknown = args
            .names
            .iter()
            .filter(|name| !self.deferred.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Ok(ToolExecResult::failed(format!(
                "cannot load unknown or eager tool(s): {}. Deferred tools: {}",
                unknown.join(", "),
                self.deferred.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let newly_loaded = args
            .names
            .into_iter()
            .filter(|name| loaded.insert(name.clone()))
            .collect::<Vec<_>>();
        if newly_loaded.is_empty() {
            return Ok(ToolExecResult::success(
                "All requested tool definitions were already loaded.",
            ));
        }
        // The contract arrives with the schema, not before it. A deferred tool's system-prompt entry
        // only says when to reach for it; everything about calling it correctly is delivered here,
        // which is the first moment it can be called at all.
        let contracts = newly_loaded
            .iter()
            .filter_map(|name| self.deferred.get(name)?.contract.as_deref())
            .collect::<Vec<_>>();
        let usage = match contracts.is_empty() {
            true => String::new(),
            false => format!("\n\nHow to call them:\n{}", contracts.join("\n")),
        };
        Ok(ToolExecResult::success(format!(
            "Loaded tool definitions: {}. They are available starting with the next model round and stay loaded for the rest of the session.{usage}",
            newly_loaded.join(", ")
        ))
        .with_loaded_tools(newly_loaded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolExecStatus;
    use crate::{Result, ToolCtx, ToolExecResult, ToolRisk};
    use async_trait::async_trait;
    use serde_json::json;

    struct Dummy(&'static str);

    #[async_trait]
    impl Tool for Dummy {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: self.0.into(),
                source: "test",
                risk: ToolRisk::Read,
            }
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: "d".into(),
                parameters: json!({ "type": "object" }),
            }
        }
        async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> Result<ToolExecResult> {
            Ok(ToolExecResult::success(self.0))
        }
    }

    struct DeferredDummy(&'static str);

    #[async_trait]
    impl Tool for DeferredDummy {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: self.0.into(),
                source: "test",
                risk: ToolRisk::Read,
            }
        }
        fn exposure(&self) -> ToolExposure {
            ToolExposure::Deferred
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: format!("Use {} for an expensive specialist operation.", self.0),
                parameters: json!({
                    "type": "object",
                    "properties": { "detail": { "type": "string" } }
                }),
            }
        }
        async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> Result<ToolExecResult> {
            Ok(ToolExecResult::success(self.0))
        }
    }

    struct UnavailableDummy;

    #[async_trait]
    impl Tool for UnavailableDummy {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "optional".into(),
                source: "test",
                risk: ToolRisk::Read,
            }
        }
        fn available(&self) -> bool {
            false
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "optional".into(),
                description: "optional".into(),
                parameters: json!({ "type": "object", "properties": {} }),
            }
        }
        async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> Result<ToolExecResult> {
            unreachable!("an unavailable tool must not be executable")
        }
    }

    struct RootDummy;

    #[async_trait]
    impl Tool for RootDummy {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "root_only".into(),
                source: "test",
                risk: ToolRisk::Read,
            }
        }
        fn root_only(&self) -> bool {
            true
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "root_only".into(),
                description: "root".into(),
                parameters: json!({ "type": "object" }),
            }
        }
        async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> Result<ToolExecResult> {
            Ok(ToolExecResult::success("root"))
        }
    }

    /// A catalogue entry is what the model decides "should I load this?" from, so it must not stop
    /// mid-sentence.
    #[test]
    fn a_long_catalogue_entry_is_cut_at_a_sentence_not_mid_word() {
        let long = format!(
            "Perform a structured audio/video operation. {} Supports everything else too.",
            "x".repeat(CATALOG_CHARS)
        );
        let summary = catalog_summary(&long);
        assert_eq!(summary, "Perform a structured audio/video operation.");

        // Nothing to cut.
        assert_eq!(catalog_summary("Short and done."), "Short and done.");
        // Only the first line is a summary; the rest of a description is detail.
        assert_eq!(catalog_summary("First line.\nSecond line."), "First line.");
        // No sentence end to fall back on: an ellipsis at least admits the cut.
        let unbroken = "y".repeat(CATALOG_CHARS + 20);
        assert!(catalog_summary(&unbroken).ends_with('…'));
        assert_eq!(
            catalog_summary(&unbroken).chars().count(),
            CATALOG_CHARS + 1
        );
    }

    /// Every field name a schema declares, including the ones only a `oneOf` branch declares.
    fn schema_keys(schema: &serde_json::Value) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
            keys.extend(properties.keys().cloned());
        }
        for branch in ["oneOf", "anyOf", "allOf"] {
            if let Some(list) = schema.get(branch).and_then(|b| b.as_array()) {
                for entry in list {
                    keys.extend(schema_keys(entry));
                }
            }
        }
        keys
    }

    /// Every example a tool shows the model has to be valid **for that tool's own schema**.
    /// A canonical example is the most literally copied text in the whole prompt: a malformed one
    /// teaches the exact mistake it was written to prevent, and an invented field name teaches a
    /// call that will be rejected. Neither is noticeable by reading — the examples live inside raw
    /// strings where a stray quote looks fine and a plausible-but-wrong key looks fine too.
    /// Field names are checked, not types or requiredness: that is where a hand-written example
    /// actually goes wrong, and checking the rest would mean carrying a JSON Schema validator.
    /// Negative examples are exempt from the field check — some of them are wrong *because* they use
    /// a field that does not exist, which is the whole lesson.
    #[test]
    fn every_prompt_example_matches_its_tools_schema() {
        let registry = ToolRegistry::with_builtins();
        let mut checked = 0;
        for name in registry.names() {
            let Some(tool) = registry.get(&name) else {
                continue;
            };
            let Some(spec) = tool.prompt_spec() else {
                continue;
            };
            assert!(
                !spec.when.is_empty() && !spec.contract.is_empty(),
                "`{name}` has an empty half"
            );
            let allowed = schema_keys(&tool.definition().parameters);
            for example in spec.positive_examples {
                let value: serde_json::Value =
                    serde_json::from_str(example).unwrap_or_else(|error| {
                        panic!("`{name}` canonical example is not JSON: {error}\n{example}")
                    });
                let object = value.as_object().unwrap_or_else(|| {
                    panic!("`{name}` canonical example is not an object: {example}")
                });
                for key in object.keys() {
                    assert!(
                        allowed.contains(key),
                        "`{name}` canonical example uses `{key}`, which its schema does not \
                         declare: {example}"
                    );
                }
                checked += 1;
            }
            for example in spec.negative_examples {
                serde_json::from_str::<serde_json::Value>(example.args).unwrap_or_else(|error| {
                    panic!(
                        "`{name}` avoid example is not JSON: {error}\n{}",
                        example.args
                    )
                });
                // The renderer supplies the dash; a reason that brings its own reads as `— — why`.
                assert!(
                    !example.why.trim_start().starts_with(['-', '—']),
                    "`{name}`'s reason should not start with a dash: {}",
                    example.why
                );
                assert!(
                    !example.why.is_empty(),
                    "`{name}` has a reason-less example"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no examples were checked at all");
    }

    /// Every built-in deferred tool has to survive that rule with a readable entry.
    /// This is the test that would have caught five mangled entries at once; it is cheap because the
    /// registry can answer it without a model or a turn.
    #[test]
    fn every_deferred_builtin_has_a_complete_catalogue_entry() {
        let materialized = ToolRegistry::with_builtins().materialize(None);
        for (name, entry) in &materialized.deferred {
            assert!(
                entry.summary.ends_with('.'),
                "`{name}`'s catalogue entry does not end at a sentence: {:?}",
                entry.summary
            );
            assert!(
                entry.summary.chars().count() <= CATALOG_CHARS,
                "`{name}`'s catalogue entry is {} chars",
                entry.summary.chars().count()
            );
        }
    }

    /// The contract arrives with the schema — that is the whole point of splitting the spec.
    #[tokio::test]
    async fn loading_a_tool_hands_over_its_input_contract() {
        struct Guided;

        #[async_trait]
        impl Tool for Guided {
            fn meta(&self) -> ToolMeta {
                ToolMeta {
                    name: "guided".into(),
                    source: "test",
                    risk: ToolRisk::Read,
                }
            }
            fn exposure(&self) -> ToolExposure {
                ToolExposure::Deferred
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "guided".into(),
                    description: "Do the guided thing.".into(),
                    parameters: json!({ "type": "object" }),
                }
            }
            fn prompt_spec(&self) -> Option<crate::ToolPromptSpec> {
                Some(crate::ToolPromptSpec {
                    when: "Reach for it when guided.",
                    contract: "Inspect before you query.",
                    positive_examples: &[r#"{"operation":"inspect"}"#],
                    negative_examples: &[crate::PromptExample {
                        args: r#"{"operation":"query"}"#,
                        why: "inspect first",
                    }],
                })
            }
            async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> Result<ToolExecResult> {
                Ok(ToolExecResult::success("guided"))
            }
        }

        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Guided));
        registry.add(Arc::new(DeferredDummy("plain")));
        let materialized = registry.materialize(None);
        let loader = materialized.get("load_tool").unwrap().clone();
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::test_ctx(dir.path());

        let out = loader
            .execute(&ctx, r#"{"names":["guided"]}"#)
            .await
            .unwrap();
        let text = out.model_text();
        assert!(text.contains("Inspect before you query."), "{text}");
        assert!(
            text.contains("- avoid: `{\"operation\":\"query\"}` — inspect first"),
            "{text}"
        );
        // The "when" line already lives in the system prompt; repeating it here is waste.
        assert!(!text.contains("Reach for it when guided."), "{text}");

        // A tool without a spec gets no usage section rather than an empty heading.
        let out = loader
            .execute(&ctx, r#"{"names":["plain"]}"#)
            .await
            .unwrap();
        assert!(!out.model_text().contains("How to call them"));
    }

    #[test]
    fn root_only_tools_are_absent_from_sub_agent_definitions() {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Dummy("ordinary")));
        registry.add(Arc::new(RootDummy));

        assert!(
            registry
                .materialize_for(None, true)
                .get("root_only")
                .is_some()
        );
        let child = registry.materialize_for(None, false);
        assert!(child.get("root_only").is_none());
        assert!(child.get("ordinary").is_some());
    }

    /// Every domain module reaches the registry. A tool that is written but never discovered is
    /// invisible to the model, and nothing else would notice.
    #[test]
    fn builtins_are_registered() {
        let names = ToolRegistry::with_builtins().names();
        for expected in [
            // file
            "read_file",
            "write_file",
            "edit",
            // search
            "list_dir",
            "glob",
            "grep",
            // compute / exec / net
            "time",
            "shell",
            "web_fetch",
            "web_search",
            // session / agent
            "ask_user",
            "skill",
            "enter_worktree",
            "exit_worktree",
            "create_agent",
            "task_get",
            "task_stop",
            "task_message",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} is not registered"
            );
        }
    }

    #[test]
    fn metadata_is_reachable_for_the_approval_pipeline() {
        let r = ToolRegistry::with_builtins();
        for name in [
            "read_file",
            "list_dir",
            "glob",
            "grep",
            "web_fetch",
            "web_search",
            "ask_user",
        ] {
            assert_eq!(
                r.meta(name).unwrap().risk,
                ToolRisk::Read,
                "{name} changes nothing"
            );
        }
        for name in ["write_file", "edit", "task_stop", "skill"] {
            assert_eq!(r.meta(name).unwrap().risk, ToolRisk::Write, "{name}");
        }
        // Irreversible, or reaching further than any single write can be reasoned about:
        // `shell` runs arbitrary code, a sub-agent can call no less than the main agent, and the
        // worktree pair creates a branch and moves where every later call runs.
        for name in ["shell", "create_agent", "enter_worktree", "exit_worktree"] {
            assert_eq!(r.meta(name).unwrap().risk, ToolRisk::High, "{name}");
        }
    }

    /// A definition the provider rejects is a broken turn, not a broken tool call, so the shape is
    /// checked for every built-in rather than per tool.
    #[test]
    fn every_definition_is_a_well_formed_object_schema() {
        let r = ToolRegistry::with_builtins();
        let mut definitions = r
            .names()
            .iter()
            .map(|name| r.get(name).unwrap().definition())
            .collect::<Vec<_>>();
        let materialized = r.materialize(None);
        definitions.push(materialized.get("load_tool").unwrap().definition());
        for def in definitions {
            assert!(
                !def.description.trim().is_empty(),
                "{} has no description",
                def.name
            );
            assert_eq!(def.parameters["type"], "object", "{}", def.name);
            assert!(def.parameters["properties"].is_object(), "{}", def.name);
            // Every required name must exist in `properties`; a stale one after a rename is a
            // schema the provider rejects outright.
            if let Some(required) = def.parameters["required"].as_array() {
                for key in required {
                    let key = key.as_str().unwrap();
                    assert!(
                        def.parameters["properties"].get(key).is_some(),
                        "{} requires `{key}`, which it does not declare",
                        def.name
                    );
                }
            }
        }
    }

    /// The name in the metadata and the name in the definition are read by different consumers —
    /// the approval pipeline and the provider — and a mismatch makes a tool uncallable.
    #[test]
    fn metadata_and_definition_agree_on_the_name() {
        let r = ToolRegistry::with_builtins();
        for name in r.names() {
            let tool = r.get(&name).unwrap();
            assert_eq!(tool.meta().name, name);
            assert_eq!(tool.definition().name, name);
        }
    }

    /// A later registration replaces the same name, so skills and MCP can substitute.
    #[test]
    fn later_registration_overrides_the_same_name() {
        let mut r = ToolRegistry::with_builtins();
        let before = r.len();
        r.add(Arc::new(Dummy("read_file")));
        assert_eq!(r.len(), before, "replacing must not add an entry");
        assert_eq!(r.meta("read_file").unwrap().source, "test");
    }

    /// The allowlist applies at materialisation: out-of-set tools never reach the model.
    #[test]
    fn whitelist_filters_at_materialisation_not_at_call_time() {
        let r = ToolRegistry::with_builtins();
        let m = r.materialize(Some(&["read_file".to_string()]));

        let defs = m.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "read_file");
        assert!(
            m.get("write_file").is_none(),
            "invisible to the model, hence uncallable"
        );
        assert!(m.missing.is_empty());
    }

    #[test]
    fn registered_but_unavailable_tools_are_silently_omitted() {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(UnavailableDummy));

        assert_eq!(registry.names(), ["optional"]);
        assert!(
            registry.available_names().is_empty(),
            "model-facing capability lists must not advertise unavailable tools"
        );
        let materialized = registry.materialize(Some(&["optional".into()]));
        assert!(materialized.tools.is_empty());
        assert!(
            materialized.missing.is_empty(),
            "the tool exists; its managed dependency is merely disabled"
        );
    }

    #[test]
    fn no_whitelist_means_everything() {
        let r = ToolRegistry::with_builtins();
        let expected = r
            .names()
            .into_iter()
            .filter(|name| r.get(name).is_some_and(|tool| tool.available()))
            .count();
        assert_eq!(r.materialize(None).tools.len(), expected);
    }

    #[tokio::test]
    async fn deferred_tools_are_catalogued_then_loaded_for_the_next_round() {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Dummy("eager")));
        registry.add(Arc::new(DeferredDummy("specialist")));
        let materialized = registry.materialize(None);

        assert_eq!(
            materialized.visible_names(),
            ["eager", "load_tool"],
            "the full deferred schema must not be sent initially"
        );
        assert!(materialized.get("specialist").is_none());
        let prompt = materialized.deferred_prompt().unwrap();
        assert!(prompt.contains("`specialist`"));
        assert!(prompt.contains("expensive specialist operation"));
        assert!(
            !prompt.contains("properties"),
            "schemas stay out of the prompt"
        );

        let dir = tempfile::tempdir().unwrap();
        let result = materialized
            .get("load_tool")
            .unwrap()
            .execute(&crate::test_ctx(dir.path()), r#"{"names":["specialist"]}"#)
            .await
            .unwrap();
        assert!(!result.is_error(), "{}", result.model_text());
        assert_eq!(
            materialized.visible_names(),
            ["eager", "specialist", "load_tool"]
        );
        assert!(materialized.get("specialist").is_some());
    }

    #[test]
    fn no_allowed_deferred_tool_means_no_loader_or_catalogue() {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Dummy("eager")));
        registry.add(Arc::new(DeferredDummy("specialist")));
        let materialized = registry.materialize(Some(&["eager".into()]));
        assert_eq!(materialized.visible_names(), ["eager"]);
        assert!(materialized.deferred_prompt().is_none());
        assert!(materialized.get("load_tool").is_none());
    }

    /// The names a previous turn recorded come back through `with_loaded`; names the registry no
    /// longer knows are dropped, and the catalogue stops offering what is already loaded.
    #[test]
    fn with_loaded_rehydrates_session_state_and_filters_the_catalogue() {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Dummy("eager")));
        registry.add(Arc::new(DeferredDummy("specialist")));
        registry.add(Arc::new(DeferredDummy("other")));
        let materialized = registry
            .materialize(None)
            .with_loaded(vec!["specialist".into(), "vanished".into()]);

        assert!(materialized.get("specialist").is_some());
        assert!(materialized.get("other").is_none());
        let prompt = materialized.deferred_prompt().unwrap();
        assert!(
            !prompt.contains("`specialist`"),
            "loaded tool stays out of the catalogue"
        );
        assert!(prompt.contains("`other`"));
    }

    /// An allowlisted tool that does not exist must be reported, not silently dropped.
    #[test]
    fn missing_whitelisted_tools_are_reported() {
        let r = ToolRegistry::with_builtins();
        let m = r.materialize(Some(&["read_file".into(), "teleport".into()]));
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.missing, ["teleport"]);
    }

    #[test]
    fn names_are_stable_and_sorted() {
        let mut r = ToolRegistry::new();
        r.add(Arc::new(Dummy("zeta")));
        r.add(Arc::new(Dummy("alpha")));
        assert_eq!(r.names(), ["alpha", "zeta"]);
    }

    #[tokio::test]
    async fn a_registered_tool_actually_runs() {
        let r = ToolRegistry::with_builtins();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "content").unwrap();
        let ctx = crate::test_ctx(dir.path());

        let tool = r.get("read_file").unwrap();
        let out = tool.execute(&ctx, r#"{"path":"f.txt"}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        // read_file now prepends a metadata block; the body must still carry the content.
        assert!(
            out.model_text().ends_with("content"),
            "{}",
            out.model_text()
        );
    }
}
