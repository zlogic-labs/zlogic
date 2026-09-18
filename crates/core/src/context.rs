//! Turning stored entries into the messages one LLM request carries.
//! This is where the shapes that cause provider 400s are prevented, so it is a **pure function**
//! over already-loaded entries: no database, no clock, no network. Everything it decides can be
//! tested by handing it a list.
//! Five steps, in this order:
//! 1. **Apply summaries.** A turn covered by a compaction entry is replaced by that summary, in the
//!    position the covered turns occupied. The entries themselves are untouched — see [`crate::compact`].
//! 2. **Group by round.** Reasoning, text and the tool-call group from one LLM response become one
//!    assistant message; a round's tool results become one `role: tool` message after it.
//! 3. **Tail completeness.** A tool-call group missing any of its results is **repaired**: the
//!    unanswered calls get a synthetic error result, so the model learns they never completed
//!    instead of re-issuing them forever. See below.
//! 4. **Raw gate.** `gate_message_for_target` decides whether provider-native payloads may be
//!    replayed. One implementation, in `protocol`.
//! 5. **Drop empties.** A message left with no content is not sent; some providers reject it and
//!    none of them need it.
//! # Why an incomplete tool-call group is repaired, never silently dropped
//! A crash between "the model asked for three tools" and "all three answered" leaves a group whose
//! results are partial. Replaying it as-is is a hard error on every provider — Anthropic requires
//! `tool_result` blocks in 1:1 correspondence with `tool_use`, and the OpenAI family requires every
//! `tool_call_id` answered before the next assistant message.
//! The group is therefore never replayed half-answered. Instead of **dropping** it (which made the
//! model believe it had never asked, and re-issue the same calls forever — the observed loop), each
//! unanswered call gets a synthetic `is_error` result that says so. The model sees exactly which
//! calls failed and why, and can decide to re-issue them or not.
//! Calls whose arguments were not valid JSON are the one exception and are **filtered** (call and
//! stale result together): they never ran, their result only ever said "you wrote bad JSON", and
//! replaying a malformed call risks a 400 on providers that re-validate args against the schema.
//! The precheck failure already reached the model live.
//! The text is kept: the user already saw it, and an assistant message followed by a user message
//! is a shape every provider accepts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use zlogic_objects::{ObjectId, ObjectStore};
use zlogic_protocol::message::{
    ContentPart, ImagePart, ImageSource, Message, Role, Source, TextPart, ToolResultPart,
    gate_message_for_target,
};
use zlogic_protocol::{MessagePart, RoundId, llm::SystemPart};
use zlogic_store::{EntryKind, EntryRecord, EntryStore};

use crate::compact::{Summary, effective_summaries};
use crate::{CoreError, Result, entry_data};

/// What went into a request, and what was left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreparedContext {
    pub messages: Vec<Message>,
    /// How many turns a summary stood in for.
    pub summarized_turns: usize,
}

/// Reads an entry's payload, from the row or the object store.
/// Taken as a closure so [`build_context`] stays pure over its inputs — tests hand it entries and
/// never touch a store.
pub type LoadData<'a> = &'a dyn Fn(&EntryRecord) -> Result<serde_json::Value>;
/// An object opened for context projection. Its size is known before any payload allocation.
pub struct ContextObject {
    pub reader: Box<dyn Read + Send>,
    pub bytes: u64,
}

/// Opens a content-addressed object for MIME-aware projection.
pub type LoadObject<'a> = &'a dyn Fn(&str) -> Result<ContextObject>;

/// Convenience loader over a real store.
pub fn store_loader<'a>(
    entries: &'a EntryStore<'a>,
    objects: &'a dyn ObjectStore,
) -> impl Fn(&EntryRecord) -> Result<serde_json::Value> + 'a {
    move |rec| entries.load_data(rec, objects).map_err(CoreError::from)
}

/// Object loader paired with [`store_loader`]. Object ids stay strings in protocol, so parsing is
/// confined to this object-store boundary.
pub fn object_loader(objects: &dyn ObjectStore) -> impl Fn(&str) -> Result<ContextObject> + '_ {
    move |raw| {
        let id: ObjectId = raw.parse().map_err(CoreError::from)?;
        let bytes = objects.size(&id).map_err(CoreError::from)?;
        let reader = objects.open(&id).map_err(CoreError::from)?;
        Ok(ContextObject { reader, bytes })
    }
}

/// Builds the message list for a request.
/// `entries` must already exclude the kinds that never reach a model — use
/// `EntryStore::list_for_context`, which applies `EntryKind::goes_to_model`.
pub fn build_context(
    entries: &[EntryRecord],
    target: &Source,
    load: LoadData<'_>,
    load_object: LoadObject<'_>,
) -> Result<PreparedContext> {
    let summaries = effective_summaries(entries, load)?;
    let (groups, covered) = group_by_round(entries, &summaries, load, load_object)?;
    let mut out = PreparedContext {
        summarized_turns: covered.len(),
        ..Default::default()
    };

    for group in groups {
        match group {
            Group::User(parts) => {
                let mut m = Message::user(parts);
                strip_empty(&mut m);
                if !m.content.is_empty() {
                    push_prepared_user(&mut out.messages, m.content);
                }
            }
            Group::Response {
                source,
                mut parts,
                mut results,
                ..
            } => {
                // Step 2: every tool call must have a result, or the group cannot be replayed
                // (1:1 pairing is a hard error on every provider). **Never silently drop the
                // group** — the model asked for tools and must learn what happened, or it will
                // re-issue the same calls forever, which is exactly the observed loop. Missing
                // results are repaired with a synthetic error result so the model sees "this
                // call never got an answer".
                for call_id in missing_call_ids(&parts, &results) {
                    results.push(ContentPart::ToolResult(ToolResultPart {
                        call_id: call_id.clone(),
                        name: String::new(),
                        content: format!(
                            "No result was recorded for tool call `{call_id}` — the call \
                             did not complete (interrupted or crashed). If you still need \
                             this, issue it again."
                        ),
                        files: Vec::new(),
                        is_error: true,
                    }));
                }
                // Step 2b: calls whose arguments were not valid JSON never ran. Their stale
                // "you wrote bad JSON" result is dropped together with the call — replaying a
                // malformed call to the model risks a 400 on providers that validate the args
                // against the tool schema on replay. The model is told the group changed by the
                // missing call, and precheck failures already came back to it in the live round.
                filter_malformed_tool_calls(&mut parts, &mut results);
                // If the filter emptied the group, the reasoning that led to it goes too — a round
                // with no surviving tool calls is an ordinary Q&A round (same rule as an
                // incomplete group before the repair below).
                if tool_calls_emptied(&parts) {
                    parts.retain(|p| {
                        !matches!(p, ContentPart::ToolCall(_) | ContentPart::Reasoning(_))
                    });
                    results.clear();
                }

                let mut assistant = Message {
                    role: Role::Assistant,
                    source: Some(source.clone()),
                    content: parts,
                };

                // Step 3: the single raw gate.
                gate_message_for_target(&mut assistant, target);

                strip_empty(&mut assistant);
                if !assistant.content.is_empty() {
                    out.messages.push(assistant);
                }

                // Results always travel with the group once every call has one. A group whose
                // calls were all malformed may have been emptied of calls — then there is nothing
                // to pair, and dropping the empty tool message is correct (the model sees the
                // assistant text, if any).
                if !results.is_empty() {
                    let projected_files = project_tool_files(&results, load_object);
                    out.messages.push(Message::tool(results));
                    if !projected_files.is_empty() {
                        push_prepared_user(&mut out.messages, projected_files);
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Appends user-role parts, merging into the trailing user message when there is one.
/// `pub(crate)` because the summary call that rides the conversation's own prefix appends its
/// instruction through **this** function: the instruction has to look exactly like a user turn of
/// this conversation, including the rule that two user messages are never adjacent.
pub(crate) fn push_prepared_user(messages: &mut Vec<Message>, parts: Vec<ContentPart>) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(parts),
        _ => messages.push(Message::user(parts)),
    }
}

const TOOL_FILE_TEXT_LIMIT: u64 = 32 * 1024;
const TOOL_FILE_TEXT_TOTAL_LIMIT: u64 = 64 * 1024;
/// Provider limits differ, so keep the derived base64 representation under a conservative ceiling.
/// The original object is never rewritten; this applies only to the request-local representation.
const IMAGE_BASE64_LIMIT: usize = 10 * 1024 * 1024;
const IMAGE_RAW_LIMIT: usize = IMAGE_BASE64_LIMIT / 4 * 3;
/// Compression still has to read the source. Refuse absurd inputs rather than moving the OOM from
/// base64 encoding into the decoder.
const IMAGE_SOURCE_LIMIT: u64 = 128 * 1024 * 1024;
const IMAGE_PIXEL_LIMIT: u64 = 100_000_000;
const VISUAL_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

fn project_user_file(part: MessagePart) -> Result<Vec<ContentPart>> {
    let MessagePart::File { path } = part else {
        return Err(CoreError::Invalid(
            "project_user_file received a non-file part".into(),
        ));
    };
    let mut parts = vec![text_part(format!(
        "<file path=\"{}\" />",
        escape_attr(&path)
    ))];
    let Some(mime) = visual_mime_for_path(Path::new(&path)) else {
        return Ok(parts);
    };
    let object = match File::open(&path).and_then(|file| {
        let bytes = file.metadata()?.len();
        Ok(ContextObject {
            reader: Box::new(file),
            bytes,
        })
    }) {
        Ok(object) => object,
        Err(error) => {
            parts.push(text_part(format!(
                "[attached image unavailable at {}: {error}]",
                escape_attr(&path)
            )));
            return Ok(parts);
        }
    };
    parts.push(project_image(object, mime).unwrap_or_else(text_part));
    Ok(parts)
}

fn visual_mime_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Projects durable tool-file metadata into the next model request.
/// Tool results themselves remain valid protocol tool results. Files follow immediately in one
/// user-role message, after the complete result group: OpenAI-compatible APIs can then use their
/// normal user image blocks, while Anthropic/Gemini/Bedrock merge the adjacent blocks into the
/// same user turn as their tool-result representation.
fn project_tool_files(results: &[ContentPart], load_object: LoadObject<'_>) -> Vec<ContentPart> {
    let mut out = Vec::new();
    let mut text_budget = TOOL_FILE_TEXT_TOTAL_LIMIT;

    for result in results {
        let ContentPart::ToolResult(result) = result else {
            continue;
        };
        for file in &result.files {
            let mime = zlogic_tools::normalize_mime(&file.mime_type);
            out.push(text_part(format!(
                "<tool_file call=\"{}\" name=\"{}\" mime=\"{}\" bytes=\"{}\">",
                escape_attr(&result.call_id),
                escape_attr(&file.name),
                escape_attr(&mime),
                file.bytes
            )));

            if VISUAL_MIMES.contains(&mime.as_str()) {
                match load_object(&file.object_id) {
                    Ok(object) => out.push(project_image(object, &mime).unwrap_or_else(text_part)),
                    Err(error) => {
                        tracing::warn!(
                            target: "zlogic::core",
                            object = %file.object_id,
                            "tool file could not be loaded for context: {error}"
                        );
                        out.push(text_part(
                            "[file unavailable: stored object could not be read]",
                        ));
                    }
                }
            } else if is_text_mime(&mime) && text_budget > 0 {
                match load_object(&file.object_id) {
                    Ok(object) if object.bytes <= TOOL_FILE_TEXT_LIMIT.min(text_budget) => {
                        match read_all(object).and_then(|bytes| {
                            String::from_utf8(bytes).map_err(|error| {
                                CoreError::Invalid(format!("tool text file is not UTF-8: {error}"))
                            })
                        }) {
                            Ok(text) => {
                                text_budget = text_budget.saturating_sub(text.len() as u64);
                                out.push(text_part(neutralize_file_fence(&text)));
                            }
                            Err(_) => out.push(text_part(
                                "[file persisted, but its declared text MIME is not valid UTF-8]",
                            )),
                        }
                    }
                    Ok(_) => out.push(text_part(
                        "[text file persisted; full content omitted because it exceeds the context \
                         file limit—no partial content was substituted]",
                    )),
                    Err(error) => {
                        tracing::warn!(
                            target: "zlogic::core",
                            object = %file.object_id,
                            "tool file could not be loaded for context: {error}"
                        );
                        out.push(text_part(
                            "[file unavailable: stored object could not be read]",
                        ));
                    }
                }
            } else if is_text_mime(&mime) {
                out.push(text_part(
                    "[file persisted; text omitted because the tool-file context budget is full]",
                ));
            } else {
                out.push(text_part(
                    "[binary file persisted; this MIME is not directly representable in model context]",
                ));
            }
            out.push(text_part("</tool_file>"));
        }
    }
    out
}

fn read_all(mut object: ContextObject) -> Result<Vec<u8>> {
    let capacity = usize::try_from(object.bytes)
        .map_err(|_| CoreError::Invalid("object is too large for this platform".into()))?;
    let mut bytes = Vec::with_capacity(capacity);
    object
        .reader
        .read_to_end(&mut bytes)
        .map_err(|error| CoreError::Invalid(format!("could not read stored object: {error}")))?;
    Ok(bytes)
}

/// Produces a validated, provider-sized image without ever changing the durable object.
/// Small images retain their original bytes and MIME. Oversized images are resized proportionally
/// (never cropped) and JPEG-encoded until their base64 representation fits the request ceiling.
fn project_image(
    object: ContextObject,
    declared_mime: &str,
) -> std::result::Result<ContentPart, String> {
    if object.bytes == 0 {
        return Err("[image unavailable: the stored file is empty]".into());
    }
    if object.bytes > IMAGE_SOURCE_LIMIT {
        return Err(format!(
            "[image persisted but omitted: {} bytes exceeds the image processing limit]",
            object.bytes
        ));
    }

    let bytes = read_all(object).map_err(|error| {
        format!("[image unavailable: stored object could not be read: {error}]")
    })?;
    let format = image::guess_format(&bytes)
        .map_err(|_| "[image omitted: stored bytes are not a recognized image]".to_string())?;
    let actual_mime = mime_for_image_format(format)
        .ok_or_else(|| "[image omitted: decoded format is not supported]".to_string())?;
    if actual_mime != declared_mime {
        return Err(format!(
            "[image omitted: declared MIME {declared_mime} does not match stored {actual_mime} data]"
        ));
    }

    let (width, height) = image::ImageReader::with_format(Cursor::new(&bytes), format)
        .into_dimensions()
        .map_err(|_| "[image omitted: image header is corrupt]".to_string())?;
    if u64::from(width) * u64::from(height) > IMAGE_PIXEL_LIMIT {
        return Err(format!(
            "[image persisted but omitted: {width}×{height} pixels exceeds the decoder limit]"
        ));
    }

    if base64_len(bytes.len()) <= IMAGE_BASE64_LIMIT {
        return Ok(ContentPart::Image(ImagePart {
            mime_type: declared_mime.to_string(),
            source: ImageSource::Base64 {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        }));
    }

    let image = image::load_from_memory_with_format(&bytes, format)
        .map_err(|_| "[image omitted: image data is corrupt]".to_string())?;
    let compressed = compress_image(image).ok_or_else(|| {
        "[image persisted but omitted: a request-sized representation could not be produced]"
            .to_string()
    })?;
    Ok(ContentPart::Image(ImagePart {
        mime_type: "image/jpeg".into(),
        source: ImageSource::Base64 {
            data: base64::engine::general_purpose::STANDARD.encode(compressed),
        },
    }))
}

fn mime_for_image_format(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

fn base64_len(bytes: usize) -> usize {
    bytes.saturating_add(2) / 3 * 4
}

fn compress_image(mut image: DynamicImage) -> Option<Vec<u8>> {
    for _ in 0..12 {
        for quality in [85, 72, 60, 48] {
            let mut encoded = Vec::new();
            if JpegEncoder::new_with_quality(&mut encoded, quality)
                .encode_image(&image)
                .is_ok()
                && encoded.len() <= IMAGE_RAW_LIMIT
            {
                return Some(encoded);
            }
        }
        let (width, height) = image.dimensions();
        if width <= 64 || height <= 64 {
            break;
        }
        image = image.resize(
            (width * 4 / 5).max(1),
            (height * 4 / 5).max(1),
            FilterType::Triangle,
        );
    }
    None
}

fn text_part(text: impl Into<String>) -> ContentPart {
    ContentPart::Text(TextPart {
        text: text.into(),
        raw: None,
        truncated: false,
    })
}

fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/ld+json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/javascript"
                | "application/sql"
                | "image/svg+xml"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn neutralize_file_fence(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.to_ascii_lowercase().contains("</tool_file") {
            out.push_str(&line.replace('<', "&lt;"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Assembles the system prompt parts for a request.
/// `cache` is set on the first part only: Anthropic bills a cache write, so marking every part
/// would pay for several breakpoints where one covers the whole prefix.
pub fn build_system(sections: Vec<String>) -> Vec<SystemPart> {
    sections
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .enumerate()
        .map(|(i, text)| SystemPart {
            text,
            cache: i == 0,
        })
        .collect()
}

enum Group {
    User(Vec<ContentPart>),
    Response {
        source: Source,
        /// Reasoning / text / tool-call, in seq order.
        parts: Vec<ContentPart>,
        /// This round's tool results.
        results: Vec<ContentPart>,
    },
}

/// Collapses entries into user messages and per-round responses, preserving order.
/// Also returns the turn numbers a summary stood in for.
fn group_by_round(
    entries: &[EntryRecord],
    summaries: &[Summary],
    load: LoadData<'_>,
    load_object: LoadObject<'_>,
) -> Result<(Vec<Group>, BTreeSet<i64>)> {
    let mut out: Vec<Group> = Vec::new();
    // Which `out` index holds each round, so a result arriving after other entries still lands on
    // its own response rather than opening a new group.
    let mut index_of_round: BTreeMap<RoundId, usize> = BTreeMap::new();
    let mut covered: BTreeSet<i64> = BTreeSet::new();
    let mut emitted: BTreeSet<usize> = BTreeSet::new();
    let mut latest_skill_loads = BTreeMap::<String, usize>::new();
    for (index, rec) in entries
        .iter()
        .enumerate()
        .filter(|(_, rec)| matches!(rec.kind, EntryKind::SkillLoad | EntryKind::SkillUnload))
    {
        let data = load(rec)?;
        let part = serde_json::from_value::<MessagePart>(data).map_err(|error| {
            CoreError::Corrupt(format!(
                "skill state entry {} has unreadable data: {error}",
                rec.entry_id
            ))
        })?;
        match part {
            MessagePart::SkillLoad { name, .. } | MessagePart::SkillUnload { name } => {
                latest_skill_loads.insert(name, index);
            }
            _ => {
                return Err(CoreError::Corrupt(format!(
                    "skill state entry {} does not contain skill state data",
                    rec.entry_id
                )));
            }
        }
    }
    // Skill loads and unload tombstones are session state, not historical prose. Project the
    // current transition for every name as a stable name-sorted prefix so compaction cannot alter
    // it and superseded transitions never enter the request.
    let mut state_parts = Vec::with_capacity(latest_skill_loads.len());
    for index in latest_skill_loads.into_values() {
        let rec = &entries[index];
        let data = load(rec)?;
        state_parts.push(if rec.kind == EntryKind::SkillLoad {
            project_skill_load(rec, &data, load_object)?
        } else {
            entry_data::from_entry(rec, &data)?
        });
    }
    if !state_parts.is_empty() {
        out.push(Group::User(state_parts));
    }

    for rec in entries {
        // A compaction entry is not conversation content; its effect was computed already.
        if rec.kind == EntryKind::Compaction {
            continue;
        }
        if matches!(rec.kind, EntryKind::SkillLoad | EntryKind::SkillUnload) {
            continue;
        }

        // Step 1: covered turns are replaced. The summary is emitted at the position of the first
        // entry it replaces, so it lands where those turns were rather than being assumed to
        // belong at the front.
        if let Some(i) = summaries.iter().position(|s| s.covers(rec.turn_seq)) {
            covered.insert(rec.turn_seq);
            if emitted.insert(i) {
                push_user(&mut out, summaries[i].to_message().content);
            }
            continue;
        }

        let data = load(rec)?;
        let file_parts = if matches!(rec.kind, EntryKind::User | EntryKind::Steering)
            && data.get("type").and_then(serde_json::Value::as_str) == Some("file")
        {
            let part: MessagePart = serde_json::from_value(data.clone()).map_err(|error| {
                CoreError::Corrupt(format!(
                    "input entry {} has unreadable file data: {error}",
                    rec.entry_id
                ))
            })?;
            Some(project_user_file(part)?)
        } else {
            None
        };
        let part = if file_parts.is_none() {
            Some(entry_data::from_entry(rec, &data)?)
        } else {
            None
        };

        match rec.kind {
            // A summary is itself a user message, so an injected message right after one merges
            // into it — which is what keeps two consecutive user messages off the wire.
            EntryKind::User | EntryKind::Steering | EntryKind::TaskUpdate => push_user(
                &mut out,
                file_parts.unwrap_or_else(|| vec![part.expect("ordinary user part")]),
            ),

            EntryKind::SkillLoad | EntryKind::SkillUnload => {
                unreachable!("skill state was projected above")
            }

            EntryKind::Thinking | EntryKind::AssistantText | EntryKind::ToolCall => {
                let round = rec.round_id.ok_or_else(|| {
                    CoreError::Corrupt(format!(
                        "assistant entry {} has no round id; it cannot be grouped",
                        rec.entry_id
                    ))
                })?;
                let source = rec.source.clone().ok_or_else(|| {
                    // Without a source the raw gate cannot decide, and guessing is how a
                    // signature from the wrong model reaches the wire.
                    CoreError::Corrupt(format!(
                        "assistant entry {} has no source stamp",
                        rec.entry_id
                    ))
                })?;

                match index_of_round.get(&round) {
                    Some(&i) => match &mut out[i] {
                        Group::Response { parts, .. } => {
                            parts.push(part.expect("assistant entries are not attachments"))
                        }
                        Group::User(_) => unreachable!("a round index never points at user input"),
                    },
                    None => {
                        index_of_round.insert(round, out.len());
                        out.push(Group::Response {
                            source,
                            parts: vec![part.expect("assistant entries are not attachments")],
                            results: Vec::new(),
                        });
                    }
                }
            }

            EntryKind::ToolResult => {
                let round = rec.round_id.ok_or_else(|| {
                    CoreError::Corrupt(format!(
                        "tool result {} has no round id; it cannot be matched to its calls",
                        rec.entry_id
                    ))
                })?;
                let Some(&i) = index_of_round.get(&round) else {
                    // A result whose call is gone. Dropping it is right: sending a result for a
                    // call the model never sees is itself a 400.
                    tracing::warn!(
                        target: "zlogic::core",
                        entry = %rec.entry_id,
                        "tool result with no surviving call; dropped"
                    );
                    continue;
                };
                match &mut out[i] {
                    Group::Response { results, .. } => {
                        results.push(part.expect("tool results are not attachments"))
                    }
                    Group::User(_) => unreachable!("a round index never points at user input"),
                }
            }

            // `list_for_context` should have removed these already; being defensive costs nothing.
            EntryKind::InteractionRequest
            | EntryKind::InteractionResponse
            | EntryKind::Event
            | EntryKind::ToolLoad
            | EntryKind::Compaction => {}
        }
    }

    // The optimized store query may omit a fully compacted prefix. Its summary rows remain, but
    // none of the covered rows are present to provide the old insertion point. Any summary not
    // emitted above therefore belongs at the front, in the same oldest-first order.
    for (index, summary) in summaries.iter().enumerate().rev() {
        if !emitted.contains(&index) {
            covered.extend(summary.from_turn..=summary.to_turn);
            out.insert(0, Group::User(summary.to_message().content));
        }
    }

    Ok((out, covered))
}

fn project_skill_load(
    rec: &EntryRecord,
    data: &serde_json::Value,
    load_object: LoadObject<'_>,
) -> Result<ContentPart> {
    let MessagePart::SkillLoad {
        name,
        revision,
        body_object,
        path,
        loaded_by,
        unsupported,
    } = serde_json::from_value::<MessagePart>(data.clone()).map_err(|error| {
        CoreError::Corrupt(format!(
            "skill load entry {} has unreadable data: {error}",
            rec.entry_id
        ))
    })?
    else {
        return Err(CoreError::Corrupt(format!(
            "skill load entry {} does not contain a skill_load part",
            rec.entry_id
        )));
    };
    let object_id: ObjectId = body_object.parse().map_err(CoreError::from)?;
    if !rec.objects.iter().any(|reference| {
        reference.role == zlogic_objects::ObjectRole::Skill && reference.object_id == object_id
    }) {
        return Err(CoreError::Corrupt(format!(
            "skill load entry {} does not retain its body object",
            rec.entry_id
        )));
    }
    let raw_body = String::from_utf8(read_all(load_object(&body_object)?)?).map_err(|error| {
        CoreError::Corrupt(format!(
            "skill load entry {} has a non-UTF-8 body: {error}",
            rec.entry_id
        ))
    })?;
    let skill_dir = Path::new(&path)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default();
    let body = zlogic_tools::session::skill::substitute_context(
        &raw_body,
        &skill_dir,
        &rec.session_id.to_string(),
    );
    let body = neutralize_skill_fence(&body);
    Ok(entry_data::skill_load_part(
        &name,
        &revision,
        &body,
        &path,
        loaded_by,
        &unsupported,
    ))
}

fn neutralize_skill_fence(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.to_ascii_lowercase().contains("</skill_load") {
            out.push_str(&line.replace('<', "&lt;"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Appends user parts, merging into the previous user group.
/// Two submissions in a row are one user turn as far as the wire is concerned, and several providers
/// reject consecutive user messages outright.
fn push_user(out: &mut Vec<Group>, parts: Vec<ContentPart>) {
    match out.last_mut() {
        Some(Group::User(existing)) => existing.extend(parts),
        _ => out.push(Group::User(parts)),
    }
}

/// The call ids in the response's tool-call group that have no result.
fn missing_call_ids(parts: &[ContentPart], results: &[ContentPart]) -> Vec<String> {
    let Some(group) = parts.iter().find_map(|p| match p {
        ContentPart::ToolCall(g) => Some(g),
        _ => None,
    }) else {
        return Vec::new();
    };
    let answered: std::collections::HashSet<&str> = results
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.call_id.as_str()),
            _ => None,
        })
        .collect();
    group
        .calls
        .iter()
        .filter(|c| !answered.contains(c.id.as_str()))
        .map(|c| c.id.clone())
        .collect()
}

/// Whether the response's tool-call group is present but has no surviving calls.
fn tool_calls_emptied(parts: &[ContentPart]) -> bool {
    parts.iter().any(|p| match p {
        ContentPart::ToolCall(g) => g.calls.is_empty(),
        _ => false,
    })
}

/// Drops tool calls whose `args` are not valid JSON, together with their results.
/// The pairing invariant this preserves is the same one `missing_results` guards: every call that
/// survives must still have its result, and every result that survives must still have its call —
/// both halves of a broken pairing are a hard error on every provider.
/// A call with unparseable arguments never ran (core's precheck rejects it before execution), so
/// its "error" result only ever said "you wrote bad JSON". Replaying that pair is risky: some
/// providers re-validate the args against the tool schema on replay, and a malformed call is a
/// 400. The model already saw the precheck failure live; the stale pair is dropped.
fn filter_malformed_tool_calls(parts: &mut Vec<ContentPart>, results: &mut Vec<ContentPart>) {
    let Some(group) = parts.iter_mut().find_map(|p| match p {
        ContentPart::ToolCall(g) => Some(g),
        _ => None,
    }) else {
        return;
    };
    let before = group.calls.len();
    group
        .calls
        .retain(|call| serde_json::from_str::<serde_json::Value>(&call.args).is_ok());
    if group.calls.len() == before {
        return;
    }
    let kept: std::collections::HashSet<&str> = group.calls.iter().map(|c| c.id.as_str()).collect();
    results.retain(|p| match p {
        ContentPart::ToolResult(r) => kept.contains(r.call_id.as_str()),
        _ => true,
    });
}

/// Removes parts with nothing in them.
/// An empty text part costs tokens for nothing and some providers reject a message with empty
/// content outright. A reasoning part with no text but a raw payload is **kept** — that is exactly
/// `redacted_thinking`, which has no plaintext and must still be replayed.
fn strip_empty(m: &mut Message) {
    m.content.retain(|p| match p {
        ContentPart::Text(t) => !t.text.is_empty(),
        ContentPart::Reasoning(r) => !r.text.is_empty() || r.raw.is_some(),
        ContentPart::ToolCall(g) => !g.calls.is_empty(),
        _ => true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zlogic_protocol::message::{
        ReasoningPart, TextPart, ToolCall, ToolCallPart, ToolResultPart,
    };
    use zlogic_protocol::{RoundId, SessionId, TurnId, WorkspaceId};
    use zlogic_store::{Db, NewEntry, NewSession};

    /// Builds real entries through the store, so the tests exercise the same read path production
    /// does — including the split between `data` and the `native` column.
    struct Fixture {
        db: Db,
        objects: zlogic_objects::MemoryObjectStore,
        session: SessionId,
        turn: TurnId,
        turn_seq: i64,
    }

    impl Fixture {
        fn new() -> Self {
            let db = Db::open_in_memory().unwrap();
            let s = db
                .sessions()
                .create(NewSession::root(WorkspaceId::new()))
                .unwrap();
            Self {
                db,
                objects: zlogic_objects::MemoryObjectStore::new(),
                session: s.session_id,
                turn: TurnId::new(),
                turn_seq: 1,
            }
        }

        fn user(&self, text: &str) {
            let part = ContentPart::Text(TextPart {
                text: text.into(),
                raw: None,
                truncated: false,
            });
            let e = entry_data::to_entry(
                self.session,
                self.turn,
                self.turn_seq,
                None,
                &entry_data::Author::User,
                part,
            )
            .unwrap();
            self.db.entries().append(e).unwrap();
        }

        fn file(&self, path: &str) {
            let e = entry_data::input_entry(
                self.session,
                self.turn,
                self.turn_seq,
                &entry_data::Author::User,
                zlogic_protocol::MessagePart::File { path: path.into() },
            )
            .unwrap();
            self.db.entries().append(e).unwrap();
        }

        fn response(&self, round: RoundId, source: &Source, parts: Vec<ContentPart>) {
            for p in parts {
                let e = entry_data::to_entry(
                    self.session,
                    self.turn,
                    self.turn_seq,
                    Some(round),
                    &entry_data::Author::Model(source.clone()),
                    p,
                )
                .unwrap();
                self.db.entries().append(e).unwrap();
            }
        }

        fn result(&self, round: RoundId, call_id: &str) {
            let part = ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: call_id.into(),
                name: "t".into(),
                content: format!("result of {call_id}"),
                is_error: false,
            });
            let e = entry_data::to_entry(
                self.session,
                self.turn,
                self.turn_seq,
                Some(round),
                &entry_data::Author::Tool,
                part,
            )
            .unwrap();
            self.db.entries().append(e).unwrap();
        }

        fn result_with_file(
            &self,
            round: RoundId,
            call_id: &str,
            name: &str,
            mime_type: &str,
            bytes: &[u8],
        ) {
            let object_id = self.objects.put(bytes).unwrap();
            let part = ContentPart::ToolResult(ToolResultPart {
                call_id: call_id.into(),
                name: "t".into(),
                content: "produced a file".into(),
                files: vec![zlogic_protocol::message::ToolResultFile {
                    name: name.into(),
                    mime_type: mime_type.into(),
                    object_id: object_id.to_string(),
                    bytes: bytes.len() as u64,
                }],
                is_error: false,
            });
            let e = entry_data::to_entry(
                self.session,
                self.turn,
                self.turn_seq,
                Some(round),
                &entry_data::Author::Tool,
                part,
            )
            .unwrap()
            .references(zlogic_objects::ObjectRef::keyed(
                object_id,
                zlogic_objects::ObjectRole::Output,
                name,
            ));
            self.db.entries().append(e).unwrap();
        }

        fn build(&self, target: &Source) -> PreparedContext {
            let entries = self.db.entries();
            let loader = store_loader(&entries, &self.objects);
            let object_loader = object_loader(&self.objects);
            let list = entries.list_for_context(self.session).unwrap();
            build_context(&list, target, &loader, &object_loader).unwrap()
        }
    }

    fn reasoning(text: &str, raw: Option<serde_json::Value>) -> ContentPart {
        ContentPart::Reasoning(ReasoningPart {
            text: text.into(),
            raw,
            truncated: false,
        })
    }

    fn text(t: &str) -> ContentPart {
        ContentPart::Text(TextPart {
            text: t.into(),
            raw: None,
            truncated: false,
        })
    }

    fn collect_text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn png() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(2, 2)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn calls(ids: &[&str]) -> ContentPart {
        calls_with_args(ids, "{\"a\": 1}")
    }

    /// A tool-call group where the given calls carry the given (raw) args.
    /// `ids` and `args` must line up; `args` is the raw string stored on each call, so passing
    /// invalid JSON exercises the malformed-args path in `filter_malformed_tool_calls`.
    fn calls_with_args(ids: &[&str], args: &str) -> ContentPart {
        ContentPart::ToolCall(ToolCallPart {
            calls: ids
                .iter()
                .map(|id| ToolCall {
                    id: (*id).into(),
                    name: "t".into(),
                    args: args.into(),
                    raw: None,
                })
                .collect(),
        })
    }

    fn model() -> Source {
        Source::new("anthropic", "claude-opus-5")
    }

    #[test]
    fn a_plain_exchange_becomes_user_then_assistant() {
        let f = Fixture::new();
        f.user("hello");
        f.response(RoundId::new(), &model(), vec![text("hi there")]);

        let ctx = f.build(&model());
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].role, Role::User);
        assert_eq!(ctx.messages[1].role, Role::Assistant);
    }

    #[test]
    fn a_stored_file_is_projected_immediately_before_the_model_request() {
        let f = Fixture::new();
        f.user("inspect ");
        f.file("/tmp/report.csv");

        let ctx = f.build(&model());
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, Role::User);
        assert_eq!(
            ctx.messages[0].content,
            vec![
                ContentPart::Text(TextPart {
                    text: "inspect ".into(),
                    raw: None,
                    truncated: false,
                }),
                ContentPart::Text(TextPart {
                    text: "<file path=\"/tmp/report.csv\" />".into(),
                    raw: None,
                    truncated: false,
                }),
            ]
        );
    }

    #[test]
    fn a_user_image_file_keeps_its_path_and_visual_content() {
        let f = Fixture::new();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("shot.png");
        let image = png();
        std::fs::write(&path, &image).unwrap();
        f.file(path.to_str().unwrap());

        let ctx = f.build(&model());
        assert!(collect_text(&ctx.messages[0]).contains(path.to_str().unwrap()));
        assert!(matches!(
            &ctx.messages[0].content[1],
            ContentPart::Image(ImagePart {
                mime_type,
                source: ImageSource::Base64 { data },
            }) if mime_type == "image/png"
                && data == &base64::engine::general_purpose::STANDARD.encode(image)
        ));
    }

    #[test]
    fn a_persisted_tool_image_is_rehydrated_after_its_tool_result() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("take a screenshot");
        f.response(round, &model(), vec![calls(&["shot"])]);
        let image = png();
        f.result_with_file(round, "shot", "screen.png", "IMAGE/PNG", &image);

        let ctx = f.build(&model());
        assert_eq!(
            ctx.messages.len(),
            4,
            "user, assistant, tool result, file projection"
        );
        assert_eq!(ctx.messages[2].role, Role::Tool);
        assert_eq!(ctx.messages[3].role, Role::User);
        assert!(matches!(
            &ctx.messages[3].content[1],
            ContentPart::Image(ImagePart {
                mime_type,
                source: ImageSource::Base64 { data },
            }) if mime_type == "image/png"
                && data == &base64::engine::general_purpose::STANDARD.encode(image)
        ));
    }

    #[test]
    fn an_oversized_image_gets_a_request_local_uncropped_representation() {
        let raster = image::RgbImage::from_fn(1_700, 1_700, |x, y| {
            let n = x
                .wrapping_mul(1_664_525)
                .wrapping_add(y.wrapping_mul(1_013_904_223));
            image::Rgb([n as u8, n.rotate_left(11) as u8, n.rotate_left(23) as u8])
        });
        let mut source = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(raster)
            .write_to(&mut source, ImageFormat::Png)
            .unwrap();
        let source = source.into_inner();
        assert!(base64_len(source.len()) > IMAGE_BASE64_LIMIT);

        let projected = project_image(
            ContextObject {
                bytes: source.len() as u64,
                reader: Box::new(Cursor::new(source)),
            },
            "image/png",
        )
        .unwrap();
        let ContentPart::Image(ImagePart {
            mime_type,
            source: ImageSource::Base64 { data },
        }) = projected
        else {
            panic!("expected a projected image");
        };
        assert_eq!(mime_type, "image/jpeg");
        assert!(data.len() <= IMAGE_BASE64_LIMIT);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        let image = image::load_from_memory(&decoded).unwrap();
        let (width, height) = image.dimensions();
        assert_eq!(width, height, "proportional resize must not crop a square");
    }

    #[test]
    fn text_and_binary_tool_files_project_by_mime() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("inspect outputs");
        f.response(round, &model(), vec![calls(&["text", "archive"])]);
        f.result_with_file(
            round,
            "text",
            "report.json",
            "application/json",
            br#"{"ok":true,"payload":"</tool_file>"}"#,
        );
        f.result_with_file(
            round,
            "archive",
            "bundle.zip",
            "application/zip",
            b"secret-binary",
        );

        let ctx = f.build(&model());
        let projection = &ctx.messages[3];
        let rendered = projection
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(r#"{"ok":true"#), "{rendered}");
        assert!(rendered.contains("&lt;/tool_file>"), "{rendered}");
        assert!(rendered.contains("application/zip"), "{rendered}");
        assert!(
            rendered.contains("binary file persisted"),
            "binary bytes must not be injected: {rendered}"
        );
        assert!(!rendered.contains("secret-binary"), "{rendered}");
    }

    /// One response is one assistant message, whatever it contained.
    #[test]
    fn one_round_collapses_into_one_assistant_message() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("go");
        f.response(
            round,
            &model(),
            vec![
                reasoning(
                    "let me look",
                    Some(json!({ "type": "thinking", "signature": "s" })),
                ),
                text("checking"),
                calls(&["c1", "c2"]),
            ],
        );
        f.result(round, "c1");
        f.result(round, "c2");

        let ctx = f.build(&model());
        assert_eq!(ctx.messages.len(), 3, "user, assistant, tool");

        let a = &ctx.messages[1];
        assert_eq!(
            a.content.len(),
            3,
            "reasoning, text and the whole call group"
        );
        // Order is preserved, which matters: thinking must precede tool_use.
        assert!(matches!(a.content[0], ContentPart::Reasoning(_)));
        assert!(matches!(a.content[2], ContentPart::ToolCall(_)));

        // Both results in one tool message; each client splits or merges as its wire needs.
        assert_eq!(ctx.messages[2].role, Role::Tool);
        assert_eq!(ctx.messages[2].content.len(), 2);
    }

    /// An incomplete tool group is repaired, not dropped: the unanswered call gets a synthetic
    /// error result so the model learns the call never completed instead of re-issuing it forever.
    #[test]
    fn an_incomplete_tool_group_repairs_the_missing_result() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("go");
        f.response(
            round,
            &model(),
            vec![
                reasoning(
                    "planning",
                    Some(json!({ "type": "thinking", "signature": "s" })),
                ),
                text("I will run two things"),
                calls(&["c1", "c2"]),
            ],
        );
        // Only one answered — the process died in between.
        f.result(round, "c1");

        let ctx = f.build(&model());

        let a = &ctx.messages[1];
        // The calls survive; reasoning stays with them (the round still has tool calls).
        assert!(matches!(a.content[0], ContentPart::Reasoning(_)));
        assert!(a.content.iter().any(|p| matches!(p, ContentPart::Text(_))));
        assert!(
            a.content
                .iter()
                .any(|p| matches!(p, ContentPart::ToolCall(_)))
        );

        // The tool message carries c1's real result and a synthetic error for the unanswered c2.
        let tool = ctx.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert_eq!(tool.content.len(), 2);
        let by_call: std::collections::HashMap<&str, &ContentPart> = tool
            .content
            .iter()
            .map(|p| match p {
                ContentPart::ToolResult(r) => (r.call_id.as_str(), p),
                _ => unreachable!(),
            })
            .collect();
        assert!(
            !matches!(
                by_call["c1"],
                ContentPart::ToolResult(r) if r.is_error
            ),
            "c1's real result is not an error"
        );
        assert!(
            matches!(by_call["c2"], ContentPart::ToolResult(r) if r.is_error),
            "the unanswered call gets a synthetic error"
        );
    }

    #[test]
    fn a_complete_group_survives_even_with_results_out_of_order() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("go");
        f.response(round, &model(), vec![calls(&["c1", "c2"])]);
        f.result(round, "c2");
        f.result(round, "c1");

        let ctx = f.build(&model());
        assert_eq!(ctx.messages.len(), 3);
    }

    /// A call whose arguments are not JSON is dropped together with its result; the surviving
    /// calls and their results stay paired and replayable.
    #[test]
    fn malformed_args_call_is_dropped_with_its_result() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("go");
        let group = ContentPart::ToolCall(ToolCallPart {
            calls: vec![
                ToolCall {
                    id: "good".into(),
                    name: "t".into(),
                    args: "{\"a\": 1}".into(),
                    raw: None,
                },
                ToolCall {
                    id: "bad".into(),
                    name: "t".into(),
                    args: "not json at all".into(),
                    raw: None,
                },
            ],
        });
        f.response(round, &model(), vec![reasoning("planning", None), group]);
        // Both have results — the malformed one was prechecked, not executed, but it still got
        // a (precheck) result row. The pairing must survive the filter.
        f.result(round, "good");
        f.result(round, "bad");

        let ctx = f.build(&model());
        let a = &ctx.messages[1];
        // Reasoning is kept: the group still has a surviving call, so it is a tool round.
        assert!(matches!(a.content[0], ContentPart::Reasoning(_)));
        let group = match &a.content[1] {
            ContentPart::ToolCall(g) => g,
            _ => panic!("expected tool-call group"),
        };
        assert_eq!(group.calls.len(), 1, "malformed call dropped");
        assert_eq!(group.calls[0].id, "good");

        // The tool message carries only the surviving call's result.
        let tool = ctx.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert_eq!(tool.content.len(), 1);
        assert!(matches!(
            &tool.content[0],
            ContentPart::ToolResult(r) if r.call_id == "good"
        ));
    }

    /// When every call in the group is malformed, the whole group — and its reasoning — goes,
    /// exactly like an incomplete group.
    #[test]
    fn group_of_only_malformed_calls_is_dropped_entirely() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("go");
        f.response(
            round,
            &model(),
            vec![
                reasoning("planning", None),
                calls_with_args(&["bad1", "bad2"], "{unclosed"),
            ],
        );
        f.result(round, "bad1");
        f.result(round, "bad2");

        let ctx = f.build(&model());
        // Assistant message had no text, so after the group and reasoning are dropped it is empty
        // and `strip_empty` removes it entirely.
        assert_eq!(ctx.messages.len(), 1, "only the user message survives");
        assert!(
            !ctx.messages.iter().any(|m| m.role == Role::Tool),
            "orphaned results must not be sent"
        );
    }

    /// The decisive case from the raw-gate design: same client, same raw shape, different model.
    #[test]
    fn switching_model_strips_reasoning_and_keeps_normalised_calls() {
        let f = Fixture::new();
        let round = RoundId::new();
        f.user("go");
        f.response(
            round,
            &model(),
            vec![
                reasoning(
                    "secret",
                    Some(json!({ "type": "thinking", "signature": "sig" })),
                ),
                text("answer"),
                calls(&["c1"]),
            ],
        );
        f.result(round, "c1");

        let ctx = f.build(&Source::new("anthropic", "claude-sonnet-5"));

        let a = &ctx.messages[1];
        assert!(
            !a.content
                .iter()
                .any(|p| matches!(p, ContentPart::Reasoning(_))),
            "a signature is bound to its model; replaying it is a 400"
        );
        match a.content.iter().find_map(|p| match p {
            ContentPart::ToolCall(g) => Some(g),
            _ => None,
        }) {
            Some(g) => {
                assert_eq!(
                    g.calls[0].args, "{\"a\": 1}",
                    "normalised calls survive verbatim"
                );
                assert!(g.calls[0].raw.is_none());
            }
            None => panic!("the call group must survive a model change"),
        }
    }

    #[test]
    fn same_model_replays_raw_byte_for_byte() {
        let f = Fixture::new();
        let raw = json!({ "type": "thinking", "thinking": "secret", "signature": "EqoBCk+/=" });
        f.user("go");
        f.response(
            RoundId::new(),
            &model(),
            vec![reasoning("secret", Some(raw.clone()))],
        );

        let ctx = f.build(&model());
        match &ctx.messages[1].content[0] {
            ContentPart::Reasoning(r) => assert_eq!(r.raw.as_ref(), Some(&raw)),
            other => panic!("{other:?}"),
        }
    }

    /// `redacted_thinking` has no plaintext but must still be replayed.
    #[test]
    fn a_reasoning_part_with_only_raw_is_kept() {
        let f = Fixture::new();
        f.user("go");
        f.response(
            RoundId::new(),
            &model(),
            vec![
                reasoning(
                    "",
                    Some(json!({ "type": "redacted_thinking", "data": "EncAA" })),
                ),
                text("answer"),
            ],
        );

        let ctx = f.build(&model());
        assert_eq!(
            ctx.messages[1].content.len(),
            2,
            "an empty-text reasoning block is not empty"
        );
    }

    #[test]
    fn empty_text_parts_are_not_sent() {
        let f = Fixture::new();
        f.user("go");
        f.response(RoundId::new(), &model(), vec![text(""), text("real")]);

        let ctx = f.build(&model());
        assert_eq!(ctx.messages[1].content.len(), 1);
    }

    /// Two submissions in a row are one user turn on the wire; several providers reject
    /// consecutive user messages.
    #[test]
    fn consecutive_user_input_merges() {
        let f = Fixture::new();
        f.user("first");
        f.user("second");
        f.response(RoundId::new(), &model(), vec![text("ok")]);

        let ctx = f.build(&model());
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].content.len(), 2);
    }

    #[test]
    fn only_the_latest_skill_body_object_is_opened() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let f = Fixture::new();
        for (turn_seq, revision, body) in [
            (1, "old-revision", "OBSOLETE SKILL BODY"),
            (2, "new-revision", "CURRENT SKILL BODY"),
        ] {
            let entry = entry_data::skill_load_entry_from_body(
                f.session,
                f.turn,
                turn_seq,
                "acme:review".into(),
                revision.into(),
                body.into(),
                "/skills/review/SKILL.md".into(),
                zlogic_protocol::SkillLoadSource::User,
                Vec::new(),
                &f.objects,
            )
            .unwrap();
            f.db.entries().append(entry).unwrap();
        }

        let entries = f.db.entries();
        let list = entries.list_for_context(f.session).unwrap();
        assert!(
            list.iter()
                .filter(|entry| entry.kind == EntryKind::SkillLoad)
                .all(|entry| !entry.is_offloaded()),
            "skill metadata must remain inline regardless of body size"
        );
        let loader = store_loader(&entries, &f.objects);
        let opens = AtomicUsize::new(0);
        let counted_object_loader = |raw: &str| {
            opens.fetch_add(1, Ordering::Relaxed);
            let id: ObjectId = raw.parse().map_err(CoreError::from)?;
            Ok(ContextObject {
                bytes: f.objects.size(&id).map_err(CoreError::from)?,
                reader: f.objects.open(&id).map_err(CoreError::from)?,
            })
        };

        let prepared = build_context(&list, &model(), &loader, &counted_object_loader).unwrap();
        let text = prepared
            .messages
            .iter()
            .map(collect_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(opens.load(Ordering::Relaxed), 1);
        assert!(text.contains("CURRENT SKILL BODY"));
        assert!(!text.contains("OBSOLETE SKILL BODY"));
    }

    #[test]
    fn a_fork_expands_skill_session_placeholders_for_the_child() {
        let f = Fixture::new();
        let entry = entry_data::skill_load_entry_from_body(
            f.session,
            f.turn,
            1,
            "review".into(),
            "revision".into(),
            "session=${SESSION_ID}; dir=${SKILL_DIR}".into(),
            "/skills/review/SKILL.md".into(),
            zlogic_protocol::SkillLoadSource::User,
            Vec::new(),
            &f.objects,
        )
        .unwrap();
        f.db.entries().append(entry).unwrap();

        let child =
            f.db.sessions()
                .create(NewSession::root(WorkspaceId::new()))
                .unwrap()
                .session_id;
        f.db.entries().copy_through(f.session, child, 1).unwrap();

        let entries = f.db.entries();
        let list = entries.list_for_context(child).unwrap();
        let loader = store_loader(&entries, &f.objects);
        let objects = object_loader(&f.objects);
        let prepared = build_context(&list, &model(), &loader, &objects).unwrap();
        let text = prepared
            .messages
            .iter()
            .map(collect_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&format!("session={child}")));
        assert!(!text.contains(&format!("session={}", f.session)));
        assert!(text.contains("dir=/skills/review"));
    }

    #[test]
    fn a_store_trimmed_compaction_prefix_still_projects_summary_and_skill_state() {
        let f = Fixture::new();
        f.user("covered detail");
        let skill = entry_data::skill_load_entry_from_body(
            f.session,
            f.turn,
            1,
            "review".into(),
            "revision".into(),
            "ACTIVE PROCEDURE".into(),
            "/skills/review/SKILL.md".into(),
            zlogic_protocol::SkillLoadSource::User,
            Vec::new(),
            &f.objects,
        )
        .unwrap();
        f.db.entries().append(skill).unwrap();
        f.db.entries()
            .append(NewEntry::new(
                f.session,
                f.turn,
                2,
                EntryKind::Compaction,
                serde_json::to_value(Summary {
                    from_turn: 1,
                    to_turn: 1,
                    content: "COMPACTED SUMMARY".into(),
                    reason: zlogic_protocol::stream::CompactionReason::Manual,
                    model_ref: None,
                    summary_tokens: None,
                })
                .unwrap(),
            ))
            .unwrap();
        f.db.entries()
            .append(
                entry_data::to_entry(
                    f.session,
                    f.turn,
                    2,
                    None,
                    &entry_data::Author::User,
                    text("tail request"),
                )
                .unwrap(),
            )
            .unwrap();

        let entries = f.db.entries();
        let list = entries.list_for_context_after(f.session, 1).unwrap();
        let loader = store_loader(&entries, &f.objects);
        let objects = object_loader(&f.objects);
        let prepared = build_context(&list, &model(), &loader, &objects).unwrap();
        let rendered = prepared
            .messages
            .iter()
            .map(collect_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(prepared.summarized_turns, 1);
        assert!(rendered.contains("COMPACTED SUMMARY"));
        assert!(rendered.contains("ACTIVE PROCEDURE"));
        assert!(rendered.contains("tail request"));
        assert!(!rendered.contains("covered detail"));
    }

    #[test]
    fn several_rounds_alternate_correctly() {
        let f = Fixture::new();
        let (r1, r2) = (RoundId::new(), RoundId::new());
        f.user("go");
        f.response(r1, &model(), vec![calls(&["c1"])]);
        f.result(r1, "c1");
        f.response(r2, &model(), vec![text("done")]);

        let ctx = f.build(&model());
        let roles: Vec<Role> = ctx.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
    }

    /// Interactions are in the timeline but never in a request.
    #[test]
    fn interactions_never_reach_the_request() {
        let f = Fixture::new();
        f.user("go");
        f.db.entries()
            .append(zlogic_store::NewEntry::new(
                f.session,
                f.turn,
                1,
                EntryKind::InteractionRequest,
                json!({ "interaction_id": "i-1" }),
            ))
            .unwrap();
        f.response(RoundId::new(), &model(), vec![text("ok")]);

        let ctx = f.build(&model());
        assert_eq!(ctx.messages.len(), 2);
    }

    /// Without a source stamp the raw gate has nothing to compare, and guessing is how a signature
    /// from the wrong model reaches the wire.
    #[test]
    fn an_assistant_entry_without_a_source_is_a_hard_error() {
        let f = Fixture::new();
        f.db.entries()
            .append(
                zlogic_store::NewEntry::new(
                    f.session,
                    f.turn,
                    1,
                    EntryKind::AssistantText,
                    serde_json::to_value(text("orphan")).unwrap(),
                )
                .in_round(RoundId::new()),
            )
            .unwrap();

        let objects = zlogic_objects::MemoryObjectStore::new();
        let entries = f.db.entries();
        let loader = store_loader(&entries, &objects);
        let object_loader = object_loader(&objects);
        let list = entries.list_for_context(f.session).unwrap();
        assert!(matches!(
            build_context(&list, &model(), &loader, &object_loader),
            Err(CoreError::Corrupt(_))
        ));
    }

    /// An offloaded payload is read back through the object store, transparently.
    #[test]
    fn offloaded_entries_are_loaded_from_the_object_store() {
        let f = Fixture::new();
        let objects = zlogic_objects::MemoryObjectStore::new();
        let long = "x".repeat(zlogic_store::entry::INLINE_LIMIT * 2);

        f.user("go");
        let part = ContentPart::Text(TextPart {
            text: long.clone(),
            raw: None,
            truncated: false,
        });
        let e = entry_data::to_entry(
            f.session,
            f.turn,
            1,
            Some(RoundId::new()),
            &entry_data::Author::Model(model()),
            part,
        )
        .unwrap();
        let rec = f.db.entries().append_with_offload(e, &objects).unwrap();
        assert!(rec.is_offloaded());

        let entries = f.db.entries();
        let loader = store_loader(&entries, &objects);
        let object_loader = object_loader(&objects);
        let list = entries.list_for_context(f.session).unwrap();
        let ctx = build_context(&list, &model(), &loader, &object_loader).unwrap();

        match &ctx.messages[1].content[0] {
            ContentPart::Text(t) => assert_eq!(t.text.len(), long.len()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn system_parts_mark_only_the_first_breakpoint() {
        let parts = build_system(vec!["preamble".into(), "".into(), "tail".into()]);
        assert_eq!(parts.len(), 2, "blank sections are dropped");
        assert!(parts[0].cache, "one breakpoint covers the whole prefix");
        assert!(!parts[1].cache);
    }

    #[test]
    fn an_empty_history_produces_an_empty_request() {
        let f = Fixture::new();
        assert_eq!(f.build(&model()), PreparedContext::default());
    }
}
