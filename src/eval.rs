//! Preset benchmark harness (issue #129).
//!
//! The harness answers one question with numbers instead of feel: **what does
//! a preset cost and what does it buy?** It runs the *same fixed task set*
//! under every preset and emits a per-run record (JSONL) plus a markdown
//! comparison whose headline is the token ratio, the wall-clock delta and the
//! success rate.
//!
//! # Axes
//!
//! * **Task set** — [`TASKS`], a fixed list of 10 tasks covering the five
//!   shapes a coding agent meets: single-file read-modify-write, multi-file
//!   search & locate, multi-step tool chains, long-context inputs, and failure
//!   recovery. Each task carries its own seed workspace and a machine-checkable
//!   [`SuccessCheck`].
//! * **Presets** — the built-in presets ([`crate::preset::builtin`]); the
//!   harness resolves each one with [`PresetEnv::default`], so an ambient
//!   `RECURSIVE_*` variable cannot silently move the baseline.
//! * **Mode** — [`Mode::Replay`] drives the task's script through a
//!   [`MockProvider`], so tokens and success are reproducible with no API key
//!   (this is what the committed report uses); [`Mode::Live`] drives the
//!   configured real provider and is the quality measurement.
//!
//! # Metrics
//!
//! Per run: the preset's system-prompt weight (the *fixed* per-request cost,
//! model-independent), input / output / cache-read tokens, wall clock, turns
//! (LLM calls), tool calls, success, and a failure class when the run did not
//! succeed.
//!
//! In replay mode the token counts are the harness's own `bytes/4` estimate
//! (the scripted provider reports none) — the fixed cost and the per-run input
//! are still real, the *model's* output is scripted. Wall clock is real tool
//! execution, so it moves a little between runs even on identical input.
//! Replay is therefore the plumbing + fixed-cost measurement; success rate and
//! latency *differences between presets* are only meaningful in live mode.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::llm::mock::MockProvider;
use crate::llm::{estimate_tokens, ChatProvider, Completion, ToolCall};
use crate::message::{Message, Role};
use crate::preset::{self, AgentPreset, ChannelSupport, PresetEnv};
use crate::runtime::AgentRuntimeBuilder;
use crate::tools::build_standard_tools;

// ── task set ──────────────────────────────────────────────────────────────

/// The five task shapes the first task set covers (issue #129 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskCategory {
    /// Read a file, change it, write it back.
    SingleFile,
    /// Locate the file (or the value) that matches a criterion across several.
    SearchLocate,
    /// A chain of dependent steps where each consumes the previous result.
    MultiStepChain,
    /// A large input — the context the preset has to carry (and, under
    /// `standard`, the threshold a compactor watches).
    LongContext,
    /// A first step fails (missing file, bad edit); the run has to recover.
    FailureRecovery,
}

impl TaskCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskCategory::SingleFile => "single-file",
            TaskCategory::SearchLocate => "search-locate",
            TaskCategory::MultiStepChain => "multi-step",
            TaskCategory::LongContext => "long-context",
            TaskCategory::FailureRecovery => "failure-recovery",
        }
    }

    /// Every category, in report order.
    pub fn all() -> [TaskCategory; 5] {
        [
            TaskCategory::SingleFile,
            TaskCategory::SearchLocate,
            TaskCategory::MultiStepChain,
            TaskCategory::LongContext,
            TaskCategory::FailureRecovery,
        ]
    }
}

/// One tool call in a task's deterministic (replay) script.
#[derive(Debug, Clone, Copy)]
pub struct ScriptCall {
    /// LLM-facing tool name (`Read` / `Write` / `Edit` / `Bash`).
    pub name: &'static str,
    /// Raw JSON object of arguments, parsed at run time.
    pub arguments: &'static str,
}

/// One step of a task's replay script: an assistant turn that either calls a
/// tool or ends the run with final text.
#[derive(Debug, Clone, Copy)]
pub struct ScriptStep {
    pub content: &'static str,
    pub call: Option<ScriptCall>,
}

/// How a task's success is decided, against the run's workspace.
#[derive(Debug, Clone, Copy)]
pub enum Expect {
    /// The file exists.
    Exists,
    /// The file does not exist.
    Absent,
    /// The file's text contains this substring.
    Contains(&'static str),
    /// The file's text is exactly this.
    Equals(&'static str),
}

/// A machine-checkable success predicate.
#[derive(Debug, Clone, Copy)]
pub struct SuccessCheck {
    /// Workspace-relative path.
    pub path: &'static str,
    pub expect: Expect,
}

impl SuccessCheck {
    /// Evaluate the check against a workspace directory.
    pub fn evaluate(&self, workspace: &Path) -> bool {
        let path = workspace.join(self.path);
        match self.expect {
            Expect::Exists => path.exists(),
            Expect::Absent => !path.exists(),
            Expect::Contains(needle) => std::fs::read_to_string(&path)
                .map(|text| text.contains(needle))
                .unwrap_or(false),
            Expect::Equals(want) => std::fs::read_to_string(&path)
                .map(|text| text == want)
                .unwrap_or(false),
        }
    }
}

/// Writes a task's seed files into a fresh workspace.
type Seeder = fn(&Path) -> io::Result<()>;

/// One benchmark task.
#[derive(Debug, Clone, Copy)]
pub struct EvalTask {
    pub id: &'static str,
    pub category: TaskCategory,
    pub goal: &'static str,
    pub seed: Seeder,
    pub script: &'static [ScriptStep],
    pub check: SuccessCheck,
}

fn write_seed(workspace: &Path, rel: &str, contents: &str) -> io::Result<()> {
    let path = workspace.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

fn seed_empty(_workspace: &Path) -> io::Result<()> {
    Ok(())
}

fn seed_data_csv(workspace: &Path) -> io::Result<()> {
    write_seed(workspace, "data.csv", "1,alice,90\n2,bob,85\n")
}

fn seed_settings_ini(workspace: &Path) -> io::Result<()> {
    write_seed(workspace, "settings.ini", "debug = false\nport = 8080\n")
}

fn seed_marker_files(workspace: &Path) -> io::Result<()> {
    write_seed(workspace, "a.txt", "alpha\n")?;
    write_seed(workspace, "b.txt", "MARKER lives here\n")?;
    write_seed(workspace, "c.txt", "gamma\n")
}

fn seed_line_files(workspace: &Path) -> io::Result<()> {
    write_seed(workspace, "x.txt", "one\ntwo\nthree\n")?;
    write_seed(workspace, "y.txt", "four\nfive\n")
}

fn seed_input_txt(workspace: &Path) -> io::Result<()> {
    write_seed(workspace, "input.txt", "abc\n")
}

fn seed_big_txt(workspace: &Path) -> io::Result<()> {
    let mut text = String::with_capacity(32 * 1024);
    for i in 0..800 {
        text.push_str(&format!("filler line {i}\n"));
    }
    text.push_str("FLAG: 12345\n");
    write_seed(workspace, "big.txt", &text)
}

fn seed_notes_md(workspace: &Path) -> io::Result<()> {
    for (name, body) in [
        ("a.md", "alpha note\n"),
        ("b.md", "beta note\n"),
        ("c.md", "gamma note\n"),
        ("d.md", "the SECRET is 42\n"),
        ("e.md", "epsilon note\n"),
        ("f.md", "zeta note\n"),
    ] {
        write_seed(workspace, name, body)?;
    }
    Ok(())
}

/// The fixed task set (issue #129 §1). Ten tasks, two per category — the
/// first version's 8–15 window, deliberately not a moving target: the set is
/// what makes runs comparable across time.
pub static TASKS: &[EvalTask] = &[
    EvalTask {
        id: "single-append-row",
        category: TaskCategory::SingleFile,
        goal: "Append the row `3,carol,88` to data.csv, keeping the rows already there.",
        seed: seed_data_csv,
        script: &[
            ScriptStep {
                content: "Reading data.csv first.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"data.csv"}"##,
                }),
            },
            ScriptStep {
                content: "Appending the new row.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"data.csv","contents":"1,alice,90\n2,bob,85\n3,carol,88\n"}"##,
                }),
            },
            ScriptStep {
                content: "Appended row 3,carol,88 to data.csv.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "data.csv",
            expect: Expect::Equals("1,alice,90\n2,bob,85\n3,carol,88\n"),
        },
    },
    EvalTask {
        id: "single-fix-config",
        category: TaskCategory::SingleFile,
        goal: "Turn on debugging in settings.ini (`debug = true`).",
        seed: seed_settings_ini,
        script: &[
            ScriptStep {
                content: "Reading settings.ini.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"settings.ini"}"##,
                }),
            },
            ScriptStep {
                content: "Flipping the debug flag.",
                call: Some(ScriptCall {
                    name: "Edit",
                    arguments: r##"{"file_path":"settings.ini","old_string":"debug = false","new_string":"debug = true"}"##,
                }),
            },
            ScriptStep {
                content: "Debugging is on in settings.ini.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "settings.ini",
            expect: Expect::Contains("debug = true"),
        },
    },
    EvalTask {
        id: "search-find-marker",
        category: TaskCategory::SearchLocate,
        goal: "Find which .txt file contains MARKER and write its name to found.txt.",
        seed: seed_marker_files,
        script: &[
            ScriptStep {
                content: "Searching for the marker.",
                call: Some(ScriptCall {
                    name: "Bash",
                    arguments: r##"{"command":"grep -rl MARKER ."}"##,
                }),
            },
            ScriptStep {
                content: "Recording the file that matched.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"found.txt","contents":"b.txt"}"##,
                }),
            },
            ScriptStep {
                content: "b.txt is the file that contains MARKER.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "found.txt",
            expect: Expect::Equals("b.txt"),
        },
    },
    EvalTask {
        id: "search-count-lines",
        category: TaskCategory::SearchLocate,
        goal: "Count the total number of lines across every .txt file and write the number to total.txt.",
        seed: seed_line_files,
        script: &[
            ScriptStep {
                content: "Counting the lines across all text files.",
                call: Some(ScriptCall {
                    name: "Bash",
                    arguments: r##"{"command":"cat *.txt | wc -l"}"##,
                }),
            },
            ScriptStep {
                content: "Writing the count.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"total.txt","contents":"5"}"##,
                }),
            },
            ScriptStep {
                content: "There are 5 lines across all .txt files.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "total.txt",
            expect: Expect::Equals("5"),
        },
    },
    EvalTask {
        id: "chain-script-run",
        category: TaskCategory::MultiStepChain,
        goal: "Create run.sh that prints hello, execute it, and save its output to out.txt.",
        seed: seed_empty,
        script: &[
            ScriptStep {
                content: "Writing the script.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"run.sh","contents":"#!/bin/sh\necho hello\n"}"##,
                }),
            },
            ScriptStep {
                content: "Running it and capturing the output.",
                call: Some(ScriptCall {
                    name: "Bash",
                    arguments: r##"{"command":"sh run.sh > out.txt"}"##,
                }),
            },
            ScriptStep {
                content: "Verifying the captured output.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"out.txt"}"##,
                }),
            },
            ScriptStep {
                content: "out.txt contains the script's output: hello.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "out.txt",
            expect: Expect::Equals("hello\n"),
        },
    },
    EvalTask {
        id: "chain-transform",
        category: TaskCategory::MultiStepChain,
        goal: "Read input.txt, uppercase it, and write the result to output.txt.",
        seed: seed_input_txt,
        script: &[
            ScriptStep {
                content: "Reading the input.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"input.txt"}"##,
                }),
            },
            ScriptStep {
                content: "Writing the uppercased result.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"output.txt","contents":"ABC\n"}"##,
                }),
            },
            ScriptStep {
                content: "Verifying the result.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"output.txt"}"##,
                }),
            },
            ScriptStep {
                content: "output.txt now holds ABC.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "output.txt",
            expect: Expect::Equals("ABC\n"),
        },
    },
    EvalTask {
        id: "long-extract-line",
        category: TaskCategory::LongContext,
        goal: "Find the line containing FLAG in big.txt and write it to flag.txt.",
        seed: seed_big_txt,
        script: &[
            ScriptStep {
                content: "Reading the large file.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"big.txt"}"##,
                }),
            },
            ScriptStep {
                content: "Saving the matching line.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"flag.txt","contents":"FLAG: 12345"}"##,
                }),
            },
            ScriptStep {
                content: "Flagged FLAG: 12345 from big.txt.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "flag.txt",
            expect: Expect::Contains("FLAG: 12345"),
        },
    },
    EvalTask {
        id: "long-many-reads",
        category: TaskCategory::LongContext,
        goal: "Read every .md file and write the name of the one containing SECRET to secret.txt.",
        seed: seed_notes_md,
        script: &[
            ScriptStep {
                content: "Reading a.md.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"a.md"}"##,
                }),
            },
            ScriptStep {
                content: "Reading b.md.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"b.md"}"##,
                }),
            },
            ScriptStep {
                content: "Reading c.md.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"c.md"}"##,
                }),
            },
            ScriptStep {
                content: "Reading d.md.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"d.md"}"##,
                }),
            },
            ScriptStep {
                content: "Reading e.md.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"e.md"}"##,
                }),
            },
            ScriptStep {
                content: "Reading f.md.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"f.md"}"##,
                }),
            },
            ScriptStep {
                content: "Recording the file that holds the secret.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"secret.txt","contents":"d.md"}"##,
                }),
            },
            ScriptStep {
                content: "d.md is the file that contains SECRET.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "secret.txt",
            expect: Expect::Equals("d.md"),
        },
    },
    EvalTask {
        id: "recover-missing-read",
        category: TaskCategory::FailureRecovery,
        goal: "Read notes.txt — it may not exist yet — and make sure it contains `hello`.",
        seed: seed_empty,
        script: &[
            ScriptStep {
                content: "Reading notes.txt.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"notes.txt"}"##,
                }),
            },
            ScriptStep {
                content: "The read failed; creating the file.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"notes.txt","contents":"hello\n"}"##,
                }),
            },
            ScriptStep {
                content: "Reading it back to confirm.",
                call: Some(ScriptCall {
                    name: "Read",
                    arguments: r##"{"path":"notes.txt"}"##,
                }),
            },
            ScriptStep {
                content: "notes.txt now contains hello.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "notes.txt",
            expect: Expect::Equals("hello\n"),
        },
    },
    EvalTask {
        id: "recover-bad-edit",
        category: TaskCategory::FailureRecovery,
        goal: "Add `a=1` to missing.cfg, creating it since it does not exist.",
        seed: seed_empty,
        script: &[
            ScriptStep {
                content: "Trying to edit missing.cfg.",
                call: Some(ScriptCall {
                    name: "Edit",
                    arguments: r##"{"file_path":"missing.cfg","old_string":"a=0","new_string":"a=1"}"##,
                }),
            },
            ScriptStep {
                content: "The edit failed; writing the file instead.",
                call: Some(ScriptCall {
                    name: "Write",
                    arguments: r##"{"path":"missing.cfg","contents":"a=1\n"}"##,
                }),
            },
            ScriptStep {
                content: "missing.cfg now sets a=1.",
                call: None,
            },
        ],
        check: SuccessCheck {
            path: "missing.cfg",
            expect: Expect::Contains("a=1"),
        },
    },
];

/// The fixed task set.
pub fn tasks() -> &'static [EvalTask] {
    TASKS
}

/// Look up a task by id.
pub fn task(id: &str) -> Option<&'static EvalTask> {
    TASKS.iter().find(|t| t.id == id)
}

// ── run modes ─────────────────────────────────────────────────────────────

/// How a run is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A [`MockProvider`] fed by the task's script. No API key; tokens and
    /// success are identical on every run, real tool execution.
    Replay,
    /// The configured real provider. This is the quality measurement.
    Live,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Replay => "replay",
            Mode::Live => "live",
        }
    }
}

// ── per-run record ────────────────────────────────────────────────────────

/// One (task × preset) measurement — the raw data behind the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub task_id: String,
    pub category: String,
    pub preset: String,
    /// The preset's system-prompt weight for this run — the model-independent
    /// fixed cost, identical for every task under the same preset. This is the
    /// headline "preset gain" number; `input_tokens` is what the run paid in
    /// total (and grows with the transcript).
    pub system_prompt_tokens: u32,
    pub mode: String,
    pub success: bool,
    /// `FinishReason`'s `Display`, or `"error"` when the run itself failed.
    pub finish_reason: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub wall_ms: u64,
    pub turns: usize,
    pub tool_calls: usize,
    /// Failure class when `success` is false (see [`classify_failure`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Bucket a failed run by why. `finish_reason` is the `FinishReason` display
/// string, or `"error"` for a run that returned `Err`.
pub fn classify_failure(finish_reason: &str) -> &'static str {
    let f = finish_reason;
    if f.starts_with("budget_exceeded") {
        "budget_exceeded"
    } else if f.starts_with("stuck") {
        "stuck"
    } else if f.starts_with("transcript_limit") {
        "context_limit"
    } else if f.starts_with("provider_stop") {
        "provider_error"
    } else if f.starts_with("wall_clock") {
        "wall_clock"
    } else if f.starts_with("cancelled") {
        "cancelled"
    } else if f.starts_with("permission_denial") {
        "permission_denied"
    } else if f == "error" {
        "run_error"
    } else {
        "wrong_result"
    }
}

// ── running ───────────────────────────────────────────────────────────────

/// Project context every task workspace carries, so a session built here sees
/// the same kind of repo the agent normally runs in (and a `standard` session's
/// project-context injection — which `minimal` discards — is measured, not
/// assumed away).
const PROJECT_CONTEXT: &str = "\
# AGENTS.md — project contract

## Working principles
- Read before you write. Skim the relevant files before editing.
- Prefer targeted edits over whole-file rewrites.
- After any non-trivial change, run the tests and quote the result.

## Layout
- `src/` is the product.
- `tests/` holds the integration suites.

New capabilities belong in tools or providers, never as a branch in the loop.
";

fn prepare_workspace(workspace: &Path, task: &EvalTask) -> io::Result<()> {
    if workspace.exists() {
        std::fs::remove_dir_all(workspace)?;
    }
    std::fs::create_dir_all(workspace)?;
    (task.seed)(workspace)?;
    write_seed(workspace, "AGENTS.md", PROJECT_CONTEXT)
}

fn completions_for(task: &EvalTask) -> Result<Vec<Completion>> {
    task.script
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let tool_calls = match step.call {
                Some(call) => {
                    let arguments: serde_json::Value = serde_json::from_str(call.arguments)
                        .map_err(|e| Error::Internal {
                            context: format!("eval task `{}` step {i}", task.id),
                            message: format!("script arguments are not JSON: {e}"),
                        })?;
                    vec![ToolCall {
                        id: format!("call-{i}"),
                        name: call.name.to_string(),
                        arguments,
                    }]
                }
                None => Vec::new(),
            };
            let finish_reason = if tool_calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            };
            Ok(Completion {
                content: step.content.to_string(),
                tool_calls,
                finish_reason: Some(finish_reason.to_string()),
                usage: None,
                reasoning_content: None,
            })
        })
        .collect()
}

struct Capture {
    finish_reason: Option<String>,
    transcript: Vec<Message>,
    provider_calls: Vec<Vec<Message>>,
    error: Option<String>,
    steps: usize,
    usage: Option<crate::llm::TokenUsage>,
    system_prompt_tokens: u32,
}

async fn drive(
    config: &Config,
    preset: &'static AgentPreset,
    task: &EvalTask,
    provider: Arc<dyn ChatProvider>,
    mock: Option<&MockProvider>,
    workspace: &Path,
) -> Capture {
    let mut run_config = config.clone();
    run_config.workspace = workspace.to_path_buf();

    let resolved = preset.resolve(&run_config, &PresetEnv::default());
    let assembled = preset::preview_system_prompt(&run_config, &resolved, &[]);
    let system_prompt_tokens = preset::system_prompt_tokens(&run_config, &resolved, &[]);
    let registry = build_standard_tools(workspace, &[], run_config.shell_timeout_secs);
    let assets = preset::assets_from_registry(&registry, Vec::new());
    let builder = preset::apply(
        AgentRuntimeBuilder::new()
            .llm(provider)
            .tools(registry)
            .system_prompt(assembled.full),
        &resolved,
        &assets,
        ChannelSupport { interactive: false },
    );

    let mut runtime = match builder.build() {
        Ok(runtime) => runtime,
        Err(e) => {
            return Capture {
                finish_reason: None,
                transcript: Vec::new(),
                provider_calls: Vec::new(),
                error: Some(format!("build: {e}")),
                steps: 0,
                usage: None,
                system_prompt_tokens,
            }
        }
    };

    let result = runtime.run(task.goal).await;
    let transcript = runtime.transcript().to_vec();
    let provider_calls = mock.map(|m| m.calls()).unwrap_or_default();
    match result {
        Ok(outcome) => Capture {
            finish_reason: Some(outcome.finish_reason.to_string()),
            transcript,
            provider_calls,
            error: None,
            steps: outcome.steps,
            usage: Some(outcome.total_usage),
            system_prompt_tokens,
        },
        Err(e) => Capture {
            finish_reason: None,
            transcript,
            provider_calls,
            error: Some(format!("run: {e}")),
            steps: 0,
            usage: None,
            system_prompt_tokens,
        },
    }
}

fn error_sample(task: &EvalTask, preset_id: &str, mode: Mode, message: String) -> Sample {
    Sample {
        task_id: task.id.to_string(),
        category: task.category.as_str().to_string(),
        preset: preset_id.to_string(),
        system_prompt_tokens: 0,
        mode: mode.as_str().to_string(),
        success: false,
        finish_reason: "error".to_string(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        wall_ms: 0,
        turns: 0,
        tool_calls: 0,
        failure: Some(format!("run_error: {message}")),
    }
}

fn estimate_messages_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .map(|m| {
            let mut n = estimate_tokens(&m.content);
            for call in &m.tool_calls {
                n = n.saturating_add(estimate_tokens(&call.name));
                n = n.saturating_add(estimate_tokens(&call.arguments.to_string()));
            }
            n
        })
        .sum()
}

/// Tokens the model itself produced: assistant content plus the arguments of
/// the tool calls it authored. Tool *results* are input, not output.
fn estimate_output_tokens(transcript: &[Message]) -> u32 {
    transcript
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| {
            let mut n = estimate_tokens(&m.content);
            for call in &m.tool_calls {
                n = n.saturating_add(estimate_tokens(&call.name));
                n = n.saturating_add(estimate_tokens(&call.arguments.to_string()));
            }
            n
        })
        .sum()
}

fn sample_from_capture(
    task: &EvalTask,
    preset_id: &str,
    mode: Mode,
    workspace: &Path,
    capture: Capture,
    wall_ms: u64,
) -> Sample {
    let success = capture.error.is_none() && task.check.evaluate(workspace);
    let finish_reason = capture
        .finish_reason
        .clone()
        .unwrap_or_else(|| "error".to_string());

    // Replay: the mock provider reports no usage, so the harness estimates the
    // tokens it saw (the system prompt is by far the largest term — which is
    // exactly the fixed cost a preset is judged on). Live: the provider's own
    // usage, cache split included.
    let (input_tokens, output_tokens, cache_read_tokens) = match mode {
        Mode::Replay => (
            capture
                .provider_calls
                .iter()
                .map(|call| estimate_messages_tokens(call))
                .sum(),
            estimate_output_tokens(&capture.transcript),
            0,
        ),
        Mode::Live => capture
            .usage
            .map(|u| (u.prompt_tokens, u.completion_tokens, u.cache_hit_tokens))
            .unwrap_or((0, 0, 0)),
    };

    let tool_calls = capture
        .transcript
        .iter()
        .map(|m| m.tool_calls.len())
        .sum::<usize>();

    let failure = match (success, &capture.error) {
        (true, _) => None,
        (false, Some(message)) => Some(format!("run_error: {message}")),
        (false, None) => Some(classify_failure(&finish_reason).to_string()),
    };

    Sample {
        task_id: task.id.to_string(),
        category: task.category.as_str().to_string(),
        preset: preset_id.to_string(),
        system_prompt_tokens: capture.system_prompt_tokens,
        mode: mode.as_str().to_string(),
        success,
        finish_reason,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        wall_ms,
        turns: capture.steps,
        tool_calls,
        failure,
    }
}

/// Run one task under one preset with a deterministic script (no API key).
///
/// `workspace` is wiped and re-seeded from the task, so a repeat run starts
/// from the same bytes.
pub async fn run_replay(
    config: &Config,
    preset: &'static AgentPreset,
    task: &EvalTask,
    workspace: &Path,
) -> Sample {
    if let Err(e) = prepare_workspace(workspace, task) {
        return error_sample(task, preset.id, Mode::Replay, format!("seed: {e}"));
    }
    let completions = match completions_for(task) {
        Ok(c) => c,
        Err(e) => return error_sample(task, preset.id, Mode::Replay, e.to_string()),
    };
    let mock = Arc::new(MockProvider::new(completions));

    let started = Instant::now();
    let capture = drive(config, preset, task, mock.clone(), Some(&mock), workspace).await;
    let wall_ms = started.elapsed().as_millis() as u64;
    sample_from_capture(task, preset.id, Mode::Replay, workspace, capture, wall_ms)
}

/// Run one task under one preset against the configured real provider.
pub async fn run_live(
    config: &Config,
    preset: &'static AgentPreset,
    task: &EvalTask,
    workspace: &Path,
    api_key: &str,
) -> Sample {
    if let Err(e) = prepare_workspace(workspace, task) {
        return error_sample(task, preset.id, Mode::Live, format!("seed: {e}"));
    }
    let provider = match crate::llm::build_llm_provider(
        config,
        api_key,
        crate::llm::RetryPolicy::default(),
        Some(config.max_search_rounds),
        config.thinking_budget,
    ) {
        Ok(p) => p,
        Err(e) => return error_sample(task, preset.id, Mode::Live, format!("provider: {e}")),
    };

    let started = Instant::now();
    let capture = drive(config, preset, task, provider, None, workspace).await;
    let wall_ms = started.elapsed().as_millis() as u64;
    sample_from_capture(task, preset.id, Mode::Live, workspace, capture, wall_ms)
}

/// One task under every selected preset, in `presets` order.
///
/// The same task is never run concurrently across presets — a task's workspace
/// is single-use — but `presets` themselves are sequential for the same
/// reason.
pub async fn run_task_across_presets(
    config: &Config,
    task: &EvalTask,
    preset_ids: &[&'static AgentPreset],
    mode: Mode,
    workdir: &Path,
    api_key: Option<&str>,
) -> Vec<Sample> {
    let mut samples = Vec::with_capacity(preset_ids.len());
    for preset in preset_ids {
        let workspace = workdir.join(preset.id).join(task.id);
        let sample = match mode {
            Mode::Replay => run_replay(config, preset, task, &workspace).await,
            Mode::Live => match api_key {
                Some(key) => run_live(config, preset, task, &workspace, key).await,
                None => error_sample(
                    task,
                    preset.id,
                    Mode::Live,
                    "no API key configured (set RECURSIVE_API_KEY)".to_string(),
                ),
            },
        };
        samples.push(sample);
    }
    samples
}

// ── report ────────────────────────────────────────────────────────────────

/// Aggregated metrics for one preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetStats {
    pub preset: String,
    /// The preset's system-prompt weight (constant across its runs) — the
    /// fixed per-request cost.
    pub system_prompt_tokens: u32,
    pub runs: usize,
    pub successes: usize,
    pub success_rate: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub wall_ms: u64,
    pub turns: u64,
    pub tool_calls: u64,
    /// Failure class → count, for every failed run.
    pub failures: BTreeMap<String, usize>,
}

/// Aggregate raw samples per preset (sorted by preset id).
pub fn summarize(samples: &[Sample]) -> Vec<PresetStats> {
    let mut by_preset: BTreeMap<&str, PresetStats> = BTreeMap::new();
    for sample in samples {
        let entry = by_preset
            .entry(sample.preset.as_str())
            .or_insert_with(|| PresetStats {
                preset: sample.preset.clone(),
                system_prompt_tokens: 0,
                runs: 0,
                successes: 0,
                success_rate: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                wall_ms: 0,
                turns: 0,
                tool_calls: 0,
                failures: BTreeMap::new(),
            });
        entry.runs += 1;
        entry.system_prompt_tokens = entry.system_prompt_tokens.max(sample.system_prompt_tokens);
        if sample.success {
            entry.successes += 1;
        }
        entry.input_tokens += sample.input_tokens as u64;
        entry.output_tokens += sample.output_tokens as u64;
        entry.cache_read_tokens += sample.cache_read_tokens as u64;
        entry.wall_ms += sample.wall_ms;
        entry.turns += sample.turns as u64;
        entry.tool_calls += sample.tool_calls as u64;
        if let Some(class) = &sample.failure {
            let key = class.split(':').next().unwrap_or(class).trim().to_string();
            *entry.failures.entry(key).or_insert(0) += 1;
        }
    }
    let mut out: Vec<PresetStats> = by_preset.into_values().collect();
    for stats in &mut out {
        stats.success_rate = if stats.runs == 0 {
            0.0
        } else {
            stats.successes as f64 / stats.runs as f64
        };
    }
    out
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// Per-category aggregate rows: `(category, stats)`.
pub fn summarize_by_category(samples: &[Sample]) -> Vec<(String, PresetStats)> {
    let mut keys: Vec<(String, String)> = Vec::new();
    for sample in samples {
        let key = (sample.category.clone(), sample.preset.clone());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.sort();
    keys.into_iter()
        .map(|(category, preset)| {
            let subset: Vec<Sample> = samples
                .iter()
                .filter(|s| s.category == category && s.preset == preset)
                .cloned()
                .collect();
            let stats = summarize(&subset)
                .into_iter()
                .next()
                .unwrap_or(PresetStats {
                    preset,
                    system_prompt_tokens: 0,
                    runs: 0,
                    successes: 0,
                    success_rate: 0.0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    wall_ms: 0,
                    turns: 0,
                    tool_calls: 0,
                    failures: BTreeMap::new(),
                });
            (category, stats)
        })
        .collect()
}

/// Observed spread of one (task × preset) across repeated runs — the variance
/// interval acceptance 2 asks for when the same configuration is re-run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    pub preset: String,
    pub task_id: String,
    pub runs: usize,
    pub min_input_tokens: u32,
    pub max_input_tokens: u32,
    pub min_wall_ms: u64,
    pub max_wall_ms: u64,
}

/// Group samples by (preset, task) and report the observed min/max. Rows with
/// `runs == 1` carry no spread information but are returned for completeness.
pub fn task_spread(samples: &[Sample]) -> Vec<Spread> {
    let mut rows: BTreeMap<(String, String), Spread> = BTreeMap::new();
    for sample in samples {
        let key = (sample.preset.clone(), sample.task_id.clone());
        let entry = rows.entry(key).or_insert_with(|| Spread {
            preset: sample.preset.clone(),
            task_id: sample.task_id.clone(),
            runs: 0,
            min_input_tokens: u32::MAX,
            max_input_tokens: 0,
            min_wall_ms: u64::MAX,
            max_wall_ms: 0,
        });
        entry.runs += 1;
        entry.min_input_tokens = entry.min_input_tokens.min(sample.input_tokens);
        entry.max_input_tokens = entry.max_input_tokens.max(sample.input_tokens);
        entry.min_wall_ms = entry.min_wall_ms.min(sample.wall_ms);
        entry.max_wall_ms = entry.max_wall_ms.max(sample.wall_ms);
    }
    rows.into_values().collect()
}

fn pct(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

/// Render the human-facing comparison report. Deterministic for a given
/// sample list: everything is sorted, nothing is timestamped here.
pub fn render_markdown(samples: &[Sample]) -> String {
    let mut out = String::new();
    out.push_str("# Preset benchmark — comparison report\n\n");
    if samples.is_empty() {
        out.push_str("No samples.\n");
        return out;
    }

    let modes: Vec<String> = {
        let mut m: Vec<String> = samples.iter().map(|s| s.mode.clone()).collect();
        m.sort();
        m.dedup();
        m
    };
    out.push_str(&format!(
        "Generated by `recursive-eval` from {} samples (mode: {}).\n\n",
        samples.len(),
        modes.join(", ")
    ));
    if modes.iter().any(|m| m == "replay") {
        out.push_str(
            "> Replay mode drives a deterministic scripted provider: tokens are the harness \
             estimate (fixed system-prompt cost is real; provider usage is not reported), \
             success/failure is real, latency is tool execution only. Compare quality in \
             live mode.\n\n",
        );
    }

    let stats = summarize(samples);
    out.push_str("## Totals by preset\n\n");
    out.push_str("| preset | sys prompt tok | runs | success | success rate | input tok | output tok | cache read tok | wall ms | turns | tool calls |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    for s in &stats {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            s.preset,
            s.system_prompt_tokens,
            s.runs,
            s.successes,
            pct(s.success_rate),
            s.input_tokens,
            s.output_tokens,
            s.cache_read_tokens,
            s.wall_ms,
            s.turns,
            s.tool_calls,
        ));
    }
    out.push('\n');

    let mut failures: Vec<(&String, &BTreeMap<String, usize>)> = stats
        .iter()
        .filter(|s| !s.failures.is_empty())
        .map(|s| (&s.preset, &s.failures))
        .collect();
    failures.sort();
    if !failures.is_empty() {
        out.push_str("### Failures\n\n");
        for (preset, classes) in failures {
            let rendered: Vec<String> = classes
                .iter()
                .map(|(class, count)| format!("{class} × {count}"))
                .collect();
            out.push_str(&format!("- **{preset}**: {}\n", rendered.join(", ")));
        }
        out.push('\n');
    }

    if stats.len() >= 2 {
        let low = &stats[0];
        let high = &stats[stats.len() - 1];
        out.push_str(&format!("## {} vs {}\n\n", low.preset, high.preset));
        out.push_str("| axis | minimal side | other side | delta |\n");
        out.push_str("|---|---|---|---|\n");
        out.push_str(&format!(
            "| system prompt tok (fixed cost) | {} | {} | {:.2}× |\n",
            low.system_prompt_tokens,
            high.system_prompt_tokens,
            ratio(
                high.system_prompt_tokens as u64,
                low.system_prompt_tokens as u64
            ),
        ));
        out.push_str(&format!(
            "| input tokens (total) | {} | {} | {:.2}× |\n",
            low.input_tokens,
            high.input_tokens,
            ratio(high.input_tokens, low.input_tokens),
        ));
        out.push_str(&format!(
            "| wall clock (total ms) | {} | {} | {:+} |\n",
            low.wall_ms,
            high.wall_ms,
            high.wall_ms as i64 - low.wall_ms as i64,
        ));
        out.push_str(&format!(
            "| success rate | {} | {} | {:+.1}pp |\n",
            pct(low.success_rate),
            pct(high.success_rate),
            (high.success_rate - low.success_rate) * 100.0,
        ));
        out.push('\n');
    }

    let spread: Vec<Spread> = task_spread(samples)
        .into_iter()
        .filter(|s| s.runs > 1)
        .collect();
    if !spread.is_empty() {
        out.push_str("## Repeat spread\n\n");
        out.push_str("| preset | task | runs | input tok min..max | wall ms min..max |\n");
        out.push_str("|---|---|---|---|---|\n");
        for s in &spread {
            out.push_str(&format!(
                "| {} | {} | {} | {}..{} | {}..{} |\n",
                s.preset,
                s.task_id,
                s.runs,
                s.min_input_tokens,
                s.max_input_tokens,
                s.min_wall_ms,
                s.max_wall_ms,
            ));
        }
        out.push('\n');
    }

    out.push_str("## By category\n\n");
    out.push_str("| category | preset | runs | success | input tok | wall ms |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for (category, s) in summarize_by_category(samples) {
        out.push_str(&format!(
            "| {} | {} | {} | {}/{} | {} | {} |\n",
            category, s.preset, s.runs, s.successes, s.runs, s.input_tokens, s.wall_ms,
        ));
    }
    out.push('\n');

    out.push_str("## Per task\n\n");
    out.push_str("| task | category | preset | success | finish | input tok | output tok | wall ms | turns | tool calls | failure |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    let mut ordered: Vec<&Sample> = samples.iter().collect();
    ordered.sort_by(|a, b| (&a.task_id, &a.preset).cmp(&(&b.task_id, &b.preset)));
    for s in ordered {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            s.task_id,
            s.category,
            s.preset,
            if s.success { "pass" } else { "FAIL" },
            s.finish_reason,
            s.input_tokens,
            s.output_tokens,
            s.wall_ms,
            s.turns,
            s.tool_calls,
            s.failure.as_deref().unwrap_or("-"),
        ));
    }
    out.push('\n');
    out
}

/// Serialize samples as newline-delimited JSON (the committed raw data).
pub fn to_jsonl(samples: &[Sample]) -> Result<String> {
    let mut out = String::new();
    for sample in samples {
        let line = serde_json::to_string(sample).map_err(|e| Error::Internal {
            context: "eval::to_jsonl".into(),
            message: e.to_string(),
        })?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

/// Parse newline-delimited JSON produced by [`to_jsonl`]. Blank lines are
/// skipped.
pub fn parse_jsonl(text: &str) -> Result<Vec<Sample>> {
    let mut samples = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let sample = serde_json::from_str(line).map_err(|e| Error::Internal {
            context: format!("eval::parse_jsonl line {}", i + 1),
            message: e.to_string(),
        })?;
        samples.push(sample);
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(workspace: &Path) -> Config {
        Config {
            workspace: workspace.to_path_buf(),
            system_prompt: crate::config::default_system_prompt(),
            max_steps: 32,
            shell_timeout_secs: 30,
            model: "eval-test-model".to_string(),
            ..crate::http::test_config_stub()
        }
    }

    fn presets() -> Vec<&'static AgentPreset> {
        vec![
            preset::find("minimal").expect("minimal"),
            preset::find("standard").expect("standard"),
        ]
    }

    #[test]
    fn task_set_covers_every_category_and_stays_in_the_first_version_window() {
        assert!(
            (8..=15).contains(&TASKS.len()),
            "the first task set is 8-15 tasks, got {}",
            TASKS.len()
        );
        for category in TaskCategory::all() {
            let count = TASKS.iter().filter(|t| t.category == category).count();
            assert!(
                (2..=3).contains(&count),
                "category {} has {count} tasks, expected 2-3",
                category.as_str()
            );
        }
        let mut ids: Vec<&str> = TASKS.iter().map(|t| t.id).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(before, ids.len(), "task ids must be unique");
        assert!(task("single-append-row").is_some());
        assert!(task("nope").is_none());
    }

    #[test]
    fn every_task_scripts_at_least_one_tool_and_ends_with_final_text() {
        for t in TASKS {
            assert!(!t.script.is_empty(), "{} has no script", t.id);
            let last = t.script.last().expect("non-empty");
            assert!(
                last.call.is_none(),
                "{} must end with a final answer, not a tool call",
                t.id
            );
            assert!(
                t.script.iter().any(|s| s.call.is_some()),
                "{} never calls a tool",
                t.id
            );
            // Replay scripts may only use tools the minimal preset ships.
            for step in t.script {
                if let Some(call) = step.call {
                    assert!(
                        crate::preset::MINIMAL_TOOLS.contains(&call.name),
                        "{} step calls `{}`, outside the core four",
                        t.id,
                        call.name
                    );
                }
            }
        }
    }

    #[test]
    fn seeders_write_the_files_the_task_needs() {
        for t in TASKS {
            let tmp = tempfile::tempdir().expect("tempdir");
            (t.seed)(tmp.path()).expect("seed");
            // Every check path is either seeded or created by the script, so
            // the seeder itself must not crash and must be re-runnable.
            (t.seed)(tmp.path()).expect("seed is idempotent");
        }
    }

    #[test]
    fn success_check_handles_every_expectation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("f.txt"), "hello world").expect("write");

        assert!(SuccessCheck {
            path: "f.txt",
            expect: Expect::Exists
        }
        .evaluate(tmp.path()));
        assert!(SuccessCheck {
            path: "missing",
            expect: Expect::Absent
        }
        .evaluate(tmp.path()));
        assert!(!SuccessCheck {
            path: "f.txt",
            expect: Expect::Absent
        }
        .evaluate(tmp.path()));
        assert!(SuccessCheck {
            path: "f.txt",
            expect: Expect::Contains("world")
        }
        .evaluate(tmp.path()));
        assert!(!SuccessCheck {
            path: "f.txt",
            expect: Expect::Contains("nope")
        }
        .evaluate(tmp.path()));
        assert!(SuccessCheck {
            path: "f.txt",
            expect: Expect::Equals("hello world")
        }
        .evaluate(tmp.path()));
        assert!(!SuccessCheck {
            path: "f.txt",
            expect: Expect::Equals("hello")
        }
        .evaluate(tmp.path()));
        assert!(!SuccessCheck {
            path: "f.txt",
            expect: Expect::Contains("x")
        }
        .evaluate(Path::new("/nonexistent-ws")));
    }

    #[test]
    fn failure_classification_buckets_each_finish_reason() {
        assert_eq!(classify_failure("budget_exceeded"), "budget_exceeded");
        assert_eq!(classify_failure("stuck:Bash:3"), "stuck");
        assert_eq!(classify_failure("transcript_limit:1/2"), "context_limit");
        assert_eq!(classify_failure("provider_stop:length"), "provider_error");
        assert_eq!(classify_failure("wall_clock_exceeded"), "wall_clock");
        assert_eq!(classify_failure("cancelled"), "cancelled");
        assert_eq!(
            classify_failure("permission_denial_limit"),
            "permission_denied"
        );
        assert_eq!(classify_failure("error"), "run_error");
        assert_eq!(classify_failure("no_more_tool_calls"), "wrong_result");
    }

    #[test]
    fn report_renders_ratios_latency_and_success_rate() {
        let samples = vec![
            Sample {
                task_id: "t1".into(),
                category: "single-file".into(),
                preset: "minimal".into(),
                system_prompt_tokens: 10,
                mode: "replay".into(),
                success: true,
                finish_reason: "no_more_tool_calls".into(),
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 0,
                wall_ms: 5,
                turns: 1,
                tool_calls: 1,
                failure: None,
            },
            Sample {
                task_id: "t1".into(),
                category: "single-file".into(),
                preset: "standard".into(),
                system_prompt_tokens: 100,
                mode: "replay".into(),
                success: false,
                finish_reason: "budget_exceeded".into(),
                input_tokens: 40,
                output_tokens: 2,
                cache_read_tokens: 0,
                wall_ms: 9,
                turns: 1,
                tool_calls: 1,
                failure: Some("budget_exceeded".into()),
            },
        ];
        let report = render_markdown(&samples);
        assert!(
            report.contains("| minimal | 10 | 1 | 1 | 100.0% |"),
            "{report}"
        );
        assert!(
            report.contains("| system prompt tok (fixed cost) | 10 | 100 | 10.00× |"),
            "{report}"
        );
        assert!(report.contains("4.00×"), "token ratio missing:\n{report}");
        assert!(
            report.contains("| wall clock (total ms) | 5 | 9 | +4 |"),
            "{report}"
        );
        assert!(
            report.contains("- **standard**: budget_exceeded × 1"),
            "{report}"
        );
        assert!(
            report.contains("| t1 | single-file | standard | FAIL |"),
            "{report}"
        );
    }

    #[test]
    fn repeat_runs_show_a_spread_interval() {
        let one = |wall: u64| Sample {
            task_id: "t".into(),
            category: "long-context".into(),
            preset: "standard".into(),
            system_prompt_tokens: 900,
            mode: "live".into(),
            success: true,
            finish_reason: "no_more_tool_calls".into(),
            input_tokens: 100,
            output_tokens: 0,
            cache_read_tokens: 0,
            wall_ms: wall,
            turns: 1,
            tool_calls: 0,
            failure: None,
        };
        let mut second = one(11);
        second.input_tokens = 130;
        let samples = vec![one(10), second];

        let spread = task_spread(&samples);
        assert_eq!(spread.len(), 1);
        assert_eq!(spread[0].runs, 2);
        assert_eq!(spread[0].min_input_tokens, 100);
        assert_eq!(spread[0].max_input_tokens, 130);
        assert_eq!(spread[0].min_wall_ms, 10);
        assert_eq!(spread[0].max_wall_ms, 11);

        let report = render_markdown(&samples);
        assert!(report.contains("## Repeat spread"), "{report}");
        assert!(
            report.contains("| standard | t | 2 | 100..130 | 10..11 |"),
            "{report}"
        );
        // A single run per (task, preset) carries no spread, so the section is
        // absent.
        assert!(!render_markdown(&samples[..1]).contains("## Repeat spread"));
    }

    #[test]
    fn empty_report_is_explicit() {
        assert!(render_markdown(&[]).contains("No samples."));
    }

    #[test]
    fn jsonl_round_trips() {
        let samples = vec![Sample {
            task_id: "t".into(),
            category: "multi-step".into(),
            preset: "minimal".into(),
            system_prompt_tokens: 12,
            mode: "replay".into(),
            success: true,
            finish_reason: "no_more_tool_calls".into(),
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            wall_ms: 4,
            turns: 5,
            tool_calls: 6,
            failure: None,
        }];
        let text = to_jsonl(&samples).expect("serialize");
        assert_eq!(text.lines().count(), 1);
        assert_eq!(parse_jsonl(&text).expect("parse"), samples);
        assert!(parse_jsonl("").expect("empty").is_empty());
    }

    #[test]
    fn summarize_aggregates_per_preset() {
        let mk = |preset: &str, success: bool, input: u32| Sample {
            task_id: "t".into(),
            category: "single-file".into(),
            preset: preset.into(),
            system_prompt_tokens: input,
            mode: "replay".into(),
            success,
            finish_reason: "no_more_tool_calls".into(),
            input_tokens: input,
            output_tokens: 0,
            cache_read_tokens: 0,
            wall_ms: 1,
            turns: 1,
            tool_calls: 0,
            failure: if success {
                None
            } else {
                Some("wrong_result".into())
            },
        };
        let samples = vec![
            mk("minimal", true, 5),
            mk("minimal", false, 5),
            mk("standard", true, 50),
        ];
        let stats = summarize(&samples);
        assert_eq!(stats.len(), 2);
        let minimal = stats
            .iter()
            .find(|s| s.preset == "minimal")
            .expect("minimal");
        assert_eq!(minimal.runs, 2);
        assert_eq!(minimal.successes, 1);
        assert!((minimal.success_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(minimal.input_tokens, 10);
        assert_eq!(minimal.failures.get("wrong_result"), Some(&1));
        let standard = stats
            .iter()
            .find(|s| s.preset == "standard")
            .expect("standard");
        assert_eq!(standard.input_tokens, 50);
    }

    /// The plumbing + fixed-cost measurement, end to end: every task runs
    /// under both presets with the scripted provider, and the standard
    /// preset's larger system prompt shows up as the token ratio.
    #[tokio::test]
    async fn replay_suite_runs_every_task_under_both_presets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_for(tmp.path());
        let presets = presets();
        let mut samples = Vec::new();
        for task in TASKS {
            let task_samples =
                run_task_across_presets(&config, task, &presets, Mode::Replay, tmp.path(), None)
                    .await;
            assert_eq!(task_samples.len(), 2);
            for sample in &task_samples {
                assert!(
                    sample.success,
                    "replay task `{}` failed under `{}`: {:?} ({})",
                    sample.task_id, sample.preset, sample.failure, sample.finish_reason
                );
                assert_eq!(sample.finish_reason, "no_more_tool_calls");
                assert!(sample.turns > 0, "{} made no LLM calls", sample.task_id);
            }
            samples.extend(task_samples);
        }

        let stats = summarize(&samples);
        let minimal = stats
            .iter()
            .find(|s| s.preset == "minimal")
            .expect("minimal");
        let standard = stats
            .iter()
            .find(|s| s.preset == "standard")
            .expect("standard");
        eprintln!(
            "replay suite — system prompt tok minimal: {}, standard: {} ({:.1}×); \
             total input tok minimal: {}, standard: {} ({:.2}×)",
            minimal.system_prompt_tokens,
            standard.system_prompt_tokens,
            ratio(
                standard.system_prompt_tokens as u64,
                minimal.system_prompt_tokens as u64
            ),
            minimal.input_tokens,
            standard.input_tokens,
            ratio(standard.input_tokens, minimal.input_tokens)
        );
        assert!(
            standard.system_prompt_tokens >= minimal.system_prompt_tokens * 10,
            "the whole point of the preset is the fixed cost: minimal={} standard={}",
            minimal.system_prompt_tokens,
            standard.system_prompt_tokens
        );
        assert!(
            standard.input_tokens > minimal.input_tokens,
            "the bigger prompt must cost more in total too: minimal={} standard={}",
            minimal.input_tokens,
            standard.input_tokens
        );

        let report = render_markdown(&samples);
        assert!(report.contains("## minimal vs standard"), "{report}");
        assert!(report.contains("| single-file | minimal |"), "{report}");
    }

    #[tokio::test]
    async fn a_failing_check_is_reported_as_wrong_result() {
        // A task whose script never creates the file its check expects.
        static BAD: EvalTask = EvalTask {
            id: "bad-task",
            category: TaskCategory::FailureRecovery,
            goal: "do nothing useful",
            seed: seed_empty,
            script: &[ScriptStep {
                content: "I did nothing.",
                call: None,
            }],
            check: SuccessCheck {
                path: "never.txt",
                expect: Expect::Exists,
            },
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_for(tmp.path());
        let sample = run_replay(
            &config,
            preset::find("standard").expect("standard"),
            &BAD,
            &tmp.path().join("bad"),
        )
        .await;
        assert!(!sample.success);
        assert_eq!(sample.finish_reason, "no_more_tool_calls");
        assert_eq!(sample.failure.as_deref(), Some("wrong_result"));
    }

    #[tokio::test]
    async fn workspace_is_reseeded_between_runs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_for(tmp.path());
        let task = task("single-append-row").expect("task");
        let preset = preset::find("minimal").expect("minimal");
        let ws = tmp.path().join("ws");
        let first = run_replay(&config, preset, task, &ws).await;
        assert!(first.success);
        std::fs::write(ws.join("junk.txt"), "left over").expect("write");
        let second = run_replay(&config, preset, task, &ws).await;
        assert!(second.success);
        assert!(!ws.join("junk.txt").exists(), "workspace must be re-seeded");
        assert_eq!(first.input_tokens, second.input_tokens, "replay is stable");
    }
}
