//! ```text
//! zlogic key delete <provider>
//! zlogic key list
//! ```

use std::sync::Arc;

use fluent_bundle::FluentArgs;
use zlogic_engine::bootstrap::{BootstrapError, BootstrapOptions};
use zlogic_engine::{Engine, EngineApi};
use zlogic_protocol::query::{CredentialDeleteReq, CredentialSetReq, CredentialState};

use crate::cli::pick;
use crate::i18n::I18n;

const HELP: &str = "\
zlogic key

Usage:
  zlogic key set                     interactive: pick a provider with ↑/↓, then type the key
  zlogic key set <provider>          interactive: type the key for that provider
  zlogic key set <provider> <value>  write straight to the keychain (entry name = provider id)
  zlogic key delete <provider>       delete that provider's key
  zlogic key list                    list which providers have / do not have a key

Examples:
  zlogic key set
  zlogic key set anthropic
  zlogic key set anthropic sk-ant-xxxxxxxx
  zlogic key list";

pub fn run(words: Vec<String>, locale: crate::i18n::Locale) -> Result<(), String> {
    let i18n = I18n::new(locale);
    match words.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            println!("{HELP}");
            Ok(())
        }
        Some("set") => run_set(&words[1..], &i18n),
        Some("delete") => {
            if words.len() != 2 {
                return Err(i18n.t("key-usage-delete").to_string());
            }
            let state = delete_with(&bootstrap()?, &words[1])?;
            let mut args = FluentArgs::new();
            args.set("provider", state.provider_id.as_str());
            println!("{}", i18n.format("key-status-deleted", Some(&args)));
            Ok(())
        }
        Some("list") => {
            if words.len() != 1 {
                return Err(i18n.t("key-usage-list").to_string());
            }
            let states = list_with(&bootstrap()?)?;
            println!("{}", i18n.t("key-list-title"));
            if states.is_empty() {
                println!("  {}", i18n.t("key-list-empty"));
            }
            for state in states {
                let marker = if state.present {
                    i18n.t("models-key-present")
                } else {
                    i18n.t("models-key-missing")
                };
                println!(
                    "  {:<20} {:<8} {}",
                    state.provider_id,
                    marker,
                    state.candidates.join(", ")
                );
            }
            Ok(())
        }
        Some(other) => {
            let mut args = FluentArgs::new();
            args.set("command", other);
            Err(format!(
                "{}\n\n{HELP}",
                i18n.format("key-unknown-command", Some(&args))
            ))
        }
    }
}

fn run_set(args: &[String], i18n: &I18n) -> Result<(), String> {
    match args.len() {
        0 => interactive_set(&bootstrap()?, None, i18n),
        1 => interactive_set(&bootstrap()?, Some(&args[0]), i18n),
        2 => {
            let state = set_with(
                &bootstrap()?,
                CredentialSetReq {
                    provider_id: args[0].clone(),
                    value: args[1].clone(),
                },
            )?;
            let mut fa = FluentArgs::new();
            fa.set("provider", state.provider_id.as_str());
            fa.set("hint", state.hint.as_deref().unwrap_or(""));
            println!("{}", i18n.format("key-status-set", Some(&fa)));
            Ok(())
        }
        _ => Err(i18n.t("key-usage-set").to_string()),
    }
}

pub fn interactive_set(
    engine: &Arc<dyn EngineApi>,
    provider: Option<&str>,
    i18n: &I18n,
) -> Result<(), String> {
    if !pick::is_interactive() {
        return Err(i18n.t("key-pick-notty").to_string());
    }
    let provider = match provider {
        Some(p) => p.to_string(),
        None => {
            let ids = list_with(engine)?
                .into_iter()
                .map(|state| state.provider_id)
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Err(i18n.t("key-list-empty").to_string());
            }
            let Some(chosen) = pick::pick_provider(&ids, i18n) else {
                return Err(i18n.t("key-cancelled").to_string());
            };
            chosen
        }
    };
    let value = pick::prompt_secret(&provider, i18n)?;
    let state = set_with(
        engine,
        CredentialSetReq {
            provider_id: provider,
            value,
        },
    )?;
    let mut args = FluentArgs::new();
    args.set("provider", state.provider_id.as_str());
    args.set("hint", state.hint.as_deref().unwrap_or(""));
    println!("{}", i18n.format("key-status-set", Some(&args)));
    Ok(())
}

fn bootstrap() -> Result<Arc<dyn EngineApi>, String> {
    Engine::bootstrap(BootstrapOptions::new("cli-rs"))
        .map(|boot| Arc::new(boot.engine) as Arc<dyn EngineApi>)
        .map_err(bootstrap_err)
}

fn bootstrap_err(e: BootstrapError) -> String {
    e.to_string()
}

fn set_with(engine: &Arc<dyn EngineApi>, req: CredentialSetReq) -> Result<CredentialState, String> {
    let engine = Arc::clone(engine);
    runtime().block_on(async move { engine.credential_set(req).await.map_err(|e| e.to_string()) })
}

fn delete_with(engine: &Arc<dyn EngineApi>, provider_id: &str) -> Result<CredentialState, String> {
    let engine = Arc::clone(engine);
    let req = CredentialDeleteReq {
        provider_id: provider_id.to_string(),
    };
    runtime().block_on(async move {
        engine
            .credential_delete(req)
            .await
            .map_err(|e| e.to_string())
    })
}

fn list_with(engine: &Arc<dyn EngineApi>) -> Result<Vec<CredentialState>, String> {
    let engine = Arc::clone(engine);
    runtime().block_on(async move { engine.credential_list().await.map_err(|e| e.to_string()) })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime")
}
