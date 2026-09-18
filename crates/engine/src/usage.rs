//! |---|---|

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use zlogic_protocol::query::{ToolUsageGroup, UsageGroup, UsageSummary};
use zlogic_protocol::settings::CostConfig;
use zlogic_protocol::stream::{ToolStats, ToolStatus};
use zlogic_protocol::usage::{
    CostSource, CostTotal, CurrencyAmount, Purpose, QuotaConfig, QuotaMetric, QuotaPeriod,
    QuotaScope, QuotaState, QuotaStatus, QuotaWeekStart, QuotaWindow, TokenUsage,
};
use zlogic_store::{ToolUsageAggregate, UsageAggregate, UsageAggregatePart, UsageRow};

pub struct ToolObservation {
    pub name: String,
    pub status: ToolStatus,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaWindowBounds {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub reset_at: Option<DateTime<Utc>>,
    pub rolling_hours: Option<u32>,
}

pub fn resolve_quota_window(
    window: &QuotaWindow,
    now: DateTime<Utc>,
) -> crate::Result<QuotaWindowBounds> {
    match window {
        QuotaWindow::Rolling { hours } => Ok(QuotaWindowBounds {
            start: now - Duration::hours(i64::from(*hours)),
            end: now,
            reset_at: None,
            rolling_hours: Some(*hours),
        }),
        QuotaWindow::Calendar {
            period,
            timezone,
            week_start,
            anchor_day,
        } => {
            let tz: Tz = timezone.parse().map_err(|_| {
                crate::EngineError::Invalid(format!("invalid quota timezone: {timezone}"))
            })?;
            let local_date = now.with_timezone(&tz).date_naive();
            let (start_date, reset_date) = match period {
                QuotaPeriod::Day => (
                    local_date,
                    local_date.succ_opt().ok_or_else(|| {
                        crate::EngineError::Invalid(
                            "quota date is out of the supported range".into(),
                        )
                    })?,
                ),
                QuotaPeriod::Week => {
                    let offset = match week_start {
                        QuotaWeekStart::Mon => local_date.weekday().num_days_from_monday(),
                        QuotaWeekStart::Sun => local_date.weekday().num_days_from_sunday(),
                    };
                    let start = local_date - Duration::days(i64::from(offset));
                    (start, start + Duration::days(7))
                }
                QuotaPeriod::Month => {
                    let anchor = (*anchor_day).clamp(1, 28);
                    let start = if local_date.day() >= u32::from(anchor) {
                        NaiveDate::from_ymd_opt(
                            local_date.year(),
                            local_date.month(),
                            u32::from(anchor),
                        )
                    } else {
                        let (year, month) = previous_month(local_date.year(), local_date.month());
                        NaiveDate::from_ymd_opt(year, month, u32::from(anchor))
                    }
                    .ok_or_else(|| {
                        crate::EngineError::Invalid("invalid quota month window".into())
                    })?;
                    let (year, month) = next_month(start.year(), start.month());
                    let reset = NaiveDate::from_ymd_opt(year, month, u32::from(anchor))
                        .ok_or_else(|| {
                            crate::EngineError::Invalid("invalid quota month window".into())
                        })?;
                    (start, reset)
                }
            };
            Ok(QuotaWindowBounds {
                start: local_midnight(tz, start_date)?,
                end: now,
                reset_at: Some(local_midnight(tz, reset_date)?),
                rolling_hours: None,
            })
        }
    }
}

fn local_midnight(tz: Tz, date: NaiveDate) -> crate::Result<DateTime<Utc>> {
    let local = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| crate::EngineError::Invalid("invalid quota date".into()))?;
    let zoned = match tz.from_local_datetime(&local) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, _) => first,
        LocalResult::None => {
            return Err(crate::EngineError::Invalid(format!(
                "{date} has no midnight in timezone {tz}"
            )));
        }
    };
    Ok(zoned.with_timezone(&Utc))
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

pub fn evaluate_quota(
    quota: &QuotaConfig,
    scope: QuotaScope,
    bounds: QuotaWindowBounds,
    aggregate: &UsageAggregate,
    cost: &CostConfig,
) -> crate::Result<QuotaStatus> {
    let parts = if let Some(model) = &scope.model_name {
        let key = format!("{}:{model}", scope.provider_name);
        aggregate
            .by_model
            .iter()
            .filter(|part| part.key == key)
            .collect::<Vec<_>>()
    } else {
        aggregate
            .by_provider
            .iter()
            .filter(|part| part.key == scope.provider_name)
            .collect::<Vec<_>>()
    };

    let used = match quota.metric {
        QuotaMetric::InputTokens => parts.iter().map(|part| part.tokens.input as f64).sum(),
        QuotaMetric::OutputTokens => parts.iter().map(|part| part.tokens.output as f64).sum(),
        QuotaMetric::TotalTokens => parts
            .iter()
            .map(|part| (part.tokens.input + part.tokens.output) as f64)
            .sum(),
        QuotaMetric::Requests => parts.iter().map(|part| f64::from(part.calls)).sum(),
        QuotaMetric::CostUsd => {
            let currency = quota_currency(quota);
            let mut total = 0.0;
            for part in parts {
                if let Some(amount) = part.cost {
                    total += cost
                        .convert_to(amount, &part.currency, &currency)
                        .ok_or_else(|| {
                            crate::EngineError::Invalid(format!(
                                "quota `{}` cannot convert {} to {}: configure an exchange rate",
                                quota.label, part.currency, currency
                            ))
                        })?;
                }
            }
            total
        }
    };
    let pct = if quota.limit > 0.0 {
        used / quota.limit
    } else {
        0.0
    };
    let warn_at = quota
        .warn_at
        .or_else(|| {
            quota
                .warn
                .filter(|value| *value > 0.0)
                .map(|value| quota.limit * value)
        })
        .unwrap_or(f64::INFINITY);
    let state = if quota.limit > 0.0 && used >= quota.limit {
        QuotaState::Exceeded
    } else if used >= warn_at {
        QuotaState::Warn
    } else {
        QuotaState::Ok
    };
    Ok(QuotaStatus {
        label: quota.label.clone(),
        scope,
        metric: quota.metric,
        used,
        limit: quota.limit,
        pct,
        state,
        currency: (quota.metric == QuotaMetric::CostUsd).then(|| quota_currency(quota)),
        window_start_ms: bounds.start.timestamp_millis(),
        window_end_ms: bounds.end.timestamp_millis(),
        window_reset_at_ms: bounds.reset_at.map(|value| value.timestamp_millis()),
        rolling_hours: bounds.rolling_hours,
    })
}

fn quota_currency(quota: &QuotaConfig) -> String {
    let currency = quota.currency.as_deref().unwrap_or("USD").trim();
    if currency.is_empty() {
        "USD".into()
    } else {
        currency.to_ascii_uppercase()
    }
}

pub fn summarise(rows: &[UsageRow], tools: &[ToolObservation], cost: &CostConfig) -> UsageSummary {
    let mut tokens = TokenUsage::default();
    let mut total = CostAcc::default();
    let mut calls = 0u32;
    let mut aux_calls = 0u32;
    let mut estimated_calls = 0u32;
    let mut aux_cost = CostAcc::default();
    let mut max_input_tokens = 0u64;
    let mut sessions = std::collections::HashSet::new();
    let mut turns = std::collections::HashSet::new();

    let mut by_model: HashMap<String, Acc> = HashMap::new();
    let mut by_provider: HashMap<String, Acc> = HashMap::new();
    let mut by_day: HashMap<String, Acc> = HashMap::new();
    let mut by_session: HashMap<String, Acc> = HashMap::new();
    let mut by_workspace: HashMap<String, Acc> = HashMap::new();
    let mut by_aux_purpose: HashMap<String, Acc> = HashMap::new();
    let mut by_cost_source: HashMap<String, Acc> = HashMap::new();
    let mut envelopes: HashMap<
        (zlogic_protocol::SessionId, zlogic_protocol::TurnId),
        (DateTime<Utc>, DateTime<Utc>),
    > = HashMap::new();

    for row in rows {
        let r = &row.record;
        calls += 1;
        if is_aux(&r.purpose) {
            aux_calls += 1;
            aux_cost.add(r);
            by_aux_purpose
                .entry(r.purpose.as_wire())
                .or_default()
                .add(r);
        }
        if r.cost_source == Some(CostSource::Estimated) {
            estimated_calls += 1;
        }
        tokens.add(&r.tokens);
        max_input_tokens = max_input_tokens.max(r.tokens.input);
        total.add(r);
        sessions.insert(r.session_id);
        if let Some(turn_id) = r.turn_id {
            turns.insert(turn_id);
            if let (Some(start), Some(end)) = (&r.request_started_at, &r.completed_at) {
                let span = envelopes
                    .entry((r.session_id, turn_id))
                    .or_insert((*start, *end));
                span.0 = span.0.min(*start);
                span.1 = span.1.max(*end);
            }
        }
        by_cost_source
            .entry(
                r.cost_source
                    .map(cost_source_key)
                    .unwrap_or("unknown")
                    .to_string(),
            )
            .or_default()
            .add(r);

        if let Some(model_ref) = &r.model_ref {
            by_model.entry(model_ref.clone()).or_default().add(r);
            let provider = model_ref
                .split_once(':')
                .map_or(model_ref.as_str(), |(provider, _)| provider);
            by_provider.entry(provider.to_string()).or_default().add(r);
        }
        by_day.entry(day_key(r.created_at)).or_default().add(r);
        by_session
            .entry(r.session_id.to_string())
            .or_default()
            .add(r);
        if let Some(ws) = row.workspace_id {
            by_workspace.entry(ws.to_string()).or_default().add(r);
        }
    }

    let mut envelope_total_ms = 0u64;
    let mut envelope_count = 0u32;
    let mut envelope_by_session: HashMap<String, (u64, u32)> = HashMap::new();
    for ((session, _turn), (start, end)) in envelopes.into_iter() {
        let ms = (end - start).num_milliseconds().max(0) as u64;
        envelope_total_ms += ms;
        envelope_count += 1;
        let span = envelope_by_session.entry(session.to_string()).or_default();
        span.0 += ms;
        span.1 += 1;
    }

    let mut ttft_sum_ms = 0u64;
    let mut ttft_n = 0u32;
    let mut response_sum_ms = 0u64;
    let mut response_n = 0u32;
    for row in rows {
        let r = &row.record;
        if r.purpose != Purpose::Main {
            continue;
        }
        if let (Some(start), Some(first)) = (&r.request_started_at, &r.first_token_at) {
            ttft_sum_ms += (*first - *start).num_milliseconds().max(0) as u64;
            ttft_n += 1;
        }
        if let (Some(start), Some(end)) = (&r.request_started_at, &r.completed_at) {
            response_sum_ms += (*end - *start).num_milliseconds().max(0) as u64;
            response_n += 1;
        }
    }

    let mut by_session = ranked(by_session, cost);
    for group in &mut by_session {
        if let Some((sum, n)) = envelope_by_session.get(&group.key) {
            group.avg_turn_ms = mean_ms(*sum, *n);
        }
    }

    UsageSummary {
        tokens,
        current_context_tokens: None,
        max_input_tokens,
        cost: total.into_total(cost),
        calls,
        main_calls: calls.saturating_sub(aux_calls),
        aux_calls,
        sessions: sessions.len() as u32,
        turns: turns.len() as u32,
        estimated_calls,
        aux_cost: aux_cost.into_total(cost),
        total_tool_calls: tools.len() as u32,
        total_tool_duration_ms: tools.iter().map(|tool| tool.duration_ms).sum(),
        avg_first_token_ms: mean_ms(ttft_sum_ms, ttft_n),
        avg_response_ms: mean_ms(response_sum_ms, response_n),
        avg_turn_ms: mean_ms(envelope_total_ms, envelope_count),
        by_model: ranked(by_model, cost),
        by_provider: ranked(by_provider, cost),
        by_day: by_date(by_day, cost),
        by_session,
        by_workspace: ranked(by_workspace, cost),
        by_aux_purpose: ranked(by_aux_purpose, cost),
        by_cost_source: ranked(by_cost_source, cost),
        latest_turn: None,
        by_tool: tool_groups(tools),
    }
}

pub fn summarise_aggregate(
    aggregate: UsageAggregate,
    tools: Vec<ToolUsageAggregate>,
    cost: &CostConfig,
) -> UsageSummary {
    let total = merge_parts(aggregate.total, cost)
        .into_iter()
        .next()
        .unwrap_or_else(|| empty_group(String::new()));
    let total_tool_calls = tools.iter().map(|tool| tool.stats.total).sum();
    let total_tool_duration_ms = tools.iter().map(|tool| tool.duration_ms).sum();
    let estimated_calls = total.estimated_calls;
    let aux_cost = total_cost(&aggregate.by_aux_purpose, cost);
    let latest_turn = merge_parts(aggregate.latest_turn, cost).into_iter().next();

    let mut envelope_total_ms = 0u64;
    let mut envelope_count = 0u32;
    let mut envelope_by_session: HashMap<String, (u64, u32)> = HashMap::new();
    for envelope in &aggregate.turn_envelopes {
        envelope_total_ms += envelope.duration_ms;
        envelope_count += 1;
        let span = envelope_by_session
            .entry(envelope.session_id.to_string())
            .or_default();
        span.0 += envelope.duration_ms;
        span.1 += 1;
    }

    let mut by_session = ranked_groups(merge_parts(aggregate.by_session, cost));
    for group in &mut by_session {
        if let Some((sum, n)) = envelope_by_session.get(&group.key) {
            group.avg_turn_ms = mean_ms(*sum, *n);
        }
    }

    UsageSummary {
        tokens: total.tokens,
        current_context_tokens: aggregate.current_context_tokens,
        max_input_tokens: total.max_input_tokens,
        cost: total.cost,
        calls: total.calls,
        main_calls: total.calls.saturating_sub(total.aux_calls),
        aux_calls: total.aux_calls,
        sessions: total.sessions,
        turns: total.turns,
        estimated_calls,
        aux_cost,
        total_tool_calls,
        total_tool_duration_ms,
        avg_first_token_ms: total.avg_first_token_ms,
        avg_response_ms: total.avg_response_ms,
        avg_turn_ms: mean_ms(envelope_total_ms, envelope_count),
        by_model: ranked_groups(merge_parts(aggregate.by_model, cost)),
        by_provider: ranked_groups(merge_parts(aggregate.by_provider, cost)),
        by_day: date_groups(merge_parts(aggregate.by_day, cost)),
        by_session,
        by_workspace: ranked_groups(merge_parts(aggregate.by_workspace, cost)),
        by_aux_purpose: ranked_groups(merge_parts(aggregate.by_aux_purpose, cost)),
        by_cost_source: key_groups(merge_parts(aggregate.by_cost_source, cost)),
        latest_turn,
        by_tool: tools
            .into_iter()
            .map(|tool| ToolUsageGroup {
                name: tool.name,
                stats: tool.stats,
                duration_ms: tool.duration_ms,
            })
            .collect(),
    }
}

fn is_aux(purpose: &Purpose) -> bool {
    !matches!(purpose, Purpose::Main | Purpose::Agent(_))
}

fn confidence(src: CostSource) -> u8 {
    match src {
        CostSource::Estimated => 0,
        CostSource::LocalPricing => 1,
        CostSource::ProviderReported => 2,
    }
}

fn day_key(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

#[derive(Default)]
struct CostAcc {
    by_currency: std::collections::BTreeMap<String, f64>,
    source: Option<CostSource>,
}

impl CostAcc {
    fn add(&mut self, r: &zlogic_store::UsageRecord) {
        let Some(amount) = r.cost else { return };
        let currency = r
            .currency
            .clone()
            .unwrap_or_else(|| "USD".into())
            .trim()
            .to_ascii_uppercase();
        *self.by_currency.entry(currency).or_default() += amount;
        self.source = Some(match (self.source, r.cost_source) {
            (Some(a), Some(b)) if confidence(b) < confidence(a) => b,
            (Some(a), _) => a,
            (None, b) => b.unwrap_or(CostSource::LocalPricing),
        });
    }

    fn add_part(&mut self, amount: Option<f64>, currency: String, source: Option<CostSource>) {
        let Some(amount) = amount else { return };
        *self.by_currency.entry(currency).or_default() += amount;
        self.source = Some(match (self.source, source) {
            (Some(a), Some(b)) if confidence(b) < confidence(a) => b,
            (Some(a), _) => a,
            (None, value) => value.unwrap_or(CostSource::LocalPricing),
        });
    }

    fn into_total(self, cost: &CostConfig) -> Option<CostTotal> {
        let source = self.source?;
        let mut amount = 0.0;
        let mut unconverted = Vec::new();
        for (currency, sum) in self.by_currency {
            match cost.convert(sum, &currency) {
                Some(converted) => amount += converted,
                None => unconverted.push(CurrencyAmount {
                    amount: sum,
                    currency,
                }),
            }
        }
        Some(CostTotal {
            amount,
            currency: cost.display(),
            source,
            unconverted,
        })
    }
}

#[derive(Default)]
struct Acc {
    tokens: TokenUsage,
    cost: CostAcc,
    calls: u32,
    aux_calls: u32,
    estimated_calls: u32,
    sessions: std::collections::HashSet<zlogic_protocol::SessionId>,
    turns: std::collections::HashSet<zlogic_protocol::TurnId>,
    max_input_tokens: u64,
    ttft_sum_ms: u64,
    ttft_n: u32,
    response_sum_ms: u64,
    response_n: u32,
}

impl Acc {
    fn add(&mut self, r: &zlogic_store::UsageRecord) {
        self.tokens.add(&r.tokens);
        self.max_input_tokens = self.max_input_tokens.max(r.tokens.input);
        self.calls += 1;
        if is_aux(&r.purpose) {
            self.aux_calls += 1;
        }
        if r.cost_source == Some(CostSource::Estimated) {
            self.estimated_calls += 1;
        }
        self.sessions.insert(r.session_id);
        if let Some(turn_id) = r.turn_id {
            self.turns.insert(turn_id);
        }
        self.cost.add(r);
        if r.purpose == Purpose::Main {
            if let (Some(start), Some(first)) = (&r.request_started_at, &r.first_token_at) {
                self.ttft_sum_ms += (*first - *start).num_milliseconds().max(0) as u64;
                self.ttft_n += 1;
            }
            if let (Some(start), Some(end)) = (&r.request_started_at, &r.completed_at) {
                self.response_sum_ms += (*end - *start).num_milliseconds().max(0) as u64;
                self.response_n += 1;
            }
        }
    }

    fn into_group(self, key: String, cost: &CostConfig) -> UsageGroup {
        UsageGroup {
            key,
            label: None,
            calls: self.calls,
            sessions: self.sessions.len() as u32,
            turns: self.turns.len() as u32,
            aux_calls: self.aux_calls,
            estimated_calls: self.estimated_calls,
            tokens: self.tokens,
            max_input_tokens: self.max_input_tokens,
            cost: self.cost.into_total(cost),
            avg_first_token_ms: mean_ms(self.ttft_sum_ms, self.ttft_n),
            avg_response_ms: mean_ms(self.response_sum_ms, self.response_n),
            avg_turn_ms: None,
        }
    }
}

#[derive(Default)]
struct PartAcc {
    label: Option<String>,
    calls: u32,
    aux_calls: u32,
    estimated_calls: u32,
    sessions: u32,
    turns: u32,
    tokens: TokenUsage,
    max_input_tokens: u64,
    cost: CostAcc,
    ttft_sum_ms: u64,
    ttft_n: u32,
    response_sum_ms: u64,
    response_n: u32,
}

fn merge_parts(parts: Vec<UsageAggregatePart>, cost: &CostConfig) -> Vec<UsageGroup> {
    let mut groups: HashMap<String, PartAcc> = HashMap::new();
    for part in parts {
        let group = groups.entry(part.key).or_default();
        if group.label.is_none() {
            group.label = part.label;
        }
        group.calls += part.calls;
        group.aux_calls += part.aux_calls;
        group.estimated_calls += part.estimated_calls;
        group.sessions += part.sessions;
        group.turns += part.turns;
        group.tokens.add(&part.tokens);
        group.max_input_tokens = group.max_input_tokens.max(part.max_input_tokens);
        group
            .cost
            .add_part(part.cost, part.currency, part.cost_source);
        group.ttft_sum_ms += part.ttft_sum_ms;
        group.ttft_n += part.ttft_n;
        group.response_sum_ms += part.response_sum_ms;
        group.response_n += part.response_n;
    }
    groups
        .into_iter()
        .map(|(key, group)| UsageGroup {
            key,
            label: group.label,
            calls: group.calls,
            sessions: group.sessions,
            turns: group.turns,
            aux_calls: group.aux_calls,
            estimated_calls: group.estimated_calls,
            tokens: group.tokens,
            max_input_tokens: group.max_input_tokens,
            cost: group.cost.into_total(cost),
            avg_first_token_ms: mean_ms(group.ttft_sum_ms, group.ttft_n),
            avg_response_ms: mean_ms(group.response_sum_ms, group.response_n),
            avg_turn_ms: None,
        })
        .collect()
}

fn total_cost(parts: &[UsageAggregatePart], cost: &CostConfig) -> Option<CostTotal> {
    let mut acc = CostAcc::default();
    for part in parts {
        acc.add_part(part.cost, part.currency.clone(), part.cost_source);
    }
    acc.into_total(cost)
}

fn empty_group(key: String) -> UsageGroup {
    UsageGroup {
        key,
        label: None,
        calls: 0,
        sessions: 0,
        turns: 0,
        aux_calls: 0,
        estimated_calls: 0,
        tokens: TokenUsage::default(),
        max_input_tokens: 0,
        cost: None,
        avg_first_token_ms: None,
        avg_response_ms: None,
        avg_turn_ms: None,
    }
}

fn mean_ms(sum: u64, n: u32) -> Option<u64> {
    if n > 0 {
        Some(sum / u64::from(n))
    } else {
        None
    }
}

fn ranked_groups(mut groups: Vec<UsageGroup>) -> Vec<UsageGroup> {
    groups.sort_by(|a, b| {
        let amount = |group: &UsageGroup| group.cost.as_ref().map(|c| c.amount).unwrap_or(0.0);
        amount(b)
            .total_cmp(&amount(a))
            .then_with(|| {
                (b.tokens.input + b.tokens.output).cmp(&(a.tokens.input + a.tokens.output))
            })
            .then_with(|| a.key.cmp(&b.key))
    });
    groups
}

fn date_groups(mut groups: Vec<UsageGroup>) -> Vec<UsageGroup> {
    groups.sort_by(|a, b| a.key.cmp(&b.key));
    groups
}

fn key_groups(mut groups: Vec<UsageGroup>) -> Vec<UsageGroup> {
    groups.sort_by(|a, b| a.key.cmp(&b.key));
    groups
}

fn cost_source_key(source: CostSource) -> &'static str {
    match source {
        CostSource::ProviderReported => "provider_reported",
        CostSource::LocalPricing => "local_pricing",
        CostSource::Estimated => "estimated",
    }
}

fn ranked(map: HashMap<String, Acc>, cost: &CostConfig) -> Vec<UsageGroup> {
    let mut groups: Vec<UsageGroup> = map
        .into_iter()
        .map(|(k, a)| a.into_group(k, cost))
        .collect();
    groups.sort_by(|a, b| {
        let amount = |g: &UsageGroup| g.cost.as_ref().map(|c| c.amount).unwrap_or(0.0);
        amount(b)
            .total_cmp(&amount(a))
            .then_with(|| {
                (b.tokens.input + b.tokens.output).cmp(&(a.tokens.input + a.tokens.output))
            })
            .then_with(|| a.key.cmp(&b.key))
    });
    groups
}

fn tool_groups(tools: &[ToolObservation]) -> Vec<ToolUsageGroup> {
    let mut map: HashMap<String, (ToolStats, u64)> = HashMap::new();
    for t in tools {
        let e = map.entry(t.name.clone()).or_default();
        e.0.record(t.status);
        e.1 += t.duration_ms;
    }
    let mut groups: Vec<ToolUsageGroup> = map
        .into_iter()
        .map(|(name, (stats, duration_ms))| ToolUsageGroup {
            name,
            stats,
            duration_ms,
        })
        .collect();
    groups.sort_by(|a, b| {
        b.duration_ms
            .cmp(&a.duration_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

fn by_date(map: HashMap<String, Acc>, cost: &CostConfig) -> Vec<UsageGroup> {
    let mut groups: Vec<UsageGroup> = map
        .into_iter()
        .map(|(k, a)| a.into_group(k, cost))
        .collect();
    groups.sort_by(|a, b| a.key.cmp(&b.key));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::{SessionId, TurnId, WorkspaceId};
    use zlogic_store::{TurnEnvelope, UsageRecord};

    fn row(
        model: Option<&str>,
        purpose: Purpose,
        input: u64,
        cost: Option<f64>,
        ws: Option<WorkspaceId>,
    ) -> UsageRow {
        UsageRow {
            record: UsageRecord {
                usage_id: zlogic_protocol::UsageId::new(),
                session_id: SessionId::new(),
                turn_id: None,
                round_id: None,
                purpose,
                model_ref: model.map(str::to_string),
                tokens: TokenUsage {
                    input,
                    output: 10,
                    ..Default::default()
                },
                cost,
                currency: cost.map(|_| "USD".to_string()),
                cost_source: cost.map(|_| CostSource::LocalPricing),
                created_at: Utc::now(),
                request_started_at: None,
                first_token_at: None,
                completed_at: None,
            },
            workspace_id: ws,
        }
    }

    fn row_in(model: &str, cost: f64, currency: &str) -> UsageRow {
        let mut r = row(Some(model), Purpose::Main, 1_000, Some(cost), None);
        r.record.currency = Some(currency.to_string());
        r
    }

    fn rates(from: &str, to: &str, rate: f64) -> CostConfig {
        CostConfig {
            display_currency: to.into(),
            rates: vec![zlogic_protocol::settings::ExchangeRate {
                from: from.into(),
                to: to.into(),
                rate,
            }],
        }
    }

    #[test]
    fn the_auxiliary_total_is_computed_here_not_by_the_ui() {
        let rows = vec![
            row(Some("openai:gpt-5"), Purpose::Main, 1_000, Some(1.0), None),
            row(Some("openai:gpt-5"), Purpose::Title, 100, Some(0.25), None),
            row(
                Some("openai:gpt-5"),
                Purpose::Compaction,
                200,
                Some(0.5),
                None,
            ),
        ];
        let s = summarise(&rows, &[], &CostConfig::default());

        let aux = s
            .aux_cost
            .expect("aux records mean there should be a total");
        assert!((aux.amount - 0.75).abs() < 1e-9, "{}", aux.amount);
        assert!((s.cost.unwrap().amount - 1.75).abs() < 1e-9);

        let by_row: f64 = s
            .by_aux_purpose
            .iter()
            .filter_map(|g| g.cost.as_ref())
            .map(|c| c.amount)
            .sum();
        assert!((aux.amount - by_row).abs() < 1e-9);
    }

    #[test]
    fn no_auxiliary_calls_means_no_auxiliary_total() {
        let rows = vec![row(
            Some("openai:gpt-5"),
            Purpose::Main,
            1_000,
            Some(1.0),
            None,
        )];
        assert_eq!(summarise(&rows, &[], &CostConfig::default()).aux_cost, None);
    }

    #[test]
    fn the_auxiliary_total_still_reports_what_it_could_not_convert() {
        let mut title = row_in("openai:gpt-5", 0.25, "USD");
        title.record.purpose = Purpose::Title;
        let mut compaction = row_in("deepseek:v4", 700.0, "CNY");
        compaction.record.purpose = Purpose::Compaction;

        let s = summarise(&[title, compaction], &[], &CostConfig::default());
        let aux = s.aux_cost.unwrap();

        assert!(
            (aux.amount - 0.25).abs() < 1e-9,
            "only the USD row can be added up"
        );
        assert_eq!(aux.unconverted.len(), 1);
        assert_eq!(aux.unconverted[0].currency, "CNY");
        assert!((aux.unconverted[0].amount - 700.0).abs() < 1e-9);
    }

    #[test]
    fn costs_in_another_currency_are_converted_before_being_added() {
        let cost = CostConfig {
            display_currency: "USD".into(),
            rates: vec![zlogic_protocol::settings::ExchangeRate {
                from: "USD".into(),
                to: "CNY".into(),
                rate: 7.0,
            }],
        };
        let rows = vec![
            row_in("openai:gpt-5", 1.0, "USD"),
            row_in("deepseek:v4", 7.0, "CNY"),
        ];
        let total = summarise(&rows, &[], &cost).cost.unwrap();
        assert_eq!(total.currency, "USD");
        assert!((total.amount - 2.0).abs() < 1e-9, "{}", total.amount);
        assert!(total.unconverted.is_empty());

        let total = summarise(&rows, &[], &rates("USD", "CNY", 7.0))
            .cost
            .unwrap();
        assert_eq!(total.currency, "CNY");
        assert!((total.amount - 14.0).abs() < 1e-9, "{}", total.amount);
    }

    #[test]
    fn a_cost_that_cannot_be_converted_is_reported_separately_not_summed() {
        let rows = vec![
            row_in("openai:gpt-5", 1.5, "USD"),
            row_in("deepseek:v4", 700.0, "CNY"),
        ];
        let total = summarise(&rows, &[], &CostConfig::default()).cost.unwrap();

        assert_eq!(total.currency, "USD");
        assert!(
            (total.amount - 1.5).abs() < 1e-9,
            "only the USD row can be added up"
        );
        assert_eq!(total.unconverted.len(), 1);
        assert_eq!(total.unconverted[0].currency, "CNY");
        assert!((total.unconverted[0].amount - 700.0).abs() < 1e-9);
    }

    #[test]
    fn every_group_is_converted_the_same_way() {
        let rows = vec![
            row_in("openai:gpt-5", 1.0, "USD"),
            row_in("deepseek:v4", 7.0, "CNY"),
        ];
        let s = summarise(&rows, &[], &rates("USD", "CNY", 7.0));
        for group in s.by_model.iter().chain(&s.by_provider) {
            let cost = group.cost.as_ref().unwrap();
            assert_eq!(cost.currency, "CNY", "{} was not converted", group.key);
            assert!(
                (cost.amount - 7.0).abs() < 1e-9,
                "{} = {}",
                group.key,
                cost.amount
            );
        }
        let day = s.by_day[0].cost.as_ref().unwrap();
        assert_eq!(day.currency, "CNY");
        assert!((day.amount - 14.0).abs() < 1e-9, "{}", day.amount);
    }

    #[test]
    fn a_row_without_a_currency_is_treated_as_usd() {
        let mut r = row_in("openai:gpt-5", 2.0, "USD");
        r.record.currency = None;
        let total = summarise(&[r], &[], &CostConfig::default()).cost.unwrap();
        assert!((total.amount - 2.0).abs() < 1e-9);
        assert!(total.unconverted.is_empty());
    }

    #[test]
    fn the_unconverted_list_is_ordered_deterministically() {
        let rows = vec![
            row_in("a:m", 1.0, "JPY"),
            row_in("b:m", 1.0, "CNY"),
            row_in("c:m", 1.0, "EUR"),
        ];
        let total = summarise(&rows, &[], &CostConfig::default()).cost.unwrap();
        let seen: Vec<&str> = total
            .unconverted
            .iter()
            .map(|c| c.currency.as_str())
            .collect();
        assert_eq!(seen, ["CNY", "EUR", "JPY"]);
    }

    #[test]
    fn totals_include_auxiliary_calls_but_count_them_separately() {
        let rows = vec![
            row(
                Some("anthropic:opus"),
                Purpose::Main,
                1_000,
                Some(0.10),
                None,
            ),
            row(
                Some("anthropic:haiku"),
                Purpose::Title,
                100,
                Some(0.01),
                None,
            ),
            row(
                Some("anthropic:haiku"),
                Purpose::Compaction,
                200,
                Some(0.02),
                None,
            ),
        ];
        let s = summarise(&rows, &[], &CostConfig::default());

        assert_eq!(s.calls, 3);
        assert_eq!(s.aux_calls, 2, "titles and compaction are aux");
        assert_eq!(s.tokens.input, 1_300);
        assert!((s.cost.unwrap().amount - 0.13).abs() < 1e-9);
    }

    #[test]
    fn a_sub_agents_own_loop_is_not_auxiliary() {
        let rows = vec![
            row(
                Some("m"),
                Purpose::Agent("researcher".into()),
                500,
                None,
                None,
            ),
            row(Some("m"), Purpose::Main, 500, None, None),
        ];
        assert_eq!(summarise(&rows, &[], &CostConfig::default()).aux_calls, 0);
    }

    #[test]
    fn provider_is_the_first_segment_of_the_model_ref() {
        let rows = vec![
            row(Some("anthropic:opus"), Purpose::Main, 100, None, None),
            row(Some("anthropic:haiku"), Purpose::Main, 100, None, None),
            row(Some("openai:gpt"), Purpose::Main, 100, None, None),
        ];
        let s = summarise(&rows, &[], &CostConfig::default());

        assert_eq!(s.by_model.len(), 3);
        let providers: Vec<&str> = s.by_provider.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(providers.len(), 2);
        assert!(providers.contains(&"anthropic"));
        let anthropic = s.by_provider.iter().find(|g| g.key == "anthropic").unwrap();
        assert_eq!(anthropic.calls, 2);
    }

    #[test]
    fn records_without_a_key_still_count_towards_the_totals() {
        let rows = vec![
            row(None, Purpose::Main, 700, Some(0.05), None),
            row(
                Some("m"),
                Purpose::Main,
                300,
                None,
                Some(WorkspaceId::new()),
            ),
        ];
        let s = summarise(&rows, &[], &CostConfig::default());

        assert_eq!(s.calls, 2);
        assert_eq!(
            s.tokens.input, 1_000,
            "the row with no model_ref still counts"
        );
        assert_eq!(s.by_model.len(), 1, "but it cannot go into by_model");
        assert_eq!(
            s.by_workspace.len(),
            1,
            "the row whose session was deleted cannot go into by_workspace"
        );
    }

    #[test]
    fn the_overall_cost_confidence_is_the_weakest_one() {
        let mut estimated = row(Some("m"), Purpose::Main, 100, Some(1.0), None);
        estimated.record.cost_source = Some(CostSource::Estimated);
        let mut reported = row(Some("m"), Purpose::Main, 100, Some(2.0), None);
        reported.record.cost_source = Some(CostSource::ProviderReported);

        let s = summarise(&[reported, estimated], &[], &CostConfig::default());
        assert_eq!(s.cost.unwrap().source, CostSource::Estimated);
        assert_eq!(
            s.estimated_calls, 1,
            "the UI uses it to decide whether to print the ~"
        );
    }

    #[test]
    fn no_costs_at_all_means_no_cost_view() {
        let rows = vec![row(Some("m"), Purpose::Main, 100, None, None)];
        assert_eq!(summarise(&rows, &[], &CostConfig::default()).cost, None);
    }

    #[test]
    fn rankings_are_stable_and_most_expensive_first() {
        let rows = vec![
            row(Some("b:cheap"), Purpose::Main, 100, Some(0.01), None),
            row(Some("a:dear"), Purpose::Main, 100, Some(1.00), None),
            row(Some("a:same"), Purpose::Main, 100, Some(0.01), None),
        ];
        let keys: Vec<String> = summarise(&rows, &[], &CostConfig::default())
            .by_model
            .into_iter()
            .map(|g| g.key)
            .collect();
        assert_eq!(keys, ["a:dear", "a:same", "b:cheap"]);
    }

    #[test]
    fn days_are_ordered_by_date_not_by_spend() {
        let mut early = row(Some("m"), Purpose::Main, 10_000, None, None);
        early.record.created_at = "2026-07-01T00:00:00Z".parse().unwrap();
        let mut late = row(Some("m"), Purpose::Main, 1, None, None);
        late.record.created_at = "2026-07-20T00:00:00Z".parse().unwrap();

        let days: Vec<String> = summarise(&[late, early], &[], &CostConfig::default())
            .by_day
            .into_iter()
            .map(|g| g.key)
            .collect();
        assert_eq!(days, ["2026-07-01", "2026-07-20"]);
    }

    // ── by_tool ──

    #[test]
    fn tools_are_grouped_by_name_with_their_statuses_and_time() {
        let tools = vec![
            ToolObservation {
                name: "shell".into(),
                status: ToolStatus::Completed,
                duration_ms: 300,
            },
            ToolObservation {
                name: "shell".into(),
                status: ToolStatus::Error,
                duration_ms: 200,
            },
            ToolObservation {
                name: "edit".into(),
                status: ToolStatus::Completed,
                duration_ms: 10,
            },
        ];
        let groups = summarise(&[], &tools, &CostConfig::default()).by_tool;

        assert_eq!(groups[0].name, "shell");
        assert_eq!(groups[0].duration_ms, 500);
        assert_eq!(groups[0].stats.succeeded, 1);
        assert_eq!(groups[0].stats.failed, 1);
        assert_eq!(groups[1].name, "edit");
    }

    #[test]
    fn refusals_are_counted_apart_from_failures() {
        let tools = vec![
            ToolObservation {
                name: "write_file".into(),
                status: ToolStatus::Denied,
                duration_ms: 0,
            },
            ToolObservation {
                name: "write_file".into(),
                status: ToolStatus::Error,
                duration_ms: 5,
            },
            ToolObservation {
                name: "write_file".into(),
                status: ToolStatus::Cancelled,
                duration_ms: 0,
            },
        ];
        let stats = &summarise(&[], &tools, &CostConfig::default()).by_tool[0].stats;
        assert_eq!(stats.denied, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.cancelled, 1);
    }

    #[test]
    fn an_empty_range_summarises_to_zero_rather_than_failing() {
        let s = summarise(&[], &[], &CostConfig::default());
        assert_eq!(s.calls, 0);
        assert_eq!(s.tokens, TokenUsage::default());
        assert_eq!(s.cost, None);
        assert!(s.by_model.is_empty() && s.by_day.is_empty() && s.by_tool.is_empty());
    }

    fn timed_row(
        sid: SessionId,
        turn: TurnId,
        start: &str,
        first: &str,
        completed: &str,
    ) -> UsageRow {
        let mut r = row(Some("openai:gpt-5"), Purpose::Main, 100, None, None);
        r.record.session_id = sid;
        r.record.turn_id = Some(turn);
        r.record.request_started_at = Some(start.parse().unwrap());
        r.record.first_token_at = Some(first.parse().unwrap());
        r.record.completed_at = Some(completed.parse().unwrap());
        r
    }

    #[test]
    fn raw_rows_produce_timing_averages() {
        let sid = SessionId::new();
        let turn1 = TurnId::new();
        let turn2 = TurnId::new();
        let rows = vec![
            timed_row(
                sid,
                turn1,
                "2026-07-30T08:00:00Z",
                "2026-07-30T08:00:00.500Z",
                "2026-07-30T08:00:02Z",
            ),
            timed_row(
                sid,
                turn1,
                "2026-07-30T08:00:03Z",
                "2026-07-30T08:00:03.250Z",
                "2026-07-30T08:00:08Z",
            ),
            timed_row(
                sid,
                turn2,
                "2026-07-30T09:00:00Z",
                "2026-07-30T09:00:00.100Z",
                "2026-07-30T09:00:01Z",
            ),
        ];
        let s = summarise(&rows, &[], &CostConfig::default());

        assert_eq!(s.avg_first_token_ms, Some(283), "(500+250+100)/3");
        assert_eq!(s.avg_response_ms, Some((2_000 + 5_000 + 1_000) / 3));
        assert_eq!(s.avg_turn_ms, Some(4_500));
        let session_group = s
            .by_session
            .iter()
            .find(|g| g.key == sid.to_string())
            .unwrap();
        assert_eq!(session_group.avg_turn_ms, Some(4_500));
        assert_eq!(session_group.avg_first_token_ms, Some(283));
        assert_eq!(s.by_model[0].avg_turn_ms, None);
    }

    #[test]
    fn aggregate_parts_produce_timing_averages() {
        let sid = SessionId::new();
        let turn1 = TurnId::new();
        let turn2 = TurnId::new();
        let part = |key: &str, ttft: u64, ttft_n: u32, resp: u64, resp_n: u32| UsageAggregatePart {
            key: key.into(),
            label: None,
            calls: 3,
            aux_calls: 0,
            estimated_calls: 0,
            sessions: 1,
            turns: 2,
            tokens: TokenUsage {
                input: 300,
                output: 30,
                ..TokenUsage::default()
            },
            max_input_tokens: 100,
            cost: None,
            currency: "USD".into(),
            cost_source: None,
            ttft_sum_ms: ttft,
            ttft_n,
            response_sum_ms: resp,
            response_n: resp_n,
        };
        let aggregate = UsageAggregate {
            total: vec![part("", 850, 3, 8_000, 3)],
            by_model: vec![part("openai:gpt-5", 850, 3, 8_000, 3)],
            by_session: vec![part(&sid.to_string(), 850, 3, 8_000, 3)],
            turn_envelopes: vec![
                TurnEnvelope {
                    session_id: sid,
                    turn_id: turn1,
                    duration_ms: 8_000,
                },
                TurnEnvelope {
                    session_id: sid,
                    turn_id: turn2,
                    duration_ms: 1_000,
                },
            ],
            ..UsageAggregate::default()
        };
        let s = summarise_aggregate(aggregate, Vec::new(), &CostConfig::default());

        assert_eq!(s.avg_first_token_ms, Some(850 / 3));
        assert_eq!(s.avg_response_ms, Some(8_000 / 3));
        assert_eq!(
            s.avg_turn_ms,
            Some(4_500),
            "the global figure = the mean over every envelope"
        );
        let session_group = s
            .by_session
            .iter()
            .find(|g| g.key == sid.to_string())
            .unwrap();
        assert_eq!(
            session_group.avg_turn_ms,
            Some(4_500),
            "the session row = the mean over that session's envelopes"
        );
        assert_eq!(
            s.by_model[0].avg_turn_ms, None,
            "other dimensions have no conversation-duration basis"
        );
    }

    #[test]
    fn no_timing_data_means_none_not_zero() {
        let s = summarise_aggregate(
            UsageAggregate::default(),
            Vec::new(),
            &CostConfig::default(),
        );
        assert_eq!(s.avg_first_token_ms, None);
        assert_eq!(s.avg_response_ms, None);
        assert_eq!(s.avg_turn_ms, None);
    }

    #[test]
    fn rolling_quota_uses_the_exact_trailing_window() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let bounds = resolve_quota_window(&QuotaWindow::Rolling { hours: 168 }, now).unwrap();
        assert_eq!(bounds.start, now - Duration::hours(168));
        assert_eq!(bounds.end, now);
        assert_eq!(bounds.reset_at, None);
        assert_eq!(bounds.rolling_hours, Some(168));
    }

    #[test]
    fn calendar_quota_honours_iana_timezone_and_month_anchor() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let bounds = resolve_quota_window(
            &QuotaWindow::Calendar {
                period: QuotaPeriod::Month,
                timezone: "Asia/Shanghai".into(),
                week_start: QuotaWeekStart::Mon,
                anchor_day: 1,
            },
            now,
        )
        .unwrap();
        assert_eq!(
            bounds.start,
            Utc.with_ymd_and_hms(2026, 6, 30, 16, 0, 0).unwrap()
        );
        assert_eq!(
            bounds.reset_at,
            Some(Utc.with_ymd_and_hms(2026, 7, 31, 16, 0, 0).unwrap())
        );
    }

    #[test]
    fn model_quota_reduces_only_that_models_fragments() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let bounds = resolve_quota_window(&QuotaWindow::Rolling { hours: 24 }, now).unwrap();
        let aggregate = UsageAggregate {
            by_model: vec![
                quota_part("openai:gpt", 2, 600, 100),
                quota_part("openai:other", 9, 9_000, 9_000),
            ],
            ..UsageAggregate::default()
        };
        let status = evaluate_quota(
            &QuotaConfig {
                label: "Daily GPT".into(),
                metric: QuotaMetric::TotalTokens,
                currency: None,
                window: QuotaWindow::Rolling { hours: 24 },
                limit: 1_000.0,
                warn: Some(0.5),
                warn_at: None,
            },
            QuotaScope {
                provider_name: "openai".into(),
                model_name: Some("gpt".into()),
            },
            bounds,
            &aggregate,
            &CostConfig::default(),
        )
        .unwrap();
        assert_eq!(status.used, 700.0);
        assert_eq!(status.state, QuotaState::Warn);
        assert_eq!(status.pct, 0.7);
    }

    fn quota_part(key: &str, calls: u32, input: u64, output: u64) -> UsageAggregatePart {
        UsageAggregatePart {
            key: key.into(),
            label: None,
            calls,
            aux_calls: 0,
            estimated_calls: 0,
            sessions: 0,
            turns: 0,
            tokens: TokenUsage {
                input,
                output,
                ..TokenUsage::default()
            },
            max_input_tokens: input,
            cost: None,
            currency: "USD".into(),
            cost_source: None,
            ttft_sum_ms: 0,
            ttft_n: 0,
            response_sum_ms: 0,
            response_n: 0,
        }
    }
}
