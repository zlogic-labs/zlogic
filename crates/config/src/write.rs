use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_yaml_ng::Value;

use crate::{ConfigError, Result};

pub type YamlPatch = BTreeMap<String, Option<Value>>;

pub fn value_or_default<T>(section: &T) -> Result<Option<Value>>
where
    T: Serialize + PartialEq + Default,
{
    if section == &T::default() {
        return Ok(None);
    }
    Ok(Some(serde_yaml_ng::to_value(section).map_err(|e| {
        ConfigError::Invalid(format!("failed to serialize section: {e}"))
    })?))
}

pub fn scalar_or<T: Serialize + ?Sized>(value: &T, default: &T) -> Result<Option<Value>> {
    let to_value = |v: &T| -> Result<Value> {
        serde_yaml_ng::to_value(v)
            .map_err(|e| ConfigError::Invalid(format!("serialization failed: {e}")))
    };
    let (a, b) = (to_value(value)?, to_value(default)?);
    Ok((a != b).then_some(a))
}

pub fn text_or_default(value: &str) -> Option<Value> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| Value::String(trimmed.to_owned()))
}

pub fn patch_yaml_file(path: &Path, patch: &YamlPatch) -> Result<()> {
    let original = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    let mut text = original;
    for (key, value) in patch {
        let block = match value {
            Some(v) => Some(emit_block(key, v)?),
            None => None,
        };
        text = upsert_top_level_key(&text, key, block.as_deref());
    }

    write_atomically(path, &text)
}

fn emit_block(key: &str, value: &Value) -> Result<String> {
    let mut map = serde_yaml_ng::Mapping::new();
    map.insert(Value::String(key.to_string()), value.clone());
    let text = serde_yaml_ng::to_string(&Value::Mapping(map))
        .map_err(|e| ConfigError::Invalid(format!("failed to serialize {key}: {e}")))?;
    Ok(text)
}

fn upsert_top_level_key(text: &str, key: &str, block: Option<&str>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let head = format!("{key}:");
    let start = lines
        .iter()
        .position(|l| *l == head || l.starts_with(&format!("{key}: ")));

    let Some(start) = start else {
        let Some(block) = block else {
            return text.to_string();
        };
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(block);
        return out;
    };

    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end];
        let is_continuation = line.trim().is_empty()
            || line.starts_with(char::is_whitespace)
            || line.trim_start().starts_with('#');
        if !is_continuation {
            break;
        }
        end += 1;
    }
    while end > start + 1 {
        let line = lines[end - 1];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            end -= 1;
        } else {
            break;
        }
    }

    let mut out: Vec<String> = lines[..start].iter().map(|s| s.to_string()).collect();
    if let Some(block) = block {
        out.extend(block.lines().map(|s| s.to_string()));
    }
    out.extend(lines[end..].iter().map(|s| s.to_string()));

    let mut joined = out.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

fn write_atomically(path: &Path, body: &str) -> Result<()> {
    let io = |e: std::io::Error| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, body).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ApprovalMode, ContextConfig, SessionConfig};

    fn patch(pairs: Vec<(&str, Option<Value>)>) -> YamlPatch {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn a_section_that_equals_the_default_is_written_as_a_removal() {
        assert_eq!(value_or_default(&ContextConfig::default()).unwrap(), None);
        assert!(
            value_or_default(&ContextConfig {
                compact_ratio: 0.5,
                ..Default::default()
            })
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn writing_a_key_leaves_every_other_line_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let original = "\
# Top-of-file explanation; this whole block is what gives this file its value.
# Second line.

# Explanation of default_model.
default_model: anthropic:opus

# Explanation of session.
session:
  approval_mode: auto
";
        std::fs::write(&path, original).unwrap();

        patch_yaml_file(
            &path,
            &patch(vec![(
                "default_model",
                Some(Value::String("openai:gpt-5".into())),
            )]),
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(
            "# Top-of-file explanation; this whole block is what gives this file its value."
        ));
        assert!(after.contains("# Explanation of default_model."), "{after}");
        assert!(after.contains("# Explanation of session."), "{after}");
        assert!(after.contains("default_model: openai:gpt-5"), "{after}");
        assert!(!after.contains("anthropic:opus"));
        assert!(after.contains("approval_mode: auto"));
    }

    #[test]
    fn removing_a_key_keeps_the_comment_that_documents_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "# Context compaction.\ncontext:\n  compact_ratio: 0.5\n  tail_turns: 4\n\n# Session.\nsession:\n  approval_mode: bypass\n",
        )
        .unwrap();

        patch_yaml_file(&path, &patch(vec![("context", None)])).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# Context compaction."), "{after}");
        assert!(!after.contains("compact_ratio"), "{after}");
        assert!(!after.contains("tail_turns"), "{after}");
        assert!(after.contains("# Session."), "{after}");
        assert!(after.contains("approval_mode: bypass"), "{after}");
    }

    #[test]
    fn a_new_key_is_appended_and_reparses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "# A template that is nothing but comments.\n# session:\n#   approval_mode: auto\n",
        )
        .unwrap();

        let section = SessionConfig {
            approval_mode: ApprovalMode::Bypass,
            ..Default::default()
        };
        patch_yaml_file(
            &path,
            &patch(vec![("session", value_or_default(&section).unwrap())]),
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("#   approval_mode: auto"), "{after}");
        let parsed: crate::ConfigFile = serde_yaml_ng::from_str(&after).unwrap();
        assert_eq!(parsed.session.unwrap().approval_mode, ApprovalMode::Bypass);
    }

    #[test]
    fn writing_into_a_missing_file_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.yaml");
        patch_yaml_file(
            &path,
            &patch(vec![("default_model", Some(Value::String("a:b".into())))]),
        )
        .unwrap();
        let parsed: crate::ConfigFile =
            serde_yaml_ng::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("a:b"));
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        patch_yaml_file(
            &path,
            &patch(vec![("default_model", Some(Value::String("a:b".into())))]),
        )
        .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn both_inline_and_block_forms_are_recognised() {
        let text = "default_model: a:b\nsession:\n  approval_mode: auto\n";
        let out = upsert_top_level_key(text, "default_model", Some("default_model: c:d\n"));
        assert_eq!(out, "default_model: c:d\nsession:\n  approval_mode: auto\n");

        let out = upsert_top_level_key(text, "session", None);
        assert_eq!(out, "default_model: a:b\n");
    }

    #[test]
    fn a_key_that_is_a_prefix_of_another_is_not_confused_for_it() {
        let text = "log_level: debug\nlog:\n  to_file: false\n";
        let out = upsert_top_level_key(text, "log", None);
        assert_eq!(out, "log_level: debug\n");
    }
}
