use std::sync::Mutex;

use zlogic_protocol::settings::{BudgetAction, BudgetConfig, CostConfig};
use zlogic_protocol::usage::{CostTally, CostTotal, CostView};

pub struct TurnBudget {
    config: BudgetConfig,
    cost: CostConfig,
    session_spent: CostTally,
    window_spent: CostTally,
    unattended: bool,
    tally: Mutex<CostTally>,
    waived: Mutex<Vec<&'static str>>,
    noticed: Mutex<Vec<String>>,
}

impl std::fmt::Debug for TurnBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnBudget")
            .field("limits_set", &self.config.is_set())
            .finish_non_exhaustive()
    }
}

impl TurnBudget {
    pub fn new(config: BudgetConfig, cost: CostConfig) -> Self {
        Self {
            config,
            cost,
            session_spent: CostTally::default(),
            window_spent: CostTally::default(),
            unattended: false,
            tally: Mutex::default(),
            waived: Mutex::default(),
            noticed: Mutex::default(),
        }
    }

    pub fn with_session_spent(mut self, spent: CostTally) -> Self {
        self.session_spent = spent;
        self
    }

    pub fn with_window_spent(mut self, spent: CostTally) -> Self {
        self.window_spent = spent;
        self
    }

    pub fn unattended(mut self, on: bool) -> Self {
        self.unattended = on;
        self
    }

    pub fn for_task_run(&self) -> TurnBudget {
        let mut config = self.config.clone();
        config.per_task = config.task_limit();
        config.per_turn = None;
        TurnBudget {
            config,
            cost: self.cost.clone(),
            session_spent: self.session_spent.clone(),
            window_spent: self.window_spent.clone(),
            unattended: true,
            tally: Mutex::default(),
            waived: Mutex::default(),
            noticed: Mutex::default(),
        }
    }

    pub fn add_round(&self, cost: &CostView) {
        self.add_cost(cost);
    }

    pub fn add_aux(&self, cost: &CostView) {
        self.add_cost(cost);
    }

    fn add_cost(&self, cost: &CostView) {
        self.tally
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add_view(cost);
    }

    pub fn turn_total(&self) -> Option<CostTotal> {
        self.tally
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .total(&self.cost)
    }

    pub fn checkpoint(&self) -> Enforcement {
        if !self.config.is_set() {
            return Enforcement::Continue;
        }

        let turn = self.tally.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut session = self.session_spent.clone();
        session.merge(&turn);
        let mut window = self.window_spent.clone();
        window.merge(&turn);

        let turn_total = turn.total(&self.cost);
        let session_total = session.total(&self.cost);
        let window_total = window.total(&self.cost);

        let mut scopes: Vec<(Scope, Option<&CostTotal>)> = Vec::new();
        let waived = self
            .waived
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let live = |scope: Scope| !waived.contains(&scope.as_str());
        if live(Scope::Turn) {
            scopes.push((Scope::Turn, turn_total.as_ref()));
        }
        if self.unattended && live(Scope::Task) {
            scopes.push((Scope::Task, turn_total.as_ref()));
        }
        if live(Scope::Session) {
            scopes.push((Scope::Session, session_total.as_ref()));
        }
        if live(Scope::Window) {
            scopes.push((Scope::Window, window_total.as_ref()));
        }

        match first_actionable(&self.config, &scopes) {
            Verdict::Within => Enforcement::Continue,
            Verdict::Exceeded(hit) => {
                let message = hit.message();
                match hit.action {
                    BudgetAction::Stop => Enforcement::Stop { message },
                    BudgetAction::Ask => Enforcement::Ask {
                        scope: hit.scope,
                        message,
                    },
                    BudgetAction::Warn => {
                        if self.once(format!("warn:{}", hit.scope.as_str())) {
                            Enforcement::Warn { message }
                        } else {
                            Enforcement::Continue
                        }
                    }
                }
            }
            Verdict::Unmeasured(unmeasured) => {
                if self.once(format!("unmeasured:{}", unmeasured.scope.as_str())) {
                    Enforcement::Warn {
                        message: unmeasured.message(),
                    }
                } else {
                    Enforcement::Continue
                }
            }
        }
    }

    pub fn waive(&self, scope: Scope) {
        let mut waived = self.waived.lock().unwrap_or_else(|e| e.into_inner());
        if !waived.contains(&scope.as_str()) {
            waived.push(scope.as_str());
        }
    }

    fn once(&self, key: String) -> bool {
        let mut noticed = self.noticed.lock().unwrap_or_else(|e| e.into_inner());
        if noticed.contains(&key) {
            return false;
        }
        noticed.push(key);
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Enforcement {
    Continue,
    Stop { message: String },
    Ask { scope: Scope, message: String },
    Warn { message: String },
}

impl Exceeded {
    /// The human-readable line for hitting the limit. Shared by the notice, `TurnEnd.reason`
    /// and the ask form — the three places must not tell three different stories.
    /// Engine-side text is deliberately neutral English (the engine does not hold a host
    /// locale); hosts that render this through a catalogue key localize it.
    pub fn message(&self) -> String {
        let scope = scope_noun(self.scope);
        format!(
            "{scope} has spent {spent:.4} {currency}, over the budget limit of {limit:.2} (the budget section of policy.yaml)",
            spent = self.spent,
            currency = self.currency,
            limit = self.limit,
        )
    }
}

impl Unmeasured {
    pub fn message(&self) -> String {
        match &self.reason {
            Unmeasurable::NoPricing => format!(
                "a budget of {:.2} is set ({scope}), but the current model has no pricing configured — the spend cannot be measured, so this budget cannot be enforced",
                self.limit,
                scope = scope_noun(self.scope)
            ),
            Unmeasurable::MissingRate { currencies } => format!(
                "no exchange rate is configured from {} to {} — that spend cannot be counted against the budget ({scope}). Add a rate in the exchange-rate table in settings",
                currencies.join(", "),
                self.currency,
                scope = scope_noun(self.scope)
            ),
        }
    }
}

fn scope_noun(scope: Scope) -> &'static str {
    match scope {
        Scope::Turn => "This submission",
        Scope::Session => "This conversation",
        Scope::Task => "This background task",
        Scope::Window => "This rolling window",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Turn,
    Session,
    Task,
    Window,
}

impl Scope {
    fn can_ask(self) -> bool {
        !matches!(self, Scope::Task)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Turn => "turn",
            Scope::Session => "session",
            Scope::Task => "task",
            Scope::Window => "window",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Within,
    Exceeded(Exceeded),
    Unmeasured(Unmeasured),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exceeded {
    pub scope: Scope,
    pub spent: f64,
    pub limit: f64,
    pub currency: String,
    pub action: BudgetAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unmeasured {
    pub scope: Scope,
    pub limit: f64,
    pub currency: String,
    pub reason: Unmeasurable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Unmeasurable {
    NoPricing,
    MissingRate { currencies: Vec<String> },
}

pub fn check(config: &BudgetConfig, scope: Scope, spent: Option<&CostTotal>) -> Verdict {
    let Some(limit) = limit_for(config, scope) else {
        return Verdict::Within;
    };

    let Some(spent) = spent else {
        return Verdict::Unmeasured(Unmeasured {
            scope,
            limit,
            currency: String::new(),
            reason: Unmeasurable::NoPricing,
        });
    };

    if !spent.unconverted.is_empty() {
        return Verdict::Unmeasured(Unmeasured {
            scope,
            limit,
            currency: spent.currency.clone(),
            reason: Unmeasurable::MissingRate {
                currencies: spent
                    .unconverted
                    .iter()
                    .map(|item| item.currency.clone())
                    .collect(),
            },
        });
    }

    if spent.amount < limit {
        return Verdict::Within;
    }

    Verdict::Exceeded(Exceeded {
        scope,
        spent: spent.amount,
        limit,
        currency: spent.currency.clone(),
        action: if config.on_exceeded == BudgetAction::Ask && !scope.can_ask() {
            BudgetAction::Stop
        } else {
            config.on_exceeded
        },
    })
}

fn limit_for(config: &BudgetConfig, scope: Scope) -> Option<f64> {
    match scope {
        Scope::Turn => config.per_turn,
        Scope::Session => config.per_session,
        Scope::Task => config.task_limit(),
        Scope::Window => config.window.as_ref().map(|w| w.amount),
    }
}

pub fn first_actionable(config: &BudgetConfig, scopes: &[(Scope, Option<&CostTotal>)]) -> Verdict {
    let mut unmeasured = None;
    for (scope, spent) in scopes {
        match check(config, *scope, *spent) {
            Verdict::Exceeded(hit) => return Verdict::Exceeded(hit),
            Verdict::Unmeasured(u) if unmeasured.is_none() => unmeasured = Some(u),
            _ => {}
        }
    }
    match unmeasured {
        Some(u) => Verdict::Unmeasured(u),
        None => Verdict::Within,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::settings::BudgetWindow;
    use zlogic_protocol::usage::{CostSource, CurrencyAmount};

    fn spent(amount: f64) -> CostTotal {
        CostTotal {
            amount,
            currency: "USD".into(),
            source: CostSource::LocalPricing,
            unconverted: Vec::new(),
        }
    }

    fn per_turn(limit: f64) -> BudgetConfig {
        BudgetConfig {
            per_turn: Some(limit),
            ..Default::default()
        }
    }

    #[test]
    fn no_budget_means_no_verdict_to_make() {
        assert_eq!(
            check(&BudgetConfig::default(), Scope::Turn, Some(&spent(999.0))),
            Verdict::Within
        );
        assert!(!BudgetConfig::default().is_set());
    }

    #[test]
    fn under_the_limit_passes_and_at_the_limit_does_not() {
        let config = per_turn(2.0);
        assert_eq!(
            check(&config, Scope::Turn, Some(&spent(1.99))),
            Verdict::Within
        );
        assert!(matches!(
            check(&config, Scope::Turn, Some(&spent(2.0))),
            Verdict::Exceeded(_)
        ));
    }

    #[test]
    fn the_verdict_carries_what_the_message_needs() {
        let hit = match check(&per_turn(2.0), Scope::Turn, Some(&spent(2.5))) {
            Verdict::Exceeded(hit) => hit,
            other => panic!("expected Exceeded, got {other:?}"),
        };
        assert_eq!(hit.spent, 2.5);
        assert_eq!(hit.limit, 2.0);
        assert_eq!(hit.currency, "USD");
        assert_eq!(hit.scope, Scope::Turn);
    }

    #[test]
    fn each_scope_reads_its_own_limit() {
        let config = BudgetConfig {
            per_turn: Some(1.0),
            per_session: Some(10.0),
            ..Default::default()
        };
        assert!(matches!(
            check(&config, Scope::Turn, Some(&spent(5.0))),
            Verdict::Exceeded(_)
        ));
        assert_eq!(
            check(&config, Scope::Session, Some(&spent(5.0))),
            Verdict::Within
        );
        assert_eq!(
            check(&config, Scope::Window, Some(&spent(1e9))),
            Verdict::Within
        );
    }

    #[test]
    fn a_task_falls_back_to_the_turn_limit() {
        let config = per_turn(1.0);
        assert!(matches!(
            check(&config, Scope::Task, Some(&spent(1.5))),
            Verdict::Exceeded(_)
        ));

        let config = BudgetConfig {
            per_turn: Some(1.0),
            per_task: Some(20.0),
            ..Default::default()
        };
        assert_eq!(
            check(&config, Scope::Task, Some(&spent(1.5))),
            Verdict::Within
        );
    }

    #[test]
    fn an_unattended_scope_stops_instead_of_asking() {
        let config = BudgetConfig {
            per_task: Some(1.0),
            on_exceeded: BudgetAction::Ask,
            ..Default::default()
        };
        let hit = match check(&config, Scope::Task, Some(&spent(2.0))) {
            Verdict::Exceeded(hit) => hit,
            other => panic!("{other:?}"),
        };
        assert_eq!(hit.action, BudgetAction::Stop, "nobody to ask");

        let config = BudgetConfig {
            per_turn: Some(1.0),
            on_exceeded: BudgetAction::Ask,
            ..Default::default()
        };
        assert!(matches!(
            check(&config, Scope::Turn, Some(&spent(2.0))),
            Verdict::Exceeded(Exceeded {
                action: BudgetAction::Ask,
                ..
            })
        ));
    }

    #[test]
    fn warn_is_not_escalated_for_an_unattended_scope() {
        let config = BudgetConfig {
            per_task: Some(1.0),
            on_exceeded: BudgetAction::Warn,
            ..Default::default()
        };
        assert!(matches!(
            check(&config, Scope::Task, Some(&spent(2.0))),
            Verdict::Exceeded(Exceeded {
                action: BudgetAction::Warn,
                ..
            })
        ));
    }

    #[test]
    fn a_model_without_pricing_makes_the_budget_unmeasurable_not_satisfied() {
        assert_eq!(
            check(&per_turn(2.0), Scope::Turn, None),
            Verdict::Unmeasured(Unmeasured {
                scope: Scope::Turn,
                limit: 2.0,
                currency: String::new(),
                reason: Unmeasurable::NoPricing,
            })
        );
        assert_eq!(
            check(&BudgetConfig::default(), Scope::Turn, None),
            Verdict::Within
        );
    }

    #[test]
    fn a_missing_rate_is_unmeasurable_even_when_the_converted_part_is_under() {
        let partial = CostTotal {
            amount: 0.5,
            currency: "USD".into(),
            source: CostSource::LocalPricing,
            unconverted: vec![CurrencyAmount {
                amount: 700.0,
                currency: "CNY".into(),
            }],
        };
        let verdict = check(&per_turn(2.0), Scope::Turn, Some(&partial));
        assert_eq!(
            verdict,
            Verdict::Unmeasured(Unmeasured {
                scope: Scope::Turn,
                limit: 2.0,
                currency: "USD".into(),
                reason: Unmeasurable::MissingRate {
                    currencies: vec!["CNY".into()],
                },
            })
        );
    }

    #[test]
    fn unmeasurable_does_not_block() {
        let verdict = check(&per_turn(2.0), Scope::Turn, None);
        assert!(
            !matches!(verdict, Verdict::Exceeded(_)),
            "an unmeasurable amount must not read as over budget"
        );
    }

    #[test]
    fn the_tightest_breached_scope_wins() {
        let config = BudgetConfig {
            per_turn: Some(1.0),
            per_session: Some(2.0),
            ..Default::default()
        };
        let session = spent(5.0);
        let turn = spent(5.0);
        match first_actionable(
            &config,
            &[(Scope::Turn, Some(&turn)), (Scope::Session, Some(&session))],
        ) {
            Verdict::Exceeded(hit) => assert_eq!(hit.scope, Scope::Turn),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_confirmed_breach_outranks_an_unmeasurable_scope() {
        let config = BudgetConfig {
            per_turn: Some(100.0),
            per_session: Some(1.0),
            ..Default::default()
        };
        let session = spent(5.0);
        match first_actionable(
            &config,
            &[(Scope::Turn, None), (Scope::Session, Some(&session))],
        ) {
            Verdict::Exceeded(hit) => assert_eq!(hit.scope, Scope::Session),
            other => panic!("expected Exceeded to outrank Unmeasured, got {other:?}"),
        }
    }

    #[test]
    fn nothing_breached_is_within() {
        let config = BudgetConfig {
            per_turn: Some(10.0),
            per_session: Some(100.0),
            ..Default::default()
        };
        let cost = spent(1.0);
        assert_eq!(
            first_actionable(
                &config,
                &[(Scope::Turn, Some(&cost)), (Scope::Session, Some(&cost))]
            ),
            Verdict::Within
        );
    }

    #[test]
    fn a_zero_or_negative_limit_is_rejected() {
        for bad in [0.0, -1.0, f64::NAN] {
            assert!(
                BudgetConfig {
                    per_turn: Some(bad),
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "{bad} must be rejected"
            );
        }
        assert!(per_turn(0.5).validate().is_ok());
        assert!(BudgetConfig::default().validate().is_ok());
    }

    #[test]
    fn a_window_needs_a_positive_amount_and_at_least_an_hour() {
        assert!(
            BudgetConfig {
                window: Some(BudgetWindow {
                    amount: 10.0,
                    hours: 0
                }),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BudgetConfig {
                window: Some(BudgetWindow {
                    amount: 0.0,
                    hours: 24
                }),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BudgetConfig {
                window: Some(BudgetWindow {
                    amount: 10.0,
                    hours: 720
                }),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn the_default_action_asks() {
        assert_eq!(BudgetConfig::default().on_exceeded, BudgetAction::Ask);
    }

    #[test]
    fn a_task_run_budget_stops_instead_of_asking() {
        let parent = TurnBudget::new(
            BudgetConfig {
                per_turn: Some(1.0),
                on_exceeded: BudgetAction::Ask,
                ..Default::default()
            },
            CostConfig::default(),
        );
        let task = parent.for_task_run();

        task.add_round(&CostView {
            amount: 2.0,
            currency: "USD".into(),
            source: zlogic_protocol::usage::CostSource::LocalPricing,
        });
        match task.checkpoint() {
            Enforcement::Stop { message } => {
                assert!(message.contains("background task"), "{message}");
            }
            other => panic!("expected Stop (nobody to ask), got {other:?}"),
        }
    }

    #[test]
    fn a_task_run_starts_with_a_fresh_tally() {
        let parent = TurnBudget::new(
            BudgetConfig {
                per_task: Some(1.0),
                ..Default::default()
            },
            CostConfig::default(),
        );
        parent.add_round(&CostView {
            amount: 100.0,
            currency: "USD".into(),
            source: zlogic_protocol::usage::CostSource::LocalPricing,
        });

        let task = parent.for_task_run();
        assert_eq!(
            task.turn_total(),
            None,
            "the parent tally does not carry over"
        );

        task.add_round(&CostView {
            amount: 0.1,
            currency: "USD".into(),
            source: zlogic_protocol::usage::CostSource::LocalPricing,
        });
        assert_eq!(task.checkpoint(), Enforcement::Continue);
    }
}
