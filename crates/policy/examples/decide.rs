use std::path::Path;

use zlogic_policy::{Dialect, Effect, Policy, evaluate_command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let policy_path = args.next().unwrap_or_else(|| "examples/policy.yaml".into());
    let command = {
        let rest = args.collect::<Vec<_>>().join(" ");
        if rest.is_empty() {
            "git status --short".into()
        } else {
            rest
        }
    };

    let policy = Policy::from_yaml_file(Path::new(&policy_path))?;
    let decision = evaluate_command(
        &policy,
        Dialect::Posix,
        &command,
        "/home/user/project", // cwd
        "/home/user/project", // workspace
        "/home/user",         // home
    )?;

    println!("policy:   {policy_path}");
    println!("command:  {command}");
    println!("decision: {:?}\n", decision.effect);
    for (op, item) in decision.ops.iter().zip(&decision.op_decisions) {
        println!("op:      {op}");
        println!("effect:  {:?}", item.effect);
        println!("rules:   {:?}", item.matched_rule_ids);
        println!("reason:  {}\n", item.reason);
    }

    match decision.effect {
        Effect::Allow => println!("result: execute command"),
        Effect::Ask => println!("result: prompt the user"),
        Effect::Deny => println!("result: block command"),
    }
    Ok(())
}
