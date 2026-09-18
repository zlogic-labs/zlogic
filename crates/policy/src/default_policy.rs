//! Generate a safe-by-default workspace policy as YAML.

use std::env;
use std::path::PathBuf;
use std::process::exit;

use zlogic_policy::default_workspace_policy;

const USAGE: &str = "usage: policy-default [--workspace DIR] [--home DIR] [--app-name NAME] [--output FILE]\n\nDefaults: workspace=current directory, home=$HOME, app-name=cli, output=stdout.";

fn main() {
    let mut workspace = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let mut app_name = String::from("cli");
    let mut output: Option<PathBuf> = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = required_value(&mut args, "--workspace").into(),
            "--home" => home = required_value(&mut args, "--home").into(),
            "--app-name" => app_name = required_value(&mut args, "--app-name"),
            "--output" => output = Some(required_value(&mut args, "--output").into()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}\n{USAGE}");
                exit(2);
            }
        }
    }
    if app_name.is_empty() || app_name.contains(['/', '\\']) {
        eprintln!("--app-name must be a non-empty path component");
        exit(2);
    }

    let yaml = default_workspace_policy(&workspace, &home, &app_name).to_yaml();
    if let Some(path) = output {
        if let Err(error) = std::fs::write(&path, yaml) {
            eprintln!("failed to write {}: {error}", path.display());
            exit(1);
        }
        println!("wrote {}", path.display());
    } else {
        print!("{yaml}");
    }
}

fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next().unwrap_or_else(|| {
        eprintln!("{flag} requires a value\n{USAGE}");
        exit(2);
    })
}
