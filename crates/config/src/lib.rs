//! # zlogic-config
//! |---|---|---|
//! ```text
//! ```
pub mod catalog;
pub mod dirs;
pub mod prices;
pub mod provider;
pub mod write;

pub use zlogic_protocol::{roles, settings};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zlogic_credential::{CredentialRef, credential_candidates};
use zlogic_protocol::config::{ResolvedModel, Tier};

pub use catalog::CatalogFile;
pub use dirs::Dirs;
pub use prices::PriceFile;
pub use provider::{ModelSettings, ProviderSettings, detect_providers, resolve_model};
pub use roles::{RoleSettings, RoleThinking, SESSION};
pub use settings::{
    ApprovalMode, AutoTitle, ContextConfig, CostConfig, ExchangeRate, LimitsConfig, LogConfig,
    NetworkSettings, SessionConfig, ShellPreference, ToolsConfig, WebSearchConfig, WorktreeConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("environment problem: {0}")]
    Env(String),
    #[error("failed to read/write {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("invalid provider configuration: {0}")]
    Provider(String),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("application name {app:?}: {reason}")]
    AppName { app: String, reason: String },
    #[error("model not found: {0}")]
    UnknownModel(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    pub default_model: Option<String>,
    pub providers: BTreeMap<String, ProviderSettings>,
    #[serde(default)]
    pub llm_roles: BTreeMap<String, RoleSettings>,
    pub session: Option<SessionConfig>,
    pub context: Option<ContextConfig>,
    pub tools: Option<ToolsConfig>,
    pub log: Option<LogConfig>,
    pub worktree: Option<WorktreeConfig>,
    pub cost: Option<CostConfig>,
    pub limits: Option<LimitsConfig>,
    pub network: Option<NetworkSettings>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsFile {
    pub auto_detect_env: Option<bool>,
    pub providers: BTreeMap<String, ProviderSettings>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub default_model: Option<String>,
    pub auto_detect_env: bool,
    pub providers: BTreeMap<String, ProviderSettings>,
    pub llm_roles: BTreeMap<String, RoleSettings>,
    pub session: SessionConfig,
    pub context: ContextConfig,
    pub tools: ToolsConfig,
    pub log: LogConfig,
    pub worktree: WorktreeConfig,
    pub cost: CostConfig,
    pub limits: LimitsConfig,
    pub network: NetworkSettings,
    pub revision: u64,
    pub warnings: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            auto_detect_env: true,
            providers: BTreeMap::new(),
            llm_roles: BTreeMap::new(),
            session: SessionConfig::default(),
            context: ContextConfig::default(),
            tools: ToolsConfig::default(),
            log: LogConfig::default(),
            worktree: WorktreeConfig::default(),
            cost: CostConfig::default(),
            limits: LimitsConfig::default(),
            network: NetworkSettings::default(),
            revision: 0,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigFiles {
    pub models: Option<ModelsFile>,
    pub config: Option<ConfigFile>,
    pub prices: Option<PriceFile>,
    pub catalog: Option<CatalogFile>,
}

impl ConfigFiles {
    pub fn read(dirs: &Dirs) -> Result<Self> {
        Ok(Self {
            models: read_optional::<ModelsFile>(&dirs.models_file())?,
            config: read_optional::<ConfigFile>(&dirs.config_file())?,
            prices: PriceFile::read(dirs),
            catalog: CatalogFile::read(dirs),
        })
    }
}

impl AppConfig {
    pub fn load(dirs: &Dirs, probe: impl Fn(&CredentialRef) -> bool) -> Result<Self> {
        let seeded = seed_config_dir(dirs);
        let mut cfg = Self::load_with_probe(dirs, |key| std::env::var(key).ok(), probe)?;
        match seeded {
            Ok(written) => {
                for path in written {
                    cfg.warnings
                        .push(format!("Wrote default config to {}", path.display()));
                }
            }
            Err(e) => cfg.warnings.push(format!(
                "Could not write default config (does not affect operation): {e}"
            )),
        }
        Ok(cfg)
    }

    pub fn load_with(dirs: &Dirs, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let probe = |c: &CredentialRef| match c {
            CredentialRef::Env(name) => env(name).is_some_and(|v| !v.trim().is_empty()),
            CredentialRef::Keyring(_) => false,
        };
        Self::load_with_probe(dirs, &env, probe)
    }

    pub fn load_with_probe(
        dirs: &Dirs,
        env: impl Fn(&str) -> Option<String>,
        probe: impl Fn(&CredentialRef) -> bool,
    ) -> Result<Self> {
        Self::from_files(&ConfigFiles::read(dirs)?, env, probe)
    }

    pub fn from_files(
        files: &ConfigFiles,
        env: impl Fn(&str) -> Option<String>,
        probe: impl Fn(&CredentialRef) -> bool,
    ) -> Result<Self> {
        let auto_detect_env = files
            .models
            .as_ref()
            .and_then(|m| m.auto_detect_env)
            .unwrap_or(true);

        let mut cfg = AppConfig {
            revision: 1,
            auto_detect_env,
            ..Default::default()
        };

        if let Some(f) = files.models.clone() {
            cfg.apply_models(f);
        }

        if auto_detect_env {
            for (id, p) in detect_providers(&env) {
                match cfg.providers.get_mut(&id) {
                    Some(existing) => merge_provider(existing, p),
                    None => {
                        cfg.providers.insert(id, p);
                    }
                }
            }
        }

        if let Some(f) = files.config.clone() {
            cfg.apply(f);
        }

        cfg.materialise_builtin_providers(&probe, files.catalog.as_ref(), files.prices.as_ref());

        cfg.validate()?;
        Ok(cfg)
    }

    fn materialise_builtin_providers(
        &mut self,
        probe: &impl Fn(&CredentialRef) -> bool,
        catalog: Option<&CatalogFile>,
        prices: Option<&PriceFile>,
    ) {
        let builtin: CatalogFile = match serde_yaml_ng::from_str(CATALOG) {
            Ok(c) => c,
            Err(e) => {
                self.warnings.push(format!(
                    "Failed to parse the built-in model catalog; ignoring it: {e}"
                ));
                return;
            }
        };
        let effective = catalog::apply_snapshots(builtin, catalog, prices);
        self.warnings.extend(effective.warnings);

        let auto = self.auto_detect_env;
        let has_key = |id: &str| credential_candidates(id, auto).iter().any(probe);

        for (pid, cat) in effective.catalog.providers {
            match self.providers.get_mut(&pid) {
                Some(mine) => {
                    if mine.sdk.is_none() {
                        mine.sdk = cat.sdk;
                    }
                    if mine.base_url.is_none() {
                        mine.base_url = cat.base_url;
                    }
                    if mine.guide_url.is_none() {
                        mine.guide_url = cat.guide_url;
                    }
                    if mine.network.is_none() {
                        mine.network = cat.network;
                    }
                    let user_listed = !mine.models.is_empty();
                    for (mid, cm) in cat.models {
                        match mine.models.get_mut(&mid) {
                            Some(existing) => fill_model_gaps(existing, cm),
                            None if !user_listed => {
                                mine.models.insert(mid, cm);
                            }
                            None => {}
                        }
                    }
                }
                None if has_key(&pid) => {
                    self.providers.insert(pid, cat);
                }
                None => {}
            }
        }

        for (id, p) in self.providers.iter_mut() {
            p.credential_ok = Some(credential_candidates(id, auto).iter().any(probe));
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let file = read_optional(path)?.unwrap_or_default();
        let mut cfg = AppConfig {
            revision: 1,
            ..Default::default()
        };
        cfg.apply(file);
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn apply(&mut self, file: ConfigFile) {
        if let Some(m) = file.default_model {
            self.default_model = Some(m);
        }
        self.merge_providers(file.providers);
        for (role, incoming) in file.llm_roles {
            self.llm_roles.insert(role, incoming);
        }
        if let Some(s) = file.session {
            self.session = s;
        }
        if let Some(c) = file.context {
            self.context = c;
        }
        if let Some(t) = file.tools {
            self.tools = t;
        }
        if let Some(l) = file.log {
            self.log = l;
        }
        if let Some(w) = file.worktree {
            self.worktree = w;
        }
        if let Some(c) = file.cost {
            self.cost = c;
        }
        if let Some(l) = file.limits {
            self.limits = l;
        }
        if let Some(n) = file.network {
            self.network = n;
        }
        self.revision += 1;
    }

    pub fn apply_models(&mut self, file: ModelsFile) {
        if let Some(a) = file.auto_detect_env {
            self.auto_detect_env = a;
        }
        self.merge_providers(file.providers);
        self.revision += 1;
    }

    fn merge_providers(&mut self, incoming: BTreeMap<String, ProviderSettings>) {
        for (id, p) in incoming {
            match self.providers.get_mut(&id) {
                Some(existing) => merge_provider(existing, p),
                None => {
                    self.providers.insert(id, p);
                }
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.context.validate().map_err(ConfigError::Invalid)?;
        self.worktree.validate().map_err(ConfigError::Invalid)?;
        self.cost.validate().map_err(ConfigError::Invalid)?;
        self.limits.validate().map_err(ConfigError::Invalid)?;
        self.network.validate().map_err(ConfigError::Invalid)?;
        self.tools
            .web_search
            .validate()
            .map_err(ConfigError::Invalid)?;
        for (provider_id, provider) in &self.providers {
            for quota in &provider.quotas {
                quota.validate().map_err(|error| {
                    ConfigError::Invalid(format!("providers.{provider_id}.quotas: {error}"))
                })?;
            }
            for (model_id, model) in &provider.models {
                for quota in &model.quotas {
                    quota.validate().map_err(|error| {
                        ConfigError::Invalid(format!(
                            "providers.{provider_id}.models.{model_id}.quotas: {error}"
                        ))
                    })?;
                }
            }
        }

        for (role, settings) in &self.llm_roles {
            for candidate in &settings.models {
                if candidate != roles::SESSION
                    && Tier::parse(candidate).is_none()
                    && !is_model_ref(candidate)
                {
                    return Err(ConfigError::Invalid(format!(
                        "llm_roles.{role}.models entries must be provider:model references, got \
                         {candidate:?}"
                    )));
                }
            }
        }

        if let Some(m) = &self.default_model {
            if !is_model_ref(m) {
                return Err(ConfigError::Invalid(format!(
                    "default_model must be a provider:model reference, got {m:?}"
                )));
            }
        }
        Ok(())
    }

    pub fn models_with_tier(&self, tier: Tier) -> Vec<String> {
        self.providers
            .iter()
            .filter(|(_, p)| usable(p))
            .flat_map(|(pid, p)| {
                p.models
                    .iter()
                    .filter(|(_, m)| m.tier == Some(tier))
                    .map(move |(mid, _)| format!("{pid}:{mid}"))
            })
            .collect()
    }

    pub fn no_think_params(&self, model_ref: &str) -> BTreeMap<String, Value> {
        self.model_settings(model_ref)
            .map(|m| m.no_think_params.clone())
            .unwrap_or_default()
    }

    fn model_settings(&self, model_ref: &str) -> Option<&ModelSettings> {
        let (pid, mid) = model_ref.split_once(':')?;
        self.providers.get(pid)?.models.get(mid)
    }

    pub fn model_refs(&self) -> Vec<String> {
        let rank = |t: Option<Tier>| match t {
            Some(Tier::Main) => 0u8,
            Some(Tier::Thinking) => 1,
            Some(Tier::Light) => 2,
            None => 3,
        };
        let mut ranked: Vec<(u8, String)> = self
            .providers
            .iter()
            .filter(|(_, p)| usable(p))
            .flat_map(|(pid, p)| {
                p.models
                    .iter()
                    .map(move |(mid, m)| (rank(m.tier), format!("{pid}:{mid}")))
            })
            .collect();
        ranked.sort();
        ranked.into_iter().map(|(_, r)| r).collect()
    }

    pub fn all_model_refs(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|(_, p)| p.enabled)
            .flat_map(|(pid, p)| p.models.keys().map(move |m| format!("{pid}:{m}")))
            .collect()
    }

    pub fn resolve(&self, model_ref: &str) -> Result<(ResolvedModel, Vec<String>)> {
        let (pid, mid) = model_ref
            .split_once(':')
            .ok_or_else(|| ConfigError::UnknownModel(model_ref.to_string()))?;
        let provider = self
            .providers
            .get(pid)
            .ok_or_else(|| ConfigError::UnknownModel(model_ref.to_string()))?;
        if !provider.enabled {
            return Err(ConfigError::Provider(format!("provider {pid} is disabled")));
        }
        let model = provider
            .models
            .get(mid)
            .ok_or_else(|| ConfigError::UnknownModel(model_ref.to_string()))?;

        let mut warnings = Vec::new();
        let resolved = resolve_model(
            pid,
            provider,
            mid,
            model,
            provider::ResolveOptions {
                default_compact_ratio: self.context.compact_ratio,
                auto_detect_env: self.auto_detect_env,
                config_revision: self.revision,
            },
            &mut warnings,
        )?;
        Ok((resolved, warnings))
    }

    pub fn resolve_default(&self) -> Result<(ResolvedModel, Vec<String>)> {
        let r = match &self.default_model {
            Some(m) => m.clone(),
            None => self
                .model_refs()
                .into_iter()
                .next()
                .ok_or_else(|| ConfigError::Invalid("no model is usable".into()))?,
        };
        self.resolve(&r)
    }
}

fn is_model_ref(value: &str) -> bool {
    matches!(
        value.split_once(':'),
        Some((provider, model)) if !provider.is_empty() && !model.is_empty()
    )
}

fn usable(p: &ProviderSettings) -> bool {
    p.enabled && p.credential_ok != Some(false)
}

const CATALOG: &str = include_str!("../defaults/models.yaml");

pub fn builtin_catalog() -> Result<CatalogFile> {
    serde_yaml_ng::from_str(CATALOG).map_err(|e| {
        ConfigError::Invalid(format!("failed to parse the built-in model catalog: {e}"))
    })
}

fn fill_model_gaps(mine: &mut ModelSettings, cat: ModelSettings) {
    if mine.wire_model.is_none() {
        mine.wire_model = cat.wire_model;
    }
    if mine.display_name.is_none() {
        mine.display_name = cat.display_name;
    }
    if mine.context_window.is_none() {
        mine.context_window = cat.context_window;
    }
    if mine.max_output_tokens.is_none() {
        mine.max_output_tokens = cat.max_output_tokens;
    }
    if mine.compaction_threshold.is_none() {
        mine.compaction_threshold = cat.compaction_threshold;
    }
    if mine.tier.is_none() {
        mine.tier = cat.tier;
    }
    if mine.vision.is_none() {
        mine.vision = cat.vision;
    }
    if mine.thinking.is_none() {
        mine.thinking = cat.thinking;
    }
    if mine.pricing.is_none() {
        mine.pricing = cat.pricing;
    }
    if mine.network.is_none() {
        mine.network = cat.network;
    }
    if mine.sdk.is_none() {
        mine.sdk = cat.sdk;
    }
    if mine.quotas.is_empty() {
        mine.quotas = cat.quotas;
    }
    for (k, v) in cat.default_params {
        mine.default_params.entry(k).or_insert(v);
    }
    for (k, v) in cat.no_think_params {
        mine.no_think_params.entry(k).or_insert(v);
    }
}

fn merge_provider(base: &mut ProviderSettings, incoming: ProviderSettings) {
    if incoming.sdk.is_some() {
        base.sdk = incoming.sdk;
    }
    if incoming.base_url.is_some() {
        base.base_url = incoming.base_url;
    }
    if incoming.guide_url.is_some() {
        base.guide_url = incoming.guide_url;
    }
    if !incoming.wiring.is_empty() {
        base.wiring = incoming.wiring;
    }
    if incoming.network.is_some() {
        base.network = incoming.network;
    }
    if incoming.generic.is_some() {
        base.generic = incoming.generic;
    }
    base.enabled = incoming.enabled;
    for (k, v) in incoming.default_params {
        base.default_params.insert(k, v);
    }
    if !incoming.quotas.is_empty() {
        base.quotas = incoming.quotas;
    }
    for (k, v) in incoming.models {
        base.models.insert(k, v);
    }
}

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let io = |e: std::io::Error| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let body = serde_json::to_string_pretty(value)
        .map_err(|e| ConfigError::Invalid(format!("failed to serialize the snapshot: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file = serde_yaml_ng::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(Some(file))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn load_env_file(dirs: &Dirs) -> Result<BTreeMap<String, String>> {
    let path = dirs.config.join("env.yaml");
    let file: Option<BTreeMap<String, String>> = read_optional(&path)?;
    let Some(file) = file else {
        return Ok(BTreeMap::new());
    };
    Ok(file
        .into_iter()
        .filter(|(key, _)| is_env_name(key))
        .collect())
}

fn is_env_name(key: &str) -> bool {
    !key.is_empty()
        && !key.contains(['=', '\0'])
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !key.as_bytes()[0].is_ascii_digit()
}

pub fn seed_config_dir(dirs: &Dirs) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(&dirs.config).map_err(|e| ConfigError::Io {
        path: dirs.config.clone(),
        source: e,
    })?;

    let mut written = Vec::new();
    for (path, body) in [
        (dirs.models_file(), MODELS_TEMPLATE),
        (dirs.config_file(), CONFIG_TEMPLATE),
    ] {
        if path.exists() {
            continue;
        }
        std::fs::write(&path, body).map_err(|e| ConfigError::Io {
            path: path.clone(),
            source: e,
        })?;
        written.push(path);
    }
    Ok(written)
}

const MODELS_TEMPLATE: &str = r#"# zlogic's provider / model list. **Only put your own additions here.**
#
# The built-in providers (anthropic / openai / gemini / deepseek / glm / dashscope / openrouter)
# are compiled into the program, not into this file, and you never have to write them: **a key makes
# them appear**; endpoints, context windows, prices and thinking tiers follow each release. Their effective values: `zlogic models` (read-only).
#
# ── where a key goes ─────────────────────────────────────────────────────
# This file has **no** place to put a key, nor a place to say where a key comes from: the config gets
# printed, ends up in logs and gets pasted into bug reports. Each provider's key is looked up by id, in
# this order, and the first one that yields a value wins:
#
#   1. ZLOGIC_<PROVIDER>_API_KEY  the one meant for zlogic alone; it does not affect other tools in the same shell
#   2. the vendor's usual variable  ANTHROPIC_API_KEY / OPENAI_API_KEY / GEMINI_API_KEY / …
#   3. <PROVIDER>_API_KEY         the rule for a custom provider (my-gw → MY_GW_API_KEY)
#   4. the system keyring          service name zlogic, entry name = provider id
#                                 (the two web-search exceptions: the entry name is web_search:exa /
#                                  web_search:parallel — exa / parallel are retrieval services, not
#                                  model providers, so they cannot share an entry with a provider of the same name)
#
# So "getting started" = export one variable, or store one entry in the keyring. No edit to this file.
# Environment variables rank above the keyring so that "this time I want a different key" works.

# Turned off, environment variables are not read at all; only the keyring counts.
auto_detect_env: true

# ── adding a provider of your own ────────────────────────────────────────
# Corporate gateways, self-hosted vLLM / SGLang, official endpoints reached through a proxy — all of these.
#
# **Do not edit the built-in anthropic / openai** — they are part of the program, and edits will clash
# with fresh built-in data on the next upgrade. Add a provider with your own id, and it gets its own key
# conventions (in the example below: ZLOGIC_MY_GW_API_KEY / MY_GW_API_KEY / keyring:my-gw):
#
# providers:
#   my-gw:
#     # openai_generic is the client dedicated to self-hosted OpenAI-like endpoints, and the only one
#     # that accepts a `generic:` declaration. If the endpoint speaks the official protocol, just write openai_chat / anthropic / …
#     sdk: openai_generic
#     base_url: https://gw.corp.internal/v1
#     generic:
#       # Which field of the delta carries the chain of thought at this endpoint; omit it when no CoT comes back.
#       reasoning_carrier: reasoning_content
#     # Provider-level quotas add up over every model beneath them; the same `quotas` can go under one model.
#     quotas:
#       - label: calls per month
#         metric: requests
#         limit: 10000
#         warn: 0.8
#         window: { kind: calendar, period: month, timezone: Asia/Shanghai }
#       - label: 7-day spend
#         metric: cost_usd
#         currency: USD
#         limit: 50
#         window: { kind: rolling, hours: 168 }
#     models:
#       qwen3-235b:
#         display_name: Internal Qwen3
#         # Primary chat / cheap tier / reasoning tier. Auxiliary calls (titles, approvals) pick light.
#         tier: main
#         context_window: 128000
#         max_output_tokens: 32000
#         vision: false
#         thinking:
#           supported: true
#           can_disable: true
#           # Which tiers this model accepts. Empty = send no tier and use the model's own default.
#           efforts: [low, medium, high]
#         # What to merge into the request body to turn thinking off. **Every vendor does it differently; it cannot be guessed from the name.**
#         no_think_params:
#           chat_template_kwargs: { enable_thinking: false }
#         pricing:
#           input_per_m: 0.5
#           output_per_m: 2.0
#
# Endpoints that do not check a token (local vLLM and the like) still need one: `export MY_GW_API_KEY=x` will do.
# A provider whose key cannot be resolved is never used to send a request — sending bare earns nothing but an uninformative 401.

# ── Azure OpenAI ──────────────────────────────────────────────────────────
# The protocol is OpenAI's as-is (body and SSE word for word); only three things differ: the endpoint carries your resource name,
# the key goes in an `api-key` header instead of `Authorization: Bearer`, and `api-version` is a required query parameter.
# So it uses the ready-made openai_responses / openai_chat codecs plus a `wiring:` section.
#
# It is **not in the built-in catalog**, because the endpoint is **different for every subscription** (`<resource name>.openai.azure.com`) —
# the "a key makes it appear" rule does not hold for it; you have to write it once yourself.
#
# providers:
#   azure:
#     sdk: openai_responses          # write openai_chat if an older resource only enabled chat completions
#     base_url: https://<your resource name>.openai.azure.com/openai/v1
#     wiring:
#       auth_header: api-key         # replaces Bearer rather than stacking on it (sending both is a 401)
#       query:
#         api-version: v1            # check which version your resource supports; do not copy this
#     models:
#       # The key is your **deployment name**, which need not equal the model name;
#       # when the model names differ, point at it with wire_model.
#       my-gpt5-deployment:
#         wire_model: gpt-5.6
#         tier: main
#         context_window: 400000
#         thinking: { supported: true, can_disable: true, efforts: [low, medium, high, xhigh] }
#
# The key follows the same conventions by provider id: ZLOGIC_AZURE_API_KEY / AZURE_API_KEY / keyring:azure.
#
# ── network timeouts (optional) ──────────────────────────────────────────
# Unset = a 30s connect fallback and no read limit. Tune it for a slow self-hosted endpoint or corporate egress:
#
# providers:
#   my-gw:
#     sdk: openai_chat
#     base_url: https://gw.corp.internal/v1
#     network:
#       connect_timeout_ms: 45000   # cap on the handshake (connect + TLS + waiting for response headers)
#       read_timeout_ms: 120000    # cap between two streamed chunks; lower it with care for long thinking
#     models:
#       internal-70b:
#         context_window: 128000
#         # A model-level value can override the provider level field by field (write only what you want to change).
#         network:
#           read_timeout_ms: 300000
#
# ── gateways (Cloudflare AI Gateway and the like) ────────────────────────
# When the gateway itself wants an auth header, use `wiring.headers`. **Do not put a key in there** —
# that value stays in this file verbatim; only the conventions above keep a secret off disk.
#
# providers:
#   my-gateway:
#     sdk: openai_chat
#     base_url: https://gateway.example/v1
#     wiring:
#       headers:
#         cf-aig-authorization: Bearer <gateway token>
"#;

const CONFIG_TEMPLATE: &str = r#"# zlogic's basic settings. The provider / model list is in models.yaml in the same directory.
# How keys are supplied is described at the top of that file as well.

# Left unset, a main model that has a key is picked automatically (tier: main is preferred).
# default_model: anthropic:claude-sonnet-5

# Context compaction. compact_ratio is "compact once the previous turn's input fills this much of the window" —
# err on the small side: compacting early only costs one extra summary, compacting late is a hard provider error.
# tail_turns is "how many recent turns are kept verbatim" (neither summarised nor sent to the summarising model); the minimum 1 =
# protect only the turn in flight; raising it keeps more recent history untouched, at the cost of more left after compaction.
# context:
#   compact_ratio: 0.8
#   tail_turns: 1
#   overflow_retries: 1

# Approval mode: auto = normal (dangerous operations are still asked about); bypass = full authority, every permission prompt skipped.
# auto is the default; think it through before turning bypass on.
# session:
#   approval_mode: auto

# Demand-side role → chain of candidate models. session is a reserved word meaning "the main model of this conversation".
# llm_roles:
#   title:
#     models: [session, light]
#     thinking: off

# ── network proxy ────────────────────────────────────────────────────────
# Which proxy outbound HTTP(S) takes. **Unset** = follow the HTTPS_PROXY / HTTP_PROXY environment variables and the system proxy settings;
# a host launched from an icon does not inherit what the shell exported, so writing it here is the easy way.
# http://, https:// and socks5:// are supported — both Clash's http port and its socks port work here.
# It affects only model calls, the price table and catalog fetches; the `web_fetch` tool **deliberately does not use the proxy** (anti-SSRF).
# network:
#   proxy: http://127.0.0.1:7890
#   no_proxy:            # hosts that skip the proxy (optional)
#     - localhost
#     - 127.0.0.1

# ── worktree ──────────────────────────────────────────────────────────────
# Where `enter_worktree` puts the checkouts it creates. Each worktree is a subdirectory of this directory.
# Relative paths resolve against the **project root**; `{workspace}` = the project directory name and `~/` expands to HOME.
# The default is a sibling directory of the project: ~/code/zlogic → ~/code/zlogic-worktrees/<name>
# Keeping it outside the repository is deliberate: inside it you would have to touch your .gitignore, and creating one from inside a worktree would nest.
# An absolute path = one pool shared by every project, and then you must keep names from colliding yourself (a collision is refused, not silently reused).
# worktree:
#   dir: "../{workspace}-worktrees"
#
# Things in a new checkout that are **not in git** are carried over, from two sources:
#   1. a short hard-coded list (.zlogic/settings.yaml, .zlogic/policy.yaml, .mcp.json …) —
#      without them a worktree behaves differently from the main directory, and that difference raises no error, it just looks like "getting dumber";
#   2. `.worktreeinclude` at the repository root (**gitignore syntax**, written by you) — the project's own local files,
#      typically `.env`. Only files **that git ignores** are copied, and **an existing target is not overwritten**.
# Do not list large directories (there is a 500-file / 64MB cap; hitting it is explained in the tool result) — symlinking yourself is better.

# ── limits ───────────────────────────────────────────────────────────────
# Safeguards against getting stuck, not a budget. max_rounds is how many LLM rounds one submission may run (a round ≈ one model call
# plus the batch of tools it asks for) — dozens of rounds is normal for a big change, so this value is deliberately generous; the UI says so when you hit it.
# limits:
#   max_rounds: 150
#   max_depth: 2
#   max_parallel_tools: 16   # how many tools may run at once in one round (0 = serial; calls that need approval are still one at a time)
#   task_wait_secs: 60      # before a turn wraps up, how long to wait for background tasks this turn started with shell (compile/test);
#                           # 0 = do not wait. If the wait is not over you are asked: keep waiting / kill it / let it run.
#
# Spending limits are not in this file — they are runtime policy, in the same class as "which paths may be changed", and are written to
# <config dir>/policy.yaml (global) or <project>/.zlogic/policy.yaml (per project; it can only be lowered, not raised):
#   budget:
#     per_turn: 2.00                     # most one submission (including sub-agents) may spend (in the display currency)
#     per_task: 0.50                     # one background task; unset falls back to per_turn, and crossing the line always stops it
#     window: { amount: 50, hours: 720 } # total over the last 30 days
#     on_exceeded: ask                   # ask (default, stop and ask) / stop / warn

# ── tools ────────────────────────────────────────────────────────────────
# tools:
#   # The backend shell uses. On Windows auto probes git_bash → ps7 → powershell → cmd
#   # and other platforms use bash. One of those values (or bash) can also be written explicitly.
#   default_shell: auto
#   # A single tool result over this many characters goes to the object store, and only head and tail are fed to the model.
#   max_result_chars: 30000
#   # Tool execution timeout (seconds). 0 = unlimited.
#   timeout_secs: 0
#
#   # Who web_search calls. **It works unconfigured too** — both vendors have a free tier, just a low quota.
#   web_search:
#     # exa / parallel. Unset = whichever side has a key, or exa when neither does.
#     #   exa      supports mode (fast/auto/deep), fresh, max_chars
#     #   parallel takes only the query itself; those parameters have no counterpart there
#     provider: exa
#     # A different endpoint: corporate egress, self-hosted proxy. Unset = the official address.
#     # exa_url: https://mcp.exa.ai/mcp
#     # parallel_url: https://search.parallel.ai/mcp
#     # Single search timeout (seconds). deep and fresh are both slow.
#     timeout_secs: 25
#
# **There is no place to put a key here**, and the rule is the same as for provider keys (see the top of models.yaml):
# look it up by name, and the first one that yields a value wins —
#   1. ZLOGIC_EXA_API_KEY / ZLOGIC_PARALLEL_API_KEY   the one meant for zlogic alone
#   2. EXA_API_KEY / PARALLEL_API_KEY             the one you may already have exported
#   3. the keyring: service name zlogic, entry name web_search:exa / web_search:parallel
# item 3 is exactly where the two key badges under "Tools → web search" in the settings page write; `zlogic key set exa` is
# equivalent. The key is read fresh on every search, so storing it takes effect at once, with no restart.
# With no key yet, apply for one in the official console (the same link is on the right of each row in the settings page):
#   exa       https://dashboard.exa.ai/api-keys
#   parallel  https://platform.parallel.ai
# Note that the **search query itself leaves this machine** (it is sent to the backend above); workspace contents do not.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zlogic_protocol::config::Sdk;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn load(dirs: &Dirs, env: &[(&str, &str)]) -> Result<AppConfig> {
        let for_read = env_of(env);
        let for_probe = env_of(env);
        AppConfig::load_with_probe(dirs, for_read, move |c| match c {
            CredentialRef::Env(n) => for_probe(n).is_some_and(|v| !v.trim().is_empty()),
            CredentialRef::Keyring(_) => false,
        })
    }

    #[test]
    fn loads_an_empty_world_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let cfg = load(&dirs, &[]).unwrap();
        assert!(cfg.providers.is_empty());
        assert!(
            cfg.resolve_default().is_err(),
            "with no models it must fail with a clear error"
        );
    }

    #[test]
    fn every_builtin_catalog_provider_has_an_official_https_guide() {
        let catalog = builtin_catalog().unwrap();
        assert!(!catalog.providers.is_empty());
        for (id, provider) in catalog.providers {
            let guide = provider
                .guide_url
                .unwrap_or_else(|| panic!("built-in provider {id} is missing guide_url"));
            assert!(
                guide.starts_with("https://"),
                "the guide_url of built-in provider {id} must be HTTPS: {guide}"
            );
        }
    }

    #[test]
    fn env_file_loads_flat_pairs_and_drops_invalid_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        dirs.ensure().unwrap();
        write(
            &dirs.config,
            "env.yaml",
            r#"
DEEPSEEK_API_KEY: sk-123
OPENAI_API_KEY: sk-456
"1_BAD": starts-with-digit
"bad key": has-space
# comments and blank lines should be ignored
GITHUB_TOKEN: ghp-xyz
"#,
        );
        let env = load_env_file(&dirs).unwrap();
        assert_eq!(
            env.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("sk-123")
        );
        assert_eq!(env.get("GITHUB_TOKEN").map(String::as_str), Some("ghp-xyz"));
        assert!(
            !env.contains_key("1_BAD"),
            "a key starting with a digit is not valid"
        );
        assert!(
            !env.contains_key("bad key"),
            "a key containing a space is not valid"
        );
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn env_file_missing_or_invalid_is_handled() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        dirs.ensure().unwrap();
        assert!(load_env_file(&dirs).unwrap().is_empty());

        write(&dirs.config, "env.yaml", "A_KEY: [unclosed\n");
        assert!(load_env_file(&dirs).is_err());

        write(&dirs.config, "env.yaml", "A_KEY: {nested: map}\n");
        assert!(load_env_file(&dirs).is_err());
    }

    #[test]
    fn parses_a_realistic_config() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        dirs.ensure().unwrap();
        write(
            &dirs.config,
            "config.yaml",
            r#"
default_model: deepseek:deepseek-v4
context:
  compact_ratio: 0.7
  tail_turns: 6
  overflow_retries: 1
providers:
  deepseek:
    sdk: deepseek
    models:
      deepseek-v4:
        context_window: 128000
        thinking: { supported: true, can_disable: true, efforts: [], budget: false }
  corp:
    sdk: openai_generic
    base_url: https://gw.corp.test/v1
    generic:
      reasoning_carrier: reasoning_content
      usage_fields: { cache_read: my.cached, input_includes_cache: false }
    models:
      internal-70b:
        context_window: 32000
"#,
        );

        let cfg = load(
            &dirs,
            &[("DEEPSEEK_API_KEY", "sk-d"), ("CORP_API_KEY", "x")],
        )
        .unwrap();
        assert_eq!(cfg.context.tail_turns, 6);
        let mut refs = cfg.model_refs();
        refs.sort();
        assert_eq!(refs, ["corp:internal-70b", "deepseek:deepseek-v4"]);

        let (m, w) = cfg.resolve_default().unwrap();
        assert_eq!(m.wire_model, "deepseek-v4");
        assert_eq!(m.compaction_threshold, Some(89_600), "0.7 × 128000");
        assert!(w.is_empty());
    }

    #[test]
    fn handwritten_config_beats_the_builtin_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "config.yaml",
            "providers:\n  openai:\n    base_url: https://my-proxy.test\n",
        );

        let cfg = load(&dirs, &[("OPENAI_API_KEY", "k")]).unwrap();
        let p = &cfg.providers["openai"];
        assert_eq!(p.base_url.as_deref(), Some("https://my-proxy.test"));
        assert_eq!(p.sdk, Some(Sdk::OpenAiResponses));
        assert!(!p.models.is_empty());
    }

    #[test]
    fn merging_a_provider_keeps_models_from_lower_layers() {
        let mut cfg = AppConfig {
            revision: 1,
            ..Default::default()
        };
        cfg.apply(
            serde_yaml_ng::from_str(
                "providers:\n  p:\n    sdk: deepseek\n    models:\n      a: {context_window: 100}\n      b: {context_window: 200}\n",
            )
            .unwrap(),
        );
        cfg.apply(
            serde_yaml_ng::from_str(
                "providers:\n  p:\n    models:\n      b: {context_window: 999}\n",
            )
            .unwrap(),
        );

        let p = &cfg.providers["p"];
        assert_eq!(p.models.len(), 2, "changing b alone must not lose a");
        assert_eq!(p.models["b"].context_window, Some(999));
        assert_eq!(
            p.sdk,
            Some(Sdk::DeepSeek),
            "changing models alone must not lose sdk"
        );
    }

    #[test]
    fn disabled_provider_disappears_from_the_catalog() {
        let mut cfg = AppConfig {
            revision: 1,
            ..Default::default()
        };
        cfg.apply(
            serde_yaml_ng::from_str(
                "providers:\n  p:\n    sdk: deepseek\n    enabled: false\n    models:\n      a: {context_window: 100}\n",
            )
            .unwrap(),
        );
        assert!(cfg.model_refs().is_empty());
        assert!(cfg.resolve("p:a").is_err());
    }

    #[test]
    fn a_network_section_replaces_wholesale_and_a_bad_proxy_fails_validation() {
        let mut cfg = AppConfig {
            revision: 1,
            ..Default::default()
        };
        assert_eq!(cfg.network.proxy, "", "no proxy is set by default");

        cfg.apply(
            serde_yaml_ng::from_str(
                "network:\n  proxy: http://127.0.0.1:7890\n  no_proxy: [localhost]\n",
            )
            .unwrap(),
        );
        assert_eq!(cfg.network.proxy_url(), Some("http://127.0.0.1:7890"));
        assert_eq!(cfg.network.no_proxy, vec!["localhost".to_string()]);
        assert!(cfg.validate().is_ok());

        cfg.apply(serde_yaml_ng::from_str("network:\n  proxy: 127.0.0.1:7890\n").unwrap());
        assert!(cfg.validate().is_err());

        cfg.apply(serde_yaml_ng::from_str("network:\n  proxy: socks5://127.0.0.1:7891\n").unwrap());
        assert!(cfg.network.no_proxy.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn typos_in_yaml_are_rejected_not_silently_defaulted() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        dirs.ensure().unwrap();
        write(&dirs.config, "config.yaml", "defualt_model: a/b\n");
        let err = load(&dirs, &[]).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "a misspelled key must be an error"
        );
    }

    #[test]
    fn bad_default_model_shape_is_rejected() {
        let mut cfg = AppConfig {
            revision: 1,
            ..Default::default()
        };
        cfg.default_model = Some("just-a-name".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_named_role_model_must_be_provider_qualified() {
        let mut cfg = AppConfig::default();
        cfg.llm_roles.insert(
            "title".into(),
            RoleSettings {
                models: vec!["anthropic/opus".into()],
                ..Default::default()
            },
        );
        assert!(cfg.validate().is_err());

        cfg.llm_roles.get_mut("title").unwrap().models = vec!["anthropic:opus".into()];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_context_ratio_fails_the_whole_load() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        dirs.ensure().unwrap();
        write(
            &dirs.config,
            "config.yaml",
            "context:\n  compact_ratio: 2.0\n  tail_turns: 4\n  overflow_retries: 1\n",
        );
        assert!(load(&dirs, &[]).is_err());
    }

    #[test]
    fn the_generated_model_catalog_still_parses() {
        let yaml = include_str!("../defaults/models.yaml");
        let cfg: CatalogFile = serde_yaml_ng::from_str(yaml).expect(
            "defaults/models.yaml failed to deserialize — \
             regenerate it from the model catalog; for a new enum value, extend protocol first",
        );

        assert!(
            !cfg.version.trim().is_empty(),
            "the catalog must carry a version, otherwise check-for-updates has nothing to compare"
        );
        assert!(!cfg.providers.is_empty());
        for (pid, p) in &cfg.providers {
            assert!(p.sdk.is_some(), "{pid} is missing sdk");
            assert!(!p.models.is_empty(), "{pid} has no models at all");
            for (mid, m) in &p.models {
                assert!(
                    m.context_window.is_some(),
                    "{pid}/{mid} is missing context_window"
                );
                assert!(m.pricing.is_some(), "{pid}/{mid} is missing pricing");
                let t = m
                    .thinking
                    .as_ref()
                    .unwrap_or_else(|| panic!("{pid}/{mid} is missing thinking"));

                let mut seen = t.efforts.clone();
                seen.sort_by_key(|e| e.rank());
                seen.dedup();
                assert_eq!(
                    seen.len(),
                    t.efforts.len(),
                    "{pid}/{mid} has duplicate efforts"
                );

                if !t.supported {
                    assert!(
                        t.efforts.is_empty() && !t.budget,
                        "{pid}/{mid} says it is unsupported yet still lists efforts"
                    );
                }

                let p = m.pricing.as_ref().expect("asserted above");
                assert!(
                    p.input_per_m >= 0.0 && p.output_per_m >= 0.0,
                    "{pid}/{mid} has a negative price, so the upstream data is broken"
                );
            }
        }
    }

    #[test]
    fn the_catalog_header_names_the_url_clients_actually_fetch() {
        let yaml = include_str!("../defaults/models.yaml");
        assert!(
            yaml.contains(crate::catalog::SOURCE),
            "the header comment of defaults/models.yaml does not contain {} — \
             the catalog generator writes this file and must name the published copy",
            crate::catalog::SOURCE
        );
    }

    #[test]
    fn a_bare_api_key_gets_the_whole_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-x")]).unwrap();

        let refs = cfg.model_refs();
        assert!(
            refs.len() > 1,
            "the catalog should fill in a whole list, got: {refs:?}"
        );
        assert!(
            refs.iter().any(|r| r == "anthropic:claude-opus-5"),
            "{refs:?}"
        );

        let m = &cfg.providers["anthropic"].models["claude-opus-5"];
        assert_eq!(m.context_window, Some(1_000_000));
        let p = m.pricing.as_ref().expect("pricing");
        assert_eq!(p.input_per_m, 5.0);
        assert_eq!(p.output_per_m, 25.0);

        let t = m.thinking.as_ref().expect("thinking");
        assert!(
            t.efforts.contains(&zlogic_protocol::llm::Effort::XHigh),
            "{:?}",
            t.efforts
        );
    }

    #[test]
    fn the_catalog_never_invents_a_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let cfg = load(&dirs, &[]).unwrap();
        assert!(
            cfg.providers.is_empty(),
            "with no credentials at all there should be no provider: {:?}",
            cfg.providers
        );
    }

    #[test]
    fn explicit_settings_beat_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "config.yaml",
            r#"
providers:
  anthropic:
    models:
      claude-opus-5:
        context_window: 200000
        pricing: { input_per_m: 1.0, output_per_m: 2.0 }
"#,
        );
        let cfg = load(&dirs, &[]).unwrap();
        let m = &cfg.providers["anthropic"].models["claude-opus-5"];

        assert_eq!(
            m.context_window,
            Some(200_000),
            "a window the user wrote must not be overridden by the catalog"
        );
        assert_eq!(
            m.pricing.as_ref().unwrap().input_per_m,
            1.0,
            "a self-hosted gateway's price really can differ"
        );
        assert!(
            m.thinking.is_some(),
            "thinking was not written by the user, so the catalog must fill it"
        );
        assert_eq!(m.max_output_tokens, Some(128_000));
    }

    #[test]
    fn an_explicit_model_list_is_not_grown_by_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "config.yaml",
            "providers:\n  anthropic:\n    models:\n      claude-opus-5: {}\n",
        );
        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-x")]).unwrap();
        assert_eq!(cfg.model_refs(), ["anthropic:claude-opus-5"]);
    }

    #[test]
    fn env_detection_merges_instead_of_replacing() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "config.yaml",
            "providers:\n  anthropic:\n    models:\n      my-tuned-model:\n        \
             context_window: 4096\n",
        );
        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-x")]).unwrap();

        let p = &cfg.providers["anthropic"];
        assert!(
            p.enabled,
            "being overridden by detection must not make the whole provider vanish"
        );
        assert_eq!(
            p.credential_ok,
            Some(true),
            "there is a key in the environment, so it must be detected"
        );
        assert!(
            p.models.contains_key("my-tuned-model"),
            "a model from the config must not be wiped out"
        );
    }

    #[test]
    fn dumping_the_config_never_leaks_a_key() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-TOPSECRET")]).unwrap();
        let dumped = format!("{cfg:?}");
        assert!(!dumped.contains("TOPSECRET"));
        assert!(!dumped.contains("sk-ant"));
        assert_eq!(
            cfg.providers["anthropic"].credential_ok,
            Some(true),
            "but it must be known to be usable"
        );
    }

    #[test]
    fn a_provider_without_a_key_is_listed_but_not_offered() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "config.yaml",
            "providers:\n  anthropic:\n    sdk: anthropic\n    models:\n      opus: {}\n  \
             deepseek:\n    sdk: deepseek\n    models:\n      v4: {}\n",
        );
        let cfg = load(&dirs, &[("DEEPSEEK_API_KEY", "sk-d")]).unwrap();

        assert_eq!(
            cfg.model_refs(),
            ["deepseek:v4"],
            "only the reachable ones enter the list"
        );
        let mut all = cfg.all_model_refs();
        all.sort();
        assert_eq!(
            all,
            ["anthropic:opus", "deepseek:v4"],
            "the settings page must show the full set"
        );
        assert_eq!(cfg.providers["anthropic"].credential_ok, Some(false));
    }

    #[test]
    fn the_zlogic_specific_variable_is_enough_on_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "config.yaml",
            "providers:\n  anthropic:\n    sdk: anthropic\n    models:\n      opus: {}\n",
        );
        let cfg = load(&dirs, &[("ZLOGIC_ANTHROPIC_API_KEY", "sk-mine")]).unwrap();
        assert_eq!(cfg.model_refs(), ["anthropic:opus"]);
    }

    #[test]
    fn turning_off_env_detection_ignores_the_environment_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "models.yaml",
            "auto_detect_env: false\nproviders:\n  anthropic:\n    sdk: anthropic\n    \
             models:\n      opus: {}\n",
        );
        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-x")]).unwrap();

        assert!(!cfg.auto_detect_env);
        assert!(
            cfg.model_refs().is_empty(),
            "after turning it off only the keyring is left, and the test probe has none"
        );
        assert_eq!(cfg.providers.len(), 1);
        let (m, _) = cfg.resolve("anthropic:opus").unwrap();
        assert_eq!(m.credential_refs, ["keyring:anthropic"]);
    }

    #[test]
    fn a_keyless_endpoint_is_not_offered() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "models.yaml",
            "providers:\n  local:\n    sdk: qwen_local\n    base_url: http://127.0.0.1:8000/v1\n    \
             models:\n      qwen3: {context_window: 32000}\n",
        );
        assert!(
            load(&dirs, &[]).unwrap().model_refs().is_empty(),
            "without a key it must not be used to send a request"
        );
        assert_eq!(
            load(&dirs, &[("LOCAL_API_KEY", "x")]).unwrap().model_refs(),
            ["local:qwen3"],
            "a placeholder value is enough"
        );
    }

    #[test]
    fn seeding_writes_both_files_once_and_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());

        let first = seed_config_dir(&dirs).unwrap();
        assert_eq!(first.len(), 2, "models.yaml + config.yaml");
        assert!(dirs.models_file().exists() && dirs.config_file().exists());

        std::fs::write(
            dirs.config_file(),
            "default_model: anthropic:claude-opus-5\n",
        )
        .unwrap();
        assert!(
            seed_config_dir(&dirs).unwrap().is_empty(),
            "an existing file must not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(dirs.config_file()).unwrap(),
            "default_model: anthropic:claude-opus-5\n"
        );
    }

    #[test]
    fn the_seeded_files_load_back_and_contain_no_builtin_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        seed_config_dir(&dirs).unwrap();

        let models = std::fs::read_to_string(dirs.models_file()).unwrap();
        let live: Vec<&str> = models
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .collect();
        assert_eq!(live, ["auto_detect_env: true"], "{live:?}");

        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-x")]).unwrap();
        assert!(
            cfg.auto_detect_env,
            "that line in the template has to take effect"
        );

        let m = &cfg.providers["anthropic"].models["claude-sonnet-5"];
        assert_eq!(m.pricing.as_ref().expect("pricing").output_per_m, 10.0);
        assert!(!m.thinking.as_ref().expect("thinking").efforts.is_empty());

        assert_eq!(cfg.providers.keys().collect::<Vec<_>>(), ["anthropic"]);
    }

    #[test]
    fn the_automatic_default_prefers_a_main_tier_model() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        seed_config_dir(&dirs).unwrap();

        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "sk-ant-x")]).unwrap();
        assert!(cfg.default_model.is_none());

        let (m, _) = cfg.resolve_default().unwrap();
        assert_eq!(m.source.model_id, "claude-sonnet-5");
        assert_eq!(
            cfg.providers["anthropic"].models["claude-haiku-4-5"].tier,
            Some(Tier::Light),
            "it comes first in lexicographic order — exactly the one that must not be picked"
        );
    }

    #[test]
    fn config_yaml_overrides_models_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "models.yaml",
            "providers:\n  anthropic:\n    sdk: anthropic\n    base_url: https://api.anthropic.com\n    \
             guide_url: https://platform.claude.com/settings/keys\n    \
             models:\n      opus: {context_window: 200000}\n",
        );
        write(
            &dirs.config,
            "config.yaml",
            "providers:\n  anthropic:\n    base_url: https://proxy.internal\n    \
             guide_url: https://console.example.test/anthropic-key\n",
        );

        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "k")]).unwrap();
        let p = &cfg.providers["anthropic"];
        assert_eq!(p.base_url.as_deref(), Some("https://proxy.internal"));
        assert_eq!(
            p.guide_url.as_deref(),
            Some("https://console.example.test/anthropic-key")
        );
        assert_eq!(
            p.models.len(),
            1,
            "changing base_url alone must not lose models"
        );
    }

    #[test]
    fn a_proxy_is_expressed_as_a_new_provider_not_a_rewritten_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "models.yaml",
            "providers:\n  my-openai:\n    sdk: openai_chat\n    base_url: https://gw.corp/v1\n    \
             models:\n      gpt-5.6: {context_window: 400000, tier: main}\n",
        );

        let cfg = load(
            &dirs,
            &[
                ("OPENAI_API_KEY", "k"),
                ("OPENAI_BASE_URL", "https://gw.corp/v1"),
                ("MY_OPENAI_API_KEY", "k"),
            ],
        )
        .unwrap();

        assert_eq!(
            cfg.providers["openai"].base_url.as_deref(),
            Some("https://api.openai.com")
        );
        assert_eq!(
            cfg.providers["my-openai"].base_url.as_deref(),
            Some("https://gw.corp/v1")
        );
        let (m, _) = cfg.resolve("my-openai:gpt-5.6").unwrap();
        assert!(
            m.credential_refs
                .contains(&"env:MY_OPENAI_API_KEY".to_string())
        );
    }

    #[test]
    fn a_workspace_model_list_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "models.yaml",
            "providers:\n  anthropic:\n    sdk: anthropic\n    models:\n      opus: {context_window: 200000}\n",
        );
        let ws = tmp.path().join("proj");
        write(
            &ws,
            ".zlogic/models.yaml",
            "providers:\n  anthropic:\n    models:\n      opus: {context_window: 400000}\n",
        );

        let cfg = load(&dirs, &[("ANTHROPIC_API_KEY", "k")]).unwrap();
        assert_eq!(
            cfg.providers["anthropic"].models["opus"].context_window,
            Some(200_000)
        );
    }

    #[test]
    fn non_model_sections_are_rejected_in_models_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        write(
            &dirs.config,
            "models.yaml",
            "context:\n  compact_ratio: 0.5\n",
        );
        assert!(matches!(load(&dirs, &[]), Err(ConfigError::Parse { .. })));
    }
}

#[cfg(test)]
mod endpoint_wiring {
    use super::*;
    use crate::dirs::Dirs;

    #[test]
    fn azure_is_expressed_as_wiring_over_the_openai_codec() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(
            dirs.models_file(),
            r#"
providers:
  azure:
    sdk: openai_responses
    base_url: https://my-res.openai.azure.com/openai/v1
    wiring:
      auth_header: api-key
      query:
        api-version: v1
    models:
      my-deployment:
        wire_model: gpt-5.6
        context_window: 400000
"#,
        )
        .unwrap();

        let cfg = AppConfig::load_with_probe(
            &dirs,
            |k| (k == "AZURE_API_KEY").then(|| "k".to_string()),
            |c| matches!(c, CredentialRef::Env(n) if n == "AZURE_API_KEY"),
        )
        .unwrap();

        let (m, _) = cfg.resolve("azure:my-deployment").unwrap();
        assert!(matches!(
            m.client,
            zlogic_protocol::config::ClientSpec::Builtin {
                sdk: zlogic_protocol::config::Sdk::OpenAiResponses
            }
        ));
        assert_eq!(m.wiring.auth_header.as_deref(), Some("api-key"));
        assert_eq!(
            m.wiring.query.get("api-version").map(String::as_str),
            Some("v1")
        );
        assert_eq!(m.wire_model, "gpt-5.6");
        assert!(m.credential_refs.contains(&"env:AZURE_API_KEY".to_string()));
    }

    #[test]
    fn builtin_providers_need_no_wiring() {
        let catalog: CatalogFile = serde_yaml_ng::from_str(CATALOG).unwrap();
        for (pid, p) in &catalog.providers {
            assert!(
                p.wiring.is_empty(),
                "{pid} carries wiring — a built-in endpoint should need no wiring"
            );
        }
    }
}

#[cfg(test)]
mod declarative_providers {
    use super::*;
    use crate::dirs::Dirs;

    fn load_with_key(var: &'static str) -> AppConfig {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        AppConfig::load_with_probe(
            &dirs,
            move |k| (k == var).then(|| "k".to_string()),
            move |c| matches!(c, CredentialRef::Env(n) if n == var),
        )
        .unwrap()
    }

    #[test]
    fn a_declarative_provider_resolves_with_its_dialect() {
        let cfg = load_with_key("XAI_API_KEY");
        let p = &cfg.providers["xai"];
        assert_eq!(p.sdk, Some(zlogic_protocol::config::Sdk::OpenAiGeneric));
        assert_eq!(p.base_url.as_deref(), Some("https://api.x.ai/v1"));

        let dialect = p.generic.as_ref().expect("dialect");
        assert_eq!(
            dialect.reasoning_carrier.as_deref(),
            Some("reasoning_content")
        );
        assert_eq!(dialect.effort_field.as_deref(), Some("reasoning_effort"));

        let (m, _) = cfg.resolve("xai:grok-4.3").unwrap();
        assert!(matches!(
            m.client,
            zlogic_protocol::config::ClientSpec::OpenAiGeneric(_)
        ));
        assert!(!m.capabilities.thinking.efforts.is_empty());
    }

    #[test]
    fn turning_thinking_off_is_covered_on_both_paths() {
        let cfg = load_with_key("XAI_API_KEY");
        assert_eq!(
            cfg.no_think_params("xai:grok-4.3").get("reasoning_effort"),
            Some(&serde_json::json!("none")),
            "a role-driven turn-off goes through no_think_params"
        );
        assert!(
            cfg.no_think_params("xai:grok-4.5").is_empty(),
            "a model with no none in its tier table must not be declared disableable"
        );
        assert!(
            !cfg.providers["xai"].models["grok-4.5"]
                .thinking
                .as_ref()
                .unwrap()
                .can_disable
        );
    }

    #[test]
    fn a_model_id_containing_a_slash_still_resolves() {
        let cfg = load_with_key("GROQ_API_KEY");
        assert!(
            cfg.model_refs()
                .contains(&"groq:openai/gpt-oss-120b".to_string())
        );
        let (m, _) = cfg.resolve("groq:openai/gpt-oss-120b").unwrap();
        assert_eq!(
            m.wire_model, "openai/gpt-oss-120b",
            "what is sent to the provider is the one without the prefix"
        );
        assert_eq!(m.source.provider_id, "groq");
    }

    #[test]
    fn a_model_id_containing_a_colon_still_resolves() {
        let mut cfg = AppConfig::default();
        let mut provider = ProviderSettings {
            sdk: Some(zlogic_protocol::config::Sdk::OpenAiChat),
            base_url: Some("https://example.test/v1".into()),
            enabled: true,
            ..Default::default()
        };
        provider
            .models
            .insert("vendor:model:revision".into(), ModelSettings::default());
        cfg.providers.insert("custom".into(), provider);

        let (model, _) = cfg.resolve("custom:vendor:model:revision").unwrap();
        assert_eq!(model.source.provider_id, "custom");
        assert_eq!(model.source.model_id, "vendor:model:revision");
        assert_eq!(model.wire_model, "vendor:model:revision");
    }
}

#[cfg(test)]
mod builtin_authority {
    use super::*;
    use crate::dirs::Dirs;

    #[test]
    fn a_builtin_providers_sdk_comes_from_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let cfg = AppConfig::load_with_probe(
            &dirs,
            |k| (k == "OPENAI_API_KEY").then(|| "sk-x".to_string()),
            |c| matches!(c, CredentialRef::Env(n) if n == "OPENAI_API_KEY"),
        )
        .unwrap();

        assert_eq!(
            cfg.providers["openai"].sdk,
            Some(zlogic_protocol::config::Sdk::OpenAiResponses),
            "the catalog says responses, so responses it is; when this one changes, detection is grabbing sdk again"
        );
        assert_eq!(cfg.providers["openai"].models.len(), 3);
    }
}
