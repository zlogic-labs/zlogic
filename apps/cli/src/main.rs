//! `zlogic` entry point. Everything lives in the library target (`lib.rs`), so a host that
//! links this CLI can drive it: the released binary is built by the closed-source repository,
//! which welds this CLI together with the `daemon` subcommand into a single executable.

fn main() -> std::process::ExitCode {
    zlogic_cli::run()
}
