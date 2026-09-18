//! CLI arguments. `--prompt` present → oneshot; else interactive TUI.

use clap::{Parser, ValueEnum};

use crate::session::dto::PermissionMode;

#[derive(Parser, Debug)]
#[command(
    name = "zlogic",
    about = "zlogic TUI (real in-process engine host)",
    after_help = "A first word of `daemon` hands the rest of the command line to `zlogic-daemon` \
(run a local or remote daemon, or manage paired devices): `zlogic daemon --help` shows its own usage."
)]
pub struct Args {
    /// Run once with this prompt (non-interactive) instead of the TUI.
    #[arg(long)]
    pub prompt: Option<String>,

    /// Model preference for the turn / session.
    #[arg(long)]
    pub model: Option<String>,

    /// Theme id, or `auto`.
    #[arg(long, default_value = "auto")]
    pub theme: String,

    /// UI language tag: auto, zh-CN, or en-US.
    #[arg(long, default_value = "auto")]
    pub locale: String,

    /// oneshot output shape.
    #[arg(long, value_enum, default_value_t = Print::Text)]
    pub print: Print,

    /// Enter Plan mode (read-only planning).
    #[arg(long)]
    pub plan: bool,

    /// Working directory for the turn / workspace (defaults to the process cwd).
    #[arg(long)]
    pub cwd: Option<String>,

    /// Resume an existing session by id instead of opening a new one.
    #[arg(long)]
    pub session: Option<String>,

    /// Resume a session: bare `--resume` reopens the most recent one in the
    /// workspace; `--resume <id>` reopens that session. Mutually exclusive with
    /// `--session`.
    #[arg(long, num_args = 0..=1, conflicts_with = "session")]
    pub resume: Option<Option<String>>,

    /// Suppress thinking/tool chatter on stderr (oneshot).
    #[arg(long)]
    pub quiet: bool,

    /// Non-interactive approval posture.
    #[arg(long, value_enum, default_value_t = Perm::Auto)]
    pub permission: Perm,

    /// Full-access mode (godMode): danger theme in TUI; maps to approve-all posture.
    #[arg(long)]
    pub god: bool,

    /// Debug: render markdown to stdout as ANSI and exit — no TUI, pipe-friendly.
    /// Value is a file path; `-` (or omitted-with-piped-stdin) reads stdin. Honors
    /// `--theme` and `--width`. E.g. `zlogic '**hi** `x`' | zlogic --render-md -`.
    #[arg(long, num_args = 0..=1, default_missing_value = "-")]
    pub render_md: Option<String>,

    /// Debug: probe terminal capabilities (env heuristics + live OSC11/CSI 6n/
    /// DECRQM queries), print the result, and exit. No session, no TUI. Handy for
    /// verifying which tier an odd shell (e.g. cmd inside Windows Terminal) lands on.
    #[arg(long, hide = true)]
    pub probe: bool,

    /// Width (columns) for `--render-md`. Defaults to the terminal width, else 80.
    #[arg(long)]
    pub width: Option<u16>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Print {
    Text,
    Json,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Perm {
    Auto,
    Deny,
    ApproveAll,
}

impl From<Perm> for PermissionMode {
    fn from(p: Perm) -> Self {
        match p {
            Perm::Auto => PermissionMode::Auto,
            Perm::Deny => PermissionMode::Deny,
            Perm::ApproveAll => PermissionMode::ApproveAll,
        }
    }
}

impl Args {
    pub fn is_oneshot(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn is_render_md(&self) -> bool {
        self.render_md.is_some()
    }

    /// God mode forces the approve-all posture (and the god theme in the TUI).
    pub fn permission_mode(&self) -> PermissionMode {
        if self.god {
            PermissionMode::ApproveAll
        } else {
            self.permission.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_interactive_auto_everything() {
        let args = Args::parse_from(["zlogic"]);
        assert!(!args.is_oneshot());
        assert_eq!(args.theme, "auto");
        assert_eq!(args.locale, "auto");
        assert_eq!(args.print, Print::Text);
        assert_eq!(args.permission, Perm::Auto);
        assert_eq!(args.permission_mode(), PermissionMode::Auto);
        assert!(!args.god);
        assert!(!args.plan);
        assert!(!args.quiet);
        assert!(args.prompt.is_none());
        assert!(args.model.is_none());
        assert!(args.cwd.is_none());
        assert!(args.session.is_none());
        assert!(args.resume.is_none());
    }

    #[test]
    fn prompt_flag_switches_to_oneshot() {
        let args = Args::parse_from(["zlogic", "--prompt", "hello"]);
        assert!(args.is_oneshot());
        assert_eq!(args.prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn permission_values_map_to_modes() {
        let deny = Args::parse_from(["zlogic", "--permission", "deny"]);
        assert_eq!(deny.permission_mode(), PermissionMode::Deny);

        let approve = Args::parse_from(["zlogic", "--permission", "approve-all"]);
        assert_eq!(approve.permission_mode(), PermissionMode::ApproveAll);

        let auto = Args::parse_from(["zlogic", "--permission", "auto"]);
        assert_eq!(auto.permission_mode(), PermissionMode::Auto);
    }

    #[test]
    fn god_forces_approve_all_over_any_permission_flag() {
        let args = Args::parse_from(["zlogic", "--god"]);
        assert_eq!(args.permission_mode(), PermissionMode::ApproveAll);

        // Even an explicit --permission deny is overridden by --god.
        let args = Args::parse_from(["zlogic", "--god", "--permission", "deny"]);
        assert_eq!(args.permission, Perm::Deny);
        assert_eq!(args.permission_mode(), PermissionMode::ApproveAll);
    }

    #[test]
    fn print_json_parses() {
        let args = Args::parse_from(["zlogic", "--print", "json"]);
        assert_eq!(args.print, Print::Json);
    }

    #[test]
    fn resume_bare_means_most_recent_session() {
        let args = Args::parse_from(["zlogic", "--resume"]);
        assert_eq!(args.resume, Some(None));
    }

    #[test]
    fn resume_with_id_parses() {
        let args = Args::parse_from(["zlogic", "--resume", "some-session-id"]);
        assert_eq!(args.resume, Some(Some("some-session-id".to_string())));
    }

    #[test]
    fn resume_with_equals_id_parses() {
        let args = Args::parse_from(["zlogic", "--resume=some-session-id"]);
        assert_eq!(args.resume, Some(Some("some-session-id".to_string())));
    }

    #[test]
    fn resume_and_session_conflict() {
        assert!(
            Args::try_parse_from(["zlogic", "--resume", "a", "--session", "b"]).is_err(),
            "resuming two different sessions at once must be a clap error"
        );
    }

    #[test]
    fn probe_flag_parses() {
        let args = Args::parse_from(["zlogic", "--probe"]);
        assert!(args.probe);

        // Hidden debug flag coexists with a real run (e.g. --prompt).
        let args = Args::parse_from(["zlogic", "--probe", "--prompt", "hi"]);
        assert!(args.probe);
        assert_eq!(args.prompt.as_deref(), Some("hi"));
    }
}
