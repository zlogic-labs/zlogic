//! ```yaml
//! policy:
//!   version: 1
//! budget:
//!   per_turn: 2.00
//!   per_task: 0.50
//!   window: { amount: 50, hours: 720 }
//!   on_exceeded: ask
//! ```

use std::path::Path;

use zlogic_protocol::settings::{BudgetAction, BudgetConfig, BudgetWindow};

#[derive(Debug, Default, serde::Deserialize)]
struct BudgetDocument {
    budget: Option<BudgetConfig>,
}

pub fn load(paths: &[&Path]) -> (BudgetConfig, Vec<String>) {
    let mut merged: Option<BudgetConfig> = None;
    let mut warnings = Vec::new();

    for path in paths {
        if !path.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                warnings.push(format!(
                    "cannot read {}: {e} (budget not applied)",
                    path.display()
                ));
                continue;
            }
        };
        let document: BudgetDocument = match serde_yaml_ng::from_str(&text) {
            Ok(document) => document,
            Err(e) => {
                warnings.push(format!(
                    "cannot read the budget section in {}: {e}",
                    path.display()
                ));
                continue;
            }
        };
        let Some(budget) = document.budget else {
            continue;
        };
        if let Err(e) = budget.validate() {
            warnings.push(format!("invalid budget in {}: {e}", path.display()));
            continue;
        }
        merged = Some(match merged {
            None => budget,
            Some(base) => tighten(base, budget),
        });
    }

    (merged.unwrap_or_default(), warnings)
}

fn tighten(base: BudgetConfig, next: BudgetConfig) -> BudgetConfig {
    BudgetConfig {
        per_turn: stricter_limit(base.per_turn, next.per_turn),
        per_session: stricter_limit(base.per_session, next.per_session),
        per_task: stricter_limit(base.per_task, next.per_task),
        window: stricter_window(base.window, next.window),
        on_exceeded: stricter_action(base.on_exceeded, next.on_exceeded),
    }
}

fn stricter_limit(base: Option<f64>, next: Option<f64>) -> Option<f64> {
    match (base, next) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

fn stricter_window(base: Option<BudgetWindow>, next: Option<BudgetWindow>) -> Option<BudgetWindow> {
    match (base, next) {
        (Some(a), Some(b)) => {
            let rate = |w: &BudgetWindow| w.amount / f64::from(w.hours.max(1));
            Some(if rate(&b) < rate(&a) { b } else { a })
        }
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

fn stricter_action(base: BudgetAction, next: BudgetAction) -> BudgetAction {
    let rank = |a: BudgetAction| match a {
        BudgetAction::Warn => 0u8,
        BudgetAction::Ask => 1,
        BudgetAction::Stop => 2,
    };
    if rank(next) > rank(base) { next } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn no_file_means_no_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (budget, warnings) = load(&[&dir.path().join("nope.yaml")]);
        assert!(!budget.is_set());
        assert!(
            warnings.is_empty(),
            "not being there is not a problem: most projects set no budget"
        );
    }

    #[test]
    fn a_policy_file_without_a_budget_section_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "policy.yaml",
            "policy:\n  version: 1\n  default: ask\n",
        );
        let (budget, warnings) = load(&[&path]);
        assert!(!budget.is_set());
        assert!(warnings.is_empty());
    }

    #[test]
    fn the_budget_section_lives_alongside_the_access_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "policy.yaml",
            r#"
policy:
  version: 1
  default: ask

budget:
  per_turn: 2.5
  on_exceeded: stop
"#,
        );
        let (budget, warnings) = load(&[&path]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(budget.per_turn, Some(2.5));
        assert_eq!(budget.on_exceeded, BudgetAction::Stop);

        let policy = zlogic_policy::Policy::from_yaml_file(&path).unwrap();
        assert_eq!(policy.default, zlogic_policy::Effect::Ask);
    }

    #[test]
    fn a_project_file_can_only_tighten_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(dir.path(), "global.yaml", "budget:\n  per_turn: 2.0\n");
        let lower = write(dir.path(), "lower.yaml", "budget:\n  per_turn: 0.5\n");
        let higher = write(dir.path(), "higher.yaml", "budget:\n  per_turn: 100.0\n");

        assert_eq!(
            load(&[&global, &lower]).0.per_turn,
            Some(0.5),
            "lowering it takes effect"
        );
        assert_eq!(
            load(&[&global, &higher]).0.per_turn,
            Some(2.0),
            "raising it is ignored — a project you just cloned must not raise your limit"
        );
    }

    #[test]
    fn a_project_limit_applies_when_the_global_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(dir.path(), "global.yaml", "budget:\n  per_session: 9.0\n");
        let project = write(dir.path(), "project.yaml", "budget:\n  per_turn: 1.0\n");
        let (budget, _) = load(&[&global, &project]);
        assert_eq!(budget.per_turn, Some(1.0));
        assert_eq!(budget.per_session, Some(9.0));
    }

    #[test]
    fn the_action_can_only_be_escalated() {
        let dir = tempfile::tempdir().unwrap();
        let ask = write(dir.path(), "ask.yaml", "budget:\n  on_exceeded: ask\n");
        let stop = write(dir.path(), "stop.yaml", "budget:\n  on_exceeded: stop\n");
        let warn = write(dir.path(), "warn.yaml", "budget:\n  on_exceeded: warn\n");

        assert_eq!(load(&[&ask, &stop]).0.on_exceeded, BudgetAction::Stop);
        assert_eq!(
            load(&[&ask, &warn]).0.on_exceeded,
            BudgetAction::Ask,
            "the project's copy must not downgrade `ask` into `warn`"
        );
    }

    #[test]
    fn windows_are_compared_whole_not_field_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let monthly = write(
            dir.path(),
            "monthly.yaml",
            "budget:\n  window: { amount: 50, hours: 720 }\n",
        );
        let daily = write(
            dir.path(),
            "daily.yaml",
            "budget:\n  window: { amount: 10, hours: 24 }\n",
        );

        let window = load(&[&daily, &monthly]).0.window.unwrap();
        assert_eq!((window.amount, window.hours), (50.0, 720));

        let tight = write(
            dir.path(),
            "tight.yaml",
            "budget:\n  window: { amount: 1, hours: 24 }\n",
        );
        let window = load(&[&monthly, &tight]).0.window.unwrap();
        assert_eq!((window.amount, window.hours), (1.0, 24));
    }

    #[test]
    fn a_broken_budget_section_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "policy.yaml",
            "budget:\n  per_turn: [not a number\n",
        );
        let (budget, warnings) = load(&[&path]);
        assert!(!budget.is_set());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("budget"), "{}", warnings[0]);
    }

    #[test]
    fn an_invalid_amount_is_reported_and_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "policy.yaml", "budget:\n  per_turn: 0\n");
        let (budget, warnings) = load(&[&path]);
        assert_eq!(
            budget.per_turn, None,
            "0 is not unlimited, and must not be treated as a limit"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn one_broken_file_does_not_discard_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), "good.yaml", "budget:\n  per_turn: 3.0\n");
        let bad = write(dir.path(), "bad.yaml", "budget:\n  per_turn: -1\n");
        let (budget, warnings) = load(&[&good, &bad]);
        assert_eq!(budget.per_turn, Some(3.0));
        assert_eq!(warnings.len(), 1);
    }
}
