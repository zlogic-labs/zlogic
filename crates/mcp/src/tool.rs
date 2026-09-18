//! One MCP tool, as an [`zlogic_tools::Tool`].
//! # What is stored, and what is looked up
//! An instance holds the server *definition* and the tool's *schema* — both static data — and the
//! pool. It does not hold a connection: `execute` resolves the definition against the workspace it
//! is being called in and asks the pool, which is what lets a connection be shared, reclaimed or
//! rebuilt without the tool catalogue changing. See [`crate::pool`].
//! # Failure is a result, not an error
//! `Err` from `execute` means "this call could not be attempted at all" and zlogic turns it into a
//! precondition failure. Everything an MCP server can do to us is short of that: an unset credential,
//! a command that is not installed, a server that hangs, a tool that reports bad arguments. Each of
//! those comes back as a *result* the model can read and act on, because the alternative — an error
//! that reads as "zlogic is broken" — leaves the model with nothing to try.
//! # Risk comes from the server's annotations, and the default is cautious
//! MCP tools may declare `readOnlyHint` / `destructiveHint`. A tool that says it only reads is
//! [`ToolRisk::Read`]; one that says it destroys is [`ToolRisk::High`]; anything that says nothing is
//! [`ToolRisk::Write`], which under zlogic's default policy means the user is asked. Believing an
//! absent annotation to mean "harmless" would auto-approve arbitrary code in someone else's process.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::{JsonObject, Tool as RmcpTool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zlogic_protocol::llm::ToolDefinition;
use zlogic_tools::{Tool, ToolCtx, ToolError, ToolExecResult, ToolMeta, ToolRisk};

use crate::conn::Label;
use crate::def::{ServerDef, tool_name};
use crate::pool::McpPool;
use crate::resolve::Resolver;
use crate::security::{has_unsafe_text, sanitize_json_strings, sanitize_untrusted_text};
use crate::{McpError, content};

/// What a server said about one of its tools.
/// Serialisable because this is what the tool-list cache holds: it is everything needed to offer the
/// tool to a model without connecting to the server first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The server's own name for it, sent back verbatim in `tools/call`. Not the namespaced name the
    /// model sees.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A JSON Schema object. Normalised on the way in — see [`ToolSpec::from_rmcp`].
    pub input_schema: Value,
    #[serde(default)]
    pub hints: Hints,
}

/// The tool annotations that change how zlogic treats a call. Presentational ones (`title`, icons) are
/// deliberately not carried: the cache would then have to be invalidated for cosmetic changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

/// How much of a description reaches the prompt.
/// **Lowering this (or [`MAX_SCHEMA_CHARS`]) requires bumping `catalog::TOOL_CACHE_VERSION`.** The
/// cache stores specs *after* truncation, so an already-cached list keeps the old, longer text until
/// its TTL expires — a change that appears to do nothing for a day.
/// Not about cost: OpenAPI-derived servers have been seen dumping tens of KB of endpoint docs into
/// `description`, and those tens of KB are **paid on every round** — what they squeeze out is the
/// conversation itself. Pointing at where the rest is would be useless (the model has no access to that
/// document), so the cut is only marked as a cut.
pub const MAX_DESCRIPTION_CHARS: usize = 2048;

/// How large one tool's parameter schema may be, serialised.
pub const MAX_SCHEMA_CHARS: usize = 8 * 1024;

/// The cap on one parameter's own description. **Trimmed first** — it is usually the reason a schema
/// is large, and trimming it makes no parameter disappear.
const MAX_PROPERTY_DESCRIPTION_CHARS: usize = 512;

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}… [truncated]")
}

impl ToolSpec {
    pub fn from_rmcp(tool: &RmcpTool) -> Self {
        let annotations = tool.annotations.as_ref();
        // Truncation happens **here**, before the cache: the cache file, the prompt and the UI all see
        // the same thing, and nobody ever holds a 60KB description. The cost is that changing these
        // constants needs the cache to expire — it has a TTL anyway.
        let (input_schema, dropped) = trim_schema(&tool.input_schema);
        let description = match tool
            .description
            .as_ref()
            .map(|d| truncate(sanitize_untrusted_text(d).trim(), MAX_DESCRIPTION_CHARS))
        {
            // Dropped parameters have to be stated. A model that cannot see them but guesses from the
            // shape of the schema fails the same call repeatedly; told plainly that they are not
            // available here, it works around them.
            Some(d) if !dropped.is_empty() => Some(format!(
                "{d}\n\n(zlogic omitted {} parameter(s) from this tool's schema because it was too \
                 large to send: {}. Calls that need them will not work here.)",
                dropped.len(),
                summarize_names(&dropped)
            )),
            None if !dropped.is_empty() => Some(format!(
                "The server provided no description. Zlogic omitted {} parameter(s) from this \
                 tool's schema because it was too large to send: {}. Calls that need them will not \
                 work here.",
                dropped.len(),
                summarize_names(&dropped)
            )),
            other => other,
        };
        Self {
            name: tool.name.to_string(),
            description,
            input_schema,
            hints: Hints {
                read_only: annotations.and_then(|a| a.read_only_hint).unwrap_or(false),
                // The MCP default for an omitted `destructiveHint` is true, except that
                // `readOnlyHint: true` already promises no environment modification. An explicit
                // destructive=true still wins in `risk` when a server sends contradictory hints.
                destructive: annotations
                    .and_then(|a| a.destructive_hint)
                    .unwrap_or_else(|| {
                        annotations.is_some()
                            && !annotations.and_then(|a| a.read_only_hint).unwrap_or(false)
                    }),
                idempotent: annotations.and_then(|a| a.idempotent_hint).unwrap_or(false),
            },
        }
    }

    fn risk(&self) -> ToolRisk {
        // Contradictory annotations must fail safe. A buggy or hostile server saying both
        // `readOnlyHint: true` and `destructiveHint: true` does not get auto-approved as a read.
        if self.hints.destructive {
            ToolRisk::High
        } else if self.hints.read_only {
            ToolRisk::Read
        } else {
            ToolRisk::Write
        }
    }
}

fn summarize_names(names: &[String]) -> String {
    const LIMIT: usize = 512;
    let mut shown = String::new();
    let mut count = 0;
    for name in names {
        let separator = if shown.is_empty() { "" } else { ", " };
        if shown.chars().count() + separator.len() + name.chars().count() > LIMIT {
            break;
        }
        shown.push_str(separator);
        shown.push_str(name);
        count += 1;
    }
    if count < names.len() {
        format!("{shown}, … and {} more", names.len() - count)
    } else {
        shown
    }
}

/// Providers reject a tool whose parameter schema is not an object schema, and the rejection fails
/// the whole request — one sloppy server would make every tool in the turn unusable. So the shape is
/// forced here, keeping whatever else the server declared.
fn normalize_schema(schema: &JsonObject) -> Value {
    let mut out = schema.clone();
    let unsafe_properties: Vec<String> = out
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.keys())
        .filter(|name| has_unsafe_text(name))
        .cloned()
        .collect();
    if let Some(properties) = out.get_mut("properties").and_then(Value::as_object_mut) {
        for name in unsafe_properties {
            properties.shift_remove(&name);
        }
    }
    let mut schema_value = Value::Object(out);
    sanitize_json_strings(&mut schema_value);
    let Value::Object(mut out) = schema_value else {
        unreachable!("the schema was constructed as an object")
    };
    out.insert("type".into(), json!("object"));
    if !out.get("properties").is_some_and(|p| p.is_object()) {
        out.insert("properties".into(), json!({}));
    }
    let property_names: std::collections::BTreeSet<String> = out["properties"]
        .as_object()
        .into_iter()
        .flat_map(|properties| properties.keys().cloned())
        .collect();
    let mut seen_required = std::collections::BTreeSet::new();
    let required: Vec<Value> = out
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| property_names.contains(*name))
        .filter(|name| seen_required.insert((*name).to_string()))
        .map(|name| Value::String(name.to_string()))
        .collect();
    if required.is_empty() {
        out.remove("required");
    } else {
        out.insert("required".into(), Value::Array(required));
    }
    if out
        .get("additionalProperties")
        .is_some_and(|v| !v.is_boolean() && !v.is_object())
    {
        out.remove("additionalProperties");
    }
    Value::Object(out)
}

/// Trims an oversized schema into budget, and **says what was removed**.
/// # Why it cannot just be truncated like a description
/// Half of a JSON document is not JSON. So what is trimmed is the **structure**, cheapest first:
/// 1. every `description`, including descriptions inside arrays and nested objects, down to
///    [`MAX_PROPERTY_DESCRIPTION_CHARS`] — this makes no parameter disappear, and it is usually
///    enough on its own;
/// 2. still over: keep every **required** parameter name, reducing its schema to the type when the
///    required set alone would overflow;
/// 3. add optional parameters individually while they fit. The dropped names go back to the caller,
///    which puts them in the description.
fn trim_schema(schema: &JsonObject) -> (Value, Vec<String>) {
    let mut out = match normalize_schema(schema) {
        Value::Object(o) => o,
        other => return (other, Vec::new()),
    };
    if json_len(&out) <= MAX_SCHEMA_CHARS {
        return (Value::Object(out), Vec::new());
    }

    // Step one: OpenAPI-derived schemas commonly put most prose below `items`, `$defs` or nested
    // object properties. Trimming only the top-level parameter descriptions throws away useful
    // parameters while leaving the actual source of the size untouched.
    let mut value = Value::Object(out);
    trim_descriptions(&mut value);
    let Value::Object(trimmed) = value else {
        unreachable!("the schema was constructed as an object")
    };
    out = trimmed;
    if json_len(&out) <= MAX_SCHEMA_CHARS {
        return (Value::Object(out), Vec::new());
    }

    // Step two: drop optional parameters. Required ones go in first, the rest in declared order until
    // the budget runs out.
    let required: Vec<String> = out
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let Some(properties) = out.get("properties").and_then(|p| p.as_object()).cloned() else {
        return (Value::Object(out), Vec::new());
    };

    let mut kept = serde_json::Map::new();
    let mut dropped = Vec::new();
    for (name, value) in properties.iter().filter(|(n, _)| required.contains(n)) {
        kept.insert(name.clone(), value.clone());
    }
    if json_len_of_properties(&out, &kept) > MAX_SCHEMA_CHARS {
        // Names are the non-negotiable part: omitting one makes every generated call invalid.
        // Detailed enums/examples are useful but recoverable from a server validation error.
        for value in kept.values_mut() {
            *value = minimal_schema(value);
        }
        if json_len_of_properties(&out, &kept) > MAX_SCHEMA_CHARS {
            // The remaining weight is outside `properties` (typically generated `$defs`,
            // `examples` or OpenAPI metadata). None of it is useful if it makes the entire tool
            // list invalid, so fall back to the portable object core.
            let mut portable = serde_json::Map::new();
            portable.insert("type".into(), json!("object"));
            if !required.is_empty() {
                portable.insert("required".into(), json!(required));
            }
            portable.insert("properties".into(), json!({}));
            out = portable;
        }
    }
    for (name, value) in properties.iter().filter(|(n, _)| !required.contains(n)) {
        kept.insert(name.clone(), value.clone());
        if json_len_of_properties(&out, &kept) > MAX_SCHEMA_CHARS {
            kept.shift_remove(name);
            dropped.push(name.clone());
        }
    }

    out.insert("properties".into(), Value::Object(kept));
    (Value::Object(out), dropped)
}

fn minimal_schema(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return json!({});
    };
    let mut minimal = serde_json::Map::new();
    if let Some(kind) = object.get("type") {
        minimal.insert("type".into(), kind.clone());
    }
    Value::Object(minimal)
}

fn trim_descriptions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(text)) = object.get_mut("description") {
                *text = truncate(text, MAX_PROPERTY_DESCRIPTION_CHARS);
            }
            for child in object.values_mut() {
                trim_descriptions(child);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(trim_descriptions),
        _ => {}
    }
}

fn json_len(object: &serde_json::Map<String, Value>) -> usize {
    serde_json::to_string(object)
        .map(|s| s.chars().count())
        .unwrap_or(usize::MAX)
}

/// How large the whole schema would be with this set of properties.
fn json_len_of_properties(
    schema: &serde_json::Map<String, Value>,
    properties: &serde_json::Map<String, Value>,
) -> usize {
    let mut probe = schema.clone();
    probe.insert("properties".into(), Value::Object(properties.clone()));
    json_len(&probe)
}

pub struct McpTool {
    def: Arc<ServerDef>,
    spec: Arc<ToolSpec>,
    pool: Arc<McpPool>,
    /// `mcp__<server>__<tool>`, computed once.
    name: String,
}

impl McpTool {
    pub fn new(def: Arc<ServerDef>, spec: Arc<ToolSpec>, pool: Arc<McpPool>) -> Self {
        let name = tool_name(&def.id, &spec.name);
        Self {
            def,
            spec,
            pool,
            name,
        }
    }

    pub fn server_id(&self) -> &str {
        &self.def.id
    }

    pub fn spec(&self) -> &ToolSpec {
        &self.spec
    }
}

#[async_trait]
impl Tool for McpTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: self.name.clone(),
            source: "mcp",
            risk: self.spec.risk(),
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            // A description is what the model chooses by. A server that supplied none gets a
            // generated one rather than an empty string, which some providers reject outright.
            description: match self.spec.description.as_deref().map(str::trim) {
                Some(d) if !d.is_empty() => d.to_string(),
                _ => format!(
                    "The `{}` tool of the MCP server `{}`. The server provided no description.",
                    sanitize_untrusted_text(&self.spec.name),
                    sanitize_untrusted_text(&self.def.id)
                ),
            },
            parameters: self.spec.input_schema.clone(),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> zlogic_tools::Result<ToolExecResult> {
        // Arguments the model got wrong are the one genuine `Err` here: the call cannot be attempted,
        // and zlogic's own message about it is better than anything this crate could invent.
        let args: Option<JsonObject> = if args.trim().is_empty() {
            None
        } else {
            Some(serde_json::from_str(args).map_err(|e| ToolError::BadArgs(e.to_string()))?)
        };

        if ctx.is_cancelled() {
            return Ok(ToolExecResult::cancelled(format!(
                "`{}` was not called",
                self.name
            )));
        }

        // Resolved per call, not cached: a token stored a moment ago is picked up without anything
        // having to be invalidated, and the workspace is whichever one this call is happening in.
        let resolver = Resolver::system(&ctx.root);
        let mut resolved = match resolver.resolve(&self.def, Some(ctx.session_id)) {
            Ok(r) => r,
            Err(e) => return Ok(ToolExecResult::failed(self.credential_hint(&e))),
        };
        // A server can ask the client for input while a tool is running. MCP does not put the
        // originating zlogic session on that reverse request, so connections which advertise
        // elicitation must not be shared across sessions. Headless connections advertise no
        // elicitation and retain the ordinary parameter-based pooling behaviour.
        if ctx.interaction.is_some() {
            resolved.bind_session(ctx.session_id);
        }

        let label = Label {
            workspace_root: ctx.root.clone(),
            session: Some(ctx.session_id),
            turn: Some(ctx.turn_id),
            call: Some(ctx.call_id.clone()),
            interaction: ctx.interaction.clone(),
        };
        // Starting a server can take seconds. Saying so beats a card that sits blank.
        ctx.progress(&format!(
            "MCP {}: {}",
            sanitize_untrusted_text(&self.def.id),
            sanitize_untrusted_text(&self.spec.name)
        ));

        let connect = self.pool.session(&self.def, &resolved, label);
        let session = match tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => {
                return Ok(ToolExecResult::cancelled(format!(
                    "`{}` was not called because connecting to MCP server `{}` was interrupted",
                    self.name, self.def.id
                )));
            }
            result = connect => result,
        } {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolExecResult::failed(sanitize_untrusted_text(
                    &e.to_string(),
                )));
            }
        };

        let timeout = self.pool.config().call_timeout;
        let call = session.call_tool(&self.spec.name, args, timeout);
        tokio::select! {
            // Stopping is not an error and not a denial: the work may well have happened on the
            // server's side, and the model has to know it is unfinished rather than impossible.
            () = ctx.cancel.cancelled() => Ok(ToolExecResult::cancelled(format!(
                "`{}` was interrupted; the server may have carried on with it",
                self.name
            ))),
            result = call => Ok(match result {
                Ok(result) => content::to_result(&self.def.id, &self.spec.name, result, ctx),
                Err(e @ McpError::Timeout { .. }) => {
                    ToolExecResult::timeout(sanitize_untrusted_text(&e.to_string()))
                }
                Err(e) => ToolExecResult::failed(sanitize_untrusted_text(&e.to_string())),
            }),
        }
    }
}

impl McpTool {
    /// Turns "a template could not be resolved" into something the user can fix.
    fn credential_hint(&self, error: &McpError) -> String {
        let refs = self.def.referenced_credentials();
        if refs.is_empty() {
            return format!("MCP server `{}` is not configured: {error}", self.def.id);
        }
        format!(
            "MCP server `{}` is not configured: {error}. It needs: {}.",
            self.def.id,
            refs.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{Origin, parse_value};
    use crate::pool::PoolConfig;
    use crate::pool::testing::{FakeConnector, fake_pool};
    use crate::test_ctx;
    use rmcp::model::{CallToolResult, ContentBlock, ToolAnnotations};
    use std::borrow::Cow;
    use std::sync::Arc;
    use zlogic_tools::ToolExecStatus;

    fn def(id: &str, v: Value) -> Arc<ServerDef> {
        let mut p = parse_value(&v, id, Origin::Global, None);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let mut d = p.servers.remove(0);
        d.id = id.to_string();
        Arc::new(d)
    }

    fn spec(name: &str) -> Arc<ToolSpec> {
        Arc::new(ToolSpec {
            name: name.to_string(),
            description: Some("does a thing".into()),
            input_schema: json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
            hints: Hints::default(),
        })
    }

    fn rmcp_tool(name: &'static str, annotations: Option<ToolAnnotations>) -> RmcpTool {
        let mut schema = JsonObject::new();
        schema.insert("properties".into(), json!({ "a": { "type": "number" } }));
        let mut tool = RmcpTool::new(Cow::Borrowed(name), "from the server", Arc::new(schema));
        tool.annotations = annotations;
        tool
    }

    #[test]
    fn the_model_sees_a_namespaced_name_and_the_server_sees_its_own() {
        let tool = McpTool::new(
            def("github", json!({ "command": "x" })),
            spec("create_issue"),
            fake_pool(Arc::new(FakeConnector::default()), PoolConfig::default()),
        );
        assert_eq!(tool.meta().name, "mcp__github__create_issue");
        assert_eq!(tool.definition().name, "mcp__github__create_issue");
        assert_eq!(
            tool.spec().name,
            "create_issue",
            "the wire name is unchanged"
        );
        assert_eq!(
            tool.meta().source,
            "mcp",
            "the approval pipeline categorises by this"
        );
    }

    #[test]
    fn an_enormous_description_is_capped() {
        let mut tool = rmcp_tool("t", None);
        tool.description = Some(Cow::Owned("x".repeat(60_000)));
        let spec = ToolSpec::from_rmcp(&tool);

        let description = spec.description.unwrap();
        assert!(description.chars().count() < 60_000);
        assert!(description.chars().count() <= MAX_DESCRIPTION_CHARS + 32);
        assert!(
            description.ends_with("[truncated]"),
            "it must say it was cut"
        );
    }

    #[test]
    fn a_normal_description_is_untouched() {
        let spec = ToolSpec::from_rmcp(&rmcp_tool("t", None));
        assert_eq!(spec.description.as_deref(), Some("from the server"));
    }

    #[test]
    fn descriptions_and_schema_strings_are_sanitised_before_caching() {
        let mut schema = JsonObject::new();
        schema.insert(
            "properties".into(),
            json!({
                "safe": {
                    "type": "string",
                    "description": "name\u{200b}\u{202e}txt\u{202c}\u{feff}\u{1b}[31m"
                },
                "bad\u{200b}name": { "type": "string" }
            }),
        );
        schema.insert("required".into(), json!(["safe", "bad\u{200b}name"]));
        let tool = RmcpTool::new_with_raw(
            "t",
            Some(std::borrow::Cow::Borrowed(
                "run\u{200b}\u{202e}safe\u{202c}",
            )),
            Arc::new(schema),
        );
        let spec = ToolSpec::from_rmcp(&tool);
        assert_eq!(spec.description.as_deref(), Some("runsafe"));
        assert_eq!(
            spec.input_schema["properties"]["safe"]["description"],
            "nametxt"
        );
        assert!(
            spec.input_schema["properties"]
                .get("bad\u{200b}name")
                .is_none()
        );
        assert_eq!(spec.input_schema["required"], json!(["safe"]));
    }

    #[test]
    fn an_oversized_schema_loses_prose_before_it_loses_parameters() {
        let mut properties = serde_json::Map::new();
        for i in 0..6 {
            properties.insert(
                format!("p{i}"),
                json!({ "type": "string", "description": "y".repeat(3_000) }),
            );
        }
        let mut schema = JsonObject::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("properties".into(), Value::Object(properties));

        let mut tool = RmcpTool::new_with_raw("t", None, Arc::new(schema));
        tool.description = Some(Cow::Borrowed("d"));
        let spec = ToolSpec::from_rmcp(&tool);

        let kept = spec.input_schema["properties"].as_object().unwrap();
        assert_eq!(kept.len(), 6, "not one parameter is missing");
        assert!(
            serde_json::to_string(&spec.input_schema)
                .unwrap()
                .chars()
                .count()
                <= MAX_SCHEMA_CHARS,
            "yet the whole thing fits the budget"
        );
        assert!(
            !spec.description.unwrap().contains("omitted"),
            "with nothing dropped it must not claim something was dropped"
        );
    }

    #[test]
    fn nested_descriptions_are_trimmed_before_parameters_are_dropped() {
        let mut properties = serde_json::Map::new();
        for i in 0..4 {
            properties.insert(
                format!("p{i}"),
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "description": "nested prose ".repeat(1_000),
                        "properties": {
                            "value": {
                                "type": "string",
                                "description": "more nested prose ".repeat(1_000)
                            }
                        }
                    }
                }),
            );
        }
        let mut schema = JsonObject::new();
        schema.insert("properties".into(), Value::Object(properties));
        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(schema)));

        assert_eq!(
            spec.input_schema["properties"].as_object().unwrap().len(),
            4
        );
        assert!(
            serde_json::to_string(&spec.input_schema)
                .unwrap()
                .chars()
                .count()
                <= MAX_SCHEMA_CHARS
        );
    }

    #[test]
    fn malformed_required_and_additional_properties_do_not_poison_the_whole_request() {
        let mut schema = JsonObject::new();
        schema.insert(
            "properties".into(),
            json!({ "known": { "type": "string" } }),
        );
        schema.insert("required".into(), json!(["known", "missing", 7]));
        schema.insert("additionalProperties".into(), json!("no"));
        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(schema)));

        assert_eq!(spec.input_schema["required"], json!(["known"]));
        assert!(spec.input_schema.get("additionalProperties").is_none());
    }

    #[test]
    fn a_hopeless_schema_drops_optional_parameters_and_says_which() {
        let mut properties = serde_json::Map::new();
        properties.insert("must".into(), json!({ "type": "string" }));
        for i in 0..60 {
            properties.insert(
                format!("opt{i}"),
                json!({ "type": "string", "description": "z".repeat(400), "enum": ["a", "b"] }),
            );
        }
        let mut schema = JsonObject::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("required".into(), json!(["must"]));
        schema.insert("properties".into(), Value::Object(properties));

        let mut tool = RmcpTool::new_with_raw("t", None, Arc::new(schema));
        tool.description = Some(Cow::Borrowed("does a thing"));
        let spec = ToolSpec::from_rmcp(&tool);

        let kept = spec.input_schema["properties"].as_object().unwrap();
        assert!(
            kept.contains_key("must"),
            "a required parameter is never dropped — dropping one dooms every call"
        );
        assert!(kept.len() < 61, "something really was dropped");
        assert!(
            serde_json::to_string(&spec.input_schema)
                .unwrap()
                .chars()
                .count()
                <= MAX_SCHEMA_CHARS
        );

        let description = spec.description.unwrap();
        assert!(
            description.starts_with("does a thing"),
            "the original description is still there: {description}"
        );
        assert!(description.contains("omitted"), "{description}");
        assert!(
            description.contains("opt59"),
            "the dropped names must be called out: {description}"
        );
    }

    #[test]
    fn omitted_parameters_are_disclosed_even_without_a_server_description() {
        let mut properties = serde_json::Map::new();
        for i in 0..80 {
            properties.insert(
                format!("optional_{i}"),
                json!({ "type": "string", "enum": ["x".repeat(500)] }),
            );
        }
        let mut schema = JsonObject::new();
        schema.insert("properties".into(), Value::Object(properties));
        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(schema)));

        let description = spec.description.expect("omissions need an explanation");
        assert!(description.contains("omitted"), "{description}");
        assert!(description.chars().count() < 1_000, "{description}");
    }

    #[test]
    fn required_parameters_are_kept_even_past_the_budget() {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for i in 0..40 {
            properties.insert(
                format!("r{i}"),
                json!({ "type": "string", "description": "q".repeat(500) }),
            );
            required.push(format!("r{i}"));
        }
        let mut schema = JsonObject::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("required".into(), json!(required));
        schema.insert("properties".into(), Value::Object(properties));

        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(schema)));
        assert_eq!(
            spec.input_schema["properties"].as_object().unwrap().len(),
            40
        );
        assert!(
            serde_json::to_string(&spec.input_schema)
                .unwrap()
                .chars()
                .count()
                <= MAX_SCHEMA_CHARS
        );
    }

    #[test]
    fn one_huge_optional_parameter_does_not_hide_smaller_parameters_after_it() {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "a_huge".into(),
            json!({ "type": "string", "enum": ["x".repeat(MAX_SCHEMA_CHARS)] }),
        );
        properties.insert("b_small".into(), json!({ "type": "boolean" }));
        let mut schema = JsonObject::new();
        schema.insert("properties".into(), Value::Object(properties));

        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(schema)));
        let kept = spec.input_schema["properties"].as_object().unwrap();
        assert!(!kept.contains_key("a_huge"));
        assert!(kept.contains_key("b_small"));
    }

    #[test]
    fn huge_top_level_schema_metadata_is_removed_as_a_last_resort() {
        let mut schema = JsonObject::new();
        schema.insert(
            "$defs".into(),
            json!({ "generated": { "type": "string", "enum": ["x".repeat(MAX_SCHEMA_CHARS * 2)] } }),
        );
        schema.insert(
            "properties".into(),
            json!({ "query": { "type": "string" } }),
        );
        schema.insert("required".into(), json!(["query"]));

        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(schema)));
        assert_eq!(spec.input_schema["required"], json!(["query"]));
        assert_eq!(spec.input_schema["properties"]["query"]["type"], "string");
        assert!(spec.input_schema.get("$defs").is_none());
    }

    #[test]
    fn a_schema_is_forced_into_the_shape_providers_accept() {
        let mut bare = JsonObject::new();
        bare.insert("description".into(), json!("no type, no properties"));
        let spec = ToolSpec::from_rmcp(&RmcpTool::new_with_raw("t", None, Arc::new(bare)));
        assert_eq!(spec.input_schema["type"], "object");
        assert!(spec.input_schema["properties"].is_object());
        assert_eq!(
            spec.input_schema["description"], "no type, no properties",
            "the rest is kept"
        );
    }

    /// An unannotated tool must not be treated as harmless: it is arbitrary code in someone else's
    /// process, and zlogic's default policy asks about writes.
    #[test]
    fn risk_follows_the_annotations_and_silence_means_ask() {
        let unannotated = ToolSpec::from_rmcp(&rmcp_tool("t", None));
        assert_eq!(unannotated.risk(), ToolRisk::Write);

        let read_only = ToolSpec::from_rmcp(&rmcp_tool(
            "t",
            Some(ToolAnnotations::new().read_only(true)),
        ));
        assert_eq!(read_only.risk(), ToolRisk::Read);

        let destructive = ToolSpec::from_rmcp(&rmcp_tool(
            "t",
            Some(ToolAnnotations::new().destructive(true)),
        ));
        assert_eq!(destructive.risk(), ToolRisk::High);
    }

    /// The spec's default for `destructiveHint` is true, and that only applies to a server that
    /// annotated something.
    #[test]
    fn an_annotated_tool_that_says_nothing_about_destruction_is_high_risk() {
        let spec = ToolSpec::from_rmcp(&rmcp_tool(
            "t",
            Some(ToolAnnotations::new().idempotent(true)),
        ));
        assert!(spec.hints.destructive);
        assert_eq!(spec.risk(), ToolRisk::High);
    }

    #[test]
    fn destructive_wins_over_a_conflicting_read_only_annotation() {
        let spec = ToolSpec::from_rmcp(&rmcp_tool(
            "t",
            Some(ToolAnnotations::new().read_only(true).destructive(true)),
        ));
        assert_eq!(spec.risk(), ToolRisk::High);
    }

    #[test]
    fn a_server_that_supplied_no_description_still_gets_one() {
        let mut s = (*spec("t")).clone();
        s.description = Some("   ".into());
        let tool = McpTool::new(
            def("s", json!({ "command": "x" })),
            Arc::new(s),
            fake_pool(Arc::new(FakeConnector::default()), PoolConfig::default()),
        );
        let description = tool.definition().description;
        assert!(
            !description.trim().is_empty(),
            "an empty description is rejected by providers"
        );
        assert!(description.contains("`s`"), "{description}");
    }

    #[tokio::test]
    async fn a_successful_call_comes_back_as_mapped_content() {
        let connector = Arc::new(FakeConnector::default());
        connector.reply(CallToolResult::success(vec![ContentBlock::text("42")]));
        let tool = McpTool::new(
            def("s", json!({ "command": "x", "cwd": "/fixed" })),
            spec("answer"),
            fake_pool(connector.clone(), PoolConfig::default()),
        );

        let out = tool.execute(&test_ctx(), r#"{"q":"life"}"#).await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(out.model_text(), "42");
        assert_eq!(connector.last_args(), Some(json!({ "q": "life" })));
        assert_eq!(
            connector.last_tool().as_deref(),
            Some("answer"),
            "the server's own name"
        );
    }

    /// Providers send `{}` — or nothing at all — for a tool with no required arguments.
    #[tokio::test]
    async fn empty_arguments_are_sent_as_none_rather_than_an_empty_object() {
        let connector = Arc::new(FakeConnector::default());
        connector.reply(CallToolResult::success(vec![ContentBlock::text("ok")]));
        let tool = McpTool::new(
            def("s", json!({ "command": "x", "cwd": "/fixed" })),
            spec("ping"),
            fake_pool(connector.clone(), PoolConfig::default()),
        );

        assert!(tool.execute(&test_ctx(), "").await.unwrap().status == ToolExecStatus::Success);
        assert_eq!(connector.last_args(), None);
    }

    #[tokio::test]
    async fn arguments_that_are_not_json_are_the_one_real_error() {
        let tool = McpTool::new(
            def("s", json!({ "command": "x" })),
            spec("t"),
            fake_pool(Arc::new(FakeConnector::default()), PoolConfig::default()),
        );
        assert!(matches!(
            tool.execute(&test_ctx(), "{not json").await,
            Err(ToolError::BadArgs(_))
        ));
    }

    /// A server that cannot start is a result the model can work around, and it names the server.
    #[tokio::test]
    async fn a_server_that_will_not_connect_fails_the_call_not_the_turn() {
        let connector = Arc::new(FakeConnector::default());
        connector
            .fail_until
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
        let tool = McpTool::new(
            def("github", json!({ "command": "x", "cwd": "/fixed" })),
            spec("t"),
            fake_pool(connector, PoolConfig::default()),
        );

        let out = tool.execute(&test_ctx(), "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("github"), "{}", out.model_text());
    }

    /// The message has to say what to set. "template `${env:GITHUB_TOKEN}` unresolved" is zlogic's
    /// vocabulary, not the user's.
    #[tokio::test]
    async fn a_missing_credential_says_which_one() {
        let tool = McpTool::new(
            def(
                "gh",
                json!({ "command": "x", "cwd": "/fixed",
                              "env": { "TOKEN": "${env:ZLOGIC_MCP_TEST_ABSENT}" } }),
            ),
            spec("t"),
            fake_pool(Arc::new(FakeConnector::default()), PoolConfig::default()),
        );
        let out = tool.execute(&test_ctx(), "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("ZLOGIC_MCP_TEST_ABSENT"), "{text}");
        assert!(text.contains("gh"), "{text}");
    }

    #[tokio::test]
    async fn a_cancelled_turn_does_not_start_a_call() {
        let connector = Arc::new(FakeConnector::default());
        let tool = McpTool::new(
            def("s", json!({ "command": "x", "cwd": "/fixed" })),
            spec("t"),
            fake_pool(connector.clone(), PoolConfig::default()),
        );
        let ctx = test_ctx();
        ctx.cancel.cancel();

        let out = tool.execute(&ctx, "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
        assert_eq!(
            connector.opens.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing was started"
        );
    }

    #[tokio::test]
    async fn cancelling_while_connecting_does_not_wait_for_the_handshake() {
        let connector = Arc::new(FakeConnector::default());
        connector.slow_connect(std::time::Duration::from_secs(10));
        let tool = McpTool::new(
            def("slow", json!({ "command": "x", "cwd": "/fixed" })),
            spec("t"),
            fake_pool(connector, PoolConfig::default()),
        );
        let ctx = test_ctx();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();
        });

        let started = std::time::Instant::now();
        let out = tool.execute(&ctx, "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(
            out.model_text().contains("connecting"),
            "{}",
            out.model_text()
        );
    }

    /// Cancelling mid-call still produces a result — a `tool_calls` group missing one is a replay
    /// error on every provider.
    #[tokio::test]
    async fn cancelling_during_a_call_still_produces_a_result() {
        let connector = Arc::new(FakeConnector::default());
        connector.hang();
        let tool = McpTool::new(
            def("s", json!({ "command": "x", "cwd": "/fixed" })),
            spec("t"),
            fake_pool(connector, PoolConfig::default()),
        );
        let ctx = test_ctx();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();
        });

        let out = tool.execute(&ctx, "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
        assert!(
            out.model_text().contains("interrupted"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn a_call_that_never_answers_times_out_as_a_timeout() {
        let connector = Arc::new(FakeConnector::default());
        connector.hang();
        let tool = McpTool::new(
            def("s", json!({ "command": "x", "cwd": "/fixed" })),
            spec("slow"),
            fake_pool(
                connector,
                PoolConfig {
                    call_timeout: std::time::Duration::from_millis(30),
                    ..Default::default()
                },
            ),
        );

        let out = tool.execute(&test_ctx(), "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Timeout);
        assert!(out.model_text().contains("slow"), "{}", out.model_text());
    }
}
