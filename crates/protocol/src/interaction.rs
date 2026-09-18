//! Asking the user something.
//! Lives in `protocol` rather than in the tools crate because there are two independent
//! callers: **core** raises permission requests from the tool-execution gate, and **tools**
//! raise forms when they need input they cannot infer. One definition, or the two drift.
//! # Two different things called "interaction"
//! 1. **The permission gate.** Core consults policy *before* running a tool. The tool is never
//!    involved and never learns it happened.
//! 2. **A form.** Something is running and needs a decision it cannot make — which of these
//!    three matches did you mean, this will overwrite 40 files, proceed?
//! Both travel the same channel, so a client renders and answers them the same way.
//! # Answers are ordinary user input, not something to be validated
//! Constraints like `required`, `max_len` and number ranges are **rendering hints**, not gates.
//! Rejecting an answer for being one character too long buys nothing and costs a round trip; the
//! model reads what the user actually wrote and copes, which is what it is good at.
//! # Except that a closed set of options is what makes `Select` a type
//! [`Form::sanitize`] enforces exactly two things, and neither is about what a *good* answer looks
//! like:
//! - A `Select` / `MultiSelect` answer must be a value that was offered.
//! - Keys must be ones that were asked about.
//! The reason is that a select answer goes to the **tool**, not to the model. A tool that asks
//! "which of these three files did you mean" then reads that path; if the answer could be
//! `/etc/shadow`, the author's entirely reasonable assumption — that the answer is one of the
//! options they offered — has been broken through a channel that looked closed. "Let the model
//! judge" does not apply, because the model never sees it.
//! Offending values are **dropped, not rejected**: the tool sees an unanswered field, which it has
//! to handle anyway now that `required` is only a hint. No round trip, no error dialog.
//! # There is deliberately no password control
//! Secrets never come in through a tool prompt. A key belongs in the keyring or an environment
//! variable, referenced by `CredentialRef`; anything typed into a form ends up in an entry, in
//! the timeline, and in a bug report.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{CallId, SessionId, TurnId};

/// Where an approval decision came from. Recorded for the audit trail.
/// Parse failures and unexpected errors always end as "ask the user" — fail closed.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// The deterministic rule layer (a red-line ask, or a safe allow).
    Rule,
    /// An existing grant (session or workspace scope, read live).
    Grant,
    Model,
    DeepModel,
    User,
}

/// How long a granted permission lasts.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    Once,
    /// The rest of **this turn**. In memory only.
    /// Its own scope because the commonest complaint about a permission system is being asked three
    /// times about near-identical commands inside one turn, and "once" genuinely means once.
    Turn,
    Session,
    Workspace,
    /// Never granted automatically; only the user can choose it.
    Global,
}

impl GrantScope {
    /// Whether a grant of this scope has to survive the process.
    /// The dividing line, and the reason it is a method rather than a `match` at each call site:
    /// `Once` / `Turn` are answers about work already in flight, so memory is the right lifetime.
    /// The other three are standing authorizations — a user who chose "for this session" and then
    /// resumed it expects it to still hold.
    pub fn is_durable(self) -> bool {
        matches!(self, Self::Session | Self::Workspace | Self::Global)
    }
}

/// What is being asked.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionBody {
    /// Approval for a tool call. Kept as its own variant rather than expressed as a form: it is
    /// the overwhelmingly common case, it has semantics a form cannot carry (grant scopes,
    /// caveats), and clients give it dedicated affordances.
    Permission {
        tool: String,
        /// Already-trimmed argument preview, for humans. Not the wire args.
        args_preview: String,
        /// Why this is being asked — what the rule layer found.
        reason: String,
        /// Warnings that must show in **every** permission mode.
        /// Typically shell commands that **bypass edit tracking** (`sed -i`, `tee`, `mv`, `cp`):
        /// they leave no edit-ledger record, so `/changes` and `/undo` cannot see them. Such
        /// calls are never auto-approved.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        caveats: Vec<String>,
        /// Scopes offered. `Once` is always available.
        offered_scopes: Vec<GrantScope>,
        /// The rule a durable grant would write, exactly as it will be matched.
        /// **Must be shown next to the durable options.** A grant is a standing authorization, and
        /// the one thing the user has to see before choosing it is what it will match later — not
        /// this call's arguments, which they can already read. Absent when no rule can be derived,
        /// which is also when no durable scope is offered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant_preview: Option<String>,
    },

    /// Environment variables a skill wants passed through.
    /// Judged against the **skill's own declaration**, not a path. Hard limits: removals pass
    /// automatically; a `global` target or `Global` scope is **never** auto-approved; an unknown
    /// skill is never auto-approved.
    EnvPassthrough {
        skill: String,
        vars: Vec<String>,
        offered_scopes: Vec<GrantScope>,
    },

    /// Anything else: a form.
    Form(Form),
}

/// A form to fill in.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Form {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub fields: Vec<FormField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_label: Option<String>,
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<FormField>) -> Self {
        Self {
            title: title.into(),
            message: None,
            fields,
            submit_label: None,
            cancel_label: None,
        }
    }

    pub fn with_message(mut self, m: impl Into<String>) -> Self {
        self.message = Some(m.into());
        self
    }

    /// The single most common form: pick one of a few options.
    pub fn single_choice(
        title: impl Into<String>,
        key: impl Into<String>,
        options: Vec<Choice>,
    ) -> Self {
        Self::new(
            title,
            vec![FormField::required(
                key,
                "",
                Control::Select {
                    options,
                    default: None,
                    free_text: false,
                },
            )],
        )
    }

    /// The other common one: yes or no.
    pub fn confirm(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(
            title,
            vec![FormField::required(
                "confirm",
                "",
                Control::Confirm { default: None },
            )],
        )
        .with_message(message)
    }

    pub fn field(&self, key: &str) -> Option<&FormField> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// Cleans an answer so it can be trusted, without ever rejecting it.
    /// Drops keys the form never asked about, and `Select`/`MultiSelect` values that were not
    /// offered. Everything else passes through untouched — including text that exceeds `max_len`
    /// and numbers outside `min`/`max`, which are hints for the UI and not rules.
    /// Returns the cleaned answer plus a note of anything dropped. The notes exist so a developer
    /// sees a client sending nonsense; they are never shown as a validation failure to the user.
    pub fn sanitize(&self, answer: FormAnswer) -> (FormAnswer, Vec<String>) {
        let mut clean = FormAnswer::new();
        let mut dropped = Vec::new();

        for (key, value) in answer.values {
            let Some(field) = self.field(&key) else {
                dropped.push(format!("{key}: never asked about"));
                continue;
            };
            match field.control.keep(&value) {
                Kept::Yes => {
                    clean.values.insert(key, value);
                }
                Kept::No(why) => dropped.push(format!("{key}: {why}")),
                Kept::Trimmed(v, why) => {
                    dropped.push(format!("{key}: {why}"));
                    clean.values.insert(key, v);
                }
            }
        }

        (clean, dropped)
    }
}

/// What [`Control::keep`] decided about a value.
enum Kept {
    Yes,
    /// Discard it entirely.
    No(String),
    /// Keep a reduced version (a multi-select with unoffered or repeated values removed), with a
    /// note of what changed.
    Trimmed(FieldValue, String),
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    /// The key the answer is stored under.
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default)]
    pub required: bool,
    pub control: Control,
}

impl FormField {
    pub fn new(key: impl Into<String>, label: impl Into<String>, control: Control) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            help: None,
            required: false,
            control,
        }
    }

    pub fn required(key: impl Into<String>, label: impl Into<String>, control: Control) -> Self {
        Self {
            required: true,
            ..Self::new(key, label, control)
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// One selectable option.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    /// Value and label the same — for options that need no prettier name.
    pub fn plain(value: impl Into<String>) -> Self {
        let v = value.into();
        Self {
            value: v.clone(),
            label: v,
            description: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Serde helper: omit `false` booleans from the wire instead of spelling them out.
fn is_false(v: &bool) -> bool {
    !*v
}

/// The form controls.
/// Note what is **not** here: no password or secret field. See the module docs.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum Control {
    /// Single-line text.
    Input {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
    },
    /// Multi-line text.
    Textarea {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// A rendering hint, not a limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
    },
    /// Pick exactly one.
    Select {
        options: Vec<Choice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        /// Let the user type an answer of their own instead of choosing one of `options`.
        /// Off by default: a closed set stays closed unless the form opts in. When on, the UI
        /// adds an "other" escape hatch, and the answer arrives as `Text` (not `Choice`), which
        /// is kept verbatim by [`Control::keep`].
        #[serde(default, skip_serializing_if = "is_false")]
        free_text: bool,
    },
    /// Pick any number.
    MultiSelect {
        options: Vec<Choice>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        defaults: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<usize>,
    },
    /// Yes or no.
    Confirm {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        /// Reject non-integers.
        #[serde(default)]
        integer: bool,
    },
    /// A filesystem path. The client may offer a picker.
    Path {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        /// Directories only.
        #[serde(default)]
        directory: bool,
        /// A hint for the picker; **existence is not validated here** — the filesystem can
        /// change between answering and using the value, so whoever uses it must check anyway.
        #[serde(default)]
        must_exist: bool,
    },
}

impl Control {
    /// The value a client should start with.
    pub fn default_value(&self) -> Option<FieldValue> {
        match self {
            Control::Input { default, .. } | Control::Textarea { default, .. } => {
                default.clone().map(FieldValue::Text)
            }
            Control::Select { default, .. } => default.clone().map(FieldValue::Choice),
            Control::MultiSelect { defaults, .. } => {
                (!defaults.is_empty()).then(|| FieldValue::Choices(defaults.clone()))
            }
            Control::Confirm { default } => default.map(FieldValue::Bool),
            Control::Number { default, .. } => default.map(FieldValue::Number),
            Control::Path { default, .. } => default.clone().map(FieldValue::Text),
        }
    }

    /// Whether a value can be trusted as an answer to this control.
    /// Only two things are checked: the value has the right *shape*, and — for the closed-set
    /// controls — it is one of the offered options. Length, range and whole-numberness are hints
    /// the UI renders; enforcing them here would only produce an error dialog.
    fn keep(&self, v: &FieldValue) -> Kept {
        match (self, v) {
            // Free text passes through as written, `max_len` or not.
            (
                Control::Input { .. } | Control::Textarea { .. } | Control::Path { .. },
                FieldValue::Text(_),
            ) => Kept::Yes,

            // A closed set: the whole point is that the answer is one of these. `free_text` is
            // the explicit opt-out — an "other" answer typed by the user arrives as free text
            // and is kept verbatim, unlike a choice that was never offered.
            (
                Control::Select {
                    free_text: true, ..
                },
                FieldValue::Text(_),
            ) => Kept::Yes,
            (Control::Select { options, .. }, FieldValue::Choice(c)) => {
                if options.iter().any(|o| &o.value == c) {
                    Kept::Yes
                } else {
                    Kept::No(format!("{c:?} was not offered"))
                }
            }
            (Control::MultiSelect { options, .. }, FieldValue::Choices(cs)) => {
                let mut kept: Vec<String> = Vec::new();
                let mut unoffered = 0usize;
                let mut duplicates = 0usize;
                for c in cs {
                    if !options.iter().any(|o| &o.value == c) {
                        unoffered += 1;
                    } else if kept.contains(c) {
                        // A repeated selection is never what anyone meant.
                        duplicates += 1;
                    } else {
                        kept.push(c.clone());
                    }
                }
                // Compare against what came in, not just `unoffered`: a purely duplicated list has
                // nothing unoffered but still needs replacing with the collapsed version.
                if kept.len() == cs.len() {
                    Kept::Yes
                } else {
                    let mut why = Vec::new();
                    if unoffered > 0 {
                        why.push(format!("{unoffered} selection(s) were not offered"));
                    }
                    if duplicates > 0 {
                        why.push(format!("{duplicates} duplicate selection(s)"));
                    }
                    Kept::Trimmed(FieldValue::Choices(kept), why.join("; "))
                }
            }

            (Control::Confirm { .. }, FieldValue::Bool(_)) => Kept::Yes,
            // NaN is not a number a tool can do anything with; that is a shape problem, not a
            // range one.
            (Control::Number { .. }, FieldValue::Number(n)) => {
                if n.is_finite() {
                    Kept::Yes
                } else {
                    Kept::No("not a finite number".into())
                }
            }

            // A value of the wrong shape entirely — a bool where a choice belongs.
            _ => Kept::No("wrong kind of value for this control".into()),
        }
    }
}

/// One answered value.
/// Adjacently tagged (`{"kind": "choice", "value": "prod"}`) rather than internally tagged:
/// serde cannot fold a tag into a newtype variant that wraps a primitive, and a variant per
/// shape is exactly what this type is.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FieldValue {
    Text(String),
    Number(f64),
    Bool(bool),
    /// A `Select` answer.
    Choice(String),
    /// A `MultiSelect` answer.
    Choices(Vec<String>),
}

impl FieldValue {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            FieldValue::Text(s) | FieldValue::Choice(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            FieldValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            FieldValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_choices(&self) -> Option<&[String]> {
        match self {
            FieldValue::Choices(v) => Some(v),
            _ => None,
        }
    }
}

/// A filled-in form.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FormAnswer {
    pub values: BTreeMap<String, FieldValue>,
}

impl FormAnswer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: impl Into<String>, value: FieldValue) -> Self {
        self.values.insert(key.into(), value);
        self
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.as_text()
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.values.get(key)?.as_bool()
    }

    pub fn number(&self, key: &str) -> Option<f64> {
        self.values.get(key)?.as_number()
    }

    pub fn choices(&self, key: &str) -> Option<&[String]> {
        self.values.get(key)?.as_choices()
    }

    /// The answer to a [`Form::single_choice`].
    pub fn single_choice(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new().set(key, FieldValue::Choice(value.into()))
    }
}

/// The answer to an interaction.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionDecision {
    Allow {
        /// Written as a grant, and **live from this round on** — grants are read fresh, not
        /// snapshotted per turn.
        scope: GrantScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<DecisionSource>,
    },
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A completed form.
    Submitted(FormAnswer),
    /// The turn was cancelled, or the process exited while waiting and this was tidied up.
    Cancelled,
}

/// What is being asked, plus enough context to route the answer back.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionRequest {
    /// Stable id shared by persistence, stream events and the control-plane answer route.
    pub interaction_id: String,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    /// Which tool call is asking, when one is. Absent for questions core raises outside a call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<CallId>,
    pub body: InteractionBody,
}

/// The engine's side of asking. Implemented where the UI is reachable.
/// Persistence belongs to core, before and after this call. The implementation only routes the
/// question to a UI and waits for an answer.
#[async_trait]
pub trait InteractionPort: Send + Sync {
    /// Blocks until the user answers or the turn is cancelled.
    /// Time spent here does not count against the turn's execution budget: a user thinking is
    /// not the agent being slow.
    async fn ask(&self, req: InteractionRequest) -> Result<InteractionDecision, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> Form {
        Form::new(
            "Deploy",
            vec![
                FormField::required(
                    "env",
                    "Environment",
                    Control::Select {
                        options: vec![Choice::plain("staging"), Choice::plain("prod")],
                        default: Some("staging".into()),
                        free_text: false,
                    },
                ),
                FormField::new(
                    "notes",
                    "Release notes",
                    Control::Textarea {
                        default: None,
                        placeholder: Some("what changed".into()),
                        rows: Some(4),
                        max_len: Some(20),
                    },
                ),
                FormField::required(
                    "regions",
                    "Regions",
                    Control::MultiSelect {
                        options: vec![
                            Choice::plain("us"),
                            Choice::plain("eu"),
                            Choice::plain("ap"),
                        ],
                        defaults: vec!["us".into()],
                        min: Some(1),
                        max: Some(2),
                    },
                ),
                FormField::new(
                    "replicas",
                    "Replicas",
                    Control::Number {
                        default: Some(2.0),
                        min: Some(1.0),
                        max: Some(10.0),
                        integer: true,
                    },
                ),
            ],
        )
    }

    fn answer() -> FormAnswer {
        FormAnswer::new()
            .set("env", FieldValue::Choice("prod".into()))
            .set(
                "regions",
                FieldValue::Choices(vec!["us".into(), "eu".into()]),
            )
    }

    #[test]
    fn a_normal_answer_passes_through_untouched() {
        let (clean, dropped) = form().sanitize(answer());
        assert_eq!(clean, answer());
        assert!(dropped.is_empty());
    }

    /// Constraints are hints. Nothing here is rejected, because rejecting would only cost a round
    /// trip — the model reads what the user actually wrote.
    #[test]
    fn hints_are_not_enforced() {
        let a = FormAnswer::new()
            // Well over max_len.
            .set("notes", FieldValue::Text("x".repeat(500)))
            // Outside min/max, and not a whole number despite `integer`.
            .set("replicas", FieldValue::Number(99.5));

        let (clean, dropped) = form().sanitize(a.clone());
        assert_eq!(clean, a, "hints must not alter or discard the answer");
        assert!(dropped.is_empty());
    }

    /// `required` is a visual marker, not a gate.
    #[test]
    fn an_empty_answer_is_accepted() {
        let (clean, dropped) = form().sanitize(FormAnswer::new());
        assert!(clean.values.is_empty());
        assert!(dropped.is_empty(), "leaving a field blank is not an error");
    }

    /// The one thing that *is* enforced, and it is not about UX: a select answer goes to a tool,
    /// which reasonably assumes it is one of the options it offered.
    #[test]
    fn a_value_that_was_never_offered_is_dropped() {
        let a = answer().set("env", FieldValue::Choice("root-shell".into()));
        let (clean, dropped) = form().sanitize(a);

        assert!(
            !clean.values.contains_key("env"),
            "the tool must see an unanswered field"
        );
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].contains("env"));
        // The rest of the answer survives — one bad value does not discard the whole form.
        assert_eq!(clean.choices("regions").unwrap().len(), 2);
    }

    #[test]
    fn a_key_the_form_never_asked_about_is_dropped() {
        let a = answer().set("sudo", FieldValue::Bool(true));
        let (clean, dropped) = form().sanitize(a);
        assert!(!clean.values.contains_key("sudo"));
        assert!(dropped.iter().any(|d| d.contains("sudo")));
    }

    /// A multi-select keeps its offered values and loses only the smuggled ones.
    #[test]
    fn a_multiselect_is_trimmed_rather_than_discarded() {
        let a = answer().set(
            "regions",
            FieldValue::Choices(vec!["us".into(), "mars".into(), "eu".into()]),
        );
        let (clean, dropped) = form().sanitize(a);
        assert_eq!(clean.choices("regions").unwrap(), ["us", "eu"]);
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].contains("not offered"), "{dropped:?}");
    }

    /// Over `max` is fine — that is a hint. Duplicates are not, because they are never meant.
    #[test]
    fn too_many_selections_are_kept_but_duplicates_collapse() {
        let over = answer().set(
            "regions",
            FieldValue::Choices(vec!["us".into(), "eu".into(), "ap".into()]),
        );
        let (clean, dropped) = form().sanitize(over);
        assert_eq!(clean.choices("regions").unwrap().len(), 3, "max is a hint");
        assert!(dropped.is_empty());

        let dupes = answer().set(
            "regions",
            FieldValue::Choices(vec!["us".into(), "us".into()]),
        );
        let (clean, dropped) = form().sanitize(dupes);
        assert_eq!(clean.choices("regions").unwrap(), ["us"]);
        assert!(dropped[0].contains("duplicate"), "{dropped:?}");
    }

    /// A value of the wrong shape is a client bug, not a user's choice.
    #[test]
    fn a_value_of_the_wrong_shape_is_dropped() {
        let a = answer().set("env", FieldValue::Bool(true));
        let (clean, dropped) = form().sanitize(a);
        assert!(!clean.values.contains_key("env"));
        assert!(dropped[0].contains("wrong kind"));
    }

    /// A closed select drops free text; a `free_text` select keeps it verbatim — that is the
    /// "other" escape hatch the ask_user UI offers.
    #[test]
    fn free_text_is_an_opt_in_for_selects() {
        let closed = Form::new(
            "Pick",
            vec![FormField::required(
                "env",
                "Environment",
                Control::Select {
                    options: vec![Choice::plain("staging")],
                    default: None,
                    free_text: false,
                },
            )],
        );
        let (clean, dropped) =
            closed.sanitize(FormAnswer::new().set("env", FieldValue::Text("prod".into())));
        assert!(!clean.values.contains_key("env"));
        assert!(dropped[0].contains("wrong kind"), "{dropped:?}");

        let open = Form::new(
            "Pick",
            vec![FormField::required(
                "env",
                "Environment",
                Control::Select {
                    options: vec![Choice::plain("staging")],
                    default: None,
                    free_text: true,
                },
            )],
        );
        let (clean, dropped) =
            open.sanitize(FormAnswer::new().set("env", FieldValue::Text("prod".into())));
        assert_eq!(clean.text("env"), Some("prod"));
        assert!(dropped.is_empty());
        // An unoffered *choice* is still dropped even when free_text is on.
        let (clean, dropped) =
            open.sanitize(FormAnswer::new().set("env", FieldValue::Choice("prod".into())));
        assert!(!clean.values.contains_key("env"));
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn nan_is_not_a_usable_number() {
        let a = answer().set("replicas", FieldValue::Number(f64::NAN));
        let (clean, dropped) = form().sanitize(a);
        assert!(!clean.values.contains_key("replicas"));
        assert!(dropped[0].contains("finite"));
    }

    #[test]
    fn defaults_are_exposed_for_clients_to_prefill() {
        let f = form();
        assert_eq!(
            f.field("env").unwrap().control.default_value(),
            Some(FieldValue::Choice("staging".into()))
        );
        assert_eq!(
            f.field("regions").unwrap().control.default_value(),
            Some(FieldValue::Choices(vec!["us".into()]))
        );
        assert_eq!(f.field("notes").unwrap().control.default_value(), None);
    }

    #[test]
    fn convenience_constructors_produce_usable_forms() {
        let f = Form::single_choice(
            "Which file?",
            "file",
            vec![Choice::new("a", "src/a.rs"), Choice::new("b", "src/b.rs")],
        );
        let (clean, _) = f.sanitize(FormAnswer::single_choice("file", "b"));
        assert_eq!(clean.text("file"), Some("b"));
        // Not offered.
        let (clean, dropped) = f.sanitize(FormAnswer::single_choice("file", "z"));
        assert!(clean.values.is_empty());
        assert_eq!(dropped.len(), 1);

        let c = Form::confirm("Overwrite?", "40 files will change.");
        let (clean, _) = c.sanitize(FormAnswer::new().set("confirm", FieldValue::Bool(true)));
        assert_eq!(clean.bool("confirm"), Some(true));
    }

    /// The absence of a password control is a decision, not an omission.
    #[test]
    fn there_is_no_secret_control() {
        let json = serde_json::to_string(&Control::Input {
            default: None,
            placeholder: None,
            max_len: None,
        })
        .unwrap();
        assert!(!json.contains("secret") && !json.contains("password"));
    }

    #[test]
    fn bodies_and_decisions_round_trip() {
        let bodies = vec![
            InteractionBody::Permission {
                tool: "shell".into(),
                args_preview: "rm -rf build".into(),
                reason: "deletes files".into(),
                caveats: vec!["bypasses edit tracking".into()],
                offered_scopes: vec![GrantScope::Once, GrantScope::Session],
                grant_preview: None,
            },
            InteractionBody::EnvPassthrough {
                skill: "deploy".into(),
                vars: vec!["AWS_PROFILE".into()],
                offered_scopes: vec![GrantScope::Session],
            },
            InteractionBody::Form(form()),
        ];
        for b in bodies {
            let j = serde_json::to_value(&b).unwrap();
            assert_eq!(serde_json::from_value::<InteractionBody>(j).unwrap(), b);
        }

        let decisions = vec![
            InteractionDecision::Allow {
                scope: GrantScope::Session,
                source: Some(DecisionSource::User),
            },
            InteractionDecision::Deny {
                reason: Some("no".into()),
            },
            InteractionDecision::Submitted(answer()),
            InteractionDecision::Cancelled,
        ];
        for d in decisions {
            let j = serde_json::to_value(&d).unwrap();
            assert_eq!(serde_json::from_value::<InteractionDecision>(j).unwrap(), d);
        }
    }

    /// A request from core has no call id; one from a tool does.
    #[test]
    fn requests_may_or_may_not_belong_to_a_tool_call() {
        let from_core = InteractionRequest {
            interaction_id: crate::EntryId::new().to_string(),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            call_id: None,
            body: InteractionBody::Form(Form::confirm("Continue?", "…")),
        };
        let j = serde_json::to_value(&from_core).unwrap();
        assert!(j.get("call_id").is_none(), "absent rather than null");
        assert_eq!(
            serde_json::from_value::<InteractionRequest>(j).unwrap(),
            from_core
        );
    }
}
