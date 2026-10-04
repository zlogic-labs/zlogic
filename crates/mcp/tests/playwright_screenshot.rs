//! 端到端验证：真的起一个 playwright MCP server，`browser_take_screenshot` 的结果必须是
//! image kind。
//!
//! 单元测试用的是构造出来的字节；这里走完整链路 —— `Catalog` 真的 spawn 出 `npx` 子进程、
//! 真的握手、真的发 `tools/call`，再断言 `zlogic_mcp::content::to_result` 把返回的图片变成了
//! `ToolDisplay::Image`。
//!
//! 需要 `npx` 和 playwright 的 chromium。缺了就跳过：CI 上没有浏览器是常态，让它红没有意义。
//! 设置 `ZLOGIC_PLAYWRIGHT_E2E=1` 才会尝试。

use std::collections::BTreeMap;
use std::sync::Arc;

use zlogic_mcp::def::{Binding, Origin, ServerDef, TransportDef};
use zlogic_mcp::{Catalog, CatalogDirs, McpTool, PoolConfig};
use zlogic_objects::MemoryObjectStore;
use zlogic_protocol::{CallId, SessionId, TurnId};
use zlogic_tools::{CancellationToken, Tool, ToolCtx, ToolDisplay, ToolExecStatus};

fn npx_available() -> bool {
    std::process::Command::new("npx")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn ctx(root: &std::path::Path, objects: Arc<MemoryObjectStore>) -> ToolCtx {
    ToolCtx {
        exec_cwd: root.to_path_buf(),
        root: root.to_path_buf(),
        session_id: SessionId::new(),
        turn_id: TurnId::new(),
        call_id: CallId::new("call_e2e"),
        objects,
        spawner: None,
        tasks: None,
        skills: None,
        worktree: None,
        interaction: None,
        output: None,
        display: None,
        max_result_chars: 200_000,
        runtime_paths: Vec::new(),
        env: None,
        computer: None,
        cancel: CancellationToken::new(),
        budget: CancellationToken::new(),
    }
}

#[tokio::test]
async fn a_playwright_screenshot_reaches_the_model_as_an_image() {
    if std::env::var("ZLOGIC_PLAYWRIGHT_E2E").is_err() {
        eprintln!(
            "skipped: set ZLOGIC_PLAYWRIGHT_E2E=1 to run against a real playwright server"
        );
        return;
    }
    if !npx_available() {
        eprintln!("skipped: npx is not on PATH");
        return;
    }

    let temp = std::env::temp_dir().join("zlogic-playwright-e2e");
    std::fs::create_dir_all(&temp).expect("state dir");

    // The same definition the marketplace installs (`recommended.ts`): `@playwright/mcp@latest`,
    // headless so the test needs no display.
    let def = ServerDef {
        id: "playwright".into(),
        label: Some("Playwright".into()),
        enabled: None,
        transport: TransportDef::Stdio {
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@playwright/mcp@latest".into(),
                "--headless".into(),
            ],
            env: BTreeMap::new(),
            cwd: None,
        },
        binding: Binding::Params,
        filter: Default::default(),
        origin: Origin::default(),
        file: None,
    };

    let pool = Arc::new(zlogic_mcp::McpPool::new(
        PoolConfig {
            // `npx -y` has to fetch and unpack the package before the server speaks MCP at all.
            connect_timeout: std::time::Duration::from_secs(240),
            call_timeout: std::time::Duration::from_secs(240),
            ..PoolConfig::default()
        },
        temp.join("shared"),
    ));
    let catalog = Catalog::new(
        CatalogDirs {
            global_file: temp.join("mcp.json"),
            global_dir: temp.join("extensions").join("mcp"),
            cache: temp.join("cache").join("mcp"),
            shared: temp.join("shared"),
        },
        pool.clone(),
    );

    let mut loaded = catalog.load(&temp, vec![def.clone()]);
    catalog.refresh(&mut loaded, &temp).await;
    let specs = loaded.tools_of("playwright").to_vec();
    assert!(
        !specs.is_empty(),
        "the server should have contributed tools; problems: {:?}",
        loaded.problems
    );
    eprintln!(
        "server offered {} tools",
        specs.len()
    );

    let spec = specs
        .iter()
        .find(|spec| spec.name == "browser_take_screenshot")
        .unwrap_or_else(|| {
            panic!(
                "browser_take_screenshot missing; offered: {:?}",
                specs.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        })
        .clone();
    let tool = McpTool::new(Arc::new(def), Arc::new(spec), pool.clone());

    let objects = Arc::new(MemoryObjectStore::new());
    let tool_ctx = ctx(&temp, objects.clone());

    // Navigate first: a screenshot of about:blank is real but tiny, and the point is to prove a
    // full page's picture survives the trip.
    let nav = tool
        .execute(
            &tool_ctx,
            r#"{"url":"data:text/html,<h1 style='font:48px sans-serif'>screenshot e2e</h1><p style='font:24px sans-serif'>a second line so the image is not blank</p>"}"#,
        )
        .await
        .expect("navigate");
    assert_eq!(nav.status, ToolExecStatus::Success, "{}", nav.model_text());

    let shot = tool
        .execute(&tool_ctx, r#"{"type":"png"}"#)
        .await
        .expect("the screenshot call itself");
    assert_eq!(shot.status, ToolExecStatus::Success, "{}", shot.model_text());

    // The card, not just the bytes: this is the whole point of the change.
    let card = shot
        .display
        .iter()
        .find(|display| matches!(display, ToolDisplay::Image { .. }))
        .unwrap_or_else(|| panic!("expected an image card, got {:?}", shot.display));
    let ToolDisplay::Image {
        mime,
        width,
        height,
        bytes,
        label,
        object_id,
    } = card
    else {
        unreachable!("just matched")
    };
    let (mime, width, height, bytes, label) =
        (mime.clone(), *width, *height, *bytes, label.clone());

    assert_eq!(mime, "image/png", "measured from the bytes, not taken on trust");
    assert!(
        width > 0 && height > 0,
        "a real screenshot has real dimensions, got {width}x{height}"
    );
    assert!(bytes > 5_000, "a real screenshot is not {bytes} bytes");
    assert!(!label.is_empty(), "the card needs a label");

    // The stored bytes must still be a decodable PNG: this is what `zlogic_core` hands the model.
    let stored = objects
        .get(object_id)
        .expect("the card's object is in the store");
    assert_eq!(
        &stored[..8],
        b"\x89PNG\r\n\x1a\n",
        "the stored bytes are a PNG, not base64 text"
    );
    assert_eq!(stored.len() as u64, bytes, "the card reports the real size");

    // The base64 must not have leaked into the transcript — that is the rule the whole MCP content
    // mapping exists for, and a 100kB screenshot would blow the context if it had.
    let model_text = shot.model_text();
    let head = base64::engine::general_purpose::STANDARD.encode(&stored[..64.min(stored.len())]);
    assert!(
        !model_text.contains(&head),
        "the base64 must not land in the transcript"
    );

    pool.shutdown();
    eprintln!(
        "OK: {width}x{height} {mime}, {bytes} bytes, label {label:?}, PNG magic verified, \
         model text {} chars",
        model_text.len()
    );
}