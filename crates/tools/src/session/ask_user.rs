//! `ask_user` — the agent asks a question and waits for the answer.
//! # Not the same thing as a permission prompt
//! The permission gate happens *to* a tool call, before it runs, and the tool never learns it
//! happened. This is a tool call that exists in order to ask. The distinction matters because the
//! answer is information the model needs — it becomes the tool result and drives the next round —
//! whereas a permission decision only ever gates.
//! # A sub-agent's only way to ask
//! A sub-agent has no channel of its own to the user. Without this it has to guess, invent a
//! plausible answer, or return a conclusion that quietly rests on an assumption nobody checked.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::interaction::{Choice, Control, FieldValue, Form, FormField};
use zlogic_protocol::llm::ToolDefinition;

use crate::{Result, Tool, ToolCtx, ToolError, ToolExecResult, ToolMeta, ToolRisk, parse_args};

/// Which control to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Text,
    Select,
    MultiSelect,
}

#[derive(Debug, Deserialize)]
struct Args {
    question: String,
    #[serde(default = "text_kind")]
    response_type: Kind,
    /// Required by `select` and `multi_select`, ignored by `text`.
    #[serde(default)]
    options: Vec<String>,
    /// A hint shown inside an empty text field.
    placeholder: Option<String>,
}

fn text_kind() -> Kind {
    Kind::Text
}

/// The one field every form here uses. Fixed so the answer lookup cannot drift from the request.
const FIELD: &str = "answer";

pub struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn meta(&self) -> ToolMeta {
        // Read: it changes nothing. It is also the mechanism by which the user is consulted, so
        // gating it behind a permission prompt would be asking permission to ask.
        ToolMeta {
            name: "ask_user".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".into(),
            description: "Ask the user something you cannot work out yourself and cannot safely \
                          assume. Keep it to one short question. Use select or multi_select with \
                          options when the answer is a choice — it is easier to answer and \
                          unambiguous to read back."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "minLength": 1, "description": "One short question" },
                    "response_type": {
                        "type": "string",
                        "enum": ["text", "select", "multi_select"],
                        "description": "The response control to show; default text"
                    },
                    "options": {
                        "type": "array", "items": { "type": "string", "minLength": 1 },
                        "description": "The choices, for select and multi_select"
                    },
                    "placeholder": { "type": "string", "description": "Hint shown in an empty text field" }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        let question = a.question.trim();
        if question.is_empty() {
            return Ok(ToolExecResult::failed("question is required"));
        }
        // A choice with nothing to choose from cannot be answered. Reported rather than silently
        // downgraded to a text field: the model asked for a closed question and needs to know it
        // did not get one.
        if a.response_type != Kind::Text && a.options.is_empty() {
            return Ok(ToolExecResult::failed(
                "options is required when response_type is select or multi_select",
            ));
        }
        if a.options.iter().any(|option| option.trim().is_empty()) {
            return Ok(ToolExecResult::failed(
                "options must not contain empty choices",
            ));
        }

        let choices: Vec<Choice> = a.options.iter().map(Choice::plain).collect();
        let control = match a.response_type {
            Kind::Text => Control::Input {
                default: None,
                placeholder: a.placeholder.clone(),
                max_len: None,
            },
            Kind::Select => Control::Select {
                options: choices,
                default: None,
                // The UI adds an "other" option: the user can type an answer instead of picking.
                free_text: true,
            },
            Kind::MultiSelect => Control::MultiSelect {
                options: choices,
                defaults: Vec::new(),
                min: None,
                max: None,
            },
        };
        let form = Form::new(
            question,
            vec![FormField::required(FIELD, question, control)],
        );

        let answer = match ctx.ask_form(form).await {
            Ok(Some(a)) => a,
            // Cancelled or declined. A failure result, because the model must not proceed as
            // though it had an answer — but not an `Err`: nothing went wrong, and the model can
            // reasonably continue by making its assumption explicit instead.
            Ok(None) => {
                return Ok(ToolExecResult::failed(
                    "the user did not answer. Do not assume one silently: either continue and \
                     state the assumption you are making, or stop and explain what you need.",
                ));
            }
            // No interface to ask through. `Err`, unlike the case above, because this is not
            // something the model can adapt to — the capability is simply absent.
            Err(ToolError::Unsupported(m)) => return Err(ToolError::Unsupported(m)),
            Err(e) => return Err(e),
        };

        // `ask_form` has already dropped any choice that was never offered, so a `Choice` here
        // is always one of the options. The `free_text` select also lets the user type their own
        // answer, which arrives as `Text` — and that must not read like a pick from the list, so
        // it is marked as such. `text` covers both `Text` and `Choice`, so a free answer and a
        // single selection read the same way; only a multi-selection is a list.
        let text = match a.response_type {
            Kind::Text => answer.text(FIELD).map(str::to_string),
            Kind::Select => match answer.values.get(FIELD) {
                Some(FieldValue::Text(t)) if !t.trim().is_empty() => Some(format!(
                    "{t} (the user typed this as a custom answer — not one of the options you offered)"
                )),
                Some(FieldValue::Text(t)) => Some(t.clone()),
                Some(FieldValue::Choice(c)) => Some(c.clone()),
                _ => None,
            },
            Kind::MultiSelect => answer.choices(FIELD).map(|choices| {
                choices
                    .iter()
                    .map(|choice| format!("- {choice}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        };

        Ok(match text.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => ToolExecResult::success(t),
            // An empty answer is distinct from no answer: the user engaged and gave nothing.
            // Saying so beats reporting an empty success the model would read as a real value.
            _ => ToolExecResult::failed("the user submitted an empty answer"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};
    use std::sync::Arc;
    use zlogic_protocol::interaction::{
        FieldValue, FormAnswer, InteractionDecision, InteractionPort, InteractionRequest,
    };

    /// A UI that replies with a fixed decision and remembers the form it was shown.
    struct Fixed {
        decision: InteractionDecision,
        seen: std::sync::Mutex<Option<InteractionRequest>>,
    }

    impl Fixed {
        fn new(decision: InteractionDecision) -> Arc<Self> {
            Arc::new(Self {
                decision,
                seen: std::sync::Mutex::new(None),
            })
        }

        fn answering(value: FieldValue) -> Arc<Self> {
            Self::new(InteractionDecision::Submitted(
                FormAnswer::new().set(FIELD, value),
            ))
        }
    }

    #[async_trait]
    impl InteractionPort for Fixed {
        async fn ask(
            &self,
            req: InteractionRequest,
        ) -> std::result::Result<InteractionDecision, String> {
            *self.seen.lock().unwrap() = Some(req);
            Ok(self.decision.clone())
        }
    }

    fn ctx_with(port: Arc<Fixed>) -> ToolCtx {
        let mut c = test_ctx(std::path::Path::new("/work"));
        c.interaction = Some(port);
        c
    }

    #[tokio::test]
    async fn a_text_answer_becomes_the_result() {
        let port = Fixed::answering(FieldValue::Text("use postgres".into()));
        let ctx = ctx_with(port);

        let out = AskUser
            .execute(&ctx, &json!({ "question": "Which database?" }).to_string())
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(out.model_text(), "use postgres");
    }

    #[tokio::test]
    async fn a_single_choice_answer_becomes_the_result() {
        // `Choice`, singular — the shape a `Select` answer has. `Choices` would be dropped by the
        // form's own sanitising as the wrong kind of value, which is the behaviour we want.
        let port = Fixed::answering(FieldValue::Choice("postgres".into()));
        let ctx = ctx_with(port.clone());

        let out = AskUser
            .execute(
                &ctx,
                &json!({
                    "question": "Which database?",
                    "response_type": "select",
                    "options": ["postgres", "sqlite"]
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.model_text(), "postgres");

        // The options really reached the UI — a select with no options is unanswerable.
        let shown = format!("{:?}", port.seen.lock().unwrap().as_ref().unwrap().body);
        assert!(shown.contains("sqlite"), "{shown}");
    }

    /// A select is not a straitjacket: the UI offers an "other" escape hatch, and a typed answer
    /// (arriving as `Text`) reads the same as a picked option — but the model is told it was the
    /// user's own words, not a pick from the list it offered.
    #[tokio::test]
    async fn a_free_text_answer_to_a_select_becomes_the_result() {
        let port = Fixed::answering(FieldValue::Text("mysql".into()));
        let ctx = ctx_with(port.clone());

        let out = AskUser
            .execute(
                &ctx,
                &json!({
                    "question": "Which database?",
                    "response_type": "select",
                    "options": ["postgres", "sqlite"]
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(
            out.model_text().starts_with("mysql"),
            "{}",
            out.model_text()
        );
        assert!(
            out.model_text()
                .contains("not one of the options you offered"),
            "{}",
            out.model_text()
        );

        // The form advertised the escape hatch: the UI must render the "other" option.
        let shown = format!("{:?}", port.seen.lock().unwrap().as_ref().unwrap().body);
        assert!(shown.contains("free_text"), "{shown}");
    }

    /// Picking "other" and submitting nothing is the same as any empty answer: reported, not
    /// passed on — and the marker must not make an empty answer look like a real one.
    #[tokio::test]
    async fn an_empty_other_answer_is_still_an_empty_answer() {
        let port = Fixed::answering(FieldValue::Text("   ".into()));
        let ctx = ctx_with(port);

        let out = AskUser
            .execute(
                &ctx,
                &json!({
                    "question": "Which database?",
                    "response_type": "select",
                    "options": ["postgres", "sqlite"]
                })
                .to_string(),
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("empty answer"));
    }

    #[tokio::test]
    async fn several_choices_come_back_as_a_readable_list() {
        let port = Fixed::answering(FieldValue::Choices(vec!["a".into(), "c".into()]));
        let ctx = ctx_with(port);

        let out = AskUser
            .execute(
                &ctx,
                &json!({
                    "question": "Which ones?",
                    "response_type": "multi_select",
                    "options": ["a", "b", "c"]
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(out.model_text(), "- a\n- c");
    }

    /// Silence must not read as consent, and the model is told what to do instead.
    #[tokio::test]
    async fn a_cancelled_question_tells_the_model_not_to_assume() {
        let ctx = ctx_with(Fixed::new(InteractionDecision::Cancelled));
        let out = AskUser
            .execute(&ctx, &json!({ "question": "Proceed?" }).to_string())
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("Do not assume"),
            "{}",
            out.model_text()
        );
    }

    /// Answered-with-nothing is not the same as unanswered.
    #[tokio::test]
    async fn an_empty_answer_is_reported_rather_than_passed_on() {
        let ctx = ctx_with(Fixed::answering(FieldValue::Text("   ".into())));
        let out = AskUser
            .execute(&ctx, &json!({ "question": "Which?" }).to_string())
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("empty answer"));
    }

    /// A closed question with nothing to choose from is the caller's mistake, and is named.
    #[tokio::test]
    async fn a_choice_without_options_is_refused() {
        let ctx = ctx_with(Fixed::answering(FieldValue::Text("x".into())));
        let out = AskUser
            .execute(
                &ctx,
                &json!({ "question": "Which?", "response_type": "select" }).to_string(),
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("options is required"));
    }

    /// A headless run must not invent an answer.
    #[tokio::test]
    async fn without_a_ui_it_fails_closed() {
        let ctx = test_ctx(std::path::Path::new("/work"));
        let err = AskUser
            .execute(&ctx, &json!({ "question": "Which?" }).to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Unsupported(_)));
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        let ctx = ctx_with(Fixed::answering(FieldValue::Text("x".into())));
        assert!(AskUser.execute(&ctx, "{}").await.is_err());
        assert!(AskUser.execute(&ctx, "not json").await.is_err());
        assert_eq!(
            AskUser
                .execute(&ctx, r#"{"question":"  "}"#)
                .await
                .unwrap()
                .status,
            ToolExecStatus::Failed
        );
    }

    /// An unknown answer kind is a schema mismatch, not something to silently default.
    #[tokio::test]
    async fn an_unknown_answer_kind_is_rejected() {
        let ctx = ctx_with(Fixed::answering(FieldValue::Text("x".into())));
        assert!(
            AskUser
                .execute(
                    &ctx,
                    &json!({ "question": "q", "response_type": "dropdown" }).to_string()
                )
                .await
                .is_err()
        );
    }
}
