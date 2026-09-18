//! A minimal MCP server over stdio. **Test fixture only** — gated behind the `test-server` feature
//! and never built into anything shipped.
//! # Why hand-written rather than the SDK's server half
//! Two reasons. A `[[bin]]` is built with the crate's normal dependencies, so using `rmcp`'s server
//! feature here would drag `schemars` into production builds for the sake of a test. And a
//! hand-written server checks something SDK-to-SDK cannot: that our client speaks the *protocol*,
//! not merely the same library's idea of it.
//! It answers `initialize`, `tools/list` and `tools/call`, and offers one tool — `report`, which
//! returns its own working directory and one environment variable. That makes it the fixture for the
//! part of `resolve.rs` no unit test can prove: that `cwd` and `env` actually reach the child.

use std::io::{BufRead, Write};

fn main() {
    // A real MCP server logs to stderr, and zlogic must drain that pipe rather than inherit it. Writing
    // here on purpose: a test asserts this never reaches the parent's own stderr.
    eprintln!("stdio_zlogic: starting up");

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // A notification has no id and takes no answer — `notifications/initialized` is the one that
        // arrives here, and replying to it is a protocol error.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        let response = match method {
            "initialize" => ok(id, initialize(&msg)),
            "tools/list" => ok(id, tools_list()),
            "tools/call" => ok(id, tools_call(&msg)),
            "ping" => ok(id, serde_json::json!({})),
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("no method {other}") }
            }),
        };
        // One JSON object per line, flushed immediately: the client is waiting on this pipe, and a
        // buffered answer is indistinguishable from a hung server.
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

fn ok(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn initialize(request: &serde_json::Value) -> serde_json::Value {
    // Zlogic the client's protocol version back: that is what a server does when it supports what was
    // asked for, and it keeps this fixture from going stale when the negotiated version moves.
    let version = request
        .get("params")
        .and_then(|p| p.get("protocolVersion"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!("2025-06-18"));
    serde_json::json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "stdio_zlogic", "version": "0.0.0" }
    })
}

fn tools_list() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "report",
                "description": "reports the server's working directory and one environment variable",
                "inputSchema": {
                    "type": "object",
                    "properties": { "var": { "type": "string" } }
                },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "explode",
                "description": "always reports failure",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

fn tools_call(request: &serde_json::Value) -> serde_json::Value {
    let params = request.get("params");
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default();
    let args = params.and_then(|p| p.get("arguments"));

    match name {
        "report" => {
            let var = args
                .and_then(|a| a.get("var"))
                .and_then(|v| v.as_str())
                .unwrap_or("PATH");
            let cwd = std::env::current_dir().unwrap_or_default();
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "cwd={}\n{var}={}",
                        cwd.display(),
                        std::env::var(var).unwrap_or_else(|_| "<unset>".into())
                    )
                }],
                "isError": false
            })
        }
        "explode" => serde_json::json!({
            "content": [{ "type": "text", "text": "as requested" }],
            "isError": true
        }),
        other => serde_json::json!({
            "content": [{ "type": "text", "text": format!("no tool {other}") }],
            "isError": true
        }),
    }
}
