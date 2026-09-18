//! Windows dialects: cmd.exe and PowerShell.
//! The caller must say WHICH interpreter will execute the string — cmd and
//! PS syntax are mutually unintelligible and guessing from the text is a
//! security hole. Cross-dialect launches recurse: `cmd /c` from PS analyzes
//! as cmd, `powershell -Command` from cmd analyzes as PS. Child launches
//! run on a CLONED state (separate process — cwd changes don't propagate);
//! PS subexpressions/blocks share the SAME state (PS scoping keeps one
//! location per runspace).

pub mod cmd;
pub mod ps;

use crate::analyze::MAX_DEPTH;
use crate::ops::Op;
use crate::winpath::{WinAbs, WinCwd, WinPathForm, WinResolver, parse_path};

pub struct WinAnalyzer {
    pub resolver: WinResolver,
}

#[derive(Clone)]
pub(crate) struct WinState {
    pub cwd: WinCwd,
    pub stack: Vec<Option<WinAbs>>,
}

impl WinAnalyzer {
    pub fn new(workspace: &str, home: &str) -> Self {
        WinAnalyzer {
            resolver: WinResolver::new(workspace, home),
        }
    }

    pub fn analyze_cmd(&self, input: &str, cwd: &str) -> Vec<Op> {
        let mut ops = Vec::new();
        let mut st = self.init_state(cwd);
        self.run_cmd(input, &mut st, 0, &mut ops);
        ops
    }

    pub fn analyze_ps(&self, input: &str, cwd: &str) -> Vec<Op> {
        let mut ops = Vec::new();
        let mut st = self.init_state(cwd);
        self.run_ps(input, &mut st, 0, &mut ops);
        ops
    }

    fn init_state(&self, cwd: &str) -> WinState {
        let cwd = match parse_path(cwd) {
            WinPathForm::Absolute(a) => WinCwd::known(a),
            _ => WinCwd::unknown(),
        };
        WinState {
            cwd,
            stack: Vec::new(),
        }
    }

    pub(crate) fn run_cmd(&self, input: &str, st: &mut WinState, depth: usize, ops: &mut Vec<Op>) {
        if depth > MAX_DEPTH {
            ops.push(Op::Unknown {
                reason: "recursion depth exceeded".into(),
                snippet: input.chars().take(80).collect(),
            });
            return;
        }
        cmd::analyze(self, input, st, depth, ops);
    }

    pub(crate) fn run_ps(&self, input: &str, st: &mut WinState, depth: usize, ops: &mut Vec<Op>) {
        if depth > MAX_DEPTH {
            ops.push(Op::Unknown {
                reason: "recursion depth exceeded".into(),
                snippet: input.chars().take(80).collect(),
            });
            return;
        }
        ps::analyze(self, input, st, depth, ops);
    }
}

/// Shared handling of a `powershell` / `pwsh` invocation's arguments,
/// callable from either dialect. `args` = (static value if any, display).
/// The host CLI accepts unambiguous flag prefixes (`-c`, `-com`, `-enc`),
/// so match by prefix.
pub(crate) fn analyze_ps_invocation(
    a: &WinAnalyzer,
    args: &[(Option<String>, String)],
    st: &WinState,
    depth: usize,
    ops: &mut Vec<Op>,
) {
    // flags of the powershell host binary that consume a value
    const VALUED: &[&str] = &[
        "-executionpolicy",
        "-windowstyle",
        "-workingdirectory",
        "-psconsolefile",
        "-version",
        "-inputformat",
        "-outputformat",
        "-configurationname",
    ];
    let mut i = 0usize;
    while i < args.len() {
        let (stat, disp) = &args[i];
        let Some(s) = stat else {
            ops.push(Op::Unknown {
                reason: "dynamic argument to powershell host".into(),
                snippet: disp.clone(),
            });
            return;
        };
        let sl = s.to_lowercase();
        if sl.starts_with('-') {
            if "-encodedcommand".starts_with(&sl) && sl.len() >= 3 || sl == "-e" || sl == "-ec" {
                ops.push(Op::Unknown {
                    reason: "powershell -EncodedCommand (base64 payload, not analyzed)".into(),
                    snippet: args.get(i + 1).map(|(_, d)| d.clone()).unwrap_or_default(),
                });
                return;
            }
            if "-command".starts_with(&sl) && sl.len() >= 2 {
                // everything after -Command joins into one script
                let rest = &args[i + 1..];
                if rest.is_empty() {
                    ops.push(Op::Unknown {
                        reason: "powershell -Command with no payload (stdin)".into(),
                        snippet: String::new(),
                    });
                    return;
                }
                if let Some(joined) = rest
                    .iter()
                    .map(|(s, _)| s.clone())
                    .collect::<Option<Vec<_>>>()
                    .map(|v| v.join(" "))
                {
                    let mut sub = st.clone(); // child process
                    a.run_ps(&joined, &mut sub, depth + 1, ops);
                } else {
                    ops.push(Op::Unknown {
                        reason: "powershell -Command with dynamic payload".into(),
                        snippet: rest
                            .iter()
                            .map(|(_, d)| d.clone())
                            .collect::<Vec<_>>()
                            .join(" "),
                    });
                }
                return;
            }
            if "-file".starts_with(&sl) && sl.len() >= 2 {
                match args.get(i + 1).and_then(|(s, _)| s.clone()) {
                    Some(f) => ops.push(Op::Script {
                        interpreter: "powershell".into(),
                        path: a.resolver.resolve(&f, &st.cwd, false),
                    }),
                    None => ops.push(Op::Unknown {
                        reason: "powershell -File with dynamic path".into(),
                        snippet: args.get(i + 1).map(|(_, d)| d.clone()).unwrap_or_default(),
                    }),
                }
                return;
            }
            if VALUED.iter().any(|v| v.starts_with(&sl)) {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // bare positional = script file
        ops.push(Op::Script {
            interpreter: "powershell".into(),
            path: a.resolver.resolve(s, &st.cwd, false),
        });
        return;
    }
    ops.push(Op::Unknown {
        reason: "powershell reading commands from stdin".into(),
        snippet: String::new(),
    });
}
