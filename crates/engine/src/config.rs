use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use zlogic_config::write::{
    YamlPatch, patch_yaml_file, scalar_or, text_or_default, value_or_default,
};
use zlogic_config::{AppConfig, Dirs};
use zlogic_core::SharedStore;
use zlogic_credential::{CredentialStore, credential_candidates};
use zlogic_protocol::config::{ModelConfig, ProviderConfig, ProviderOrigin, Sdk};
use zlogic_protocol::query::{
    ApiError, ApiResult, CatalogCheck, CatalogSnapshot, ConfigCreateScope, ConfigRemoveProviderReq,
    ConfigUpdateReq, ConfigView, OpenAiCompatibleProviderReq, PriceSnapshot, ProviderCatalog,
    SettingsView, UsageSummary, UsageSummaryReq,
};
use zlogic_protocol::usage::{QuotaScope, QuotaStatus};
use zlogic_store::UsageQuery;

use crate::service::{ConfigService, WorkspaceService};
use crate::{BypassFlag, EngineError, ModelRouter, Result};

pub struct Config {
    current: RwLock<Arc<AppConfig>>,
    dirs: Dirs,
    bypass_flag: Option<BypassFlag>,
    /// Routing reads the same effective config as the settings view. Reload replaces this snapshot
    /// too, otherwise a newly added model appears in the picker but `session_set_model` rejects it.
    router: Option<Arc<ModelRouter>>,

    credential_store: Arc<dyn CredentialStore>,
    transport: Option<Arc<dyn zlogic_llm::transport::HttpTransport>>,
    store: SharedStore,
    workspaces: Arc<dyn WorkspaceService>,
    /// Reports are asked for far more often than the usage rows change; see
    /// [`crate::usage_cache`].
    usage_reports: crate::usage_cache::ReportCache,
}

impl Config {
    pub fn new(
        config: Arc<AppConfig>,
        dirs: Dirs,
        credential_store: Arc<dyn CredentialStore>,
        store: SharedStore,
        workspaces: Arc<dyn WorkspaceService>,
    ) -> Self {
        Self {
            current: RwLock::new(config),
            dirs,
            bypass_flag: None,
            router: None,
            credential_store,
            transport: None,
            store,
            workspaces,
            usage_reports: crate::usage_cache::ReportCache::new(),
        }
    }

    pub fn with_bypass_flag(mut self, cell: BypassFlag) -> Self {
        self.bypass_flag = Some(cell);
        self
    }

    pub fn with_router(mut self, router: Arc<ModelRouter>) -> Self {
        self.router = Some(router);
        self
    }

    pub fn with_transport(
        mut self,
        transport: Arc<dyn zlogic_llm::transport::HttpTransport>,
    ) -> Self {
        self.transport = Some(transport);
        self
    }

    fn push_policies(&self, cfg: &AppConfig) {
        if let Some(flag) = &self.bypass_flag {
            flag.set(cfg.session.approval_mode == zlogic_protocol::settings::ApprovalMode::Bypass);
        }
        if let Some(transport) = &self.transport {
            transport.apply_network(&cfg.network);
        }
    }

    pub async fn snapshot(&self) -> Arc<AppConfig> {
        self.current.read().await.clone()
    }

    fn view(&self, cfg: &AppConfig) -> ConfigView {
        let declared = declared_providers(&self.dirs);
        ConfigView {
            providers: providers_of(cfg, &declared),
            default_model: cfg.default_model.clone(),
            settings: SettingsView {
                session: cfg.session.clone(),
                context: cfg.context.clone(),
                tools: cfg.tools.clone(),
                log: cfg.log.clone(),
                worktree: cfg.worktree.clone(),
                cost: cfg.cost.clone(),
                limits: cfg.limits.clone(),
                network: cfg.network.clone(),
                auto_detect_env: cfg.auto_detect_env,
            },
            llm_roles: cfg.llm_roles.clone(),
            global_path: self.dirs.config_file().to_string_lossy().into_owned(),
            models_path: self.dirs.models_file().to_string_lossy().into_owned(),
            config_dir: self.dirs.config.to_string_lossy().into_owned(),
            log_dir: self.dirs.logs().to_string_lossy().into_owned(),
            warnings: cfg.warnings.clone(),
            revision: cfg.revision,
        }
    }

    async fn reload_inner(&self) -> Result<ConfigView> {
        let loaded = AppConfig::load(&self.dirs, |reference| {
            self.credential_store.is_available(reference)
        })
        .map_err(EngineError::from)?;

        self.push_policies(&loaded);

        let mut guard = self.current.write().await;
        let revision = guard.revision.max(loaded.revision) + 1;
        let next = Arc::new(AppConfig { revision, ..loaded });
        *guard = next.clone();
        drop(guard);
        if let Some(router) = &self.router {
            router.replace_config(next.clone());
        }

        Ok(self.view(&next))
    }

    async fn write(&self, req: ConfigUpdateReq) -> Result<ConfigView> {
        if let Some(expected) = req.expected_revision {
            let current = self.snapshot().await.revision;
            if current != expected {
                return Err(EngineError::Conflict(format!(
                    "config has changed since you last read it (expected revision {expected}, \
                     current {current}). \
                     Open the settings page again and retry — overwriting directly would \
                     discard changes written elsewhere just now"
                )));
            }
        }

        let mut candidate = (*self.snapshot().await).clone();
        apply_update(&mut candidate, &req);
        candidate.validate().map_err(EngineError::from)?;

        let mut config_patch = YamlPatch::new();
        let mut models_patch = YamlPatch::new();

        if let Some(model) = &req.default_model {
            config_patch.insert("default_model".into(), text_or_default(model));
        }
        if let Some(v) = &req.session {
            config_patch.insert("session".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.context {
            config_patch.insert("context".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.tools {
            config_patch.insert("tools".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.log {
            config_patch.insert("log".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.worktree {
            config_patch.insert("worktree".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.cost {
            config_patch.insert("cost".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.limits {
            config_patch.insert("limits".into(), value_or_default(v)?);
        }
        if let Some(v) = &req.network {
            config_patch.insert("network".into(), value_or_default(v)?);
        }
        if let Some(roles) = &req.llm_roles {
            config_patch.insert("llm_roles".into(), value_or_default(roles)?);
        }
        if let Some(on) = req.auto_detect_env {
            models_patch.insert("auto_detect_env".into(), scalar_or(&on, &true)?);
        }

        if !config_patch.is_empty() {
            patch_yaml_file(&self.dirs.config_file(), &config_patch)?;
        }
        if !models_patch.is_empty() {
            patch_yaml_file(&self.dirs.models_file(), &models_patch)?;
        }

        self.reload_inner().await
    }

    async fn upsert_openai_compatible_inner(
        &self,
        req: OpenAiCompatibleProviderReq,
    ) -> Result<ConfigView> {
        if let Some(expected) = req.expected_revision {
            let current = self.snapshot().await.revision;
            if current != expected {
                return Err(EngineError::Conflict(format!(
                    "config has changed since it was read (expected revision {expected}, \
                     current {current})"
                )));
            }
        }

        let provider_id = req.provider_id.trim();
        if provider_id.is_empty()
            || !provider_id.chars().enumerate().all(|(index, ch)| {
                ch.is_ascii_lowercase()
                    || ch.is_ascii_digit()
                    || (index > 0 && matches!(ch, '-' | '_' | '.'))
            })
        {
            return Err(EngineError::Invalid(
                "provider id may only contain lowercase letters, digits, '-', '_' and '.', \
                 and must start with a letter or digit"
                    .into(),
            ));
        }
        let model_id = req
            .model_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if req.context_window == Some(0) || req.max_output_tokens == Some(0) {
            return Err(EngineError::Invalid(
                "token count must be greater than 0".into(),
            ));
        }
        if let Some(id) = model_id {
            validate_model_id(id)?;
        }
        /* Rename: `rename_from` is the old id, `model_id` the new one. The two ids being equal
         * means the user did not change it, so take the normal edit path (otherwise it goes down
         * the "delete the old, create the new" route and moves for nothing). */
        let rename_from = req
            .rename_from
            .as_deref()
            .map(str::trim)
            .filter(|old| !old.is_empty())
            .filter(|old| model_id != Some(*old));
        if rename_from.is_some() && model_id.is_none() {
            return Err(EngineError::Invalid(
                "rename_from requires model_id: the new id has to be named".into(),
            ));
        }
        if rename_from.is_some() && req.create_scope.is_some() {
            return Err(EngineError::Invalid(
                "rename_from is an edit, not a create: create_scope must be omitted".into(),
            ));
        }

        let base_url = req.base_url.trim().trim_end_matches('/');
        let parsed = reqwest::Url::parse(base_url)
            .map_err(|error| EngineError::Invalid(format!("invalid base URL: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
            return Err(EngineError::Invalid(
                "base URL must be a full http:// or https:// address".into(),
            ));
        }

        let files = zlogic_config::ConfigFiles::read(&self.dirs).map_err(EngineError::from)?;
        let mut providers = files.models.unwrap_or_default().providers;
        let builtin = zlogic_config::builtin_catalog().map_err(EngineError::from)?;
        /* A rename can only move something **within the same file**. A provider may also be
         * declared in config.yaml: that layer is an override, and models are merged as a union by
         * id (see `merge_provider`), so "delete the old, create the new in models.yaml" would
         * coexist with the old key in config.yaml, yielding two models, one of them still under
         * the old name — which looks exactly like the rename did not take effect. Better to
         * refuse explicitly at the entry point and have the user change it in the file where it is
         * declared. */
        if let Some(old) = rename_from
            && !providers
                .get(provider_id)
                .is_some_and(|provider| provider.models.contains_key(old))
        {
            return Err(EngineError::Invalid(format!(
                "model {provider_id}:{old} is not declared in models.yaml; \
                 rename it where it is declared"
            )));
        }
        match req.create_scope {
            Some(ConfigCreateScope::Provider) => {
                if providers.contains_key(provider_id)
                    || builtin.providers.contains_key(provider_id)
                {
                    return Err(EngineError::Conflict(format!(
                        "provider {provider_id} already exists; names must be unique"
                    )));
                }
            }
            Some(ConfigCreateScope::Model) => {
                /* Any provider the **user declared themselves** can take new models — the
                 * `providers` map holds only the content of user files, the built-in catalog is
                 * not in it, so this one get is the whole gate.
                 *
                 * This once additionally required sdk == openai_generic. That restriction expired
                 * along with "copy preserves the original sdk": none of the model-level fields
                 * (context / thinking / pricing / tier) has anything to do with the dialect, and
                 * there is no reason to block a user from adding models to their own anthropic
                 * provider. */
                let model_id = model_id.ok_or_else(|| {
                    EngineError::Invalid("adding a model requires a model_id".into())
                })?;
                let target = providers.get(provider_id).ok_or_else(|| {
                    EngineError::Invalid(format!(
                        "models can only be added to a provider you declared yourself: \
                         {provider_id}"
                    ))
                })?;
                if target.models.contains_key(model_id) {
                    return Err(EngineError::Conflict(format!(
                        "model {provider_id}:{model_id} already exists; names must be unique"
                    )));
                }
            }
            None if !providers.contains_key(provider_id) => {
                if builtin.providers.contains_key(provider_id) {
                    return Err(EngineError::Invalid(format!(
                        "{provider_id} is a built-in provider; pick a different custom id so the \
                         built-in catalog is not frozen into user config"
                    )));
                }
            }
            None => {}
        }

        let mut provider = providers.remove(provider_id).unwrap_or_default();
        /* sdk: the request decides, then whatever already exists is kept, and only then does it
         * default to openai_generic.
         *
         * This used to write openai_generic unconditionally, so "copy anthropic" produced an
         * openai_generic provider pointing at api.anthropic.com — the save succeeded and the
         * request came back 404. */
        provider.sdk = req.sdk.or(provider.sdk).or(Some(Sdk::OpenAiGeneric));
        provider.base_url = Some(base_url.to_string());
        provider.enabled = true;
        /* The dialect belongs only to openai_generic. Carrying a dialect over to another sdk makes
         * `resolve_client` treat it as a config error, so switching sdk has to clear it rather
         * than leave a dead setting that is configured but never read. */
        if provider.sdk == Some(Sdk::OpenAiGeneric) {
            if let Some(generic) = req.generic {
                provider.generic = Some(generic);
            }
        } else {
            provider.generic = None;
        }
        if let Some(wiring) = req.wiring {
            provider.wiring = wiring;
        }
        if let Some(network) = req.network.as_ref() {
            provider.network = Some(network.clone());
        } else if model_id.is_none() {
            /* A pure provider-level edit: a request without network means back to the default (no
             * limit), and must not leave an old setting the UI can neither show nor delete. Same
             * convention as `pricing`/`tier`, where omitting it clears it. A model-level edit must
             * not touch the provider level — it may not have changed the timeout at all. */
            provider.network = None;
        }
        /* `model_network` is only persisted at the model level (see the model branch below). It is
         * deliberately not handled here, so a model-level timeout is not written into the
         * provider level by accident. */
        if let Some(params) = req.provider_default_params {
            provider.default_params = params;
        }

        let mut model = None;
        let mut renamed: Option<(String, String)> = None;
        if let Some(model_id) = model_id {
            /* Confirm the target id is free before moving: overwriting another model silently eats
             * its price and tier, and that is the kind of loss that is hardest to notice (the UI is
             * left with a single model under the new name). */
            if rename_from.is_some() && provider.models.contains_key(model_id) {
                return Err(EngineError::Conflict(format!(
                    "model {provider_id}:{model_id} already exists; model ids must be unique \
                     within a provider"
                )));
            }
            let mut next = match rename_from {
                Some(old) => provider.models.remove(old).ok_or_else(|| {
                    EngineError::NotFound(format!("no model {old} under provider {provider_id}"))
                })?,
                None => provider.models.remove(model_id).unwrap_or_default(),
            };
            next.wire_model = nonempty(req.wire_model);
            next.display_name = nonempty(req.display_name);
            if req.context_window.is_some() {
                next.context_window = req.context_window;
            }
            if req.max_output_tokens.is_some() {
                next.max_output_tokens = req.max_output_tokens;
            }
            if let Some(params) = req.model_default_params {
                next.default_params = params;
            }
            if let Some(network) = req.model_network.as_ref() {
                next.network = Some(network.clone());
            } else {
                /* A model-level network timeout behaves like pricing / tier: omitting it clears it.
                 * When a request carries model-level fields, the front end (ModelEditor) fills in
                 * the effective value of the model itself (inheritance included) so that nothing
                 * is deleted by mistake. */
                next.network = None;
            }
            if let Some(params) = req.no_think_params {
                next.no_think_params = params;
            }
            if req.vision.is_some() {
                next.vision = req.vision;
            }
            if let Some(thinking) = req.thinking {
                next.thinking = Some(thinking);
            }
            /* Same convention as wire_model: omitting it clears it. See
             * `OpenAiCompatibleProviderReq::pricing` — a mistyped local price has to be removable,
             * otherwise it permanently shadows the correct price refreshed from models.dev. */
            next.pricing = req.pricing;
            next.tier = req.tier;
            provider.models.insert(model_id.to_string(), next);
            if let Some(old) = rename_from {
                renamed = Some((old.to_string(), model_id.to_string()));
            }
            model = Some(model_id);
        }

        let current = self.snapshot().await;
        let mut warnings = Vec::new();
        /* A pure provider-level edit has no "just written model" to validate, so it runs resolution
         * over the first model — it already exists, and if it resolves then the provider-level
         * fields (sdk / dialect / wiring) were not broken; an empty provider with no models has
         * nothing to validate, so it is skipped outright. */
        let validation_target = match model {
            Some(model_id) => Some((
                model_id,
                provider.models.get(model_id).expect("just inserted"),
            )),
            None => provider
                .models
                .iter()
                .next()
                .map(|(id, settings)| (id.as_str(), settings)),
        };
        if let Some((validation_id, validation_model)) = validation_target {
            zlogic_config::resolve_model(
                provider_id,
                &provider,
                validation_id,
                validation_model,
                zlogic_config::provider::ResolveOptions {
                    default_compact_ratio: current.context.compact_ratio,
                    auto_detect_env: current.auto_detect_env,
                    config_revision: current.revision,
                },
                &mut warnings,
            )
            .map_err(EngineError::from)?;
        }

        providers.insert(provider_id.to_string(), provider);
        let mut patch = YamlPatch::new();
        patch.insert(
            "providers".into(),
            Some(serde_yaml_ng::to_value(providers).map_err(|error| {
                EngineError::Invalid(format!("failed to serialize provider: {error}"))
            })?),
        );
        patch_yaml_file(&self.dirs.models_file(), &patch)?;

        /* The rename also rewrites the references pointing at it in `config.yaml`, and that has to
         * happen **before** this reload: with the name already changed but the references still
         * pointing at the old id, the `default_model` read by this reload would point at a model
         * that does not exist (`resolve_default` errors straight out and the main conversation
         * cannot start), while the role chain silently falls back to `session`. */
        if let Some((old_id, new_id)) = &renamed {
            rewrite_model_ref(&self.dirs, provider_id, old_id, new_id)?;
        }

        self.reload_inner().await
    }

    async fn remove_provider_inner(&self, req: ConfigRemoveProviderReq) -> Result<ConfigView> {
        if let Some(expected) = req.expected_revision {
            let current = self.snapshot().await.revision;
            if current != expected {
                return Err(EngineError::Conflict(format!(
                    "config has changed since it was read (expected revision {expected}, \
                     current {current})"
                )));
            }
        }

        let provider_id = req.provider_id.trim();
        let files = zlogic_config::ConfigFiles::read(&self.dirs).map_err(EngineError::from)?;
        let mut providers = files.models.unwrap_or_default().providers;

        match req.model_id.as_deref().map(str::trim) {
            None => {
                if providers.remove(provider_id).is_none() {
                    return Err(EngineError::NotFound(format!(
                        "{provider_id} is not in your own config — built-in providers cannot be \
                         removed (disable it instead)"
                    )));
                }
            }
            Some(model_id) => {
                let provider = providers.get_mut(provider_id).ok_or_else(|| {
                    EngineError::NotFound(format!(
                        "{provider_id} is not in your own config — models of built-in \
                         providers cannot be removed"
                    ))
                })?;
                if provider.models.remove(model_id).is_none() {
                    return Err(EngineError::NotFound(format!(
                        "no model {model_id} under provider {provider_id}"
                    )));
                }
                /* An empty provider is kept — see `ConfigRemoveProviderReq::model_id`:
                 * "delete every model" and "this provider is no longer wanted" are two different
                 * intents, and we do not merge them on the user's behalf. */
            }
        }

        let mut patch = YamlPatch::new();
        patch.insert(
            "providers".into(),
            Some(serde_yaml_ng::to_value(providers).map_err(|error| {
                EngineError::Invalid(format!("failed to serialize provider: {error}"))
            })?),
        );
        patch_yaml_file(&self.dirs.models_file(), &patch)?;
        self.reload_inner().await
    }

    fn catalog_view(&self, cfg: &AppConfig) -> Result<ProviderCatalog> {
        let files = zlogic_config::ConfigFiles::read(&self.dirs).unwrap_or_default();
        let builtin = zlogic_config::builtin_catalog().map_err(EngineError::from)?;
        let effective = zlogic_config::catalog::apply_snapshots(
            builtin,
            files.catalog.as_ref(),
            files.prices.as_ref(),
        );

        let mut as_config = AppConfig::default();
        as_config.auto_detect_env = cfg.auto_detect_env;
        as_config.apply_models(zlogic_config::ModelsFile {
            auto_detect_env: None,
            providers: effective.catalog.providers,
        });

        Ok(ProviderCatalog {
            providers: providers_of(&as_config, &std::collections::BTreeSet::new()),
            prices: files.prices.as_ref().map(|f| PriceSnapshot {
                source: f.source.clone(),
                fetched_at: f
                    .fetched_at
                    .parse()
                    .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH),
                models: f.prices.len() as u32,
            }),
            catalog: files.catalog.as_ref().map(|f| CatalogSnapshot {
                source: zlogic_config::catalog::SOURCE.to_string(),
                version: f.version.clone(),
                fetched_at: f
                    .fetched_at
                    .as_deref()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH),
                models: f.model_count(),
            }),
        })
    }

    async fn fetch_catalog(&self) -> Result<zlogic_config::CatalogFile> {
        let transport = self.transport.as_ref().ok_or_else(|| {
            EngineError::Invalid(
                "this process has no HTTP transport wired up; the catalog cannot be fetched".into(),
            )
        })?;

        let body = transport
            .get(zlogic_config::catalog::SOURCE)
            .await
            .map_err(|e| {
                EngineError::Invalid(format!(
                    "failed to fetch {}: {e}",
                    zlogic_config::catalog::SOURCE
                ))
            })?;
        let text = String::from_utf8(body).map_err(|e| {
            EngineError::Invalid(format!(
                "{} did not return UTF-8 text: {e}",
                zlogic_config::catalog::SOURCE
            ))
        })?;

        let (mut catalog, rejected) =
            zlogic_config::catalog::parse_snapshot(&text).map_err(|e| {
                EngineError::Invalid(format!("{}: {e}", zlogic_config::catalog::SOURCE))
            })?;
        if !rejected.is_empty() {
            tracing::warn!(
                target: "zlogic::config",
                "the catalog on {} has {} unreadable entr{}; skipped: {}",
                zlogic_config::catalog::SOURCE,
                rejected.len(),
                if rejected.len() == 1 { "y" } else { "ies" },
                rejected.join("; ")
            );
        }
        catalog.fetched_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(catalog)
    }

    async fn check_remote_catalog(&self) -> Result<CatalogCheck> {
        let remote = self.fetch_catalog().await?;
        let files = zlogic_config::ConfigFiles::read(&self.dirs).unwrap_or_default();
        let builtin = zlogic_config::builtin_catalog().map_err(EngineError::from)?;
        let current = zlogic_config::catalog::effective_version(&builtin, files.catalog.as_ref());
        let models = remote.model_count();

        Ok(CatalogCheck {
            source: zlogic_config::catalog::SOURCE.to_string(),
            update_available: zlogic_config::catalog::freshness(&remote.version, &current)
                == zlogic_config::catalog::Freshness::Newer,
            current_version: current,
            remote_version: remote.version,
            models,
        })
    }

    async fn apply_remote_catalog(&self) -> Result<ProviderCatalog> {
        let catalog = self.fetch_catalog().await?;
        catalog.write(&self.dirs).map_err(EngineError::from)?;
        self.reload_inner().await?;
        self.catalog_view(self.snapshot().await.as_ref())
    }

    async fn fetch_prices(&self) -> Result<ProviderCatalog> {
        let transport = self.transport.as_ref().ok_or_else(|| {
            EngineError::Invalid(
                "this process has no HTTP transport wired up; prices cannot be refreshed".into(),
            )
        })?;

        let body = transport
            .get(zlogic_config::prices::SOURCE)
            .await
            .map_err(|e| {
                EngineError::Invalid(format!(
                    "failed to fetch {}: {e}",
                    zlogic_config::prices::SOURCE
                ))
            })?;
        let api: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            EngineError::Invalid(format!(
                "{} did not return JSON: {e}",
                zlogic_config::prices::SOURCE
            ))
        })?;

        let catalog = zlogic_config::builtin_catalog().map_err(EngineError::from)?;
        let prices = zlogic_config::prices::prices_from_models_dev(&api, &catalog);
        if prices.is_empty() {
            return Err(EngineError::Invalid(format!(
                "no known model prices could be extracted from {} — its data shape may \
                 have changed",
                zlogic_config::prices::SOURCE
            )));
        }

        zlogic_config::PriceFile {
            source: zlogic_config::prices::SOURCE.into(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
            prices,
        }
        .write(&self.dirs)
        .map_err(EngineError::from)?;

        self.reload_inner().await?;
        self.catalog_view(self.snapshot().await.as_ref())
    }

    async fn summarise(&self, req: UsageSummaryReq) -> Result<UsageSummary> {
        let workspace_id = match req.workspace {
            Some(sel) => Some(
                self.workspaces
                    .get(sel)
                    .await
                    .map_err(|e| EngineError::Invalid(e.to_string()))?
                    .workspace_id,
            ),
            None => None,
        };
        let query = UsageQuery {
            workspace_id,
            session_id: req.session_id,
            self_only: req.self_only,
            session_kind: req.session_kind.map(|kind| match kind {
                zlogic_protocol::UsageSessionKind::Chat => zlogic_store::SessionKind::Chat,
                zlogic_protocol::UsageSessionKind::Task => zlogic_store::SessionKind::Task,
            }),
            turn_id: None,
            since: req.since,
            until: req.until,
            utc_offset_minutes: req.utc_offset_minutes,
        };

        let report = self
            .usage_reports
            .get(crate::usage_cache::ReportKey::of(&query), || {
                let store = self.store.clone();
                let query = query.clone();
                async move {
                    crate::store_call::report_at(
                        &store,
                        "usage.summary",
                        std::panic::Location::caller(),
                        move |db| {
                            let aggregate = db.usage().aggregate(&query)?;
                            let tools = db.usage().aggregate_tools(&query)?;
                            Ok::<_, zlogic_store::StoreError>((aggregate, tools))
                        },
                    )
                    .await
                    .map(|(aggregate, tools)| crate::usage_cache::UsageReport { aggregate, tools })
                    .map_err(EngineError::from)
                }
            })
            .await?;

        Ok(crate::usage::summarise_aggregate(
            report.aggregate.clone(),
            report.tools.clone(),
            &self.snapshot().await.cost,
        ))
    }

    async fn quota_statuses(&self) -> Result<Vec<QuotaStatus>> {
        let cfg = self.snapshot().await;
        let now = chrono::Utc::now();

        // Collect first, aggregate second: several quotas usually resolve to the same window, and
        // running one aggregate per quota made an N-quota config cost N passes over `usage_event`.
        let mut pending = Vec::new();
        for (provider_name, provider) in &cfg.providers {
            if !provider.enabled {
                continue;
            }
            for quota in &provider.quotas {
                let bounds = crate::usage::resolve_quota_window(&quota.window, now)?;
                pending.push((
                    QuotaScope {
                        provider_name: provider_name.clone(),
                        model_name: None,
                    },
                    quota,
                    bounds,
                ));
            }
            for (model_name, model) in &provider.models {
                for quota in &model.quotas {
                    let bounds = crate::usage::resolve_quota_window(&quota.window, now)?;
                    pending.push((
                        QuotaScope {
                            provider_name: provider_name.clone(),
                            model_name: Some(model_name.clone()),
                        },
                        quota,
                        bounds,
                    ));
                }
            }
        }

        let mut aggregates: std::collections::HashMap<
            (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
            zlogic_store::UsageAggregate,
        > = std::collections::HashMap::new();

        let mut statuses = Vec::with_capacity(pending.len());
        for (scope, quota, bounds) in pending {
            let window = (bounds.start, bounds.end);
            if !aggregates.contains_key(&window) {
                let query = UsageQuery {
                    since: Some(bounds.start),
                    until: Some(bounds.end),
                    ..UsageQuery::default()
                };
                let aggregate = crate::store_call::report_at(
                    &self.store,
                    "usage.quotas",
                    std::panic::Location::caller(),
                    move |db| db.usage().aggregate(&query),
                )
                .await?;
                aggregates.insert(window, aggregate);
            }
            let aggregate = aggregates
                .get(&window)
                .expect("the window was aggregated just above");
            statuses.push(crate::usage::evaluate_quota(
                quota, scope, bounds, aggregate, &cfg.cost,
            )?);
        }
        Ok(statuses)
    }
}

#[async_trait]
impl ConfigService for Config {
    async fn get(&self) -> ApiResult<ConfigView> {
        let cfg = self.snapshot().await;
        Ok(self.view(&cfg))
    }

    async fn reload(&self) -> ApiResult<ConfigView> {
        self.reload_inner().await.map_err(ApiError::from)
    }

    async fn update(&self, req: ConfigUpdateReq) -> ApiResult<ConfigView> {
        self.write(req).await.map_err(ApiError::from)
    }

    async fn upsert_openai_compatible(
        &self,
        req: OpenAiCompatibleProviderReq,
    ) -> ApiResult<ConfigView> {
        self.upsert_openai_compatible_inner(req)
            .await
            .map_err(ApiError::from)
    }

    async fn remove_provider(&self, req: ConfigRemoveProviderReq) -> ApiResult<ConfigView> {
        self.remove_provider_inner(req)
            .await
            .map_err(ApiError::from)
    }

    async fn catalog(&self) -> ApiResult<ProviderCatalog> {
        self.catalog_view(self.snapshot().await.as_ref())
            .map_err(ApiError::from)
    }

    async fn refresh_prices(&self) -> ApiResult<ProviderCatalog> {
        self.fetch_prices().await.map_err(ApiError::from)
    }

    async fn check_catalog(&self) -> ApiResult<CatalogCheck> {
        self.check_remote_catalog().await.map_err(ApiError::from)
    }

    async fn apply_catalog(&self) -> ApiResult<ProviderCatalog> {
        self.apply_remote_catalog().await.map_err(ApiError::from)
    }

    async fn usage_summary(&self, req: UsageSummaryReq) -> ApiResult<UsageSummary> {
        self.summarise(req).await.map_err(ApiError::from)
    }

    async fn usage_quotas(&self) -> ApiResult<Vec<QuotaStatus>> {
        self.quota_statuses().await.map_err(ApiError::from)
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn validate_model_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(EngineError::Invalid("model id must not be empty".into()));
    }
    if id.trim() != id {
        return Err(EngineError::Invalid(
            "model id must not start or end with whitespace; trim it first".into(),
        ));
    }
    if id.contains(':') {
        return Err(EngineError::Invalid(
            "model id must not contain ':': that is the separator of a provider:model reference"
                .into(),
        ));
    }
    if id.chars().any(char::is_control) {
        return Err(EngineError::Invalid(
            "model id must not contain control characters".into(),
        ));
    }
    Ok(())
}

fn rewrite_model_ref(dirs: &Dirs, provider_id: &str, old_id: &str, new_id: &str) -> Result<()> {
    let old_ref = format!("{provider_id}:{old_id}");
    let new_ref = format!("{provider_id}:{new_id}");
    let mut patch = YamlPatch::new();

    if let Some(file) = zlogic_config::ConfigFiles::read(dirs)
        .map_err(EngineError::from)?
        .config
    {
        if file.default_model.as_deref() == Some(old_ref.as_str()) {
            patch.insert("default_model".into(), text_or_default(&new_ref));
        }
        let mut roles = file.llm_roles;
        let mut touched = false;
        for settings in roles.values_mut() {
            for candidate in &mut settings.models {
                if candidate == &old_ref {
                    *candidate = new_ref.clone();
                    touched = true;
                }
            }
        }
        if touched {
            patch.insert("llm_roles".into(), value_or_default(&roles)?);
        }
    }

    if !patch.is_empty() {
        patch_yaml_file(&dirs.config_file(), &patch)?;
    }
    Ok(())
}

fn declared_providers(dirs: &Dirs) -> std::collections::BTreeSet<String> {
    let files = zlogic_config::ConfigFiles::read(dirs).unwrap_or_default();
    let from_models = files.models.iter().flat_map(|f| f.providers.keys());
    let from_config = files.config.iter().flat_map(|f| f.providers.keys());
    from_models.chain(from_config).cloned().collect()
}

fn apply_update(cfg: &mut AppConfig, req: &ConfigUpdateReq) {
    if let Some(m) = &req.default_model {
        let t = m.trim();
        cfg.default_model = (!t.is_empty()).then(|| t.to_owned());
    }
    if let Some(v) = &req.session {
        cfg.session = v.clone();
    }
    if let Some(v) = &req.context {
        cfg.context = v.clone();
    }
    if let Some(v) = &req.tools {
        cfg.tools = v.clone();
    }
    if let Some(v) = &req.log {
        cfg.log = v.clone();
    }
    if let Some(v) = &req.worktree {
        cfg.worktree = v.clone();
    }
    if let Some(v) = &req.cost {
        cfg.cost = v.clone();
    }
    if let Some(v) = &req.limits {
        cfg.limits = v.clone();
    }
    if let Some(v) = &req.network {
        cfg.network = v.clone();
    }
    if let Some(v) = &req.auto_detect_env {
        cfg.auto_detect_env = *v;
    }
    if let Some(v) = &req.llm_roles {
        cfg.llm_roles = v.clone();
    }
}

fn providers_of(
    cfg: &AppConfig,
    declared: &std::collections::BTreeSet<String>,
) -> Vec<ProviderConfig> {
    cfg.providers
        .iter()
        .filter(|(_, p)| p.enabled)
        .map(|(id, p)| ProviderConfig {
            provider_id: id.clone(),
            origin: if declared.contains(id) {
                ProviderOrigin::User
            } else {
                ProviderOrigin::Builtin
            },
            sdk: p.sdk.unwrap_or(zlogic_protocol::config::Sdk::OpenAiChat),
            base_url: p.base_url.clone(),
            guide_url: p.guide_url.clone(),
            credential_refs: credential_candidates(id, cfg.auto_detect_env)
                .iter()
                .map(ToString::to_string)
                .collect(),
            generic: p.generic.clone(),
            wiring: p.wiring.clone(),
            network: p.network.clone(),
            default_params: p.default_params.clone(),
            quotas: p.quotas.clone(),
            models: p
                .models
                .keys()
                .filter_map(|mid| {
                    let model_ref = format!("{id}:{mid}");
                    match cfg.resolve(&model_ref) {
                        Ok((resolved, _)) => Some(ModelConfig {
                            model_id: mid.clone(),
                            wire_model: Some(resolved.wire_model),
                            display_name: Some(resolved.display_name),
                            context_window: resolved.context_window,
                            max_output_tokens: resolved.max_output_tokens,
                            compaction_threshold: resolved.compaction_threshold,
                            capabilities: resolved.capabilities,
                            pricing: resolved.pricing,
                            default_params: resolved.default_params,
                            no_think_params: p.models[mid].no_think_params.clone(),
                            tier: p.models[mid].tier,
                            quotas: p.models[mid].quotas.clone(),
                            network: p.models[mid].network.clone(),
                            sdk: match &resolved.client {
                                zlogic_protocol::config::ClientSpec::Builtin { sdk } => Some(*sdk),
                                zlogic_protocol::config::ClientSpec::OpenAiGeneric(_) => {
                                    Some(zlogic_protocol::config::Sdk::OpenAiGeneric)
                                }
                            },
                        }),
                        Err(e) => {
                            tracing::warn!(
                                target: "zlogic::engine",
                                model = %model_ref,
                                "a model in the config could not be parsed and will be missing from the settings page: {e}"
                            );
                            None
                        }
                    }
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeCredentialStore(HashMap<String, String>);

    impl CredentialStore for FakeCredentialStore {
        fn resolve(&self, credential_ref: &str) -> Option<String> {
            self.0.get(credential_ref).cloned()
        }
    }

    struct Rig {
        config: Config,
        _home: tempfile::TempDir,
    }

    impl Rig {
        fn new(yaml: &str, keys: &[(&str, &str)]) -> Self {
            let home = tempfile::tempdir().unwrap();
            let dirs = Dirs {
                config: home.path().join("config"),
                data: home.path().join("data"),
                state: home.path().join("state"),
                cache: home.path().join("cache"),
            };
            std::fs::create_dir_all(&dirs.config).unwrap();
            std::fs::write(dirs.config_file(), yaml).unwrap();

            let cfg = Arc::new(AppConfig::load_with(&dirs, |_| None).unwrap());
            let store = SharedStore::new(zlogic_store::Db::open_in_memory().unwrap());
            let workspaces: Arc<dyn WorkspaceService> =
                Arc::new(crate::Workspaces::new(store.clone()));
            let credential_store = Arc::new(FakeCredentialStore(
                keys.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));

            let config = Config::new(cfg, dirs, credential_store, store, workspaces);
            Self {
                config,
                _home: home,
            }
        }
    }

    const YAML: &str = r#"
default_model: anthropic:opus
providers:
  anthropic:
    sdk: anthropic
    models:
      opus:
        context_window: 200000
        display_name: Opus
"#;

    #[tokio::test]
    async fn the_view_reports_the_effective_configuration() {
        let rig = Rig::new(YAML, &[("env:ANTHROPIC_API_KEY", "sk-ant-secret-tail")]);
        let view = rig.config.get().await.unwrap();

        assert_eq!(view.default_model.as_deref(), Some("anthropic:opus"));
        assert!(
            view.settings.session.approval_mode == zlogic_protocol::settings::ApprovalMode::Auto,
            "full authorization must never be on by default"
        );
        assert!(view.global_path.ends_with("config.yaml") || view.global_path.contains("config"));

        let provider = &view.providers[0];
        assert_eq!(provider.provider_id, "anthropic");
        assert_eq!(
            provider.guide_url.as_deref(),
            Some("https://platform.claude.com/settings/keys")
        );
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].context_window, 200_000);
        assert_eq!(provider.models[0].display_name.as_deref(), Some("Opus"));
    }

    #[tokio::test]
    async fn a_model_without_an_explicit_window_shows_the_effective_default() {
        let rig = Rig::new(
            r#"
providers:
  anthropic:
    sdk: anthropic
    models:
      opus: {}
"#,
            &[],
        );
        let view = rig.config.get().await.unwrap();
        assert_eq!(view.providers[0].models[0].context_window, 200_000);
        assert!(
            view.providers[0].models[0].capabilities.thinking.supported,
            "anthropic's thinking capability should be derived"
        );
    }

    // ── reload ──

    #[tokio::test]
    async fn reloading_picks_up_a_changed_file_and_bumps_the_revision() {
        let rig = Rig::new(YAML, &[]);
        let before = rig.config.get().await.unwrap();

        std::fs::write(
            rig.config.dirs.config_file(),
            YAML.replace(
                "default_model: anthropic:opus",
                "default_model: anthropic:haiku",
            )
            .replace("      opus:", "      haiku:"),
        )
        .unwrap();

        let after = rig.config.reload().await.unwrap();
        assert_eq!(after.default_model.as_deref(), Some("anthropic:haiku"));
        assert!(
            after.revision > before.revision,
            "the snapshot pinned within a turn uses it to detect staleness"
        );
    }

    #[tokio::test]
    async fn reloading_an_unchanged_file_still_bumps_the_revision() {
        let rig = Rig::new(YAML, &[]);
        let a = rig.config.reload().await.unwrap().revision;
        let b = rig.config.reload().await.unwrap().revision;
        assert!(b > a);
    }

    #[tokio::test]
    async fn writing_a_section_lands_on_disk_and_shows_up_in_the_view() {
        let rig = Rig::new(YAML, &[]);
        let view = rig
            .config
            .update(ConfigUpdateReq {
                context: Some(zlogic_config::ContextConfig {
                    compact_ratio: 0.6,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!((view.settings.context.compact_ratio - 0.6).abs() < 1e-6);
        let on_disk = std::fs::read_to_string(rig.config.dirs.config_file()).unwrap();
        assert!(on_disk.contains("compact_ratio: 0.6"), "{on_disk}");
        assert!(
            on_disk.contains("default_model: anthropic:opus"),
            "{on_disk}"
        );
    }

    #[tokio::test]
    async fn resetting_a_section_removes_the_key_instead_of_freezing_todays_default() {
        let rig = Rig::new(YAML, &[]);
        rig.config
            .update(ConfigUpdateReq {
                context: Some(zlogic_config::ContextConfig {
                    compact_ratio: 0.6,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(rig.config.dirs.config_file())
                .unwrap()
                .contains("context:")
        );

        rig.config
            .update(ConfigUpdateReq {
                context: Some(zlogic_config::ContextConfig::default()),
                ..Default::default()
            })
            .await
            .unwrap();

        let on_disk = std::fs::read_to_string(rig.config.dirs.config_file()).unwrap();
        assert!(!on_disk.contains("context:"), "{on_disk}");
        assert!(!on_disk.contains("compact_ratio"), "{on_disk}");
    }

    #[tokio::test]
    async fn clearing_the_default_model_removes_the_key() {
        let rig = Rig::new(YAML, &[]);
        let view = rig
            .config
            .update(ConfigUpdateReq {
                default_model: Some(String::new()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(view.default_model, None);
        let on_disk = std::fs::read_to_string(rig.config.dirs.config_file()).unwrap();
        assert!(!on_disk.contains("default_model"), "{on_disk}");
    }

    #[tokio::test]
    async fn auto_detect_env_is_written_to_the_models_file() {
        let rig = Rig::new(YAML, &[]);
        let view = rig
            .config
            .update(ConfigUpdateReq {
                auto_detect_env: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!view.settings.auto_detect_env);

        let models = std::fs::read_to_string(rig.config.dirs.models_file()).unwrap();
        assert!(models.contains("auto_detect_env: false"), "{models}");
        let config = std::fs::read_to_string(rig.config.dirs.config_file()).unwrap();
        assert!(!config.contains("auto_detect_env"), "{config}");
    }

    #[tokio::test]
    async fn an_invalid_value_is_refused_and_the_file_is_left_alone() {
        let rig = Rig::new(YAML, &[]);
        let before = std::fs::read_to_string(rig.config.dirs.config_file()).unwrap();

        let err = rig
            .config
            .update(ConfigUpdateReq {
                context: Some(zlogic_config::ContextConfig {
                    compact_ratio: 2.0,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect_err("2.0 must be rejected");
        assert!(
            format!("{err:?}").contains("Invalid") || format!("{err:?}").contains("invalid"),
            "{err:?}"
        );

        assert_eq!(
            std::fs::read_to_string(rig.config.dirs.config_file()).unwrap(),
            before,
            "a rejected write must not leave any trace"
        );
    }

    #[tokio::test]
    async fn a_stale_expected_revision_is_refused() {
        let rig = Rig::new(YAML, &[]);
        let stale = rig.config.get().await.unwrap().revision;
        rig.config.reload().await.unwrap();

        let err = rig
            .config
            .update(ConfigUpdateReq {
                default_model: Some("anthropic:opus".into()),
                expected_revision: Some(stale),
                ..Default::default()
            })
            .await
            .expect_err("a stale revision must be rejected");
        assert!(
            format!("{err:?}").to_lowercase().contains("conflict"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn writing_bumps_the_revision() {
        let rig = Rig::new(YAML, &[]);
        let before = rig.config.get().await.unwrap().revision;
        let after = rig
            .config
            .update(ConfigUpdateReq {
                default_model: Some("anthropic:opus".into()),
                expected_revision: Some(before),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(after.revision > before);
    }

    #[tokio::test]
    async fn openai_compatible_upsert_writes_a_generic_provider_without_a_secret() {
        let rig = Rig::new("", &[]);
        let before = rig.config.get().await.unwrap().revision;
        let request = OpenAiCompatibleProviderReq {
            provider_id: "company-gateway".into(),
            base_url: "https://llm.example.test/v1/".into(),
            model_id: Some("coding-model".into()),
            rename_from: None,
            wire_model: None,
            display_name: Some("Coding Model".into()),
            context_window: Some(64_000),
            max_output_tokens: Some(8_192),
            sdk: None,
            tier: None,
            pricing: None,
            generic: Some(zlogic_protocol::config::GenericOpenAiDialect {
                reasoning_carrier: Some("reasoning_content".into()),
                effort_field: Some("reasoning_effort".into()),
                usage_fields: zlogic_protocol::config::UsageFieldMap {
                    input: Some("prompt_tokens".into()),
                    output: Some("completion_tokens".into()),
                    reasoning: Some("completion_tokens_details.reasoning_tokens".into()),
                    input_includes_cache: Some(true),
                    output_includes_reasoning: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            }),
            wiring: None,
            network: None,
            model_network: None,
            provider_default_params: None,
            model_default_params: None,
            no_think_params: Some(std::collections::BTreeMap::from([(
                "reasoning_effort".into(),
                serde_json::json!("none"),
            )])),
            vision: None,
            thinking: Some(zlogic_protocol::config::ThinkingCapability {
                supported: true,
                can_disable: true,
                efforts: vec![
                    zlogic_protocol::llm::Effort::Low,
                    zlogic_protocol::llm::Effort::High,
                ],
                budget: false,
            }),
            create_scope: Some(ConfigCreateScope::Provider),
            expected_revision: Some(before),
        };
        let view = rig
            .config
            .upsert_openai_compatible(request.clone())
            .await
            .unwrap();

        let provider = view
            .providers
            .iter()
            .find(|provider| provider.provider_id == "company-gateway")
            .unwrap();
        assert_eq!(provider.origin, ProviderOrigin::User);
        assert_eq!(provider.sdk, Sdk::OpenAiGeneric);
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://llm.example.test/v1")
        );
        assert_eq!(provider.models[0].model_id, "coding-model");
        assert_eq!(provider.models[0].context_window, 64_000);

        let yaml = std::fs::read_to_string(rig.config.dirs.models_file()).unwrap();
        assert!(yaml.contains("sdk: openai_generic"));
        assert!(yaml.contains("usage_fields:"));
        assert!(yaml.contains("completion_tokens_details.reasoning_tokens"));
        assert!(yaml.contains("no_think_params:"));
        assert!(!yaml.contains("api_key"));
        assert!(!yaml.contains("credential"));

        let duplicate_model = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                create_scope: Some(ConfigCreateScope::Model),
                expected_revision: Some(view.revision),
                ..request.clone()
            })
            .await
            .expect_err("duplicate names are not allowed when creating a model");
        assert!(
            format!("{duplicate_model:?}")
                .to_lowercase()
                .contains("conflict"),
            "{duplicate_model:?}"
        );

        let duplicate = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                expected_revision: Some(view.revision),
                ..request
            })
            .await
            .expect_err("duplicate names are not allowed when creating a provider");
        assert!(
            format!("{duplicate:?}").to_lowercase().contains("conflict"),
            "{duplicate:?}"
        );
    }

    #[tokio::test]
    async fn provider_only_upsert_preserves_models_and_rewrites_provider_fields() {
        let rig = Rig::new("", &[]);
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                create_scope: Some(ConfigCreateScope::Provider),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                model_id: Some("m2".into()),
                display_name: Some("Second".into()),
                context_window: Some(128_000),
                create_scope: Some(ConfigCreateScope::Model),
                ..bare_upsert("gw", "m2")
            })
            .await
            .unwrap();

        let view = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                base_url: "https://new.example.test/v1".into(),
                provider_default_params: Some(std::collections::BTreeMap::from([(
                    "timeout".into(),
                    serde_json::json!(30),
                )])),
                model_id: None,
                expected_revision: None,
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();

        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://new.example.test/v1")
        );
        assert_eq!(provider.default_params["timeout"], serde_json::json!(30));

        let ids: Vec<&str> = provider
            .models
            .iter()
            .map(|m| m.model_id.as_str())
            .collect();
        assert_eq!(ids, ["m1", "m2"], "both models must be there");
        let m2 = provider.models.iter().find(|m| m.model_id == "m2").unwrap();
        assert_eq!(m2.display_name.as_deref(), Some("Second"));
        assert_eq!(
            m2.context_window, 128_000,
            "a model-level field must not have been touched"
        );
    }

    #[tokio::test]
    async fn toggling_bypass_reaches_the_policy_gate_without_a_restart() {
        let flag = crate::BypassFlag::new(false);
        let rig = Rig::new(YAML, &[]);
        let config = rig.config.with_bypass_flag(flag.clone());

        config
            .update(ConfigUpdateReq {
                session: Some(zlogic_config::SessionConfig {
                    approval_mode: zlogic_protocol::settings::ApprovalMode::Bypass,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(flag.enabled(), "turning it on must take effect immediately");

        config
            .update(ConfigUpdateReq {
                session: Some(zlogic_config::SessionConfig::default()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !flag.enabled(),
            "turning it off must take effect immediately"
        );
    }

    #[tokio::test]
    async fn a_declared_provider_is_marked_as_the_users_own() {
        let rig = Rig::new(YAML, &[]);
        let view = rig.config.get().await.unwrap();
        let anthropic = view
            .providers
            .iter()
            .find(|p| p.provider_id == "anthropic")
            .unwrap();
        assert_eq!(
            anthropic.origin,
            zlogic_protocol::config::ProviderOrigin::User
        );
    }

    #[tokio::test]
    async fn a_broken_file_leaves_the_running_configuration_alone() {
        let rig = Rig::new(YAML, &[]);
        std::fs::write(
            rig.config.dirs.config_file(),
            "providers: [this is not a map",
        )
        .unwrap();

        assert!(rig.config.reload().await.is_err());

        let view = rig.config.get().await.unwrap();
        assert_eq!(view.default_model.as_deref(), Some("anthropic:opus"));
        assert_eq!(view.providers[0].models.len(), 1);
    }

    // ── usage_summary ──

    #[tokio::test]
    async fn an_empty_database_summarises_to_zero() {
        let rig = Rig::new(YAML, &[]);
        let s = rig
            .config
            .usage_summary(UsageSummaryReq {
                workspace: None,
                session_id: None,
                session_kind: None,
                since: None,
                until: None,
                utc_offset_minutes: 0,
                self_only: false,
            })
            .await
            .unwrap();
        assert_eq!(s.calls, 0);
        assert_eq!(s.cost, None);
    }

    #[tokio::test]
    async fn the_summary_covers_recorded_usage() {
        let rig = Rig::new(YAML, &[]);
        let session = rig.config.store.with(|db| {
            let ws = db
                .workspaces()
                .resolve("/tmp/zlogic-config-test")
                .unwrap()
                .0;
            db.sessions()
                .create(zlogic_store::NewSession::root(ws.workspace_id))
                .unwrap()
        });
        let turn = zlogic_protocol::TurnId::new();
        rig.config.store.with(|db| {
            db.usage()
                .record(
                    zlogic_store::NewUsage::new(
                        session.session_id,
                        zlogic_store::Purpose::Main,
                        zlogic_protocol::usage::TokenUsage {
                            input: 1_000,
                            output: 50,
                            ..Default::default()
                        },
                    )
                    .in_round(turn, zlogic_protocol::RoundId::new())
                    .with_timing(
                        Some("2026-07-30T08:00:00Z".parse().unwrap()),
                        Some("2026-07-30T08:00:00.500Z".parse().unwrap()),
                        Some("2026-07-30T08:00:02Z".parse().unwrap()),
                    )
                    .with_cost(
                        0.25,
                        "USD",
                        zlogic_protocol::usage::CostSource::LocalPricing,
                    ),
                )
                .unwrap();
        });

        let s = rig
            .config
            .usage_summary(UsageSummaryReq {
                workspace: None,
                session_id: None,
                session_kind: None,
                since: None,
                until: None,
                utc_offset_minutes: 0,
                self_only: false,
            })
            .await
            .unwrap();

        assert_eq!(s.calls, 1);
        assert_eq!(s.tokens.input, 1_000);
        assert!((s.cost.unwrap().amount - 0.25).abs() < 1e-9);
        assert_eq!(s.by_session.len(), 1);
        assert_eq!(s.by_day.len(), 1);
        assert_eq!(s.avg_first_token_ms, Some(500));
        assert_eq!(s.avg_response_ms, Some(2_000));
        assert_eq!(
            s.avg_turn_ms,
            Some(2_000),
            "a single turn's envelope is itself"
        );

        let scoped = rig
            .config
            .usage_summary(UsageSummaryReq {
                workspace: None,
                session_id: Some(session.session_id),
                session_kind: None,
                since: None,
                until: None,
                utc_offset_minutes: 0,
                self_only: false,
            })
            .await
            .unwrap();
        assert_eq!(scoped.current_context_tokens, Some(1_000));
    }

    #[tokio::test]
    async fn usage_summary_filters_task_sessions_end_to_end() {
        let rig = Rig::new(YAML, &[]);
        let (chat, task) = rig.config.store.with(|db| {
            let ws = db
                .workspaces()
                .resolve("/tmp/zlogic-usage-kind-test")
                .unwrap()
                .0;
            (
                db.sessions()
                    .create(zlogic_store::NewSession::root(ws.workspace_id))
                    .unwrap(),
                db.sessions()
                    .create(zlogic_store::NewSession::task(ws.workspace_id))
                    .unwrap(),
            )
        });
        rig.config.store.with(|db| {
            for (session_id, input) in [(chat.session_id, 100), (task.session_id, 900)] {
                db.usage()
                    .record(zlogic_store::NewUsage::new(
                        session_id,
                        zlogic_store::Purpose::Main,
                        zlogic_protocol::usage::TokenUsage {
                            input,
                            ..Default::default()
                        },
                    ))
                    .unwrap();
            }
        });

        let summary = rig
            .config
            .usage_summary(UsageSummaryReq {
                workspace: None,
                session_id: None,
                session_kind: Some(zlogic_protocol::UsageSessionKind::Task),
                since: None,
                until: None,
                utc_offset_minutes: 0,
                self_only: false,
            })
            .await
            .unwrap();
        assert_eq!(summary.tokens.input, 900);
        assert_eq!(summary.sessions, 1);
    }

    #[tokio::test]
    async fn the_range_filter_excludes_records_outside_it() {
        let rig = Rig::new(YAML, &[]);
        let session = rig.config.store.with(|db| {
            let ws = db.workspaces().resolve("/tmp/zlogic-range-test").unwrap().0;
            db.sessions()
                .create(zlogic_store::NewSession::root(ws.workspace_id))
                .unwrap()
        });
        rig.config.store.with(|db| {
            db.usage()
                .record(zlogic_store::NewUsage::new(
                    session.session_id,
                    zlogic_store::Purpose::Main,
                    zlogic_protocol::usage::TokenUsage {
                        input: 5,
                        ..Default::default()
                    },
                ))
                .unwrap();
        });

        let s = rig
            .config
            .usage_summary(UsageSummaryReq {
                workspace: None,
                session_id: None,
                session_kind: None,
                since: Some("2020-01-01T00:00:00Z".parse().unwrap()),
                until: Some("2020-01-02T00:00:00Z".parse().unwrap()),
                utc_offset_minutes: 0,
                self_only: false,
            })
            .await
            .unwrap();
        assert_eq!(s.calls, 0);
    }

    fn bare_upsert(provider_id: &str, model_id: &str) -> OpenAiCompatibleProviderReq {
        OpenAiCompatibleProviderReq {
            provider_id: provider_id.into(),
            base_url: "https://llm.example.test/v1".into(),
            model_id: Some(model_id.into()),
            rename_from: None,
            wire_model: None,
            display_name: None,
            context_window: Some(64_000),
            max_output_tokens: None,
            sdk: None,
            generic: None,
            wiring: None,
            network: None,
            model_network: None,
            provider_default_params: None,
            model_default_params: None,
            no_think_params: None,
            vision: None,
            thinking: None,
            pricing: None,
            tier: None,
            create_scope: None,
            expected_revision: None,
        }
    }

    #[tokio::test]
    async fn a_copied_provider_keeps_its_sdk_and_carries_no_dialect() {
        let rig = Rig::new("", &[]);
        let request = OpenAiCompatibleProviderReq {
            base_url: "https://api.anthropic.com".into(),
            sdk: Some(Sdk::Anthropic),
            create_scope: Some(ConfigCreateScope::Provider),
            ..bare_upsert("anthropic-copy", "claude-sonnet-5")
        };
        let view = rig.config.upsert_openai_compatible(request).await.unwrap();

        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "anthropic-copy")
            .unwrap();
        assert_eq!(provider.sdk, Sdk::Anthropic);
        assert!(provider.generic.is_none());

        let yaml = std::fs::read_to_string(rig.config.dirs.models_file()).unwrap();
        assert!(yaml.contains("sdk: anthropic"), "{yaml}");
        assert!(!yaml.contains("generic"), "{yaml}");
    }

    #[tokio::test]
    async fn switching_away_from_generic_drops_the_dialect() {
        let rig = Rig::new("", &[]);
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                generic: Some(zlogic_protocol::config::GenericOpenAiDialect {
                    reasoning_carrier: Some("reasoning_content".into()),
                    ..Default::default()
                }),
                create_scope: Some(ConfigCreateScope::Provider),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(rig.config.dirs.models_file())
                .unwrap()
                .contains("generic:")
        );

        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                sdk: Some(Sdk::DeepSeek),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();

        let view = rig.config.get().await.unwrap();
        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert_eq!(provider.sdk, Sdk::DeepSeek);
        assert!(provider.generic.is_none());

        let yaml = std::fs::read_to_string(rig.config.dirs.models_file()).unwrap();
        assert!(yaml.contains("sdk: deepseek"), "{yaml}");
        assert!(!yaml.contains("reasoning_carrier"), "{yaml}");
    }

    #[tokio::test]
    async fn pricing_and_tier_round_trip_and_can_be_cleared() {
        let rig = Rig::new("", &[]);
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                pricing: Some(zlogic_protocol::config::Pricing {
                    input_per_m: 2.0,
                    cached_input_per_m: Some(0.5),
                    cache_write_per_m: None,
                    output_per_m: 8.0,
                    currency: "CNY".into(),
                }),
                tier: Some(zlogic_protocol::config::Tier::Light),
                create_scope: Some(ConfigCreateScope::Provider),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();

        let view = rig.config.get().await.unwrap();
        let model = &view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap()
            .models[0];
        let pricing = model.pricing.as_ref().unwrap();
        assert_eq!(pricing.input_per_m, 2.0);
        assert_eq!(pricing.output_per_m, 8.0);
        assert_eq!(pricing.currency, "CNY");
        assert_eq!(model.tier, Some(zlogic_protocol::config::Tier::Light));

        rig.config
            .upsert_openai_compatible(bare_upsert("gw", "m1"))
            .await
            .unwrap();
        let view = rig.config.get().await.unwrap();
        let model = &view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap()
            .models[0];
        assert!(model.pricing.is_none());
        assert!(model.tier.is_none());
    }

    #[tokio::test]
    async fn network_timeouts_round_trip_and_can_be_cleared() {
        let rig = Rig::new("", &[]);
        let network = zlogic_protocol::config::NetworkConfig {
            connect_timeout_ms: Some(10_000),
            read_timeout_ms: Some(120_000),
        };
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                network: Some(network.clone()),
                create_scope: Some(ConfigCreateScope::Provider),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();

        let view = rig.config.get().await.unwrap();
        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert_eq!(provider.network, Some(network));
        assert!(provider.models[0].network.is_none());

        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                model_network: Some(zlogic_protocol::config::NetworkConfig {
                    connect_timeout_ms: None,
                    read_timeout_ms: Some(600_000),
                }),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();
        let view = rig.config.get().await.unwrap();
        let model = &view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap()
            .models[0];
        assert_eq!(model.network.as_ref().unwrap().connect_timeout_ms, None);
        assert_eq!(
            model.network.as_ref().unwrap().read_timeout_ms,
            Some(600_000)
        );

        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                model_id: None,
                network: None,
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();
        let view = rig.config.get().await.unwrap();
        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert!(provider.network.is_none());
    }

    #[tokio::test]
    async fn removing_a_model_leaves_the_provider_behind() {
        let rig = Rig::new("", &[]);
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                create_scope: Some(ConfigCreateScope::Provider),
                ..bare_upsert("gw", "m1")
            })
            .await
            .unwrap();
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                create_scope: Some(ConfigCreateScope::Model),
                ..bare_upsert("gw", "m2")
            })
            .await
            .unwrap();

        let view = rig
            .config
            .remove_provider(ConfigRemoveProviderReq {
                provider_id: "gw".into(),
                model_id: Some("m1".into()),
                expected_revision: None,
            })
            .await
            .unwrap();
        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].model_id, "m2");

        rig.config
            .remove_provider(ConfigRemoveProviderReq {
                provider_id: "gw".into(),
                model_id: Some("m2".into()),
                expected_revision: None,
            })
            .await
            .unwrap();
        let yaml = std::fs::read_to_string(rig.config.dirs.models_file()).unwrap();
        assert!(yaml.contains("gw:"), "{yaml}");

        rig.config
            .remove_provider(ConfigRemoveProviderReq {
                provider_id: "gw".into(),
                model_id: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        let yaml = std::fs::read_to_string(rig.config.dirs.models_file()).unwrap();
        assert!(!yaml.contains("gw:"), "{yaml}");
    }

    #[tokio::test]
    async fn a_builtin_provider_cannot_be_removed() {
        let rig = Rig::new("", &[]);
        let error = rig
            .config
            .remove_provider(ConfigRemoveProviderReq {
                provider_id: "anthropic".into(),
                model_id: None,
                expected_revision: None,
            })
            .await
            .unwrap_err();
        assert!(
            error.category == zlogic_protocol::ErrorCategory::NotFound,
            "deleting a built-in provider must be not_found, actually {error:?}"
        );
    }

    #[tokio::test]
    async fn renaming_a_model_moves_the_whole_entry_and_keeps_its_fields() {
        let rig = Rig::new("", &[]);
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                pricing: Some(zlogic_protocol::config::Pricing {
                    input_per_m: 2.0,
                    cached_input_per_m: None,
                    cache_write_per_m: None,
                    output_per_m: 8.0,
                    currency: "USD".into(),
                }),
                tier: Some(zlogic_protocol::config::Tier::Main),
                ..bare_upsert("gw", "vendor/model")
            })
            .await
            .unwrap();
        let existing = rig.config.get().await.unwrap();
        let source = existing
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap()
            .models
            .iter()
            .find(|m| m.model_id == "vendor/model")
            .unwrap()
            .clone();

        let view = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                model_id: Some("model".into()),
                rename_from: Some("vendor/model".into()),
                wire_model: source.wire_model.clone(),
                display_name: source.display_name.clone(),
                context_window: Some(source.context_window),
                max_output_tokens: source.max_output_tokens,
                pricing: source.pricing.clone(),
                tier: source.tier,
                thinking: Some(source.capabilities.thinking.clone()),
                vision: source.capabilities.vision,
                ..bare_upsert("gw", "model")
            })
            .await
            .unwrap();

        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert!(
            provider.models.is_empty()
                || provider.models.iter().all(|m| m.model_id != "vendor/model"),
            "the old id should no longer be in the config"
        );
        let model = provider
            .models
            .iter()
            .find(|m| m.model_id == "model")
            .expect("the new id should be in the config");
        assert_eq!(model.wire_model.as_deref(), Some("vendor/model"));
        assert_eq!(model.tier, Some(zlogic_protocol::config::Tier::Main));
        assert_eq!(model.pricing.as_ref().map(|p| p.output_per_m), Some(8.0));
    }

    #[tokio::test]
    async fn renaming_a_model_rewrites_the_references_in_config_yaml() {
        let rig = Rig::new(
            r#"
default_model: gw:vendor/model
llm_roles:
  title:
    models: ["gw:vendor/model", light]
  compaction:
    models: [main]
"#,
            &[],
        );
        rig.config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                ..bare_upsert("gw", "vendor/model")
            })
            .await
            .unwrap();

        let view = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                model_id: Some("model".into()),
                rename_from: Some("vendor/model".into()),
                wire_model: None,
                ..bare_upsert("gw", "model")
            })
            .await
            .unwrap();

        assert_eq!(view.default_model.as_deref(), Some("gw:model"));
        let title = view.llm_roles.get("title").unwrap();
        assert_eq!(title.models, ["gw:model", "light"]);
        assert_eq!(view.llm_roles.get("compaction").unwrap().models, ["main"]);
        let yaml = std::fs::read_to_string(rig.config.dirs.config_file()).unwrap();
        assert!(!yaml.contains("gw:vendor/model"), "{yaml}");
    }

    #[tokio::test]
    async fn renaming_onto_an_existing_model_is_a_conflict() {
        let rig = Rig::new("", &[]);
        for id in ["vendor/model", "model"] {
            rig.config
                .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                    ..bare_upsert("gw", id)
                })
                .await
                .unwrap();
        }

        let error = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                model_id: Some("model".into()),
                rename_from: Some("vendor/model".into()),
                ..bare_upsert("gw", "model")
            })
            .await
            .unwrap_err();
        assert!(
            error.category == zlogic_protocol::ErrorCategory::Conflict,
            "renaming onto an existing name must be conflict, actually {error:?}"
        );
        let view = rig.config.get().await.unwrap();
        let provider = view
            .providers
            .iter()
            .find(|p| p.provider_id == "gw")
            .unwrap();
        assert!(provider.models.iter().any(|m| m.model_id == "vendor/model"));
        assert!(provider.models.iter().any(|m| m.model_id == "model"));
    }

    #[tokio::test]
    async fn model_ids_reject_colons_and_accept_slashes() {
        let rig = Rig::new("", &[]);
        let with_slash = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                ..bare_upsert("gw", "vendor/model")
            })
            .await;
        assert!(
            with_slash.is_ok(),
            "an id with a slash should be accepted: {with_slash:?}"
        );

        let with_colon = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                ..bare_upsert("gw", "vendor:model")
            })
            .await;
        let error = with_colon.expect_err("an id with a colon must be rejected");
        assert!(
            error.category == zlogic_protocol::ErrorCategory::InvalidArgument,
            "it should be invalid, actually {error:?}"
        );
    }

    #[tokio::test]
    async fn renaming_a_model_declared_in_config_yaml_is_refused_clearly() {
        let rig = Rig::new(
            r#"
providers:
  gw:
    base_url: https://llm.example.test/v1
    models:
      "vendor/model":
        context_window: 64000
"#,
            &[],
        );
        let error = rig
            .config
            .upsert_openai_compatible(OpenAiCompatibleProviderReq {
                provider_id: "gw".into(),
                base_url: "https://llm.example.test/v1".into(),
                model_id: Some("model".into()),
                rename_from: Some("vendor/model".into()),
                context_window: Some(64_000),
                ..bare_upsert("gw", "model")
            })
            .await
            .unwrap_err();
        assert!(
            error.category == zlogic_protocol::ErrorCategory::InvalidArgument,
            "renaming a model declared in config.yaml should be refused outright, actually {error:?}"
        );
    }

    #[tokio::test]
    async fn the_usage_report_is_cached_per_query_and_fresh_for_another_one() {
        let rig = Rig::new(YAML, &[]);
        let session = rig
            .config
            .store
            .with(|db| {
                db.sessions()
                    .create(zlogic_store::session::NewSession::root(
                        zlogic_protocol::WorkspaceId::new(),
                    ))
            })
            .unwrap();
        let record = |input: u64| {
            let mut usage = zlogic_store::NewUsage::new(
                session.session_id,
                zlogic_store::Purpose::Main,
                zlogic_protocol::TokenUsage {
                    input,
                    ..Default::default()
                },
            );
            usage.cost = Some(1.0);
            usage.currency = Some("USD".into());
            rig.config
                .store
                .with(|db| db.usage().record(usage))
                .unwrap();
        };
        let request = |offset: i32| UsageSummaryReq {
            workspace: None,
            session_id: None,
            self_only: false,
            session_kind: None,
            since: None,
            until: None,
            utc_offset_minutes: offset,
        };

        record(100);
        let first = rig.config.summarise(request(480)).await.unwrap();
        assert_eq!((first.calls, first.tokens.input), (1, 100));

        // The identical request is answered from the cache: it cannot see the row written just now.
        record(100);
        let cached = rig.config.summarise(request(480)).await.unwrap();
        assert_eq!(
            (cached.calls, cached.tokens.input),
            (1, 100),
            "a repeated request must be served from the cache, not recomputed"
        );

        // A different query is a different entry, so it sees both rows.
        let fresh = rig.config.summarise(request(-300)).await.unwrap();
        assert_eq!((fresh.calls, fresh.tokens.input), (2, 200));
    }
}
