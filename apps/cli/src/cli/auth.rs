//! ```text
//! zlogic auth                    which providers are signed in
//! zlogic auth login <provider>   sign a subscription provider in (browser, or --device)
//! zlogic auth logout <provider>  forget the stored subscription
//! zlogic auth refresh <provider> ask the backend which models the account may call
//! ```

use std::sync::Arc;
use std::time::Duration;

use fluent_bundle::FluentArgs;
use zlogic_engine::bootstrap::{BootstrapError, BootstrapOptions};
use zlogic_engine::{Engine, EngineApi};
use zlogic_protocol::query::{
    ApiResult, CredentialDeleteReq, ProviderModelsReq, ProviderSignInBeginReq,
    ProviderSignInMethod, ProviderSignInState, ProviderSignInStatusReq,
};

use crate::i18n::I18n;

/// Drive one engine call to completion. Each command owns its runtime; the engine calls are the
/// only asynchronous thing in this file.
fn run_api<T>(future: impl std::future::Future<Output = ApiResult<T>>) -> Result<T, String> {
    runtime()
        .block_on(future)
        .map_err(|error| error.to_string())
}

const HELP: &str = "\
zlogic auth

Usage:
  zlogic auth                      list which providers are signed in
  zlogic auth list                 the same, spelled out
  zlogic auth login <provider>     sign in to a subscription provider (opens a browser)
  zlogic auth login <provider> --device
                                   sign in with a device code instead (headless)
  zlogic auth logout <provider>    forget the stored subscription
  zlogic auth refresh <provider>   re-read the model list the provider offers

Examples:
  zlogic auth
  zlogic auth login codex
  zlogic auth login codex --device
  zlogic auth refresh codex
  zlogic auth logout codex";

pub fn run(words: Vec<String>, locale: crate::i18n::Locale) -> Result<(), String> {
    let i18n = I18n::new(locale);
    match words.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{HELP}");
            Ok(())
        }
        None => list_with(&bootstrap()?, &i18n),
        Some("list") => {
            if words.len() != 1 {
                return Err(i18n.t("auth-usage-list"));
            }
            list_with(&bootstrap()?, &i18n)
        }
        Some("login") => run_login(&words[1..], &i18n),
        Some("logout") => run_logout(&words[1..], &i18n),
        Some("refresh") => run_refresh(&words[1..], &i18n),
        Some(other) => {
            let mut args = FluentArgs::new();
            args.set("command", other);
            Err(format!(
                "{}\n\n{HELP}",
                i18n.format("auth-unknown-command", Some(&args))
            ))
        }
    }
}

fn run_login(args: &[String], i18n: &I18n) -> Result<(), String> {
    let device = args.iter().any(|arg| arg == "--device");
    let named: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with("--"))
        .cloned()
        .collect();
    if named.len() != 1 {
        return Err(i18n.t("auth-usage-login"));
    }
    let provider = named[0].clone();
    let engine = bootstrap()?;
    let method = if device {
        ProviderSignInMethod::Device
    } else {
        ProviderSignInMethod::Browser
    };

    let begin = run_api({
        let engine = Arc::clone(&engine);
        let provider = provider.clone();
        async move {
            engine
                .credential_sign_in_begin(ProviderSignInBeginReq {
                    provider_id: provider,
                    method,
                })
                .await
        }
    })?;

    match &begin.user_code {
        Some(code) => {
            let url = begin.authorization_url.clone();
            let code = code.clone();
            let mut values = FluentArgs::new();
            values.set("url", url.as_str());
            values.set("code", code.as_str());
            println!("{}", i18n.format("auth-device-code", Some(&values)));
        }
        None => {
            let url = begin.authorization_url.clone();
            let mut values = FluentArgs::new();
            values.set("url", url.as_str());
            println!("{}", i18n.format("auth-open-url", Some(&values)));
            if let Err(reason) = open_in_browser(&url) {
                let mut values = FluentArgs::new();
                values.set("reason", reason.as_str());
                eprintln!("{}", i18n.format("auth-open-failed", Some(&values)));
            }
        }
    }
    println!("{}", i18n.t("auth-waiting"));

    match runtime().block_on(poll(Arc::clone(&engine), begin.flow_id))? {
        ProviderSignInState::Succeeded { label, plan, .. } => {
            let account = label.unwrap_or_else(|| provider.clone());
            let plan = plan.unwrap_or_default();
            let mut values = FluentArgs::new();
            values.set("account", account.as_str());
            values.set("plan", plan.as_str());
            println!("{}", i18n.format("auth-signed-in-as", Some(&values)));
        }
        ProviderSignInState::Failed { message } => {
            let mut values = FluentArgs::new();
            values.set("message", message.as_str());
            return Err(i18n.format("auth-sign-in-failed", Some(&values)));
        }
        ProviderSignInState::Expired => return Err(i18n.t("auth-sign-in-expired")),
        ProviderSignInState::Cancelled => return Err(i18n.t("auth-sign-in-cancelled")),
        ProviderSignInState::Pending => return Err(i18n.t("auth-sign-in-pending")),
    }

    // Signed in is the successful end of this command; what the plan may call is the next question,
    // so a model list that cannot be fetched is a warning and not a failure.
    match models_with(&engine, &provider, i18n) {
        Ok(count) => {
            let mut values = FluentArgs::new();
            values.set("count", count as i64);
            values.set("provider", provider.as_str());
            println!("{}", i18n.format("auth-models-count", Some(&values)));
        }
        Err(error) => eprintln!("{error}"),
    }
    Ok(())
}

async fn poll(engine: Arc<dyn EngineApi>, flow_id: String) -> Result<ProviderSignInState, String> {
    loop {
        let status = engine
            .credential_sign_in_status(ProviderSignInStatusReq {
                flow_id: flow_id.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        match status.state {
            ProviderSignInState::Pending => tokio::time::sleep(Duration::from_millis(1000)).await,
            decided => return Ok(decided),
        }
    }
}

fn run_logout(args: &[String], i18n: &I18n) -> Result<(), String> {
    if args.len() != 1 {
        return Err(i18n.t("auth-usage-logout"));
    }
    let provider = args[0].clone();
    let engine = bootstrap()?;
    run_api({
        let engine = Arc::clone(&engine);
        let provider = provider.clone();
        async move {
            engine
                .credential_delete(CredentialDeleteReq {
                    provider_id: provider,
                })
                .await
        }
    })?;
    let mut values = FluentArgs::new();
    values.set("provider", provider.as_str());
    println!("{}", i18n.format("auth-signed-out", Some(&values)));
    Ok(())
}

fn run_refresh(args: &[String], i18n: &I18n) -> Result<(), String> {
    if args.len() != 1 {
        return Err(i18n.t("auth-usage-refresh"));
    }
    let provider = args[0].clone();
    let engine = bootstrap()?;
    let count = models_with(&engine, &provider, i18n)?;
    let mut values = FluentArgs::new();
    values.set("count", count as i64);
    values.set("provider", provider.as_str());
    println!("{}", i18n.format("auth-models-count", Some(&values)));
    Ok(())
}

fn models_with(engine: &Arc<dyn EngineApi>, provider: &str, i18n: &I18n) -> Result<usize, String> {
    let result = run_api({
        let engine = Arc::clone(engine);
        let provider = provider.to_string();
        async move {
            engine
                .provider_models(ProviderModelsReq {
                    provider_id: provider,
                })
                .await
        }
    })?;

    let mut values = FluentArgs::new();
    values.set("provider", provider);
    println!("{}", i18n.format("auth-models-title", Some(&values)));
    for model in &result.models {
        println!(
            "  {:<26} {:<28} {:>9}",
            model.model_id,
            model.display_name.as_deref().unwrap_or(""),
            model.context_window
        );
    }
    Ok(result.models.len())
}

fn list_with(engine: &Arc<dyn EngineApi>, i18n: &I18n) -> Result<(), String> {
    let states = run_api({
        let engine = Arc::clone(engine);
        async move { engine.credential_list().await }
    })?;

    println!("{}", i18n.t("auth-list-title"));
    if states.is_empty() {
        println!("  {}", i18n.t("auth-list-empty"));
        return Ok(());
    }
    for state in states {
        let marker = if state.present {
            i18n.t("auth-marker-in")
        } else {
            i18n.t("auth-marker-out")
        };
        match state.hint {
            Some(hint) if !hint.trim().is_empty() => {
                println!("  {:<20} {:<14} {hint}", state.provider_id, marker)
            }
            _ => println!("  {:<20} {marker}", state.provider_id),
        }
    }
    Ok(())
}

fn open_in_browser(url: &str) -> Result<(), String> {
    let mut command = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .args(browser_args(url))
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// How the URL is handed to the system's browser.
///
/// Windows has no shell-free way to ask for the default handler, so it goes through `cmd` — and
/// there the URL has to be quoted, because an authorization URL is full of `&`, which `cmd` would
/// otherwise read as a command separator and open a truncated link.
fn browser_args(url: &str) -> Vec<String> {
    if cfg!(target_os = "windows") {
        vec!["/C".into(), "start".into(), "".into(), format!("\"{url}\"")]
    } else {
        vec![url.to_string()]
    }
}

fn bootstrap() -> Result<Arc<dyn EngineApi>, String> {
    Engine::bootstrap(BootstrapOptions::new("cli-rs"))
        .map(|boot| Arc::new(boot.engine) as Arc<dyn EngineApi>)
        .map_err(|error: BootstrapError| error.to_string())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn a_bad_command_line_is_answered_before_anything_is_bootstrapped() {
        for (args, expected) in [
            (vec!["login"], "Usage: zlogic auth login"),
            (
                vec!["login", "codex", "--device", "extra"],
                "Usage: zlogic auth login",
            ),
            (vec!["logout"], "Usage: zlogic auth logout"),
            (
                vec!["logout", "codex", "extra"],
                "Usage: zlogic auth logout",
            ),
            (vec!["refresh"], "Usage: zlogic auth refresh"),
        ] {
            let error = run(words(&args), Locale::EnUs).unwrap_err();
            assert!(error.contains(expected), "{args:?}: {error}");
        }
    }

    #[test]
    fn an_unknown_subcommand_shows_the_whole_usage() {
        let error = run(words(&["signin"]), Locale::EnUs).unwrap_err();
        assert!(error.contains("Unknown auth subcommand: signin"), "{error}");
        assert!(
            error.contains("zlogic auth login <provider>"),
            "the listing of what does exist follows: {error}"
        );
    }

    #[test]
    fn the_help_text_lists_every_subcommand() {
        assert!(run(words(&["--help"]), Locale::EnUs).is_ok());
        for command in ["login", "logout", "refresh"] {
            assert!(HELP.contains(command), "`{command}` is missing from {HELP}");
        }
    }

    #[test]
    fn the_url_reaches_the_browser_intact() {
        // An authorization URL is full of `&`; whatever this returns has to keep it in one piece.
        let url = "http://127.0.0.1:1455/auth/callback?code=1&state=2";
        let args = browser_args(url);
        if cfg!(target_os = "windows") {
            assert_eq!(args.last().unwrap(), &format!("\"{url}\""));
        } else {
            assert_eq!(args, [url.to_string()]);
        }
    }
}
