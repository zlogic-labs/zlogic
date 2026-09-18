//! The stdio path, end to end, against a real child process.
//! This is the transport almost every MCP server uses, and it is the one path where the interesting
//! failures are not protocol-shaped: a command that is not on `PATH`, a working directory that is not
//! what the definition said, an environment variable that never arrived, a server whose stderr fills
//! its pipe. None of that is reachable with an in-process fake, so these tests spawn
//! `stdio_zlogic` — a hand-written MCP server that reports its own `cwd` and environment
//! (see `src/bin/stdio_zlogic.rs`).
//! What is being checked is the whole chain: definition → template expansion → pool → child process
//! → `tools/list` → registry → `tools/call` → a result the model can read.

use std::path::Path;
use std::sync::Arc;

use zlogic_mcp::{Catalog, CatalogDirs, McpPool, PoolConfig};
use zlogic_tools::{ToolExecStatus, ToolRegistry, ToolSource};

/// The fixture server's path, resolved by cargo for this test binary.
const SERVER: &str = env!("CARGO_BIN_EXE_stdio_zlogic");

struct Fixture {
    _tmp: tempfile::TempDir,
    dirs: CatalogDirs,
    workspace: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dirs = CatalogDirs {
        global_file: root.join("config/mcp.json"),
        global_dir: root.join("data/extensions/mcp"),
        cache: root.join("cache/mcp"),
    };
    let workspace = root.join("work");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&dirs.global_dir).unwrap();
    Fixture {
        _tmp: tmp,
        dirs,
        workspace,
    }
}

fn define(f: &Fixture, id: &str, value: serde_json::Value) {
    let path = f.dirs.global_dir.join(format!("{id}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn catalog(f: &Fixture) -> (Catalog, Arc<McpPool>) {
    let pool = Arc::new(McpPool::new(PoolConfig {
        connect_timeout: std::time::Duration::from_secs(10),
        call_timeout: std::time::Duration::from_secs(10),
        ..PoolConfig::default()
    }));
    (Catalog::new(f.dirs.clone(), pool.clone()), pool)
}

/// A `ToolCtx` pointed at a workspace. Mirrors what core builds per call.
fn ctx(root: &Path) -> zlogic_tools::ToolCtx {
    zlogic_tools::ToolCtx {
        exec_cwd: root.to_path_buf(),
        root: root.to_path_buf(),
        session_id: zlogic_protocol::SessionId::new(),
        turn_id: zlogic_protocol::TurnId::new(),
        call_id: zlogic_protocol::CallId::new("call_stdio_test"),
        objects: Arc::new(zlogic_objects::MemoryObjectStore::new()),
        spawner: None,
        tasks: None,
        worktree: None,
        interaction: None,
        output: None,
        skills: None,
        max_result_chars: 10_000,
        runtime_paths: Vec::new(),
        cancel: zlogic_tools::CancellationToken::new(),
    }
}

async fn registry_of(f: &Fixture, catalog: &Catalog) -> ToolRegistry {
    let mut loaded = catalog.load(&f.workspace, vec![]);
    catalog.refresh(&mut loaded, &f.workspace).await;
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    let mut registry = ToolRegistry::new();
    registry.add_source(&catalog.source(&loaded));
    registry
}

/// The whole chain, with nothing faked.
#[tokio::test]
async fn a_real_server_contributes_tools_and_answers_a_call() {
    let f = fixture();
    define(&f, "zlogicer", serde_json::json!({ "command": SERVER }));
    let (catalog, _pool) = catalog(&f);

    let registry = registry_of(&f, &catalog).await;
    assert_eq!(
        registry.names(),
        ["mcp__zlogicer__explode", "mcp__zlogicer__report"]
    );

    // The annotations the server sent decide what the approval pipeline sees.
    assert_eq!(
        registry.meta("mcp__zlogicer__report").unwrap().risk,
        zlogic_tools::ToolRisk::Read
    );
    assert_eq!(
        registry.meta("mcp__zlogicer__report").unwrap().source,
        "mcp"
    );
    assert_eq!(
        registry.meta("mcp__zlogicer__explode").unwrap().risk,
        zlogic_tools::ToolRisk::Write,
        "a tool that annotates nothing must not be treated as read-only"
    );

    let tool = registry.get("mcp__zlogicer__report").unwrap();
    let out = tool
        .execute(&ctx(&f.workspace), r#"{"var":"ZLOGIC_MCP_STDIO_TEST"}"#)
        .await
        .unwrap();
    assert_eq!(out.status, ToolExecStatus::Success);
    assert!(out.model_text().contains("cwd="), "{}", out.model_text());
}

/// The ruling that makes per-workspace isolation a rule rather than a heuristic: a stdio server's
/// default working directory is the workspace root, and it is the *server* that has to see that.
#[tokio::test]
async fn the_child_runs_in_the_workspace_and_receives_the_declared_environment() {
    let f = fixture();
    define(
        &f,
        "zlogicer",
        serde_json::json!({
            "command": SERVER,
            "env": { "ZLOGIC_MCP_STDIO_TEST": "from-the-definition" }
        }),
    );
    let (catalog, _pool) = catalog(&f);
    let registry = registry_of(&f, &catalog).await;

    let out = registry
        .get("mcp__zlogicer__report")
        .unwrap()
        .execute(&ctx(&f.workspace), r#"{"var":"ZLOGIC_MCP_STDIO_TEST"}"#)
        .await
        .unwrap();

    let text = out.model_text();
    // `canonicalize` on both sides: the temporary directory is behind a symlink on macOS
    // (`/var` → `/private/var`), and the child reports where it really is.
    let expected = std::fs::canonicalize(&f.workspace).unwrap();
    let reported = text
        .lines()
        .find_map(|l| l.strip_prefix("cwd="))
        .map(|p| std::fs::canonicalize(p).unwrap())
        .expect(&text);
    assert_eq!(
        reported, expected,
        "the child must run in the workspace: {text}"
    );
    assert!(
        text.contains("ZLOGIC_MCP_STDIO_TEST=from-the-definition"),
        "{text}"
    );
}

/// The child also has to keep the ambient environment: a server launched without `PATH` or `HOME`
/// fails in ways that look nothing like a configuration mistake.
#[tokio::test]
async fn the_child_inherits_the_ambient_environment_too() {
    let f = fixture();
    define(&f, "zlogicer", serde_json::json!({ "command": SERVER }));
    let (catalog, _pool) = catalog(&f);
    let registry = registry_of(&f, &catalog).await;

    let out = registry
        .get("mcp__zlogicer__report")
        .unwrap()
        .execute(&ctx(&f.workspace), r#"{"var":"PATH"}"#)
        .await
        .unwrap();
    assert!(
        !out.model_text().contains("PATH=<unset>"),
        "{}",
        out.model_text()
    );
}

/// A tool that ran and failed is a `Failed` result the model can read — not a broken connection and
/// not an aborted turn.
#[tokio::test]
async fn a_tool_reporting_failure_is_a_failed_result() {
    let f = fixture();
    define(&f, "zlogicer", serde_json::json!({ "command": SERVER }));
    let (catalog, _pool) = catalog(&f);
    let registry = registry_of(&f, &catalog).await;

    let out = registry
        .get("mcp__zlogicer__explode")
        .unwrap()
        .execute(&ctx(&f.workspace), "{}")
        .await
        .unwrap();
    assert_eq!(out.status, ToolExecStatus::Failed);
    assert!(out.model_text().contains("as requested"));
}

/// One child, however many calls — and the tool list came from the cache the second time, so a
/// second catalogue does not start a second process.
#[tokio::test]
async fn the_process_is_reused_across_calls_and_across_catalogues() {
    let f = fixture();
    // `cwd` pinned, so the pool key does not vary with the workspace and reuse is what is being
    // measured rather than key derivation.
    define(
        &f,
        "zlogicer",
        serde_json::json!({ "command": SERVER, "cwd": "." }),
    );
    let (catalog, pool) = catalog(&f);
    let registry = registry_of(&f, &catalog).await;

    let tool = registry.get("mcp__zlogicer__report").unwrap();
    for _ in 0..3 {
        let out = tool.execute(&ctx(&f.workspace), "{}").await.unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);
    }
    assert_eq!(pool.len(), 1, "three calls, one child process");
    assert!(pool.status()[0].connected);

    // A second catalogue over the same cache: the tool list is read from disk, so nothing reconnects.
    let second = Catalog::new(
        f.dirs.clone(),
        Arc::new(McpPool::new(PoolConfig::default())),
    );
    let loaded = second.load(&f.workspace, vec![]);
    assert!(
        loaded.needs_refresh(chrono::Utc::now()).is_empty(),
        "the list is cached"
    );
    assert_eq!(second.source(&loaded).discover().len(), 2);
}

/// Killing the pool must actually stop the child: a stdio server is a process on the user's machine,
/// and one left behind per session is a leak they will notice.
#[tokio::test]
async fn dropping_the_connection_stops_the_child() {
    let f = fixture();
    define(
        &f,
        "zlogicer",
        serde_json::json!({ "command": SERVER, "cwd": "." }),
    );
    let (catalog, pool) = catalog(&f);
    let registry = registry_of(&f, &catalog).await;
    let tool = registry.get("mcp__zlogicer__report").unwrap();
    tool.execute(&ctx(&f.workspace), "{}").await.unwrap();

    pool.shutdown();
    assert!(pool.is_empty());

    // The next call reconnects rather than failing — the pool being emptied is invisible to the model.
    let out = tool.execute(&ctx(&f.workspace), "{}").await.unwrap();
    assert_eq!(out.status, ToolExecStatus::Success);
    assert_eq!(pool.len(), 1);
}

/// A definition that names a command nobody has must fail the *call*, and say what is missing.
#[tokio::test]
async fn a_missing_command_fails_the_call_and_names_itself() {
    let f = fixture();
    define(
        &f,
        "absent",
        serde_json::json!({ "command": "zlogic-mcp-not-a-real-command" }),
    );
    let (catalog, _pool) = catalog(&f);

    let mut loaded = catalog.load(&f.workspace, vec![]);
    catalog.refresh(&mut loaded, &f.workspace).await;
    assert_eq!(loaded.problems.len(), 1, "the user has to be told");
    assert!(
        loaded.problems[0]
            .reason
            .contains("zlogic-mcp-not-a-real-command"),
        "{}",
        loaded.problems[0].reason
    );
    // And it contributes nothing rather than breaking the tool set.
    assert_eq!(catalog.source(&loaded).discover().len(), 0);
}
