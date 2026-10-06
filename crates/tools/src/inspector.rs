//! What the approval gate needs to know about a `computer` call, and who tells it.
//!
//! These two types live in the engine's tool crate rather than with the tool itself, because the
//! gate needs them and the gate runs **before** any tool does. The closed half owns the `computer`
//! tool; the engine owns the question "what is this call about to touch", and the only way to ask
//! it without the engine depending on the tool is a trait the tool implements and the host hands
//! up. See `ProWiring::computer`.

/// What the approval gate needs to know about a call **before** it runs.
///
/// Produced by the tool from the arguments alone — a hit test, never an action — because a prompt
/// that says "click at (640, 360)" is not a question anyone can answer, and one that says
/// `Click "Transfer" (Button) in "Transfer — Bank of Example"` is. The whole difference is this
/// one call, made before the decision rather than after it.
#[derive(Debug, Clone, Default)]
pub struct ComputerFacts {
    /// The action name, as the policy rules see it.
    pub action: String,
    /// What the action is about to touch, already phrased for a human.
    pub target: Option<String>,
    /// The window it would land in.
    pub window: Option<String>,
    /// The process that owns it.
    pub process: Option<String>,
    /// A password field, so the prompt can say so instead of quoting the characters.
    pub password: bool,
    /// Characters about to be sent, for a prompt that should state the size of the thing.
    pub char_count: Option<usize>,
    /// True when nothing could be identified — a canvas, or a tree the platform would not return.
    /// The gate is expected to be stricter when this is set, because the prompt is about to be
    /// vague and the user should know that before they answer it.
    pub unidentified: bool,
    /// Something the caller could not parse, so the gate never has to guess at a half-read call.
    pub malformed: Option<String>,
}

/// What the gate needs, from the arguments alone.
///
/// A hit test, never an action. This runs on the approval path, so it is the one place in the tool
/// that must be cheap: one `ElementFromPoint`, or a bounded walk of a `ref`'s path.
pub trait ComputerInspector: Send + Sync {
    fn facts(&self, args: &str) -> ComputerFacts;
}
