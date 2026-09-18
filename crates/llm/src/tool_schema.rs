use serde_json::{Map, Value};

const SCHEMA_INTENT_KEYS: &[&str] = &[
    "type",
    "properties",
    "items",
    "prefixItems",
    "enum",
    "const",
    "$ref",
    "additionalProperties",
    "patternProperties",
    "required",
    "not",
    "if",
    "then",
    "else",
];

fn has_combiner(v: &Map<String, Value>) -> bool {
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .any(|k| v.get(*k).is_some_and(Value::is_array))
}

fn has_schema_intent(v: &Value) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    has_combiner(obj) || SCHEMA_INTENT_KEYS.iter().any(|k| obj.contains_key(*k))
}

fn type_is(obj: &Map<String, Value>, want: &str) -> bool {
    obj.get("type").and_then(Value::as_str) == Some(want)
}

fn stringify(v: &Value) -> Value {
    match v {
        Value::String(_) => v.clone(),
        Value::Null => Value::String("null".into()),
        other => Value::String(other.to_string()),
    }
}

fn sanitize(node: &Value) -> Value {
    let Some(obj) = node.as_object() else {
        if let Some(arr) = node.as_array() {
            return Value::Array(arr.iter().map(sanitize).collect());
        }
        return node.clone();
    };

    let mut out = Map::new();
    for (k, v) in obj {
        if k == "enum" && v.is_array() {
            out.insert(
                k.clone(),
                Value::Array(v.as_array().unwrap().iter().map(stringify).collect()),
            );
        } else {
            out.insert(k.clone(), sanitize(v));
        }
    }

    if out.get("enum").is_some_and(Value::is_array)
        && (type_is(&out, "integer") || type_is(&out, "number"))
    {
        out.insert("type".into(), Value::String("string".into()));
    }

    if type_is(&out, "object")
        && let Some(props) = out.get("properties").and_then(Value::as_object).cloned()
        && let Some(req) = out.get("required").and_then(Value::as_array)
    {
        let kept: Vec<Value> = req
            .iter()
            .filter(|f| f.as_str().is_some_and(|name| props.contains_key(name)))
            .cloned()
            .collect();
        out.insert("required".into(), Value::Array(kept));
    }

    if type_is(&out, "array") && !has_combiner(&out) {
        let items = out
            .get("items")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let items = match items {
            Value::Object(mut m) if !has_schema_intent(&Value::Object(m.clone())) => {
                m.insert("type".into(), Value::String("string".into()));
                Value::Object(m)
            }
            other => other,
        };
        out.insert("items".into(), items);
    }

    if let Some(t) = out.get("type").and_then(Value::as_str)
        && t != "object"
        && !has_combiner(&out)
    {
        out.remove("properties");
        out.remove("required");
    }

    Value::Object(out)
}

fn is_empty_object(obj: &Map<String, Value>) -> bool {
    type_is(obj, "object")
        && obj
            .get("properties")
            .is_none_or(|p| p.as_object().is_none_or(Map::is_empty))
        && obj.get("additionalProperties").is_none()
}

fn project(node: &Value) -> Option<Value> {
    let obj = node.as_object()?;
    if is_empty_object(obj) {
        return None;
    }

    let mut out = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            out.insert(k.to_string(), v);
        }
    };

    put("description", obj.get("description").cloned());
    put("required", obj.get("required").cloned());
    put("format", obj.get("format").cloned());

    match obj.get("type") {
        Some(Value::Array(types)) => {
            let first = types.iter().find(|t| t.as_str() != Some("null")).cloned();
            put("type", first);
            if types.iter().any(|t| t.as_str() == Some("null")) {
                put("nullable", Some(Value::Bool(true)));
            }
        }
        Some(t) => put("type", Some(t.clone())),
        None => {}
    }

    put(
        "enum",
        match obj.get("const") {
            Some(c) => Some(Value::Array(vec![stringify(c)])),
            None => obj.get("enum").cloned(),
        },
    );

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        let projected: Map<String, Value> = props
            .iter()
            .filter_map(|(k, v)| project(v).map(|p| (k.clone(), p)))
            .collect();
        put("properties", Some(Value::Object(projected)));
    }

    match obj.get("items") {
        Some(Value::Array(items)) => {
            put(
                "items",
                Some(Value::Array(items.iter().filter_map(project).collect())),
            );
        }
        Some(one) => put("items", project(one)),
        None => {}
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            put(
                key,
                Some(Value::Array(arr.iter().filter_map(project).collect())),
            );
        }
    }
    put("minLength", obj.get("minLength").cloned());

    Some(Value::Object(out))
}

pub fn gemini(schema: &Value) -> Option<Value> {
    project(&sanitize(schema))
}

pub fn anthropic_output(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(anthropic_output).collect()),
        Value::Object(obj) => {
            let mut out: Map<String, Value> = obj
                .iter()
                .map(|(k, v)| (k.clone(), anthropic_output(v)))
                .collect();
            if type_is(obj, "object") {
                out.insert("additionalProperties".into(), Value::Bool(false));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn additional_properties_is_dropped() {
        let s = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });
        let out = gemini(&s).unwrap();
        assert!(out.get("additionalProperties").is_none());
        assert_eq!(out["properties"]["path"]["type"], "string");
        assert_eq!(out["required"], json!(["path"]));
    }

    #[test]
    fn schema_metadata_keys_are_dropped() {
        let s = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Args",
            "default": {},
            "type": "object",
            "properties": { "a": { "type": "string", "title": "A", "default": "x" } }
        });
        let out = gemini(&s).unwrap();
        for k in ["$schema", "title", "default"] {
            assert!(out.get(k).is_none(), "{k} should be dropped");
        }
        let a = &out["properties"]["a"];
        assert!(a.get("title").is_none() && a.get("default").is_none());
        assert_eq!(a["type"], "string");
    }

    #[test]
    fn integer_enums_become_string_enums() {
        let s = json!({
            "type": "object",
            "properties": { "level": { "type": "integer", "enum": [1, 2, 3] } }
        });
        let level = &gemini(&s).unwrap()["properties"]["level"];
        assert_eq!(
            level["type"], "string",
            "the type declaration changes with it, or the schema contradicts itself"
        );
        assert_eq!(level["enum"], json!(["1", "2", "3"]));
    }

    #[test]
    fn required_entries_without_a_property_are_removed() {
        let s = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a", "ghost"]
        });
        assert_eq!(gemini(&s).unwrap()["required"], json!(["a"]));
    }

    #[test]
    fn arrays_always_get_a_typed_items() {
        let s = json!({ "type": "object", "properties": { "xs": { "type": "array" } } });
        let xs = &gemini(&s).unwrap()["properties"]["xs"];
        assert_eq!(
            xs["items"]["type"], "string",
            "an array without items is rejected"
        );

        let s = json!({
            "type": "object",
            "properties": { "xs": { "type": "array", "items": { "type": "number" } } }
        });
        assert_eq!(
            gemini(&s).unwrap()["properties"]["xs"]["items"]["type"],
            "number"
        );
    }

    #[test]
    fn scalars_do_not_keep_object_only_keys() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "properties": { "junk": {} }, "required": ["junk"] }
            }
        });
        let name = &gemini(&s).unwrap()["properties"]["name"];
        assert!(name.get("properties").is_none());
        assert!(name.get("required").is_none());
    }

    #[test]
    fn nullable_union_becomes_the_nullable_flag() {
        let s = json!({
            "type": "object",
            "properties": { "maybe": { "type": ["string", "null"] } }
        });
        let m = &gemini(&s).unwrap()["properties"]["maybe"];
        assert_eq!(m["type"], "string");
        assert_eq!(m["nullable"], true);
    }

    #[test]
    fn const_becomes_a_single_value_enum() {
        let s = json!({ "type": "object", "properties": { "kind": { "const": "fixed" } } });
        assert_eq!(
            gemini(&s).unwrap()["properties"]["kind"]["enum"],
            json!(["fixed"])
        );
    }

    #[test]
    fn a_no_arg_tool_yields_no_parameters_at_all() {
        assert!(gemini(&json!({ "type": "object", "properties": {} })).is_none());
        assert!(gemini(&json!({ "type": "object" })).is_none());
        assert!(
            gemini(&json!({ "type": "object", "properties": { "a": { "type": "string" } } }))
                .is_some()
        );
    }

    #[test]
    fn nested_structures_are_projected_all_the_way_down() {
        let s = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "$schema": "x",
                        "properties": {
                            "old": { "type": "string" },
                            "count": { "type": "integer", "enum": [1, 2] }
                        },
                        "required": ["old", "nope"]
                    }
                }
            }
        });
        let item = &gemini(&s).unwrap()["properties"]["edits"]["items"];
        assert!(
            item.get("additionalProperties").is_none(),
            "nested levels are cleaned too"
        );
        assert!(item.get("$schema").is_none());
        assert_eq!(item["required"], json!(["old"]));
        assert_eq!(item["properties"]["count"]["type"], "string");
    }

    #[test]
    fn combiners_survive() {
        let s = json!({
            "type": "object",
            "properties": {
                "v": { "anyOf": [{ "type": "string" }, { "type": "number" }] }
            }
        });
        let v = &gemini(&s).unwrap()["properties"]["v"];
        assert_eq!(v["anyOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn descriptions_are_preserved_at_every_level() {
        let s = json!({
            "type": "object",
            "description": "top",
            "properties": { "a": { "type": "string", "description": "inner" } }
        });
        let out = gemini(&s).unwrap();
        assert_eq!(out["description"], "top");
        assert_eq!(out["properties"]["a"]["description"], "inner");
    }
}
