//! An MCP tool result, mapped onto the three audiences zlogic's results have.
//! MCP returns a list of content blocks; zlogic splits a result by *who reads it* — `content` for the
//! model, `display` for the UI, `objects` for persistence (see `zlogic_tools`). The mapping is mostly
//! obvious and interesting in exactly two places.
//! **Binary payloads do not go into the transcript.** An image or a blob arrives as base64 inside
//! the JSON-RPC response. Kept as text it would be written to the database and re-read on every
//! replay, for a screenshot nobody may ever open. So the bytes are decoded into the object store
//! once and the result carries a reference: the model gets a placeholder it can reason about, the UI
//! gets a card it can render, and the row stays small.
//! **An unknown block is preserved, not dropped.** MCP's content union grows, and a server speaking
//! a newer revision than this build must not produce a call that looks like it returned nothing. An
//! unrecognised block becomes a visible placeholder plus a log line — the model can then ask, which
//! is strictly better than silence.

use base64::Engine;
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use zlogic_objects::ObjectRole;
#[cfg(test)]
use zlogic_tools::ToolDisplay;
use zlogic_tools::{CapturedFile, Recovery, ToolCtx, ToolExecResult, ToolExecStatus};

use crate::security::sanitize_untrusted_text;

/// Maps one `tools/call` response.
/// Never fails: a result the model cannot read is worse than one that says what went wrong.
/// Object-store failures degrade the affected block to a placeholder and are logged.
pub fn to_result(
    server: &str,
    tool: &str,
    result: CallToolResult,
    ctx: &ToolCtx,
) -> ToolExecResult {
    let safe_tool = sanitize_untrusted_text(tool);
    let mut text = Vec::new();
    let mut out = ToolExecResult::new(if result.is_error.unwrap_or(false) {
        // The tool ran and reported a problem. Not an `Err` — the model is expected to read this and
        // correct itself, most often because its arguments were wrong.
        ToolExecStatus::Failed
    } else {
        ToolExecStatus::Success
    });

    for (index, block) in result.content.into_iter().enumerate() {
        match block {
            ContentBlock::Text(t) => text.push(t.text),
            ContentBlock::Image(image) => {
                let name = format!("{safe_tool}-image-{}", index + 1);
                let mime = sanitize_untrusted_text(&image.mime_type);
                match capture(ctx, name, mime.clone(), &image.data) {
                    Some(file) => out = file.add_to(out),
                    None => text.push(format!("[image: {mime}, could not be stored]")),
                }
            }
            ContentBlock::Audio(audio) => {
                let name = format!("{safe_tool}-audio-{}", index + 1);
                let mime = sanitize_untrusted_text(&audio.mime_type);
                match capture(ctx, name, mime.clone(), &audio.data) {
                    Some(file) => out = file.add_to(out),
                    None => text.push(format!("[audio: {mime}, could not be stored]")),
                }
            }
            ContentBlock::Resource(embedded) => match embedded.resource {
                ResourceContents::TextResourceContents {
                    uri, text: body, ..
                } => {
                    let uri = sanitize_untrusted_text(&uri);
                    text.push(format!("<resource uri=\"{uri}\">\n{body}\n</resource>"));
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let uri = sanitize_untrusted_text(&uri);
                    let mime = sanitize_untrusted_text(
                        &mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                    );
                    match capture(ctx, uri.clone(), mime.clone(), &blob) {
                        Some(file) => out = file.add_to(out),
                        None => text.push(format!("[resource {uri}: {mime}, could not be stored]")),
                    }
                }
                // `ResourceContents` is `#[non_exhaustive]`: a newer revision's variant must still
                // produce something the model can see.
                other => {
                    tracing::warn!(target: "zlogic::mcp", server, tool, "unknown resource contents");
                    text.push(format!("[resource of an unrecognised kind: {other:?}]"));
                }
            },
            ContentBlock::ResourceLink(link) => {
                // A link is a reference the server expects to be followed by a later call, not
                // content — so it is named, never fetched here.
                text.push(match link.description {
                    Some(d) => format!("[resource link {} → {} — {d}]", link.name, link.uri),
                    None => format!("[resource link {} → {}]", link.name, link.uri),
                });
            }
            other => {
                tracing::warn!(
                    target: "zlogic::mcp",
                    server, tool,
                    "content block of a kind this build does not know: {other:?}"
                );
                text.push("[content of a kind zlogic does not recognise was omitted]".to_string());
            }
        }
    }

    if let Some(structured) = result.structured_content {
        // Pretty-printed: a model reading a one-line blob of nested JSON makes more mistakes than
        // one reading it indented, and the token difference is small.
        let rendered =
            serde_json::to_string_pretty(&structured).unwrap_or_else(|_| structured.to_string());
        text.push(format!("structured content:\n{rendered}"));
    }

    let joined = sanitize_untrusted_text(&text.join("\n"));
    if joined.trim().is_empty() {
        // A tool that legitimately returns nothing has to say so. An empty result reads to the model
        // as a failure it should retry, and it retries.
        let failed = out.status == ToolExecStatus::Failed;
        return out.with_text(if failed {
            format!("`{tool}` reported failure but returned no error details.")
        } else {
            format!("`{tool}` returned no content.")
        });
    }

    // Large output goes to the object store with head and tail kept, the same treatment every other
    // tool's output gets — an MCP server dumping a 200k-line file must not blow up the context.
    // `offload_if_large` is not reusable here: it builds a *fresh* successful result, and this one
    // already carries the status, the cards and the references collected above.
    if joined.chars().count() <= ctx.max_result_chars {
        return out.with_text(joined);
    }
    match ctx.capture(ObjectRole::Output, None, &joined, Recovery::Narrow) {
        Ok(capture) => capture.add_to(out),
        Err(e) => {
            tracing::warn!(target: "zlogic::mcp", server, tool, "could not offload output: {e}");
            out.with_text(joined)
        }
    }
}

/// Decodes one MCP binary block into zlogic's generic persisted-file result. `None` on either
/// failure, either of which is the server's fault or the disk's and neither of which should take
/// the whole call down.
fn capture(
    ctx: &ToolCtx,
    name: String,
    mime_type: String,
    base64_data: &str,
) -> Option<CapturedFile> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(base64_data) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: "zlogic::mcp", "a content block was not valid base64: {e}");
            return None;
        }
    };
    match ctx.capture_file(name, mime_type, &bytes) {
        Ok(file) => Some(file),
        Err(e) => {
            tracing::warn!(target: "zlogic::mcp", "could not store a content block: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ctx;
    use rmcp::model::{ContentBlock, EmbeddedResource, Resource};

    fn text_of(result: &ToolExecResult) -> String {
        result.model_text()
    }

    #[test]
    fn text_blocks_pass_through_in_order() {
        let ctx = test_ctx();
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![
                ContentBlock::text("first"),
                ContentBlock::text("second"),
            ]),
            &ctx,
        );
        assert_eq!(out.status, ToolExecStatus::Success);
        assert_eq!(text_of(&out), "first\nsecond");
        assert!(out.objects.is_empty(), "text costs no object");
        assert!(out.display.is_empty());
    }

    #[test]
    fn server_text_cannot_inject_terminal_or_bidi_controls() {
        let ctx = test_ctx();
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![ContentBlock::text(
                "safe\u{200b}\u{202e}txt\u{202c}\rnext\x1b[2Kdone",
            )]),
            &ctx,
        );
        assert_eq!(text_of(&out), "safetxt\nnextdone");
    }

    /// A tool that ran and failed is `Failed`, not an `Err`: the model is meant to read it and fix
    /// its arguments.
    #[test]
    fn a_tool_level_error_is_a_failed_result_the_model_can_read() {
        let ctx = test_ctx();
        let mut raw = CallToolResult::success(vec![ContentBlock::text("no such repository")]);
        raw.is_error = Some(true);
        let out = to_result("s", "t", raw, &ctx);
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(text_of(&out).contains("no such repository"));
    }

    /// The rule the module exists for: base64 never reaches the transcript.
    #[test]
    fn an_image_goes_to_the_object_store_and_the_model_gets_a_placeholder() {
        let ctx = test_ctx();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNGnot-really");
        let out = to_result(
            "s",
            "screenshot",
            CallToolResult::success(vec![ContentBlock::image(png.clone(), "image/png")]),
            &ctx,
        );

        assert_eq!(
            out.objects.len(),
            1,
            "the bytes are kept alive by a reference"
        );
        assert_eq!(out.display.len(), 1, "and the UI gets a card");
        assert!(
            out.dangling_display_objects().is_empty(),
            "card and reference must agree"
        );
        match &out.display[0] {
            ToolDisplay::File {
                mime,
                bytes,
                object_id,
                ..
            } => {
                assert_eq!(mime.as_deref(), Some("image/png"));
                assert_eq!(*bytes, 14);
                assert!(object_id.is_some());
            }
            other => panic!("{other:?}"),
        }
        let text = text_of(&out);
        assert!(text.contains("image/png"), "{text}");
        assert!(
            !text.contains(&png),
            "the base64 must not land in the transcript: {text}"
        );

        let stored = ctx.objects.get(&out.objects[0].object_id).unwrap();
        assert_eq!(stored, b"\x89PNGnot-really");
    }

    #[test]
    fn a_blob_resource_is_stored_and_named_by_its_uri() {
        let ctx = test_ctx();
        let blob = base64::engine::general_purpose::STANDARD.encode(b"binary");
        let contents = ResourceContents::blob(blob, "file:///tmp/x.bin");
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![ContentBlock::Resource(EmbeddedResource::new(
                contents,
            ))]),
            &ctx,
        );
        assert_eq!(out.objects.len(), 1);
        match &out.display[0] {
            ToolDisplay::File { path, .. } => assert_eq!(path, "file:///tmp/x.bin"),
            other => panic!("{other:?}"),
        }
        assert!(text_of(&out).contains("file:///tmp/x.bin"));
    }

    #[test]
    fn a_text_resource_keeps_its_uri_beside_its_body() {
        let ctx = test_ctx();
        let contents = ResourceContents::text("fn main() {}", "file:///src/main.rs");
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![ContentBlock::Resource(EmbeddedResource::new(
                contents,
            ))]),
            &ctx,
        );
        let text = text_of(&out);
        assert!(text.contains("file:///src/main.rs"), "{text}");
        assert!(text.contains("fn main() {}"), "{text}");
        assert!(
            out.objects.is_empty(),
            "text is text, whatever it is wrapped in"
        );
    }

    /// A link is a reference to be followed by a later call. Fetching it here would turn a listing
    /// into an unbounded download.
    #[test]
    fn a_resource_link_is_named_never_fetched() {
        let ctx = test_ctx();
        let link = Resource::new("https://example.test/doc", "doc").with_description("the manual");
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![ContentBlock::ResourceLink(link)]),
            &ctx,
        );
        let text = text_of(&out);
        assert!(text.contains("https://example.test/doc"), "{text}");
        assert!(text.contains("the manual"), "{text}");
    }

    #[test]
    fn structured_content_is_appended_readably() {
        let ctx = test_ctx();
        let mut raw = CallToolResult::success(vec![ContentBlock::text("done")]);
        raw.structured_content = Some(serde_json::json!({ "count": 2 }));
        let text = text_of(&to_result("s", "t", raw, &ctx));
        assert!(text.starts_with("done"));
        assert!(text.contains("\"count\": 2"), "pretty-printed: {text}");
    }

    /// An empty result must not look to the model like a failure it should retry.
    #[test]
    fn an_empty_result_says_so() {
        let ctx = test_ctx();
        let out = to_result("s", "list_files", CallToolResult::success(vec![]), &ctx);
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(text_of(&out).contains("returned no content"));
    }

    #[test]
    fn an_empty_error_result_says_that_details_are_missing() {
        let ctx = test_ctx();
        let mut raw = CallToolResult::success(vec![]);
        raw.is_error = Some(true);
        let out = to_result("s", "broken", raw, &ctx);
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(text_of(&out).contains("no error details"));
    }

    #[test]
    fn several_media_blocks_get_distinct_display_names() {
        let ctx = test_ctx();
        let data = base64::engine::general_purpose::STANDARD.encode(b"image");
        let out = to_result(
            "s",
            "screenshots",
            CallToolResult::success(vec![
                ContentBlock::image(data.clone(), "image/png"),
                ContentBlock::image(data, "image/png"),
            ]),
            &ctx,
        );
        let paths: Vec<&str> = out
            .display
            .iter()
            .filter_map(|display| match display {
                ToolDisplay::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, ["screenshots-image-1", "screenshots-image-2"]);
    }

    /// Large output gets the same head-and-tail treatment as any other tool's.
    #[test]
    fn large_output_is_offloaded_with_a_card_to_expand_it() {
        let ctx = test_ctx();
        let big: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![ContentBlock::text(big.clone())]),
            &ctx,
        );

        let text = text_of(&out);
        assert!(
            text.chars().count() < big.chars().count(),
            "the model gets head and tail"
        );
        assert!(text.contains("omitted"), "{text}");
        assert!(matches!(
            out.display[0],
            ToolDisplay::Output {
                truncated: true,
                ..
            }
        ));
        assert!(out.dangling_display_objects().is_empty());
        let stored =
            String::from_utf8(ctx.objects.get(&out.objects[0].object_id).unwrap()).unwrap();
        assert_eq!(stored, big, "the store keeps all of it");
    }

    /// Base64 the server got wrong must not take the whole call down.
    #[test]
    fn undecodable_data_degrades_to_a_placeholder() {
        let ctx = test_ctx();
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![ContentBlock::image("not base64!!", "image/png")]),
            &ctx,
        );
        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(text_of(&out).contains("could not be stored"));
        assert!(out.objects.is_empty());
    }

    /// Several payloads in one result: every one keeps its own object alive.
    #[test]
    fn multiple_payloads_each_keep_their_own_reference() {
        let ctx = test_ctx();
        let a = base64::engine::general_purpose::STANDARD.encode(b"one");
        let b = base64::engine::general_purpose::STANDARD.encode(b"two");
        let out = to_result(
            "s",
            "t",
            CallToolResult::success(vec![
                ContentBlock::image(a, "image/png"),
                ContentBlock::audio(b, "audio/wav"),
                ContentBlock::text("and a note"),
            ]),
            &ctx,
        );
        assert_eq!(out.objects.len(), 2);
        assert_eq!(out.display.len(), 2);
        assert!(out.dangling_display_objects().is_empty());
        assert!(text_of(&out).contains("and a note"));
    }
}
