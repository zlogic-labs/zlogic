//! zlogic Rust TUI — the real engine host (desktop/cli flow). `run` dispatches to
//! the headless one-shot or the interactive render loop.
//! Both drive the same `CoreSession`: `EngineSession` (in-process `zlogic_engine`).
//!
//! The implementation lives in the library target so a host crate can call it: the
//! released `zlogic` binary is built by the closed-source repository, which welds this
//! CLI and the `daemon` subcommand into one executable (`crates/daemon/src/bin/zlogic.rs`).

#![allow(dead_code)] // reserved theme/widget surface; active paths are clippy-clean

mod app;
mod cli;
mod clipboard;
mod glyph;
mod i18n;
mod log;
mod markdown;
mod render;
mod session;
mod splash;
mod term;
mod theme;
mod widgets;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::args::Args;
use crate::session::engine::{ConnectOptions, EngineSession, WorkspacePick};
use crate::session::CoreSession;
use crate::term::caps::TermCaps;

pub fn run() -> ExitCode {
    // `daemon` as the first word is intercepted BEFORE clap: the remaining words belong to
    // `zlogic-daemon`'s own CLI (`devices` / `revoke <id>` / `doctor` / `--listen …`, its
    // `--help` included), so they are handed over verbatim. A host that links the daemon in
    // (the released binary) intercepts `daemon` before ever reaching this function; the
    // handover below is what a from-source CLI, which ships no daemon, falls back to.
    if std::env::args().nth(1).as_deref() == Some("daemon") {
        let words: Vec<String> = std::env::args().skip(2).collect();
        return cli::daemon::run(words);
    }

    // `key` is a first-word subcommand intercepted BEFORE clap: it must work
    // even when no model is configured — that is exactly when the app refuses to
    // start and the /model panel is unreachable, so this is the documented way to
    // add a key headlessly.
    if std::env::args().nth(1).as_deref() == Some("key") {
        let words: Vec<String> = std::env::args().skip(2).collect();
        let locale = key_locale(&words);
        return match cli::keys::run(words, locale) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zlogic: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let args = Args::parse();

    let mut caps = TermCaps::detect();

    // Debug: `--probe` — print detected capabilities (env heuristics folded with the
    // live OSC11 / CSI 6n / DECRQM measurements) and exit. No session, no TUI, so it
    // is safe in any shell (cmd inside Windows Terminal included).
    if args.probe {
        let outcome = term::probe::run(true);
        caps.refine_with_probe(&outcome);
        println!(
            "terminal_kind={:?}",
            crate::term::caps::detect_terminal_kind()
        );
        println!("color_tier={:?}", caps.color_tier);
        println!("icon_tier={:?}", caps.icon_tier);
        println!("ambiguous_wide={}", caps.ambiguous_wide);
        println!("background={:?}", caps.background);
        println!("supports_sync={}", caps.supports_sync);
        println!("legacy_windows_console={}", caps.legacy_windows_console);
        println!("probe_background={:?}", outcome.background);
        println!("probe_ambiguous_wide={:?}", outcome.ambiguous_wide);
        println!("probe_supports_sync={:?}", outcome.supports_sync);
        return ExitCode::SUCCESS;
    }

    // Gate DEC 2026 sync output on capability — unsupported terminals leak
    // `?2026` as a stray `[`. Must be set before any render_frame runs.
    term::set_supports_sync(caps.supports_sync);
    if caps.legacy_windows_console {
        eprintln!(
            "zlogic: legacy Windows console detected; use Windows Terminal, WezTerm, Alacritty, or VS Code terminal for best rendering."
        );
    }

    let locale = match i18n::Locale::resolve(&args.locale) {
        Ok(locale) => locale,
        Err(e) => {
            eprintln!("zlogic: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Debug: headless markdown render (no session, no probe, no TUI).
    if args.is_render_md() {
        let theme = theme::resolve::resolve_state(&args.theme, args.god, &caps);
        let code = cli::render_md::run(&args, &theme, caps.icon_tier);
        return ExitCode::from(code as u8);
    }

    // Backend: real in-process engine — the only host (desktop/cli flow).
    let session: Box<dyn CoreSession> = {
        let cwd = args
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let (session_id, resume_latest) = match &args.resume {
            Some(None) => (None, true),
            Some(Some(id)) => (Some(id.clone()), false),
            None => (args.session.clone(), false),
        };
        let interactive = !args.is_oneshot()
            && args.resume.is_none()
            && args.session.is_none()
            && cli::pick::is_interactive();
        match EngineSession::connect(ConnectOptions {
            cwd,
            session_id,
            resume_latest,
            god: args.god,
            locale,
            pick: if interactive {
                WorkspacePick::Interactive
            } else {
                WorkspacePick::Auto
            },
        }) {
            Ok(session) => Box::new(session),
            Err(e) => {
                eprintln!("zlogic: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    if args.is_oneshot() {
        let code = cli::oneshot::run(&args, session);
        return ExitCode::from(code as u8);
    }

    // Startup probes, interactive only — they briefly own raw mode, so they
    // must run before the TUI takes the terminal. CSI 6n measures ambiguous width
    // (replaces the Terminal.app guess → block logo/icons where they render fine);
    // OSC 11 asks the background for `theme: auto` (COLORFGBG is the fallback).
    let want_background =
        args.theme == "auto" && !args.god && std::env::var_os("NO_COLOR").is_none();
    let outcome = term::probe::run(want_background);
    caps.refine_with_probe(&outcome);
    // Re-apply after the probe: the DECRQM answer may have flipped `supports_sync`
    // either way. The early set above stays so oneshot mode (which skips the probe)
    // still gets the env-based value.
    term::set_supports_sync(caps.supports_sync);
    let icons = caps.icon_tier;

    // Theme-first: resolve before anything renders. God forces the god theme.
    let theme_state = theme::resolve::resolve_state(&args.theme, args.god, &caps);

    // Interactive TUI. Arm recovery (panic hook) BEFORE entering raw mode; the RAII
    // Guard inside `run` restores on every normal exit path.
    app::loop_::arm_recovery();
    match app::loop_::run(
        session,
        theme_state,
        icons,
        locale,
        args.plan,
        args.god,
        args.permission_mode(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zlogic: {e}");
            ExitCode::FAILURE
        }
    }
}

fn key_locale(words: &[String]) -> i18n::Locale {
    let explicit = words
        .windows(2)
        .find(|pair| pair[0] == "--locale")
        .and_then(|pair| pair.get(1))
        .cloned();
    match explicit {
        Some(tag) => match i18n::Locale::resolve(&tag) {
            Ok(locale) => locale,
            Err(_) => i18n::Locale::detect(),
        },
        None => i18n::Locale::detect(),
    }
}
