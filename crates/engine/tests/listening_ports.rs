//! The one thing a mock cannot check: that a port bound *below* the pid the shell tool spawned
//! is found again.
//!
//! Every other part of the port story is bookkeeping that a table can prove. This one is the
//! assumption the whole feature rests on — that a `pid -> process tree -> listen table` walk finds
//! a listener the recorded pid never touched itself. A fake would agree with the implementation by
//! construction; only a real process can disagree.

#![cfg(windows)]

use std::net::TcpListener;
use std::process::Stdio;

use zlogic_engine::ports::ProcessRoots;
use zlogic_proctree::{Console, Tree};
use zlogic_task::TaskId;

/// A port nothing is using. Bound and released, so the listener below gets it without the test
/// colliding with whatever else the machine is running.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read it back")
        .port()
}

#[tokio::test]
async fn a_port_below_the_wrapper_pid_is_found_and_forgotten_with_the_run() {
    let port = free_port();
    // Single-quoted so bash does not expand `$l`. The listener is the powershell grandchild: the
    // same shape as `bash -c "npm run dev"` leaving vite two generations down.
    let inner = format!(
        "powershell -NoProfile -Command '$l = New-Object System.Net.Sockets.TcpListener([Net.IPAddress]::Loopback,{port}); $l.Start(); Start-Sleep -Seconds 30'"
    );
    let mut command = tokio::process::Command::new("bash");
    command
        .args(["--noprofile", "--norc", "-c", &inner])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut tree = Tree::spawn(command, Console::Hidden).expect("spawn");
    let root = tree.id().expect("the spawned process has a pid");

    let roots = ProcessRoots::new();
    let task_id = TaskId::new();
    roots.record(task_id, root);

    // A server that takes a moment to bind is the normal case, not an edge case — a dev server
    // compiles first. Poll rather than read once.
    let mut found = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        found = roots
            .listening_ports(&[task_id])
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        if !found.is_empty() {
            break;
        }
    }
    tree.terminate().await;
    assert_eq!(found, vec![port], "the grandchild's port, not the wrapper's");

    // And it goes away with the run, because a badge pointing at a recycled port is worse than no
    // badge. The process is terminated above; the registry must not outlive it.
    roots.forget(&task_id);
    assert!(roots.listening_ports(&[task_id]).is_empty());
}
