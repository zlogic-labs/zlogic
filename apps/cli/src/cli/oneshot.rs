//! Non-interactive single run. No TUI, no alt-screen, no raw mode — pure
//! pipe-friendly output. Interactive events are answered by a fail-closed policy
//! responder: never skip them (they'd hang core), never silently approve.

use std::io::Write;

use crate::cli::args::{Args, Print};
use crate::session::dto::*;
use crate::session::CoreSession;

/// Returns the process exit code (0 Done / 1 Error / 130 handled elsewhere).
pub fn run(args: &Args, session: Box<dyn CoreSession>) -> i32 {
    let events = session.subscribe();
    let permission = args.permission_mode();

    let prompt = args.prompt.clone().unwrap_or_default();
    let started = session.send(Command::TurnStart {
        messages: vec![Message::text(prompt)],
        model: args.model.clone(),
        cwd: args.cwd.clone(),
        plan: args.plan,
        permission,
    });
    if !started && session.has_usable_model() {
        eprintln!(
            "zlogic: message was queued but no turn started (session busy or held by another process) — non-interactive mode has nothing to wait for"
        );
        return 1;
    }

    let json = args.print == Print::Json;
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut exit = 0;

    macro_rules! write_output {
        ($expr:expr) => {
            if let Err(error) = $expr {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    return 0;
                }
                eprintln!("zlogic: output error: {error}");
                return 1;
            }
        };
    }

    for ev in events.iter() {
        if json {
            let line = match serde_json::to_string(&ev) {
                Ok(line) => line,
                Err(error) => {
                    eprintln!("zlogic: cannot encode event: {error}");
                    return 1;
                }
            };
            write_output!(writeln!(stdout, "{line}"));
        }

        match &ev {
            CoreEvent::TextDelta { text, .. } if !json => {
                write_output!(write!(stdout, "{text}"));
                write_output!(stdout.flush());
            }
            CoreEvent::ThinkingDelta { text, .. } if !json && !args.quiet => {
                write_output!(write!(stderr, "{text}"));
            }
            CoreEvent::ToolCallStart { name, .. } if !json && !args.quiet => {
                write_output!(writeln!(stderr, "\n· {name}…"));
            }

            // ── policy responder for blocking interactive events ──
            CoreEvent::PermissionRequest {
                id, action, target, ..
            } => {
                let answer = match permission {
                    PermissionMode::ApproveAll => Answer::Allow {
                        scope: GrantScope::Session,
                    },
                    // Auto/Deny both fail-closed on an escalated ask.
                    _ => Answer::Deny {
                        message: Some("non-interactive run".into()),
                    },
                };
                if !args.quiet {
                    let verb = matches!(answer, Answer::Allow { .. });
                    write_output!(writeln!(
                        stderr,
                        "· permission {action} {target} → {}",
                        if verb { "allow" } else { "deny (fail-closed)" }
                    ));
                }
                session.send(Command::Respond {
                    id: id.clone(),
                    answer,
                });
            }
            CoreEvent::ConfirmationRequest { id, .. } => {
                session.send(Command::Respond {
                    id: id.clone(),
                    answer: Answer::Deny {
                        message: Some("non-interactive run".into()),
                    },
                });
            }
            CoreEvent::InputRequest { id, .. } => {
                session.send(Command::Respond {
                    id: id.clone(),
                    answer: Answer::Input {
                        text: "(non-interactive: proceed with best judgment or stop)".into(),
                    },
                });
            }
            CoreEvent::FormRequest { id, .. } => {
                session.send(Command::Respond {
                    id: id.clone(),
                    answer: Answer::Deny {
                        message: Some("non-interactive run".into()),
                    },
                });
            }

            CoreEvent::Error { message } => {
                if !json {
                    write_output!(writeln!(stderr, "\nerror: {message}"));
                }
                exit = 1;
                break;
            }
            CoreEvent::TurnDone { turn_id } => {
                session.send(Command::RequestTurnSummary {
                    turn_id: turn_id.clone(),
                });
            }
            CoreEvent::TurnSummaryLoaded { .. } => break,
            _ => {}
        }
    }

    if !json {
        write_output!(writeln!(stdout));
    }
    exit
}
