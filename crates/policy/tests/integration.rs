//! End-to-end: command string → ops, against a real temp filesystem
//! (so symlink resolution and macOS /var→/private/var are exercised).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use zlogic_policy::path::physical_resolve;
use zlogic_policy::{Access, Analyzer, Op, Zone};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Env {
    ws: PathBuf,
    home: PathBuf,
    outside: PathBuf,
    analyzer: Analyzer,
}

fn setup() -> Env {
    let base = std::env::temp_dir().join(format!(
        "zlogic-policy-it-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let ws = base.join("ws");
    let home = base.join("home");
    let outside = base.join("outside");
    fs::create_dir_all(ws.join("sub")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/id_rsa"), "key").unwrap();
    fs::write(outside.join("notes.txt"), "n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();

    let analyzer = Analyzer::new(ws.clone(), home.clone());
    Env {
        ws: physical_resolve(&ws),
        home: physical_resolve(&home),
        outside: physical_resolve(&outside),
        analyzer,
    }
}

fn path_ops(ops: &[Op], access: Access) -> Vec<&Op> {
    ops.iter()
        .filter(|o| matches!(o, Op::Path { access: a, .. } if *a == access))
        .collect()
}

fn resolved_of(op: &Op) -> Option<&Path> {
    match op {
        Op::Path { path, .. } | Op::Script { path, .. } | Op::CwdChange { path } => {
            path.resolved.as_deref()
        }
        _ => None,
    }
}

fn zone_of(op: &Op) -> Option<Zone> {
    match op {
        Op::Path { path, .. } | Op::Script { path, .. } | Op::CwdChange { path } => Some(path.zone),
        _ => None,
    }
}

fn has_exec(ops: &[Op], head: &str) -> bool {
    ops.iter()
        .any(|o| matches!(o, Op::Exec { head: h, .. } if h == head))
}

fn has_unknown(ops: &[Op]) -> bool {
    ops.iter().any(|o| matches!(o, Op::Unknown { .. }))
}

impl Env {
    fn abs_of(&self, posix: &str) -> PathBuf {
        physical_resolve(&self.ws.join(posix))
    }

    fn cmd_display(&self, path: &Path) -> String {
        let mut s = path.display().to_string();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            s = rest.to_owned();
        }
        s.replace('\\', "/")
    }
}

#[test]
fn cd_tracks_relative_paths() {
    let e = setup();
    let ops = e.analyzer.analyze("cd sub && rm -f x.txt", &e.ws);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 1);
    assert_eq!(resolved_of(dels[0]), Some(e.ws.join("sub/x.txt").as_path()));
    assert_eq!(zone_of(dels[0]), Some(Zone::Workspace));
}

#[test]
fn subshell_cwd_is_scoped() {
    let e = setup();
    let ops = e.analyzer.analyze("(cd / && ls); touch new.txt", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    // the touch resolves against the ORIGINAL cwd, not the subshell's /
    assert_eq!(resolved_of(writes[0]), Some(e.ws.join("new.txt").as_path()));
    assert_eq!(zone_of(writes[0]), Some(Zone::Workspace));
}

#[test]
fn dynamic_cd_poisons_relative_paths() {
    let e = setup();
    let ops = e.analyzer.analyze("cd $DIR && cat data.txt", &e.ws);
    assert!(has_unknown(&ops));
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(reads.len(), 1);
    assert!(resolved_of(reads[0]).is_none());
    assert_eq!(zone_of(reads[0]), Some(Zone::Unresolved));
    // …but absolute paths after a poisoned cd still resolve
    let ops = e.analyzer.analyze(
        &format!("cd $DIR && cat {}/notes.txt", e.cmd_display(&e.outside)),
        &e.ws,
    );
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(
        resolved_of(reads[0]),
        Some(e.outside.join("notes.txt").as_path())
    );
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_judged_outside() {
    let e = setup();
    let ops = e.analyzer.analyze("cat link/notes.txt", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(reads.len(), 1);
    // ws/link -> outside: zone must NOT be Workspace
    assert_eq!(
        resolved_of(reads[0]),
        Some(e.outside.join("notes.txt").as_path())
    );
    assert_eq!(zone_of(reads[0]), Some(Zone::Other));
}

#[test]
fn sensitive_floor_via_tilde() {
    let e = setup();
    let ops = e.analyzer.analyze("cat ~/.ssh/id_rsa", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(zone_of(reads[0]), Some(Zone::Sensitive));
}

#[test]
fn command_substitution_surfaces_inner_ops() {
    let e = setup();
    let ops = e.analyzer.analyze("zlogic $(cat ~/.ssh/id_rsa)", &e.ws);
    assert!(has_exec(&ops, "cat"));
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(zone_of(reads[0]), Some(Zone::Sensitive));
    // the zlogic itself carries dynamic args
    assert!(ops.iter().any(
        |o| matches!(o, Op::Exec { head, dynamic_args, .. } if head == "zlogic" && *dynamic_args)
    ));
}

#[test]
fn bash_dash_c_recurses_with_inherited_cwd() {
    let e = setup();
    let ops = e.analyzer.analyze("bash -c 'rm -rf sub'", &e.ws);
    assert!(has_exec(&ops, "bash"));
    assert!(has_exec(&ops, "rm"));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(resolved_of(dels[0]), Some(e.ws.join("sub").as_path()));
    // dynamic payload fails closed
    let ops = e.analyzer.analyze("bash -c \"$PAYLOAD\"", &e.ws);
    assert!(has_unknown(&ops));
}

#[test]
fn bash_long_option_before_dash_c_still_surfaces_payload() {
    let e = setup();
    // long options that happen to contain the letter 'c' (`--norc`,
    // `--restricted`) must NOT be mistaken for `-c`; the real payload has to
    // be decomposed, else a `rm -rf` slips through as a bare `exec bash`.
    for cmd in [
        "bash --norc -c 'rm -rf sub'",
        "bash --restricted -c 'rm -rf sub'",
        // value-taking long option: its value must not be swallowed as payload
        "bash --rcfile /tmp/x -c 'rm -rf sub'",
    ] {
        let ops = e.analyzer.analyze(cmd, &e.ws);
        assert!(has_exec(&ops, "rm"), "no rm surfaced for `{cmd}`: {ops:?}");
        let dels = path_ops(&ops, Access::Delete);
        assert_eq!(
            dels.first().and_then(|o| resolved_of(o)),
            Some(e.ws.join("sub").as_path()),
            "delete of ws/sub not surfaced for `{cmd}`: {ops:?}"
        );
    }
}

#[test]
fn script_files_become_script_ops() {
    let e = setup();
    let ops = e
        .analyzer
        .analyze("python3 scripts/run.py --verbose", &e.ws);
    let script = ops.iter().find(|o| matches!(o, Op::Script { .. })).unwrap();
    assert_eq!(
        resolved_of(script),
        Some(e.ws.join("scripts/run.py").as_path())
    );
    // executing a file by path is also a Script op
    let ops = e.analyzer.analyze("./build.sh release", &e.ws);
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::Script { interpreter, .. } if interpreter == "direct"))
    );
}

#[test]
fn sudo_is_surfaced_and_inner_command_still_analyzed() {
    let e = setup();
    let ops = e.analyzer.analyze("sudo rm -rf /etc/hosts", &e.ws);
    assert!(has_exec(&ops, "sudo"));
    assert!(has_exec(&ops, "rm"));
    let dels = path_ops(&ops, Access::Delete);
    #[cfg(unix)]
    assert_eq!(zone_of(dels[0]), Some(Zone::System));
    #[cfg(windows)]
    let _ = dels;
}

#[test]
fn redirect_targets_are_write_ops() {
    let e = setup();
    let ops = e
        .analyzer
        .analyze("zlogic secret > ~/.ssh/authorized_keys", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(zone_of(writes[0]), Some(Zone::Sensitive));
    // 2>&1 dup carries no path op
    let ops = e.analyzer.analyze("make 2>&1", &e.ws);
    assert!(path_ops(&ops, Access::Write).is_empty());
}

#[test]
fn glob_delete_is_unresolved() {
    let e = setup();
    let ops = e.analyzer.analyze("rm -rf *.log", &e.ws);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 1);
    assert_eq!(zone_of(dels[0]), Some(Zone::Unresolved));
}

#[test]
fn dangerous_env_prefix_flags_ask() {
    let e = setup();
    // PATH= prefix can hijack which `git` runs — must not silently allow
    let ops = e.analyzer.analyze("PATH=/evil:$PATH git status", &e.ws);
    assert!(has_unknown(&ops));
    assert!(has_exec(&ops, "git"));
    // LD_PRELOAD via env(1) too
    let ops = e.analyzer.analyze("env LD_PRELOAD=/tmp/x.so ls", &e.ws);
    assert!(has_unknown(&ops));
    // a benign assignment does not
    let ops = e.analyzer.analyze("FOO=1 make", &e.ws);
    assert!(!has_unknown(&ops));
}

#[test]
fn brace_expansion_is_unresolved() {
    let e = setup();
    let ops = e.analyzer.analyze("rm /{etc,usr}/x", &e.ws);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 1);
    // must NOT be judged as the literal path "/{etc,usr}/x" (zone Other)
    assert_eq!(zone_of(dels[0]), Some(Zone::Unresolved));
}

#[test]
fn dynamic_head_fails_closed() {
    let e = setup();
    let ops = e.analyzer.analyze("$CMD --version", &e.ws);
    assert!(has_unknown(&ops));
    assert!(!ops.iter().any(|o| matches!(o, Op::Exec { .. })));
}

#[test]
fn for_loop_body_surfaces_with_dynamic_target() {
    let e = setup();
    let ops = e
        .analyzer
        .analyze("for f in a.log b.log; do rm $f; done", &e.ws);
    assert!(has_exec(&ops, "rm"));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(zone_of(dels[0]), Some(Zone::Unresolved));
    // list words never become commands
    assert!(!has_exec(&ops, "a.log"));
}

#[test]
fn heredoc_body_is_data_and_piped_sh_is_opaque() {
    let e = setup();
    let ops = e
        .analyzer
        .analyze("cat <<EOF > gen.txt\nrm -rf /\nEOF", &e.ws);
    // the rm inside the heredoc is data, not a command
    assert!(!has_exec(&ops, "rm"));
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(resolved_of(writes[0]), Some(e.ws.join("gen.txt").as_path()));
    // …but piping anything into a bare shell is opaque
    let ops = e.analyzer.analyze("cat gen.txt | sh", &e.ws);
    assert!(has_unknown(&ops));
}

#[test]
fn sed_inplace_is_write() {
    let e = setup();
    let ops = e.analyzer.analyze("sed -i 's/a/b/' notes.md", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(
        resolved_of(writes[0]),
        Some(e.ws.join("notes.md").as_path())
    );
    let ops = e.analyzer.analyze("sed 's/a/b/' notes.md", &e.ws);
    assert!(!path_ops(&ops, Access::Read).is_empty());
    assert!(path_ops(&ops, Access::Write).is_empty());
}

#[test]
fn sed_glued_expression_keeps_first_file_operand() {
    let e = setup();
    let ops = e.analyzer.analyze("sed -i -es/x/y/ /etc/passwd", &e.ws);
    assert!(ops.iter().any(|op| matches!(
        op,
        Op::Path { access: Access::Write, path } if path.requested == "/etc/passwd"
    )));
}

#[test]
fn dd_dynamic_output_is_an_unresolved_write() {
    let e = setup();
    let ops = e.analyzer.analyze("dd if=/dev/zero of=$TARGET", &e.ws);
    assert!(ops.iter().any(|op| matches!(
        op,
        Op::Path { access: Access::Write, path }
            if path.requested == "$TARGET" && path.zone == Zone::Unresolved
    )));
}

#[test]
fn source_poisons_cwd() {
    let e = setup();
    let ops = e.analyzer.analyze("source env.sh && touch marker", &e.ws);
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::Script { interpreter, .. } if interpreter == "source"))
    );
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(zone_of(writes[0]), Some(Zone::Unresolved));
}

#[test]
fn mv_emits_read_delete_write() {
    let e = setup();
    let ops = e.analyzer.analyze("mv sub/a.txt sub/b.txt", &e.ws);
    assert_eq!(path_ops(&ops, Access::Read).len(), 1);
    assert_eq!(path_ops(&ops, Access::Delete).len(), 1);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(
        resolved_of(writes[0]),
        Some(e.ws.join("sub/b.txt").as_path())
    );
}

#[test]
fn unknown_command_sensitive_arg_still_surfaces() {
    let e = setup();
    let ops = e.analyzer.analyze(
        &format!("my-custom-tool {}/.ssh/id_rsa", e.home.display()),
        &e.ws,
    );
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(reads.len(), 1);
    assert_eq!(zone_of(reads[0]), Some(Zone::Sensitive));
}

#[test]
fn wrapper_peeling() {
    let e = setup();
    let ops = e.analyzer.analyze("env FOO=1 timeout 30 rm -rf sub", &e.ws);
    assert!(has_exec(&ops, "rm"));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(resolved_of(dels[0]), Some(e.ws.join("sub").as_path()));
    // xargs marks the inner exec as stdin-driven (dynamic)
    let ops = e.analyzer.analyze("find . -name '*.o' | xargs rm", &e.ws);
    assert!(ops.iter().any(
        |o| matches!(o, Op::Exec { head, dynamic_args, .. } if head == "rm" && *dynamic_args)
    ));
}

#[test]
fn env_chdir_scopes_wrapped_command() {
    let e = setup();
    // env -C DIR: the wrapped rm resolves its target against DIR
    let ops = e.analyzer.analyze("env -C /tmp rm -rf x", &e.ws);
    assert!(has_exec(&ops, "rm"));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 1);
    assert_eq!(resolved_of(dels[0]), Some(e.abs_of("/tmp/x").as_path()));

    // long form, and the chdir must NOT leak to later commands in the chain
    let ops = e.analyzer.analyze("env --chdir /tmp rm x && cat y", &e.ws);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(resolved_of(dels[0]), Some(e.abs_of("/tmp/x").as_path()));
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(resolved_of(reads[0]), Some(e.ws.join("y").as_path())); // back to original cwd

    // dynamic -C target poisons relative paths (safe direction)
    let ops = e.analyzer.analyze("env -C $D rm x", &e.ws);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(zone_of(dels[0]), Some(Zone::Unresolved));

    // -S changes arg parsing → fail closed
    let ops = e.analyzer.analyze("env -S 'rm x' ", &e.ws);
    assert!(ops.iter().any(|o| matches!(o, Op::Unknown { .. })));
}

#[test]
fn xargs_arg_file_surfaces_command_and_read() {
    let e = setup();
    let ops = e.analyzer.analyze("xargs -a files.txt rm -rf sub", &e.ws);
    // -a FILE is read, and rm is analyzed as the real command
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(
        resolved_of(reads[0]),
        Some(e.ws.join("files.txt").as_path())
    );
    assert!(ops.iter().any(
        |o| matches!(o, Op::Exec { head, dynamic_args, .. } if head == "rm" && *dynamic_args)
    ));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(resolved_of(dels[0]), Some(e.ws.join("sub").as_path()));
    // glued --arg-file= form
    let ops = e.analyzer.analyze("xargs --arg-file=files.txt rm", &e.ws);
    assert!(!path_ops(&ops, Access::Read).is_empty());
    assert!(has_exec(&ops, "rm"));
}

#[test]
fn cp_target_directory_flag() {
    let e = setup();
    // -t DIR: DIR is the write target, all positionals are sources
    let ops = e.analyzer.analyze("cp -t /tmp/out a b", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(reads.len(), 2);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(resolved_of(writes[0]), Some(e.abs_of("/tmp/out").as_path()));
    // long glued form
    let ops = e
        .analyzer
        .analyze("cp --target-directory=/tmp/out a", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(resolved_of(writes[0]), Some(e.abs_of("/tmp/out").as_path()));
    // no -t: last operand is the destination (unchanged behavior)
    let ops = e.analyzer.analyze("cp a sub/b", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(resolved_of(writes[0]), Some(e.ws.join("sub/b").as_path()));
}

#[test]
fn rsync_delete_emits_delete_on_dest() {
    let e = setup();
    let ops = e.analyzer.analyze("rsync --delete sub/ /tmp/dst/", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(resolved_of(writes[0]), Some(e.abs_of("/tmp/dst").as_path()));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 1);
    assert_eq!(resolved_of(dels[0]), Some(e.abs_of("/tmp/dst").as_path()));
    // plain rsync (no --delete) has no delete op
    let ops = e.analyzer.analyze("rsync sub/ /tmp/dst/", &e.ws);
    assert!(path_ops(&ops, Access::Delete).is_empty());
    // rsync's dest is the TRAILING operand (rsync has no -t target-dir; -t is
    // --times), so other flags before it don't move the delete off the dest
    let ops = e
        .analyzer
        .analyze("rsync -a --delete sub/ /tmp/dst/", &e.ws);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 1);
    assert_eq!(resolved_of(dels[0]), Some(e.abs_of("/tmp/dst").as_path()));
}

// ── spec-driven wrapper invariants ─────────────────────────────────────

#[test]
fn wrapper_unknown_option_fails_closed() {
    let e = setup();
    // THE invariant: an option not in the wrapper's spec must NOT let us guess
    // the command head. Previously `env --frob VALUE rm x` analyzed `VALUE` as
    // the command; now it is Unknown and the inner `rm` is NOT reached.
    let ops = e
        .analyzer
        .analyze("env --frobnicate VALUE rm -rf sub", &e.ws);
    assert!(has_unknown(&ops));
    assert!(!has_exec(&ops, "rm"));
    assert!(!has_exec(&ops, "VALUE"));
    assert!(path_ops(&ops, Access::Delete).is_empty());

    // same for a bare unknown short option
    let ops = e.analyzer.analyze("timeout -Z 5 rm x", &e.ws);
    assert!(has_unknown(&ops));
    assert!(!has_exec(&ops, "rm"));

    // sudo still surfaces its own exec (floor), then bails on the unknown option
    let ops = e.analyzer.analyze("sudo --frob rm x", &e.ws);
    assert!(has_exec(&ops, "sudo"));
    assert!(has_unknown(&ops));
    assert!(!has_exec(&ops, "rm"));
}

#[test]
fn wrapper_dynamic_token_fails_closed() {
    let e = setup();
    // a dynamic token in the peel zone could be an option or the command —
    // refuse to guess
    let ops = e.analyzer.analyze("env $MAYBE_OPT rm x", &e.ws);
    assert!(has_unknown(&ops));
    assert!(!has_exec(&ops, "rm"));
}

#[test]
fn wrapper_valued_options_do_not_eat_the_command() {
    let e = setup();
    // every valued option consumes its value, never the command head
    for cmd in [
        "nice -n 10 rm -rf sub",
        "timeout -s KILL -k 3 5 rm -rf sub",
        "env -u PATH -u HOME rm -rf sub",
        "stdbuf -oL -eL rm -rf sub",
        "timeout --signal=KILL 5 rm -rf sub",
    ] {
        let ops = e.analyzer.analyze(cmd, &e.ws);
        assert!(has_exec(&ops, "rm"), "{cmd}: rm not reached");
        let dels = path_ops(&ops, Access::Delete);
        assert_eq!(dels.len(), 1, "{cmd}");
        assert_eq!(
            resolved_of(dels[0]),
            Some(e.ws.join("sub").as_path()),
            "{cmd}"
        );
    }
}

#[test]
fn nested_wrappers_peel_and_scope() {
    let e = setup();
    let ops = e
        .analyzer
        .analyze("sudo env -C /tmp timeout 5 rm -rf x", &e.ws);
    assert!(has_exec(&ops, "sudo"));
    assert!(has_exec(&ops, "rm"));
    let dels = path_ops(&ops, Access::Delete);
    // env -C /tmp scopes the resolution even through sudo + timeout
    assert_eq!(resolved_of(dels[0]), Some(e.abs_of("/tmp/x").as_path()));
}

// ── spec-driven path-role invariants ───────────────────────────────────

#[test]
fn grep_pattern_source_options_suppress_skip() {
    let e = setup();
    // with -e, there is NO inline pattern positional, so the file must not be
    // skipped as "the pattern"
    let ops = e.analyzer.analyze("grep -e foo data.txt", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(reads.len(), 1);
    assert_eq!(resolved_of(reads[0]), Some(e.ws.join("data.txt").as_path()));

    // -f PATTERNFILE: the pattern file is itself read, AND the data file is read
    let ops = e.analyzer.analyze("grep -f pats.txt data.txt", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    let paths: Vec<_> = reads.iter().filter_map(|o| resolved_of(o)).collect();
    assert!(paths.contains(&e.ws.join("pats.txt").as_path()));
    assert!(paths.contains(&e.ws.join("data.txt").as_path()));

    // without -e/-f the first positional is still the (non-path) pattern
    let ops = e.analyzer.analyze("grep foo data.txt", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(reads.len(), 1);
    assert_eq!(resolved_of(reads[0]), Some(e.ws.join("data.txt").as_path()));
}

#[test]
fn chmod_reference_suppresses_mode_skip() {
    let e = setup();
    // --reference=RFILE means there is no mode positional; the operand is the
    // write target, and RFILE is read
    let ops = e
        .analyzer
        .analyze("chmod --reference=ref.txt target.txt", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(
        resolved_of(writes[0]),
        Some(e.ws.join("target.txt").as_path())
    );
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(resolved_of(reads[0]), Some(e.ws.join("ref.txt").as_path()));
    // plain chmod: first positional is the mode, not a path
    let ops = e.analyzer.analyze("chmod 600 secret.txt", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(
        resolved_of(writes[0]),
        Some(e.ws.join("secret.txt").as_path())
    );
    assert!(!has_exec(&ops, "600"));
}

#[test]
fn truncate_size_value_not_a_path() {
    let e = setup();
    // -s SIZE must be consumed as a value, not treated as a write target
    let ops = e.analyzer.analyze("truncate -s 0 data.log", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(
        resolved_of(writes[0]),
        Some(e.ws.join("data.log").as_path())
    );
}

#[test]
fn touch_reference_file_is_read() {
    let e = setup();
    let ops = e.analyzer.analyze("touch -r template.txt newfile", &e.ws);
    let reads = path_ops(&ops, Access::Read);
    assert_eq!(
        resolved_of(reads[0]),
        Some(e.ws.join("template.txt").as_path())
    );
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(resolved_of(writes[0]), Some(e.ws.join("newfile").as_path()));
}

#[test]
fn common_flags_do_not_eat_path_operands() {
    let e = setup();
    for cmd in ["grep -v foo ~/.ssh/id_rsa", "rg -v foo ~/.ssh/id_rsa"] {
        let ops = e.analyzer.analyze(cmd, &e.ws);
        let reads = path_ops(&ops, Access::Read);
        assert_eq!(reads.len(), 1, "{cmd}");
        assert_eq!(zone_of(reads[0]), Some(Zone::Sensitive), "{cmd}");
    }

    let ops = e.analyzer.analyze("touch -m ~/.ssh/id_rsa", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(zone_of(writes[0]), Some(Zone::Sensitive));

    let ops = e.analyzer.analyze("mkdir -m 700 ~/.ssh/newdir", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(zone_of(writes[0]), Some(Zone::Sensitive));

    let ops = e.analyzer.analyze("tee -a ~/.ssh/authorized_keys", &e.ws);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(writes.len(), 1);
    assert_eq!(zone_of(writes[0]), Some(Zone::Sensitive));
}

#[test]
fn strict_specs_still_accept_common_flag_clusters() {
    let e = setup();
    let ops = e.analyzer.analyze("rm -rf sub", &e.ws);
    assert!(!has_unknown(&ops));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(resolved_of(dels[0]), Some(e.ws.join("sub").as_path()));

    let ops = e
        .analyzer
        .analyze("rsync -av --delete sub/ /tmp/dst/", &e.ws);
    assert!(!has_unknown(&ops));
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(resolved_of(dels[0]), Some(e.abs_of("/tmp/dst").as_path()));
}

#[test]
fn mv_target_directory_flag() {
    let e = setup();
    // mv -t DIR a b: a,b are ReadDelete sources, DIR is the write dest
    let ops = e.analyzer.analyze("mv -t /tmp/out a b", &e.ws);
    assert_eq!(path_ops(&ops, Access::Read).len(), 2);
    let dels = path_ops(&ops, Access::Delete);
    assert_eq!(dels.len(), 2);
    let writes = path_ops(&ops, Access::Write);
    assert_eq!(resolved_of(writes[0]), Some(e.abs_of("/tmp/out").as_path()));
}
