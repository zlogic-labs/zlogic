use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zlogic_config::{AppConfig, ConfigFile};
use zlogic_engine::{AwsCredentials, CredentialStore, EngineError, ModelRouter};
use zlogic_llm::transport::{HttpTransport, RecordingTransport, ReplayTransport};
use zlogic_protocol::llm::{CacheSpec, LlmRequest, RequestMeta, ThinkingIntent, ThinkingMode};
use zlogic_protocol::message::{ContentPart, Message, TextPart};
use zlogic_protocol::usage::Purpose;

#[derive(Default)]
struct FakeKeys {
    values: Mutex<HashMap<String, String>>,
    asked: Mutex<Vec<String>>,
}

impl FakeKeys {
    fn with(pairs: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            values: Mutex::new(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn set(&self, reference: &str, value: &str) {
        self.values
            .lock()
            .unwrap()
            .insert(reference.to_string(), value.to_string());
    }
}

impl CredentialStore for FakeKeys {
    fn resolve(&self, credential_ref: &str) -> Option<String> {
        self.asked.lock().unwrap().push(credential_ref.to_string());
        self.values.lock().unwrap().get(credential_ref).cloned()
    }

    fn aws(&self) -> Option<AwsCredentials> {
        None
    }
}

fn transport() -> Arc<dyn HttpTransport> {
    Arc::new(ReplayTransport::whole(""))
}

fn config(yaml: &str) -> Arc<AppConfig> {
    let file: ConfigFile = serde_yaml_ng::from_str(yaml).unwrap();
    let mut cfg = AppConfig {
        revision: 1,
        ..Default::default()
    };
    cfg.apply(file);
    cfg.validate().unwrap();
    Arc::new(cfg)
}

fn router(yaml: &str, keys: &[(&str, &str)]) -> ModelRouter {
    ModelRouter::new(config(yaml), transport(), FakeKeys::with(keys))
}

#[test]
fn replacing_config_makes_a_newly_added_model_routable() {
    let r = router(
        r#"
        providers:
          old:
            sdk: deepseek
            models:
              first: { context_window: 64000 }
        "#,
        &[("env:OLD_API_KEY", "old"), ("env:NAMC_API_KEY", "new")],
    );
    assert_eq!(
        r.resolve(&Purpose::Main, Some("namc:qwen"))
            .unwrap()
            .model_ref(),
        "old:first"
    );

    r.replace_config(config(
        r#"
        providers:
          namc:
            sdk: deepseek
            models:
              qwen: { context_window: 128000 }
        "#,
    ));

    assert_eq!(
        r.resolve(&Purpose::Main, Some("namc:qwen"))
            .unwrap()
            .model_ref(),
        "namc:qwen"
    );
}

const TIERED: &str = r#"
default_model: big:opus
providers:
  big:
    sdk: anthropic
    models:
      opus:
        tier: main
        context_window: 200000
  cheap:
    sdk: deepseek
    models:
      small:
        tier: light
        context_window: 64000
        no_think_params:
          thinking: { type: disabled }
"#;

const KEYS: &[(&str, &str)] = &[("env:BIG_API_KEY", "k1"), ("env:CHEAP_API_KEY", "k2")];

#[test]
fn the_main_role_is_the_session_model() {
    let r = router(TIERED, KEYS);
    let routed = r.resolve(&Purpose::Main, Some("cheap:small")).unwrap();
    assert_eq!(
        routed.model_ref(),
        "cheap:small",
        "whatever the session picked is what is used"
    );
    assert_eq!(routed.via, "session");
}

#[test]
fn short_output_roles_prefer_the_session_model_then_light() {
    let r = router(TIERED, KEYS);
    for role in [Purpose::Title, Purpose::Approval, Purpose::Utility] {
        let routed = r.resolve(&role, Some("big:opus")).unwrap();
        assert_eq!(
            routed.model_ref(),
            "big:opus",
            "{role} should use the session model"
        );
        assert_eq!(routed.via, "session");
    }
}

#[test]
fn short_output_roles_fall_back_when_session_is_unavailable() {
    let r = router(TIERED, &[("env:CHEAP_API_KEY", "k2")]);
    for role in [Purpose::Title, Purpose::Approval, Purpose::Utility] {
        let routed = r.resolve(&role, Some("big:opus")).unwrap();
        assert_eq!(
            routed.model_ref(),
            "cheap:small",
            "{role} should fall back to the light tier when the session model is unavailable"
        );
    }
}

#[test]
fn compaction_runs_on_the_session_model() {
    let r = router(TIERED, KEYS);
    let routed = r.resolve(&Purpose::Compaction, Some("cheap:small")).unwrap();
    assert_eq!(
        routed.model_ref(),
        "cheap:small",
        "the summary is written by the conversation's own model"
    );
    assert_eq!(routed.via, "session");
}

#[test]
fn deep_approval_prefers_the_session_model() {
    let r = router(TIERED, KEYS);
    let routed = r.resolve(&Purpose::ApprovalDeep, Some("cheap:small")).unwrap();
    assert_eq!(
        routed.model_ref(),
        "cheap:small",
        "deep approval starts from the conversation's own model, like every other role"
    );
    assert_eq!(routed.via, "session");
}

#[test]
fn with_no_tiers_configured_every_role_falls_back_to_session() {
    let r = router(
        r#"
        default_model: p:m
        providers:
          p:
            sdk: deepseek
            models:
              m: { context_window: 100000 }
        "#,
        &[("env:P_API_KEY", "k")],
    );
    for role in [
        Purpose::Main,
        Purpose::Title,
        Purpose::Compaction,
        Purpose::Utility,
    ] {
        let routed = r.resolve(&role, Some("p:m")).unwrap();
        assert_eq!(routed.model_ref(), "p:m");
        assert_eq!(
            routed.via, "session",
            "{role} fell through to the end of the chain"
        );
    }
}

#[test]
fn a_sub_agent_defaults_to_the_session_model_and_can_be_overridden() {
    let r = router(
        &format!("{TIERED}\nllm_roles:\n  \"agent:researcher\":\n    models: [cheap:small]\n"),
        KEYS,
    );
    assert_eq!(
        r.resolve(&Purpose::Agent("reviewer".into()), Some("big:opus"))
            .unwrap()
            .model_ref(),
        "big:opus",
        "an unconfigured sub-agent uses the session model"
    );
    assert_eq!(
        r.resolve(&Purpose::Agent("researcher".into()), Some("big:opus"))
            .unwrap()
            .model_ref(),
        "cheap:small",
        "a configured one follows the configuration"
    );
}

#[test]
fn a_configured_chain_replaces_the_builtin_one() {
    let r = router(
        &format!("{TIERED}\nllm_roles:\n  title:\n    models: [big:opus]\n"),
        KEYS,
    );
    assert_eq!(
        r.resolve(&Purpose::Title, Some("cheap:small"))
            .unwrap()
            .model_ref(),
        "big:opus"
    );
}

#[test]
fn session_terminates_every_chain_even_when_not_written() {
    let r = router(
        &format!("{TIERED}\nllm_roles:\n  title:\n    models: [ghost:nothing]\n"),
        KEYS,
    );
    let routed = r.resolve(&Purpose::Title, Some("big:opus")).unwrap();
    assert_eq!(
        routed.model_ref(),
        "big:opus",
        "after a candidate that does not exist it falls back to the session model"
    );
    assert_eq!(routed.via, "session");
}

#[test]
fn a_candidate_without_a_usable_credential_is_skipped() {
    let r = router(TIERED, &[("env:BIG_API_KEY", "k1")]);
    let routed = r.resolve(&Purpose::Title, Some("big:opus")).unwrap();
    assert_eq!(
        routed.model_ref(),
        "big:opus",
        "the light tier has no key, so it falls back to the session model"
    );
}

#[test]
fn a_tier_with_several_models_tries_them_in_turn() {
    let yaml = r#"
    default_model: big:opus
    providers:
      big:
        sdk: anthropic
        models:
          opus: { tier: main, context_window: 200000 }
      a:
        sdk: deepseek
        models:
          first: { tier: light, context_window: 64000 }
      b:
        sdk: deepseek
        models:
          second: { tier: light, context_window: 64000 }
    "#;
    let r = router(yaml, &[("env:B_API_KEY", "k")]);
    assert_eq!(
        r.resolve(&Purpose::Title, Some("big:opus"))
            .unwrap()
            .model_ref(),
        "b:second"
    );
}

#[test]
fn no_usable_model_reports_what_it_tried() {
    let r = router(
        r#"
        providers:
          p:
            sdk: deepseek
            models:
              m: { context_window: 1000 }
        "#,
        &[],
    );
    match r.resolve(&Purpose::Main, Some("p:m")) {
        Err(EngineError::NoModel { role, tried }) => {
            assert_eq!(role, "main");
            assert!(tried.contains(&"p:m".to_string()), "{tried:?}");
        }
        other => panic!("{:?}", other.map(|r| r.model_ref())),
    }
}

#[test]
fn a_stale_session_pin_falls_back_to_the_default_chain() {
    let r = router(
        r#"
        default_model: big:opus
        providers:
          big:
            sdk: anthropic
            models:
              opus: { context_window: 200000 }
        "#,
        &[("env:BIG_API_KEY", "k")],
    );
    let routed = r.resolve(&Purpose::Main, Some("gone:opus")).unwrap();
    assert_eq!(
        routed.model_ref(),
        "big:opus",
        "the old pin no longer works → falls back to default_model"
    );
    assert_eq!(routed.via, "session");

    assert_eq!(
        r.resolve(&Purpose::Main, Some("big:opus"))
            .unwrap()
            .model_ref(),
        "big:opus"
    );
}

#[test]
fn params_merge_in_the_documented_order() {
    let yaml = r#"
    default_model: p:m
    providers:
      p:
        sdk: deepseek
        models:
          m:
            context_window: 64000
            default_params: { temperature: 0.9, top_p: 0.8 }
            no_think_params: { temperature: 0.1, reasoning: false }
    llm_roles:
      compaction:
        models: [p:m]
        params: { temperature: 0.3 }
    "#;
    let r = router(yaml, &[("env:P_API_KEY", "k")]);

    let compaction = r.resolve(&Purpose::Compaction, Some("p:m")).unwrap();
    let p = &compaction.model.default_params;
    assert_eq!(p["temperature"], 0.3, "the role's params win");
    assert_eq!(p["reasoning"], false, "no_think keeps its other entries");
    assert_eq!(p["top_p"], 0.8, "the model's own other entries are kept");

    let main = r.resolve(&Purpose::Main, Some("p:m")).unwrap();
    assert_eq!(main.model.default_params["temperature"], 0.9);
    assert!(!main.model.default_params.contains_key("reasoning"));
}

#[test]
fn auxiliary_roles_turn_thinking_off_by_default() {
    use zlogic_protocol::llm::ThinkingMode;
    let r = router(TIERED, KEYS);

    for role in [
        Purpose::Title,
        Purpose::Compaction,
        Purpose::Approval,
        Purpose::ApprovalDeep,
        Purpose::Utility,
    ] {
        let routed = r.resolve(&role, Some("big:opus")).unwrap();
        assert_eq!(
            routed.thinking.mode,
            ThinkingMode::Off,
            "{role} should turn thinking off"
        );
    }
    for role in [Purpose::Main, Purpose::Agent("x".into())] {
        let routed = r.resolve(&role, Some("big:opus")).unwrap();
        assert_eq!(routed.thinking.mode, ThinkingMode::Default);
    }
}

#[test]
fn a_role_can_flip_thinking_either_way() {
    use zlogic_protocol::llm::ThinkingMode;
    let yaml = format!(
        "{TIERED}\nllm_roles:\n  title:\n    models: [big:opus]\n    thinking: on\n  \"agent:x\":\n    thinking: off\n"
    );
    let r = router(&yaml, KEYS);

    assert_eq!(
        r.resolve(&Purpose::Title, Some("big:opus"))
            .unwrap()
            .thinking
            .mode,
        ThinkingMode::Default,
        "after an explicit on it is no longer forced off"
    );
    assert_eq!(
        r.resolve(&Purpose::Agent("x".into()), Some("big:opus"))
            .unwrap()
            .thinking
            .mode,
        ThinkingMode::Off
    );
}

#[test]
fn no_think_params_are_not_applied_when_thinking_is_on() {
    let yaml =
        format!("{TIERED}\nllm_roles:\n  title:\n    models: [cheap:small]\n    thinking: on\n");
    let r = router(&yaml, KEYS);
    let routed = r.resolve(&Purpose::Title, Some("big:opus")).unwrap();
    assert!(
        !routed.model.default_params.contains_key("thinking"),
        "thinking is on and yet the params that turn it off were merged in: {:?}",
        routed.model.default_params
    );
}

#[test]
fn each_resolve_builds_a_fresh_client_so_the_key_is_reread() {
    let keys = FakeKeys::with(KEYS);
    let r = ModelRouter::new(config(TIERED), transport(), keys.clone());

    let a = r.resolve(&Purpose::Title, Some("big:opus")).unwrap();
    let b = r.resolve(&Purpose::Utility, Some("big:opus")).unwrap();
    assert_eq!(a.model_ref(), b.model_ref(), "the same model");
    assert!(
        !Arc::ptr_eq(&a.client, &b.client),
        "not cached — reusing it would make the second call skip reading the key"
    );
    assert_eq!(
        keys.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|r| *r == "env:BIG_API_KEY")
            .count(),
        2
    );
}

#[test]
fn a_rotated_key_takes_effect_on_the_next_resolve() {
    let keys = FakeKeys::with(&[("env:BIG_API_KEY", "old")]);
    let r = ModelRouter::new(config(TIERED), transport(), keys.clone());
    assert_eq!(
        r.resolve(&Purpose::Main, Some("big:opus"))
            .unwrap()
            .model_ref(),
        "big:opus"
    );

    keys.set("env:BIG_API_KEY", "new");
    r.resolve(&Purpose::Main, Some("big:opus")).unwrap();

    let asked = keys.asked.lock().unwrap();
    assert!(
        asked.iter().filter(|r| *r == "env:BIG_API_KEY").count() >= 2,
        "the second resolve has to ask again, otherwise it still gets the old one: {asked:?}"
    );
}

#[test]
fn adding_a_missing_key_takes_effect_without_a_restart() {
    let keys = FakeKeys::with(&[]);
    let r = ModelRouter::new(config(TIERED), transport(), keys.clone());
    assert!(matches!(
        r.resolve(&Purpose::Main, Some("big:opus")),
        Err(EngineError::NoModel { .. })
    ));

    keys.set("env:BIG_API_KEY", "k");
    assert_eq!(
        r.resolve(&Purpose::Main, Some("big:opus"))
            .unwrap()
            .model_ref(),
        "big:opus"
    );
}

#[test]
fn replacing_the_config_switches_later_routing() {
    let r = router(
        TIERED,
        &[("env:CHEAP_API_KEY", "k2"), ("env:OTHER_API_KEY", "k3")],
    );
    assert_eq!(
        r.resolve(&Purpose::Title, Some("big:opus"))
            .unwrap()
            .model_ref(),
        "cheap:small",
        "in TIERED the light tier is cheap:small"
    );

    let next = config(
        r#"
        default_model: big:opus
        providers:
          big: { sdk: anthropic, models: { opus: { tier: main, context_window: 200000 } } }
          other: { sdk: deepseek, models: { alt: { tier: light, context_window: 64000 } } }
    "#,
    );
    r.replace_config(next);
    assert_eq!(
        r.resolve(&Purpose::Title, Some("big:opus"))
            .unwrap()
            .model_ref(),
        "other:alt",
        "after the reload the light tier moves to other:alt, and routing follows it"
    );
}

#[test]
fn credentials_are_read_at_resolve_time_and_never_stored() {
    let keys = FakeKeys::with(KEYS);
    let r = ModelRouter::new(config(TIERED), transport(), keys.clone());
    assert!(
        keys.asked.lock().unwrap().is_empty(),
        "loading the configuration must not read a key"
    );

    let routed = r.resolve(&Purpose::Main, Some("big:opus")).unwrap();
    assert_eq!(
        keys.asked.lock().unwrap().as_slice(),
        ["env:ZLOGIC_BIG_API_KEY", "env:BIG_API_KEY"]
    );

    let dumped = format!("{:?}", routed.model);
    assert!(dumped.contains("env:BIG_API_KEY"));
    assert!(
        !dumped.contains("k1"),
        "the key's value must not appear in ResolvedModel"
    );
}

/// The subscription provider as `defaults/codex.yaml` declares it: the backend is not
/// `api.openai.com`, and there is no path `/v1` under it.
const SUBSCRIPTION: &str = r#"
default_model: codex:gpt-test
providers:
  codex:
    sdk: openai_responses
    auth: chatgpt
    base_url: https://chatgpt.com/backend-api/codex
    wiring:
      path: responses
    models:
      gpt-test:
        context_window: 400000
        display_name: GPT Test
"#;

/// A stored sign-in, as `zlogic auth login codex` would have written it.
fn subscription_tokens() -> String {
    serde_json::json!({
        "access_token": "access-1",
        "refresh_token": "refresh-1",
        "expires_at": 9_999_999_999_999i64,
        "account_id": "acct-1",
    })
    .to_string()
}

fn request(model: &str) -> LlmRequest {
    LlmRequest {
        model: model.to_string(),
        system: Vec::new(),
        messages: vec![Message::user(vec![ContentPart::Text(TextPart {
            text: "hello".into(),
            raw: None,
            truncated: false,
        })])],
        tools: Vec::new(),
        thinking: ThinkingIntent {
            mode: ThinkingMode::Off,
            ..Default::default()
        },
        params: Default::default(),
        response_format: None,
        cache: CacheSpec::off(),
        meta: RequestMeta {
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            round_id: "round-1".into(),
            purpose: Purpose::Main,
        },
    }
}

#[tokio::test]
async fn a_subscription_model_sends_its_token_and_the_codex_headers() {
    let recorder = RecordingTransport::new();
    let keys = FakeKeys::with(&[("keyring:codex_oauth", &subscription_tokens())]);
    let router = ModelRouter::new(
        config(SUBSCRIPTION),
        Arc::new(recorder.clone()),
        keys.clone(),
    );

    let routed = router
        .resolve(&Purpose::Main, Some("codex:gpt-test"))
        .unwrap();
    assert_eq!(routed.model_ref(), "codex:gpt-test");

    let mut stream = routed.client.stream(request("gpt-test")).await.unwrap();
    while futures_util::StreamExt::next(&mut stream).await.is_some() {}

    let sent = recorder.last().expect("the request has to reach the wire");
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(
        sent.url, "https://chatgpt.com/backend-api/codex/responses",
        "a subscription has no /v1 and no api.openai.com"
    );
    assert_eq!(
        header("authorization").as_deref(),
        Some("Bearer access-1"),
        "the token is read per request, not baked into the client"
    );
    assert_eq!(header("chatgpt-account-id").as_deref(), Some("acct-1"));
    assert_eq!(header("session_id").as_deref(), Some("session-1"));
    assert!(header("originator").is_some());
    assert!(
        keys.asked
            .lock()
            .unwrap()
            .iter()
            .all(|reference| reference == "keyring:codex_oauth"),
        "a subscription looks in exactly one place: {:?}",
        keys.asked.lock().unwrap()
    );
}

#[test]
fn a_subscription_with_no_sign_in_is_not_routed_anywhere_else() {
    let router = router(SUBSCRIPTION, &[]);
    // Routing reports the chain it walked, not the reason each candidate was dropped; the reason
    // (`provider codex is not signed in`) is logged by the client builder that refused it.
    match router.resolve(&Purpose::Main, Some("codex:gpt-test")) {
        Err(EngineError::NoModel { role, tried }) => {
            assert_eq!(role, "main");
            assert!(tried.contains(&"codex:gpt-test".to_string()), "{tried:?}");
        }
        other => panic!("{:?}", other.map(|routed| routed.model_ref())),
    }
}
