//! `memory_update` — persist a user-confirmed fact outside conversation history.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::interaction::{Choice, Form};
use zlogic_protocol::llm::ToolDefinition;
use zlogic_protocol::{MemoryCategory, MemoryId, MemoryRecord, MemoryScope, SessionId, TurnId};

use crate::{
    Result, Tool, ToolCtx, ToolDisplay, ToolError, ToolExecResult, ToolMeta, ToolRisk, parse_args,
};

#[async_trait]
pub trait MemoryHost: Send + Sync {
    async fn get(&self, memory_id: MemoryId) -> std::result::Result<MemoryRecord, String>;
    async fn add(
        &self,
        scope: MemoryScope,
        category: MemoryCategory,
        fact: String,
        source_quote: String,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> std::result::Result<MemoryRecord, String>;
    async fn update(
        &self,
        memory_id: MemoryId,
        category: MemoryCategory,
        fact: String,
        source_quote: String,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> std::result::Result<MemoryRecord, String>;
    async fn remove(
        &self,
        memory_id: MemoryId,
        source_quote: String,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> std::result::Result<MemoryRecord, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Add,
    Update,
    Remove,
}

#[derive(Debug, Deserialize)]
struct Args {
    operation: Operation,
    #[serde(default)]
    scope: Option<MemoryScope>,
    #[serde(default)]
    category: Option<MemoryCategory>,
    #[serde(default)]
    memory_id: Option<MemoryId>,
    #[serde(default)]
    fact: Option<String>,
    #[serde(default)]
    source_quote: Option<String>,
}

pub struct MemoryUpdate {
    host: Arc<dyn MemoryHost>,
}

impl MemoryUpdate {
    pub fn new(host: Arc<dyn MemoryHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl Tool for MemoryUpdate {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "memory_update".into(),
            source: "builtin",
            risk: ToolRisk::Write,
        }
    }

    fn root_only(&self) -> bool {
        true
    }

    fn affects_workspace(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_update".into(),
            description: "Persist, correct, or remove a durable fact that the user explicitly \
                          stated. Store only user preferences, corrections, long-term goals, or \
                          references. Never store facts inferred from code, one-off task state, or \
                          anything recoverable by reading the repository. source_quote must be a \
                          short exact quote from the user's current message. New memories default \
                          to workspace scope; use global only when the fact remains true in other \
                          repositories. update/remove require the id shown in injected memory. \
                          Every operation requires source_quote from the current user message."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "enum": ["add", "update", "remove"] },
                    "scope": { "type": "string", "enum": ["global", "workspace"], "description": "add only; defaults to workspace" },
                    "category": { "type": "string", "enum": ["preference", "correction", "goal", "reference"], "description": "required for add/update" },
                    "memory_id": { "type": "string", "description": "required for update/remove" },
                    "fact": { "type": "string", "minLength": 1, "description": "one atomic durable fact; required for add/update" },
                    "source_quote": { "type": "string", "minLength": 2, "description": "short exact quote from the user's current message; required for every operation" }
                },
                "required": ["operation"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        let existing = match a.memory_id {
            Some(id) => Some(self.host.get(id).await.map_err(ToolError::Failed)?),
            None => None,
        };
        let scope = match a.operation {
            Operation::Add => a.scope.unwrap_or(MemoryScope::Workspace),
            Operation::Update | Operation::Remove => {
                existing
                    .as_ref()
                    .ok_or_else(|| ToolError::BadArgs("memory_id is required".into()))?
                    .scope
            }
        };

        if scope == MemoryScope::Global && !confirm_global(ctx, &a, existing.as_ref()).await? {
            return Ok(ToolExecResult::cancelled(
                "global memory change was not confirmed",
            ));
        }

        let record = match a.operation {
            Operation::Add => {
                let (category, fact, source_quote) = write_fields(&a)?;
                self.host
                    .add(
                        scope,
                        category,
                        fact,
                        source_quote,
                        ctx.session_id,
                        ctx.turn_id,
                    )
                    .await
            }
            Operation::Update => {
                let memory_id = a
                    .memory_id
                    .ok_or_else(|| ToolError::BadArgs("memory_id is required".into()))?;
                let (category, fact, source_quote) = write_fields(&a)?;
                self.host
                    .update(
                        memory_id,
                        category,
                        fact,
                        source_quote,
                        ctx.session_id,
                        ctx.turn_id,
                    )
                    .await
            }
            Operation::Remove => {
                let memory_id = a
                    .memory_id
                    .ok_or_else(|| ToolError::BadArgs("memory_id is required".into()))?;
                let source_quote = source_quote(&a)?;
                self.host
                    .remove(memory_id, source_quote, ctx.session_id, ctx.turn_id)
                    .await
            }
        }
        .map_err(ToolError::Failed)?;

        let verb = match a.operation {
            Operation::Add => "remembered",
            Operation::Update => "updated",
            Operation::Remove => "removed",
        };
        Ok(ToolExecResult::success(format!(
            "{verb} {} memory {}: {}",
            record.scope, record.memory_id, record.fact
        ))
        .with_display(ToolDisplay::text(format!(
            "📝 {verb}: {} ({}/{})",
            record.fact, record.scope, record.category
        ))))
    }
}

fn write_fields(a: &Args) -> Result<(MemoryCategory, String, String)> {
    let category = a
        .category
        .ok_or_else(|| ToolError::BadArgs("category is required".into()))?;
    let fact = a
        .fact
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::BadArgs("fact is required".into()))?
        .to_string();
    let source = source_quote(a)?;
    Ok((category, fact, source))
}

fn source_quote(a: &Args) -> Result<String> {
    Ok(a.source_quote
        .as_deref()
        .map(str::trim)
        .filter(|s| s.chars().count() >= 2)
        .ok_or_else(|| {
            ToolError::BadArgs("source_quote must contain at least 2 characters".into())
        })?
        .to_string())
}

async fn confirm_global(
    ctx: &ToolCtx,
    args: &Args,
    existing: Option<&MemoryRecord>,
) -> Result<bool> {
    let fact = args
        .fact
        .as_deref()
        .or_else(|| existing.map(|m| m.fact.as_str()))
        .unwrap_or("(unknown)");
    let form = Form::single_choice(
        "Confirm global memory",
        "action",
        vec![
            Choice::new("confirm", "Save for every workspace"),
            Choice::new("cancel", "Cancel"),
        ],
    )
    .with_message(format!(
        "This affects future conversations in every workspace:\n\n{fact}"
    ));
    Ok(ctx
        .ask_form(form)
        .await?
        .is_some_and(|answer| answer.text("action") == Some("confirm")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remove_args(source_quote: Option<&str>) -> Args {
        Args {
            operation: Operation::Remove,
            scope: None,
            category: None,
            memory_id: Some(MemoryId::new()),
            fact: None,
            source_quote: source_quote.map(str::to_string),
        }
    }

    #[test]
    fn remove_requires_a_meaningful_current_user_quote() {
        assert!(source_quote(&remove_args(None)).is_err());
        assert!(source_quote(&remove_args(Some("x"))).is_err());
        assert_eq!(
            source_quote(&remove_args(Some("  forget it  "))).unwrap(),
            "forget it"
        );
    }
}
