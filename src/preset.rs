//! Agent presets (issue #127): session-level declarative composition.
//!
//! A preset is the declared answer to "how is this session wired": which
//! prompt profile, which tool profile, which context management (compaction /
//! microcompaction / transcript cap) and which post-compaction re-injection.
//! Every frontend (HTTP, CLI, TUI) resolves the same [`ResolvedPreset`] for a
//! session and hands it to [`apply`] — the single assembly point — so the
//! channels cannot drift apart and adding a preset is one declaration plus
//! zero builder branches.
//!
//! # Precedence
//!
//! ```text
//! environment variable  >  preset declaration
//! ```
//!
//! The env vars stay the operator escape hatch — they are what the channels
//! read today, so the default behaviour of the `standard` preset is unchanged
//! — but the preset turns the scattered "the mechanism exists yet is off by
//! default" switches into an explicit inventory ([`Capability`]) that a
//! session's preset lists in full.
//!
//! # Session selection
//!
//! `POST /sessions`'s `preset` field (HTTP) / `RECURSIVE_AGENT_PRESET` (every
//! channel). An unknown id is an error, never a silent fallback.

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::compact::micro::Microcompactor;
use crate::compact::{Compactor, FileReinjector, SkillReinjector};
use crate::config::Config;
use crate::llm::{default_compact_threshold_chars, default_compact_threshold_tokens};
use crate::runtime::AgentRuntimeBuilder;
use crate::skills::Skill;
use crate::system_prompt::{AssembledPrompt, PromptSegments};
use crate::tools::fs::ReadFileState;

// ── env var names (declared once: the inventory and the resolver share them) ─

/// Selects the preset for a session when the channel has no explicit id.
pub const PRESET_ENV_VAR: &str = "RECURSIVE_AGENT_PRESET";
/// Cross-turn compaction char threshold (`0`/`off`/`false` disables).
pub const COMPACT_THRESHOLD_ENV: &str = "RECURSIVE_COMPACT_THRESHOLD";
/// Proactive tool-result pruning trigger (unset/`0`/`off`/`false` = off).
pub const MICROCOMPACT_TRIGGER_ENV: &str = "RECURSIVE_MICROCOMPACT_TRIGGER";
/// Keep-count for the microcompactor.
pub const MICROCOMPACT_KEEP_ENV: &str = "RECURSIVE_MICROCOMPACT_KEEP";
/// Hard transcript char cap (unset/`0`/`off`/`false` = unlimited).
pub const MAX_TRANSCRIPT_CHARS_ENV: &str = "RECURSIVE_MAX_TRANSCRIPT_CHARS";
/// Recently-read files re-injected after compaction (`0`/`off`/`false` = off).
pub const REINJECT_FILES_ENV: &str = "RECURSIVE_REINJECT_FILES";
/// Token budget for the file re-injection.
pub const REINJECT_FILE_BUDGET_ENV: &str = "RECURSIVE_REINJECT_FILE_BUDGET";
/// Invoked skills re-injected after compaction (`0`/`off`/`false` = off).
pub const REINJECT_SKILLS_ENV: &str = "RECURSIVE_REINJECT_SKILLS";
/// Token budget for the skill re-injection.
pub const REINJECT_SKILL_BUDGET_ENV: &str = "RECURSIVE_REINJECT_SKILL_BUDGET";
/// Programmatic tool calling, `run_code` (issue #134). `0`/`off`/`false` (or
/// an empty value) = off, any other value = on, unset takes the preset
/// declaration.
pub const RUN_CODE_ENV: &str = "RECURSIVE_RUN_CODE";

/// File re-injection defaults (mirrors [`FileReinjector`]'s).
pub const DEFAULT_REINJECT_FILES: usize = 5;
pub const DEFAULT_REINJECT_FILE_BUDGET: usize = 50_000;
pub const DEFAULT_PER_FILE_BUDGET: usize = 5_000;
/// Skill re-injection defaults (mirrors [`SkillReinjector`]'s).
pub const DEFAULT_REINJECT_SKILL_BUDGET: usize = 25_000;
pub const DEFAULT_PER_SKILL_BUDGET: usize = 5_000;

/// The `minimal` preset's whole system prompt — one line, and that is the
/// point (issue #128). Nothing is assembled around it.
pub const MINIMAL_PROMPT: &str = "You are a helpful software engineer assistant.";

/// The `minimal` preset's entire tool surface: read, write, edit, shell —
/// names as the model sees them. Navigation and search are the shell's job.
pub static MINIMAL_TOOLS: &[&str] = &["Read", "Write", "Edit", "Bash"];

// ── declaration ───────────────────────────────────────────────────────────

/// Prompt profile: what the session's system prompt is composed of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PromptProfile {
    /// When `Some`, this text IS the session's whole system prompt (the
    /// DeepSeek-Harness `complete: true` form): the assembled base, the
    /// project context, the sub-agent note, the skill catalog and the
    /// `<environment>` segment are all discarded — whatever the channel
    /// assembled, and whatever the caller asked for in the request body.
    ///
    /// `None` = the channel's assembled prompt is used, with
    /// [`Self::persona_suffix`] appended.
    pub complete: Option<&'static str>,
    /// Appended to the assembled base prompt. `None` = the channel's own
    /// prompt is used verbatim. Ignored when [`Self::complete`] is set.
    pub persona_suffix: Option<&'static str>,
    /// Auto-load the full bodies of the skills matching the session goal (the
    /// CLI's goal-based injection). Channels with no goal at prompt-build time
    /// cannot honour it.
    pub auto_skill_injection: bool,
    /// Ship the skill catalog as the per-turn `<system-reminder>`. `false`
    /// leaves the session with no skill surface at all.
    pub skill_catalog: bool,
}

/// Tool profile: which tools the preset asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToolProfile {
    /// Register `enter_plan_mode` / `exit_plan_mode` / `request_plan_mode`.
    /// These block on a live human, so the channel must also report itself
    /// interactive ([`ChannelSupport`]) for them to be registered at all.
    pub plan_mode_tools: bool,
    /// `Some(names)` = prune the session's registry to exactly these tool
    /// names (`ToolRegistry::retain_tools`; case-insensitive). `None` = the
    /// channel's full surface. Names are the LLM-facing tool names
    /// (`Read` / `Write` / …), which are also the registry keys.
    pub allow: Option<&'static [&'static str]>,
    /// Register `run_code` (issue #134): a program that calls tools as async
    /// functions in one step. Off by default — it executes model-authored code
    /// in a subprocess, which an operator opts into; see `RECURSIVE_RUN_CODE`.
    pub run_code: bool,
}

/// How the cross-turn compactor's threshold is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CompactionMode {
    /// From the model's context window (80 % of the effective window).
    Auto,
    /// Explicit character count.
    Chars(usize),
}

/// Proactive tool-result pruning (no LLM call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MicrocompactionSpec {
    pub trigger_tool_count: usize,
    pub keep_recent: usize,
}

/// Recently-read files re-injected after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FileReinjectSpec {
    pub max_files: usize,
    pub token_budget: usize,
    pub per_file_budget: usize,
}

/// Invoked skills re-injected after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SkillReinjectSpec {
    pub token_budget: usize,
    pub per_skill_budget: usize,
}

/// Context management a preset declares. `None` means "capability off".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContextProfile {
    pub compaction: Option<CompactionMode>,
    pub microcompaction: Option<MicrocompactionSpec>,
    pub max_transcript_chars: Option<usize>,
    pub reinject_recent_files: Option<FileReinjectSpec>,
    pub reinject_invoked_skills: Option<SkillReinjectSpec>,
}

/// Whether a declared capability is on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityDefault {
    Enabled,
    Disabled,
}

/// One row of a preset's capability inventory.
///
/// The point of the inventory is discoverability: a capability that exists but
/// is off by default must be listed here with the switch that turns it on
/// (the DeepSeek-Harness `disabled: true` row semantics), instead of only
/// being visible to whoever reads the assembly code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capability {
    pub name: &'static str,
    pub default: CapabilityDefault,
    /// Env var (or config key) that toggles it; `""` when the channel alone
    /// decides.
    pub toggle: &'static str,
    pub note: &'static str,
}

/// A session preset: data only. Every frontend resolves it the same way and
/// never branches on the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentPreset {
    pub id: &'static str,
    pub description: &'static str,
    pub prompt: PromptProfile,
    pub tools: ToolProfile,
    pub context: ContextProfile,
    pub capabilities: &'static [Capability],
}

impl AgentPreset {
    /// Resolve this declaration into the concrete numbers a session runs with:
    /// the declaration with the environment overlaid.
    ///
    /// Pure — the env is passed in as a [`PresetEnv`] snapshot, so the result
    /// is testable without touching process-global state.
    pub fn resolve(&self, config: &Config, env: &PresetEnv) -> ResolvedPreset {
        ResolvedPreset {
            id: self.id.to_string(),
            prompt: self.prompt,
            tools: self.resolve_tools(env),
            context: self.resolve_context(&config.model, env),
        }
    }

    /// The tool profile with the environment overlaid. `RECURSIVE_RUN_CODE`
    /// is the operator escape hatch for the one capability here that executes
    /// model-authored code: unset takes the declaration, `0`/`off`/`false`
    /// forces it off, anything else forces it on.
    ///
    /// A set-but-empty value (`RECURSIVE_RUN_CODE=` in a Dockerfile / compose
    /// file) is OFF, matching [`parse_toggle`]'s tolerance everywhere else in
    /// this file — the alternative silently opts a deployment into host code
    /// execution.
    fn resolve_tools(&self, env: &PresetEnv) -> ToolProfile {
        let run_code = match env.run_code.as_deref().map(str::trim) {
            None => self.tools.run_code,
            Some("") | Some("0") | Some("off") | Some("false") => false,
            Some(_) => true,
        };
        ToolProfile {
            run_code,
            ..self.tools
        }
    }

    fn resolve_context(&self, model: &str, env: &PresetEnv) -> ContextFacts {
        let decl = self.context;

        // Compaction: env `0`/`off`/`false` (or anything unparsable) disables;
        // a positive integer is an explicit char threshold; unset takes the
        // declaration. The token threshold is always derived from the model —
        // it is the more reliable trigger for CJK content.
        let compaction = match parse_toggle(env.compact_threshold.as_deref()) {
            Toggle::Unset => decl.compaction,
            Toggle::Disabled => None,
            Toggle::Value(n) => Some(CompactionMode::Chars(n)),
        }
        .map(|mode| {
            let threshold_chars = match mode {
                CompactionMode::Auto => default_compact_threshold_chars(model),
                CompactionMode::Chars(n) => n,
            };
            // Borrow the default keep-count from the compactor itself so the
            // two cannot drift.
            let template = Compactor::new(threshold_chars);
            CompactionFacts {
                threshold_chars,
                threshold_prompt_tokens: Some(default_compact_threshold_tokens(model)),
                keep_recent_n: template.keep_recent_n,
            }
        });

        let microcompaction = match parse_toggle(env.microcompact_trigger.as_deref()) {
            // Unset: the declaration is already the resolved shape.
            Toggle::Unset => decl.microcompaction,
            Toggle::Disabled => None,
            // Env-derived: reuse the historical parser so the keep-count
            // default and the `<= 0` handling stay in one place.
            Toggle::Value(_) => crate::compact::micro::build_microcompactor_from_env(
                env.microcompact_trigger.as_deref(),
                env.microcompact_keep.as_deref(),
            )
            .map(MicrocompactionSpec::facts),
        };

        let max_transcript_chars = match parse_toggle(env.max_transcript_chars.as_deref()) {
            Toggle::Unset => decl.max_transcript_chars,
            Toggle::Disabled => None,
            Toggle::Value(n) => Some(n),
        };

        // Re-injection: an explicit env value is parsed by the same helpers
        // the pre-preset channels used (so its tolerance rules stay in one
        // place); unset takes the declaration, with the budget env still able
        // to override the declared budget.
        let reinject_recent_files = match env.reinject_files.as_deref() {
            None => decl.reinject_recent_files.map(|spec| FileReinjectSpec {
                token_budget: parse_positive(env.reinject_file_budget.as_deref())
                    .unwrap_or(spec.token_budget),
                ..spec
            }),
            Some(_) => crate::compact::reinject::file_reinjector_spec_from_env(
                env.reinject_files.as_deref(),
                env.reinject_file_budget.as_deref(),
            ),
        };

        let reinject_invoked_skills = match env.reinject_skills.as_deref() {
            None => decl.reinject_invoked_skills.map(|spec| SkillReinjectSpec {
                token_budget: parse_positive(env.reinject_skill_budget.as_deref())
                    .unwrap_or(spec.token_budget),
                ..spec
            }),
            Some(_) => crate::compact::reinject::skill_reinjector_spec_from_env(
                env.reinject_skills.as_deref(),
                env.reinject_skill_budget.as_deref(),
            ),
        };

        ContextFacts {
            compaction,
            microcompaction,
            max_transcript_chars,
            reinject_recent_files,
            reinject_invoked_skills,
        }
    }
}

impl MicrocompactionSpec {
    fn facts(mc: Microcompactor) -> Self {
        Self {
            trigger_tool_count: mc.trigger_tool_count,
            keep_recent: mc.keep_recent,
        }
    }
}

// ── resolved form ─────────────────────────────────────────────────────────

/// Concrete compactor settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompactionFacts {
    pub threshold_chars: usize,
    pub threshold_prompt_tokens: Option<u32>,
    pub keep_recent_n: usize,
}

/// The context management a session actually runs with. Produced by
/// [`AgentPreset::resolve`] and read back from an assembled builder / runtime
/// via `context_management_facts`, so a cross-channel parity test compares
/// like with like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct ContextFacts {
    pub compaction: Option<CompactionFacts>,
    pub microcompaction: Option<MicrocompactionSpec>,
    pub max_transcript_chars: Option<usize>,
    pub reinject_recent_files: Option<FileReinjectSpec>,
    pub reinject_invoked_skills: Option<SkillReinjectSpec>,
}

/// A preset declaration with the environment overlaid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPreset {
    pub id: String,
    pub prompt: PromptProfile,
    pub tools: ToolProfile,
    pub context: ContextFacts,
}

/// An unknown preset id, with the ids that do exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPreset {
    pub id: String,
    pub known: Vec<&'static str>,
}

impl std::fmt::Display for UnknownPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown agent preset {:?} (known: {})",
            self.id,
            self.known.join(", ")
        )
    }
}

impl std::error::Error for UnknownPreset {}

// ── env snapshot ──────────────────────────────────────────────────────────

/// Snapshot of the preset-related environment.
///
/// Values are kept raw: parsing (and its tolerance rules) belongs to
/// [`AgentPreset::resolve`], and an unparsable value must behave exactly as
/// the channel read it before presets existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetEnv {
    pub agent_preset: Option<String>,
    pub compact_threshold: Option<String>,
    pub microcompact_trigger: Option<String>,
    pub microcompact_keep: Option<String>,
    pub max_transcript_chars: Option<String>,
    pub reinject_files: Option<String>,
    pub reinject_file_budget: Option<String>,
    pub reinject_skills: Option<String>,
    pub reinject_skill_budget: Option<String>,
    /// `RECURSIVE_RUN_CODE`: overrides the preset's `run_code` declaration.
    pub run_code: Option<String>,
}

impl PresetEnv {
    /// Read the process environment.
    pub fn from_process() -> Self {
        Self {
            agent_preset: var(PRESET_ENV_VAR),
            compact_threshold: var(COMPACT_THRESHOLD_ENV),
            microcompact_trigger: var(MICROCOMPACT_TRIGGER_ENV),
            microcompact_keep: var(MICROCOMPACT_KEEP_ENV),
            max_transcript_chars: var(MAX_TRANSCRIPT_CHARS_ENV),
            reinject_files: var(REINJECT_FILES_ENV),
            reinject_file_budget: var(REINJECT_FILE_BUDGET_ENV),
            reinject_skills: var(REINJECT_SKILLS_ENV),
            reinject_skill_budget: var(REINJECT_SKILL_BUDGET_ENV),
            run_code: var(RUN_CODE_ENV),
        }
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `on` / `off` / value tri-state used by every "count or switch" env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Toggle {
    /// Not set — the declaration decides.
    Unset,
    /// Explicitly off, or unparsable (the historical tolerance).
    Disabled,
    Value(usize),
}

fn parse_toggle(raw: Option<&str>) -> Toggle {
    match raw {
        None => Toggle::Unset,
        Some("0") | Some("off") | Some("false") => Toggle::Disabled,
        Some(s) => match parse_positive(Some(s)) {
            Some(n) => Toggle::Value(n),
            None => Toggle::Disabled,
        },
    }
}

fn parse_positive(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|s| s.parse::<usize>().ok()).filter(|&n| n > 0)
}

// ── registry ──────────────────────────────────────────────────────────────

/// Capability inventory of the `standard` preset.
static STANDARD_CAPABILITIES: &[Capability] = &[
    Capability {
        name: "cross-turn-compaction",
        default: CapabilityDefault::Enabled,
        toggle: COMPACT_THRESHOLD_ENV,
        note: "LLM-summary compaction when the transcript outgrows the model's window; \
               the threshold auto-derives from the model unless the env var pins it.",
    },
    Capability {
        name: "proactive-tool-result-pruning",
        default: CapabilityDefault::Disabled,
        toggle: MICROCOMPACT_TRIGGER_ENV,
        note: "no-LLM microcompaction; opt-in because the old default fired at ~5% of a \
               1M-token window.",
    },
    Capability {
        name: "transcript-char-cap",
        default: CapabilityDefault::Disabled,
        toggle: MAX_TRANSCRIPT_CHARS_ENV,
        note: "pre-turn hard trim of the transcript; unset means unlimited.",
    },
    Capability {
        name: "recently-read-file-reinjection",
        default: CapabilityDefault::Enabled,
        toggle: REINJECT_FILES_ENV,
        note: "re-attach the last N read files as System messages after compaction, so \
               the model does not pay to re-read them.",
    },
    Capability {
        name: "invoked-skill-reinjection",
        default: CapabilityDefault::Enabled,
        toggle: REINJECT_SKILLS_ENV,
        note: "re-attach invoked skill bodies after compaction, so the model keeps \
               following them.",
    },
    Capability {
        name: "plan-mode-tools",
        default: CapabilityDefault::Enabled,
        toggle: "",
        note: "enter_plan_mode / exit_plan_mode / request_plan_mode; also requires an \
               interactive channel to answer the approval prompt.",
    },
    Capability {
        name: "programmatic-tool-calling",
        default: CapabilityDefault::Disabled,
        toggle: RUN_CODE_ENV,
        note: "`run_code`: the model writes a program that calls tools as async functions, \
               so a batch of ≥5 calls plus its aggregation costs one step instead of many. \
               Opt-in because it executes model-authored code in a subprocess.",
    },
    Capability {
        name: "goal-based-skill-injection",
        default: CapabilityDefault::Enabled,
        toggle: "",
        note: "auto-load skill bodies matching the session goal; requires a goal at \
               prompt-build time (CLI run, not HTTP sessions).",
    },
    Capability {
        name: "subagent-delegation",
        default: CapabilityDefault::Disabled,
        toggle: "RECURSIVE_SUBAGENT_ENABLED",
        note: "the `agent` tool plus the coordinator prompt; channel-agnostic, switched \
               on when the operator enables it.",
    },
    Capability {
        name: "self-scheduling-wakeup",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "ScheduleWakeup for loop / TUI sessions; registered by channels that own a \
               wakeup slot.",
    },
    Capability {
        name: "skill-install-tool",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "InstallSkill; TUI-only because it prompts through the UI channel.",
    },
    Capability {
        name: "mcp-tools",
        default: CapabilityDefault::Disabled,
        toggle: "RECURSIVE_MCP_CONFIG",
        note: "mcp__* tools; added to the process-wide registry at startup.",
    },
];

/// The built-in `standard` preset: the behaviour every channel had before
/// presets existed, stated once.
pub static STANDARD: AgentPreset = AgentPreset {
    id: "standard",
    description: "General coding session: auto compaction, post-compaction re-injection \
                  of recently-read files and invoked skills, plan-mode tools.",
    prompt: PromptProfile {
        complete: None,
        persona_suffix: None,
        auto_skill_injection: true,
        skill_catalog: true,
    },
    tools: ToolProfile {
        plan_mode_tools: true,
        allow: None,
        run_code: false,
    },
    context: ContextProfile {
        compaction: Some(CompactionMode::Auto),
        microcompaction: None,
        max_transcript_chars: None,
        reinject_recent_files: Some(FileReinjectSpec {
            max_files: DEFAULT_REINJECT_FILES,
            token_budget: DEFAULT_REINJECT_FILE_BUDGET,
            per_file_budget: DEFAULT_PER_FILE_BUDGET,
        }),
        reinject_invoked_skills: Some(SkillReinjectSpec {
            token_budget: DEFAULT_REINJECT_SKILL_BUDGET,
            per_skill_budget: DEFAULT_PER_SKILL_BUDGET,
        }),
    },
    capabilities: STANDARD_CAPABILITIES,
};

/// Capability inventory of the `minimal` preset (issue #128).
///
/// Most rows are *off*: the point of this preset is that the session pays for
/// nothing it does not need, so what is absent has to be discoverable here
/// rather than inferred from the assembly code.
static MINIMAL_CAPABILITIES: &[Capability] = &[
    Capability {
        name: "one-line-persona",
        default: CapabilityDefault::Enabled,
        toggle: "",
        note: "the whole system prompt; the assembled base, project context, memory \
               layers, sub-agent note and environment segment are discarded.",
    },
    Capability {
        name: "core-tool-subset",
        default: CapabilityDefault::Enabled,
        toggle: "",
        note: "Read / Write / Edit / Bash only — the registry is pruned to exactly this \
               set before the runtime is built.",
    },
    Capability {
        name: "memory-layers",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "user / project / summary / scratchpad / facts / episodic layers are baked \
               into `Config::system_prompt` before any preset runs; a `complete` prompt \
               profile replaces that base wholesale, so none of them is sent.",
    },
    Capability {
        name: "project-context",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "AGENTS.md / CLAUDE.md are not read into the prompt.",
    },
    Capability {
        name: "environment-segment",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "a channel still renders the transport's `<environment>` segment; the \
               complete prompt profile drops it.",
    },
    Capability {
        name: "goal-based-skill-injection",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "no skill bodies are auto-loaded from the session goal.",
    },
    Capability {
        name: "skill-catalog-reminder",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "no per-turn `<system-reminder>` skill catalog, and no `LoadSkill` tool.",
    },
    Capability {
        name: "cross-turn-compaction",
        default: CapabilityDefault::Disabled,
        toggle: COMPACT_THRESHOLD_ENV,
        note: "the declaration has none; the env var still wins and can pin an explicit \
               threshold (env > declaration, see the module docs).",
    },
    Capability {
        name: "recently-read-file-reinjection",
        default: CapabilityDefault::Disabled,
        toggle: REINJECT_FILES_ENV,
        note: "nothing is re-attached after compaction — there is no compaction.",
    },
    Capability {
        name: "invoked-skill-reinjection",
        default: CapabilityDefault::Disabled,
        toggle: REINJECT_SKILLS_ENV,
        note: "same: no invoked skills, no re-injection.",
    },
    Capability {
        name: "plan-mode-tools",
        default: CapabilityDefault::Disabled,
        toggle: "",
        note: "the plan tools block on a live human and are outside the core subset.",
    },
    Capability {
        name: "subagent-delegation",
        default: CapabilityDefault::Disabled,
        toggle: "RECURSIVE_SUBAGENT_ENABLED",
        note: "the `agent` tool is pruned by the core tool subset even when the operator \
               enables sub-agents process-wide.",
    },
];

/// The built-in `minimal` preset (issue #128): one-line prompt, four tools,
/// no context management. Borrowed from the DeepSeek-Harness `minimal` preset
/// (`packages/bundle/web-app/presets/minimal.patch.yml`), whose stated purpose
/// is "the agent completes tasks with terminal tools only — suitable for
/// testing and comparing its baseline behaviour". It is also the cheapest
/// request this runtime can make, and therefore the floor a benchmark compares
/// `standard` against.
pub static MINIMAL: AgentPreset = AgentPreset {
    id: "minimal",
    description: "Minimal session: a one-line system prompt, Read/Write/Edit/Bash only, \
                  no memory / skill / project-context / environment injection and no \
                  compaction or re-injection. For simple tasks and for measuring a \
                  model's baseline.",
    prompt: PromptProfile {
        complete: Some(MINIMAL_PROMPT),
        persona_suffix: None,
        auto_skill_injection: false,
        skill_catalog: false,
    },
    tools: ToolProfile {
        plan_mode_tools: false,
        allow: Some(MINIMAL_TOOLS),
    },
    context: ContextProfile {
        compaction: None,
        microcompaction: None,
        max_transcript_chars: None,
        reinject_recent_files: None,
        reinject_invoked_skills: None,
    },
    capabilities: MINIMAL_CAPABILITIES,
};

/// Every built-in preset. Adding one is a declaration plus a line here — no
/// builder branch anywhere (issue #127 acceptance 3).
static BUILTIN: [&AgentPreset; 2] = [&STANDARD, &MINIMAL];

/// Every built-in preset.
pub fn builtin() -> &'static [&'static AgentPreset] {
    &BUILTIN
}

/// Look up a built-in preset by id.
pub fn find(id: &str) -> Option<&'static AgentPreset> {
    builtin().iter().copied().find(|p| p.id == id)
}

/// The ids of every built-in preset.
pub fn builtin_ids() -> Vec<&'static str> {
    builtin().iter().map(|p| p.id).collect()
}

/// Pick the preset for a session: explicit id, else `RECURSIVE_AGENT_PRESET`,
/// else `standard`.
pub fn select(
    explicit: Option<&str>,
    env: &PresetEnv,
) -> Result<&'static AgentPreset, UnknownPreset> {
    let wanted = explicit.or(env.agent_preset.as_deref());
    let Some(id) = wanted else {
        return Ok(&STANDARD);
    };
    find(id).ok_or_else(|| UnknownPreset {
        id: id.to_string(),
        known: builtin_ids(),
    })
}

/// Resolve the preset a session runs with. This is the entry every frontend
/// uses; it exists so "which preset is this?" is answered in one place.
pub fn resolve_session(
    explicit: Option<&str>,
    config: &Config,
    env: &PresetEnv,
) -> Result<ResolvedPreset, UnknownPreset> {
    Ok(select(explicit, env)?.resolve(config, env))
}

// ── assembly ──────────────────────────────────────────────────────────────

/// What the calling channel can host.
///
/// A preset declares a capability; the channel reports whether it can actually
/// support it. Plan-mode tools block until a human answers the approval
/// prompt, so a headless channel must never get them even though the preset
/// asks for them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelSupport {
    pub interactive: bool,
}

/// Side objects the reinjectors need. Missing pieces simply disable the
/// corresponding re-injection (a channel with no shared read state has nothing
/// to re-inject).
#[derive(Default, Clone)]
pub struct PresetAssets {
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
    pub skills: Vec<Skill>,
}

impl PresetAssets {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_read_state(mut self, read_state: Arc<Mutex<ReadFileState>>) -> Self {
        self.read_state = Some(read_state);
        self
    }

    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.skills = skills;
        self
    }
}

/// Build a [`PresetAssets`] from a tool registry (read state) and a skill
/// catalog. Every frontend goes through this so the assets are gathered the
/// same way.
pub fn assets_from_registry(
    registry: &crate::tools::ToolRegistry,
    skills: Vec<Skill>,
) -> PresetAssets {
    let assets = PresetAssets::new().with_skills(skills);
    match registry.read_file_state() {
        Some(state) => assets.with_read_state(state),
        None => assets,
    }
}

/// The single assembly point (issue #127): install everything the resolved
/// preset declares onto `builder`. Called once per builder chain, so "off" is
/// expressed by not installing anything.
pub fn apply(
    mut builder: AgentRuntimeBuilder,
    preset: &ResolvedPreset,
    assets: &PresetAssets,
    channel: ChannelSupport,
) -> AgentRuntimeBuilder {
    if let Some(c) = preset.context.compaction {
        let mut compactor = Compactor::new(c.threshold_chars).keep_recent_n(c.keep_recent_n);
        if let Some(tokens) = c.threshold_prompt_tokens {
            compactor = compactor.threshold_prompt_tokens(tokens);
        }
        builder = builder.compactor(compactor);
    }

    if let Some(mc) = preset.context.microcompaction {
        builder =
            builder.microcompactor(Microcompactor::new(mc.trigger_tool_count, mc.keep_recent));
    }

    if let Some(n) = preset.context.max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }

    if let (Some(spec), Some(read_state)) = (
        preset.context.reinject_recent_files,
        assets.read_state.clone(),
    ) {
        builder = builder.file_reinjector(FileReinjector {
            max_files: spec.max_files,
            token_budget: spec.token_budget,
            per_file_budget: spec.per_file_budget,
            read_state,
        });
    }

    if let Some(spec) = preset.context.reinject_invoked_skills {
        // Installed even with an empty catalog (it then has nothing to look
        // up) — the channels did exactly that before presets, and keeping the
        // assembly a pure function of (preset, assets) is what lets a parity
        // test compare `context_management_facts()` against the declaration.
        builder = builder.skill_reinjector(SkillReinjector {
            token_budget: spec.token_budget,
            per_skill_budget: spec.per_skill_budget,
            skills: assets.skills.clone(),
        });
    }

    // Issue #128: a declared tool subset prunes the registry (in `build()`,
    // before the runtime re-registers its sinked placeholders), so "the
    // preset decides the surface" holds for every channel that assembles here
    // rather than for whichever channel remembered to filter.
    if let Some(allow) = preset.tools.allow {
        builder = builder.with_tool_allow(allow.iter().map(|name| (*name).to_string()).collect());
    }

    // Same for the skill catalog: it is the kernel's per-turn
    // `<system-reminder>`, so a preset that declares no skill surface must
    // clear it here — after the channel installed it.
    if !preset.prompt.skill_catalog {
        builder = builder.skills(Vec::new());
    }

    builder
        .with_plan_mode_tools(preset.tools.plan_mode_tools && channel.interactive)
        .with_run_code(preset.tools.run_code)
        .with_preset_id(preset.id.clone())
}

/// Apply the preset's prompt profile to an already-assembled system prompt.
///
/// A `complete` profile replaces the prompt *and* its segment breakdown, so
/// the local context-breakdown estimator sizes what the request really
/// carries rather than what the channel assembled on the way there.
pub fn apply_prompt(assembled: AssembledPrompt, preset: &ResolvedPreset) -> AssembledPrompt {
    if let Some(complete) = preset.prompt.complete {
        return AssembledPrompt {
            full: complete.to_string(),
            segments: PromptSegments {
                system_prompt: complete.to_string(),
                ..PromptSegments::default()
            },
        };
    }
    match preset.prompt.persona_suffix {
        Some(suffix) => AssembledPrompt {
            full: format!("{}\n{suffix}", assembled.full),
            ..assembled
        },
        None => assembled,
    }
}

/// The system prompt a session created under `preset` would send on its first
/// request, for the server's default base and no request-scoped override
/// (issue #128 item 3: every preset's fixed per-request cost must be
/// observable, not folklore).
///
/// The `<environment>` segment is transport-specific and therefore never part
/// of this preview; a channel that injects one measures its own.
pub fn preview_system_prompt(
    config: &Config,
    preset: &ResolvedPreset,
    skills: &[Skill],
) -> AssembledPrompt {
    apply_prompt(
        crate::assemble_system_prompt(
            &config.system_prompt,
            &config.workspace,
            skills,
            config.subagent_enabled,
        ),
        preset,
    )
}

/// Estimated token weight of [`preview_system_prompt`] — the per-request
/// system cost of one session under `preset`, in the same `bytes/4` estimator
/// the context breakdown uses. Only relative comparisons are meaningful.
pub fn system_prompt_tokens(config: &Config, preset: &ResolvedPreset, skills: &[Skill]) -> u32 {
    crate::llm::estimate_tokens(preview_system_prompt(config, preset, skills).full())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockProvider;

    fn config_for(model: &str) -> Config {
        Config {
            model: model.to_string(),
            ..crate::http::test_config_stub()
        }
    }

    fn fresh_builder() -> AgentRuntimeBuilder {
        AgentRuntimeBuilder::new().llm(Arc::new(MockProvider::new(vec![])))
    }

    /// An assembled prompt whose segments claim exactly the text in `full`.
    fn assembled(full: &str) -> AssembledPrompt {
        AssembledPrompt {
            full: full.to_string(),
            segments: PromptSegments {
                system_prompt: full.to_string(),
                ..PromptSegments::default()
            },
        }
    }

    // ── declaration / resolution ──────────────────────────────────────────

    #[test]
    fn standard_preset_declares_compaction_and_reinjection() {
        let config = config_for("preset-test-model");
        let resolved = STANDARD.resolve(&config, &PresetEnv::default());

        let compaction = resolved.context.compaction.expect("compaction on");
        assert_eq!(
            compaction.threshold_chars,
            default_compact_threshold_chars("preset-test-model")
        );
        assert_eq!(
            compaction.threshold_prompt_tokens,
            Some(default_compact_threshold_tokens("preset-test-model"))
        );
        assert!(compaction.keep_recent_n > 0);
        assert_eq!(
            resolved.context.reinject_recent_files,
            Some(FileReinjectSpec {
                max_files: DEFAULT_REINJECT_FILES,
                token_budget: DEFAULT_REINJECT_FILE_BUDGET,
                per_file_budget: DEFAULT_PER_FILE_BUDGET,
            })
        );
        assert_eq!(
            resolved.context.reinject_invoked_skills,
            Some(SkillReinjectSpec {
                token_budget: DEFAULT_REINJECT_SKILL_BUDGET,
                per_skill_budget: DEFAULT_PER_SKILL_BUDGET,
            })
        );
        // Disabled-by-default stays disabled until something turns it on.
        assert!(resolved.context.microcompaction.is_none());
        assert!(resolved.context.max_transcript_chars.is_none());
    }

    /// The declaration's reinjection defaults must stay equal to the
    /// constructors' defaults, or the preset and the reinjectors disagree.
    #[test]
    fn declaration_defaults_match_the_reinjector_constructors() {
        let skills = SkillReinjector::new(Vec::new());
        assert_eq!(skills.token_budget, DEFAULT_REINJECT_SKILL_BUDGET);
        assert_eq!(skills.per_skill_budget, DEFAULT_PER_SKILL_BUDGET);

        let read_state: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let files = FileReinjector::new(read_state);
        assert_eq!(files.max_files, DEFAULT_REINJECT_FILES);
        assert_eq!(files.token_budget, DEFAULT_REINJECT_FILE_BUDGET);
        assert_eq!(files.per_file_budget, DEFAULT_PER_FILE_BUDGET);
    }

    #[test]
    fn env_overrides_every_declared_context_knob() {
        let config = config_for("preset-test-model");

        // Compaction off.
        for raw in ["0", "off", "false", "not-a-number"] {
            let env = PresetEnv {
                compact_threshold: Some(raw.to_string()),
                ..PresetEnv::default()
            };
            assert!(
                STANDARD.resolve(&config, &env).context.compaction.is_none(),
                "{raw:?} must disable compaction"
            );
        }

        // Explicit compaction threshold.
        let env = PresetEnv {
            compact_threshold: Some("4242".to_string()),
            ..PresetEnv::default()
        };
        let resolved = STANDARD.resolve(&config, &env);
        let compaction = resolved.context.compaction.expect("compactor");
        assert_eq!(compaction.threshold_chars, 4242);
        assert_eq!(
            compaction.threshold_prompt_tokens,
            Some(default_compact_threshold_tokens("preset-test-model")),
            "the model-derived token trigger survives an explicit char threshold"
        );

        // Microcompaction is opt-in, and env is the opt-in.
        let env = PresetEnv {
            microcompact_trigger: Some("40".to_string()),
            microcompact_keep: Some("6".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(
            STANDARD.resolve(&config, &env).context.microcompaction,
            Some(MicrocompactionSpec {
                trigger_tool_count: 40,
                keep_recent: 6,
            })
        );

        // Transcript cap.
        let env = PresetEnv {
            max_transcript_chars: Some("999".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(
            STANDARD.resolve(&config, &env).context.max_transcript_chars,
            Some(999)
        );

        // Reinjection off / budget override.
        let env = PresetEnv {
            reinject_files: Some("off".to_string()),
            reinject_skills: Some("0".to_string()),
            ..PresetEnv::default()
        };
        let resolved = STANDARD.resolve(&config, &env);
        assert!(resolved.context.reinject_recent_files.is_none());
        assert!(resolved.context.reinject_invoked_skills.is_none());

        let env = PresetEnv {
            reinject_file_budget: Some("1234".to_string()),
            reinject_skill_budget: Some("5678".to_string()),
            ..PresetEnv::default()
        };
        let resolved = STANDARD.resolve(&config, &env);
        let files = resolved.context.reinject_recent_files.expect("files on");
        assert_eq!(files.token_budget, 1234);
        assert_eq!(files.max_files, DEFAULT_REINJECT_FILES);
        let sk = resolved.context.reinject_invoked_skills.expect("skills on");
        assert_eq!(sk.token_budget, 5678);
    }

    /// The resolver must reproduce the historical env helpers' decision
    /// (enabled/disabled + budgets) exactly, or switching a channel over to
    /// presets would silently change its behaviour. Expected values are
    /// spelled out — not recomputed — so this pins behaviour, not arithmetic.
    #[test]
    fn resolve_matches_the_file_reinjector_env_helper() {
        let config = config_for("preset-test-model");
        let full = |max_files: usize, token_budget: usize| FileReinjectSpec {
            max_files,
            token_budget,
            per_file_budget: 5_000,
        };
        let cases: [(Option<&str>, Option<&str>, Option<FileReinjectSpec>); 8] = [
            (None, None, Some(full(5, 50_000))),
            (None, Some("1234"), Some(full(5, 1_234))),
            (None, Some("nope"), Some(full(5, 50_000))),
            (Some("7"), None, Some(full(7, 50_000))),
            (Some("7"), Some("1234"), Some(full(7, 1_234))),
            (Some("0"), None, None),
            (Some("off"), Some("1234"), None),
            (Some("nope"), None, None),
        ];
        for (files, budget, expected) in cases {
            let env = PresetEnv {
                reinject_files: files.map(str::to_string),
                reinject_file_budget: budget.map(str::to_string),
                ..PresetEnv::default()
            };
            assert_eq!(
                STANDARD
                    .resolve(&config, &env)
                    .context
                    .reinject_recent_files,
                expected,
                "files={files:?} budget={budget:?}"
            );
        }
    }

    /// Same contract for skills — including its deliberate difference: here an
    /// unparsable value means "on with the default budget", not "off".
    #[test]
    fn resolve_matches_the_skill_reinjector_env_helper() {
        let config = config_for("preset-test-model");
        let full = |token_budget: usize| SkillReinjectSpec {
            token_budget,
            per_skill_budget: 5_000,
        };
        let cases: [(Option<&str>, Option<&str>, Option<SkillReinjectSpec>); 8] = [
            (None, None, Some(full(25_000))),
            (None, Some("4321"), Some(full(4_321))),
            (None, Some("nope"), Some(full(25_000))),
            (Some("1234"), None, Some(full(1_234))),
            (Some("1234"), Some("4321"), Some(full(1_234))),
            (Some("0"), None, None),
            (Some("false"), Some("4321"), None),
            (Some("nope"), None, Some(full(25_000))),
        ];
        for (skills, budget, expected) in cases {
            let env = PresetEnv {
                reinject_skills: skills.map(str::to_string),
                reinject_skill_budget: budget.map(str::to_string),
                ..PresetEnv::default()
            };
            assert_eq!(
                STANDARD
                    .resolve(&config, &env)
                    .context
                    .reinject_invoked_skills,
                expected,
                "skills={skills:?} budget={budget:?}"
            );
        }
    }

    // ── selection ─────────────────────────────────────────────────────────

    #[test]
    fn selection_prefers_the_explicit_id_then_env_then_standard() {
        let env = PresetEnv::default();
        assert_eq!(select(None, &env).expect("default").id, "standard");

        let env = PresetEnv {
            agent_preset: Some("standard".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(select(None, &env).expect("env").id, "standard");

        let env = PresetEnv {
            agent_preset: Some("standard".to_string()),
            ..PresetEnv::default()
        };
        assert_eq!(
            select(Some("standard"), &env).expect("explicit").id,
            "standard"
        );
    }

    #[test]
    fn an_unknown_preset_id_is_an_error_listing_the_known_ids() {
        let err = select(Some("nope"), &PresetEnv::default()).expect_err("must reject");
        assert_eq!(err.id, "nope");
        assert_eq!(err.known, vec!["standard", "minimal"]);
        assert!(err.to_string().contains("standard"));
        assert!(err.to_string().contains("minimal"));

        let env = PresetEnv {
            agent_preset: Some("also-nope".to_string()),
            ..PresetEnv::default()
        };
        assert!(select(None, &env).is_err());
    }

    /// The inventory is only useful if it names the real switches: every
    /// toggled row must carry an env var the resolver actually reads, and
    /// every env var the resolver reads must be discoverable in a row.
    #[test]
    fn capability_inventory_names_every_toggle() {
        let toggled: Vec<&str> = STANDARD
            .capabilities
            .iter()
            .map(|c| c.toggle)
            .filter(|t| !t.is_empty())
            .collect();
        for env in [
            COMPACT_THRESHOLD_ENV,
            MICROCOMPACT_TRIGGER_ENV,
            MAX_TRANSCRIPT_CHARS_ENV,
            REINJECT_FILES_ENV,
            REINJECT_SKILLS_ENV,
            RUN_CODE_ENV,
        ] {
            assert!(toggled.contains(&env), "{env} must appear in a row");
        }

        // Names are unique, and the preset id itself is selectable by env.
        let mut names: Vec<&str> = STANDARD.capabilities.iter().map(|c| c.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "capability names must be unique");
        assert_eq!(
            select(None, &PresetEnv::default()).expect("default").id,
            STANDARD.id
        );
    }

    #[test]
    fn disabled_by_default_capabilities_are_listed_explicitly() {
        let disabled: Vec<&str> = STANDARD
            .capabilities
            .iter()
            .filter(|c| c.default == CapabilityDefault::Disabled)
            .map(|c| c.name)
            .collect();
        for expected in [
            "proactive-tool-result-pruning",
            "transcript-char-cap",
            "subagent-delegation",
            "self-scheduling-wakeup",
            "programmatic-tool-calling",
        ] {
            assert!(
                disabled.contains(&expected),
                "{expected} exists but is off by default — it must be discoverable"
            );
        }
    }

    // ── assembly ──────────────────────────────────────────────────────────

    #[test]
    fn apply_installs_the_declared_assembly_and_stamps_the_preset_id() {
        std::env::remove_var(COMPACT_THRESHOLD_ENV);
        let config = config_for("preset-test-model");
        let mut resolved = STANDARD.resolve(&config, &PresetEnv::default());
        resolved.context.microcompaction = Some(MicrocompactionSpec {
            trigger_tool_count: 33,
            keep_recent: 2,
        });
        resolved.context.max_transcript_chars = Some(4096);

        let read_state: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let builder = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new()
                .with_read_state(read_state)
                .with_skills(vec![crate::skills::skill_from_content(
                    "s",
                    "---\nname: s\ndescription: d\n---\n\nbody",
                    Vec::new(),
                )]),
            ChannelSupport { interactive: true },
        );

        assert_eq!(builder.preset_id(), Some("standard"));
        assert_eq!(builder.context_management_facts(), resolved.context);

        // A channel with no shared read state has nothing to re-inject: the
        // file half of the declaration is skipped rather than installed inert.
        let without_state = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new(),
            ChannelSupport { interactive: true },
        );
        let facts = without_state.context_management_facts();
        assert!(facts.reinject_recent_files.is_none());
        assert_eq!(
            facts.reinject_invoked_skills, resolved.context.reinject_invoked_skills,
            "the skill half only needs the catalog, which is always available"
        );
        assert_eq!(facts.compaction, resolved.context.compaction);
    }

    #[test]
    fn a_channel_that_cannot_prompt_never_gets_plan_mode_tools() {
        let config = config_for("preset-test-model");
        let resolved = STANDARD.resolve(&config, &PresetEnv::default());
        assert!(resolved.tools.plan_mode_tools, "the preset asks for them");

        // Non-interactive: the blocking plan tools stay out of the registry.
        let headless = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new(),
            ChannelSupport { interactive: false },
        );
        assert!(
            !headless.with_plan_mode_tools_for_test(),
            "a headless channel must not register blocking plan tools"
        );

        let interactive = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new(),
            ChannelSupport { interactive: true },
        );
        assert!(interactive.with_plan_mode_tools_for_test());
    }

    /// Acceptance 3: a new preset is one declaration, no builder branch.
    /// Two different declarations run through the same [`apply`] and produce
    /// exactly what they declare.
    #[test]
    fn a_new_preset_is_one_declaration_with_no_builder_branch() {
        let config = config_for("preset-test-model");
        let lean = AgentPreset {
            id: "lean-test-only",
            description: "test fixture",
            prompt: PromptProfile {
                complete: None,
                persona_suffix: Some("be terse"),
                auto_skill_injection: false,
                skill_catalog: false,
            },
            tools: ToolProfile {
                plan_mode_tools: false,
                allow: Some(&["Read"]),
                run_code: true,
            },
            context: ContextProfile {
                compaction: Some(CompactionMode::Chars(1000)),
                microcompaction: Some(MicrocompactionSpec {
                    trigger_tool_count: 5,
                    keep_recent: 1,
                }),
                max_transcript_chars: Some(2048),
                reinject_recent_files: None,
                reinject_invoked_skills: None,
            },
            capabilities: &[],
        };
        let resolved = lean.resolve(&config, &PresetEnv::default());
        let read_state: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let builder = apply(
            fresh_builder(),
            &resolved,
            &PresetAssets::new().with_read_state(read_state),
            ChannelSupport { interactive: true },
        );
        assert_eq!(builder.context_management_facts(), resolved.context);
        assert_eq!(builder.preset_id(), Some("lean-test-only"));
        assert!(!builder.with_plan_mode_tools_for_test());
        assert!(
            builder.with_run_code_for_test(),
            "the `lean` declaration opted into run_code"
        );
        assert_eq!(
            apply_prompt(assembled("base"), &resolved).full,
            "base\nbe terse"
        );
        assert!(
            builder.skills_for_test().is_empty(),
            "a preset that declares no skill catalog must leave the kernel without one"
        );
        assert_eq!(
            builder.tool_allow_for_test(),
            Some(["Read".to_string()].as_slice())
        );
        // ...and the standard preset is unaffected by that declaration.
        let standard_facts = STANDARD.resolve(&config, &PresetEnv::default()).context;
        assert_ne!(standard_facts, resolved.context);
    }

    #[test]
    fn prompt_suffix_is_a_no_op_for_the_standard_preset() {
        let config = config_for("preset-test-model");
        let resolved = STANDARD.resolve(&config, &PresetEnv::default());
        assert_eq!(apply_prompt(assembled("base"), &resolved).full, "base");
    }

    // ── Issue #128: the `minimal` preset ──────────────────────────────────

    fn demo_skill() -> Skill {
        crate::skills::skill_from_content(
            "demo",
            "---\nname: demo\ndescription: A demo skill\n---\n\nbody",
            Vec::new(),
        )
    }

    /// Acceptance 3, prompt half: whatever the channel assembled — project
    /// context, memory layers, the sub-agent note, the skill catalog, the
    /// `<environment>` segment — a `complete` prompt profile replaces it, and
    /// the segment breakdown is replaced with it (so the local breakdown
    /// estimator sizes what the request carries, not what was assembled on the
    /// way there).
    #[test]
    fn a_complete_prompt_profile_replaces_the_whole_assembly() {
        let config = config_for("preset-test-model");
        let resolved = MINIMAL.resolve(&config, &PresetEnv::default());

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "## AGENTS.md\n\ndeep project lore",
        )
        .expect("write");
        let skills = vec![demo_skill()];
        let mut assembled =
            crate::assemble_system_prompt("base prompt + memory layers", tmp.path(), &skills, true);
        const ENV: &str = "<environment>container tier</environment>";
        assembled.full.push_str(ENV);
        assembled.segments.environment = ENV.to_string();
        assert!(assembled.full.contains("deep project lore"), "fixture");

        let out = apply_prompt(assembled, &resolved);
        assert_eq!(out.full, MINIMAL_PROMPT);
        assert_eq!(out.segments.system_prompt, MINIMAL_PROMPT);
        for (name, segment) in [
            ("rules", &out.segments.rules),
            ("skills", &out.segments.skills),
            ("subagents", &out.segments.subagents),
            ("environment", &out.segments.environment),
        ] {
            assert!(
                segment.is_empty(),
                "a minimal session must carry no {name} segment, got {segment:?}"
            );
        }
    }

    /// Acceptance 3, tools half: the declaration is a four-tool subset and no
    /// context management — and the *built* runtime matches it, including the
    /// tool surface (which is pruned in `AgentRuntimeBuilder::build`, after
    /// the channel installed the full registry).
    #[test]
    fn minimal_prunes_the_built_runtime_to_the_core_tools() {
        let config = config_for("preset-test-model");
        let resolved = MINIMAL.resolve(&config, &PresetEnv::default());
        assert_eq!(resolved.context, ContextFacts::default());
        assert_eq!(resolved.tools.allow, Some(MINIMAL_TOOLS));
        assert!(!resolved.tools.plan_mode_tools);
        assert_eq!(resolved.prompt.complete, Some(MINIMAL_PROMPT));
        assert!(!resolved.prompt.skill_catalog);

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = crate::tools::build_standard_tools(tmp.path(), &[], 30);
        assert!(
            registry.find_by_name("Glob").is_some(),
            "fixture: the channel hands over the full standard surface"
        );

        let catalog = vec![demo_skill()];
        let builder = apply(
            fresh_builder()
                .tools(registry)
                .system_prompt(MINIMAL_PROMPT)
                .skills(catalog.clone()),
            &resolved,
            &PresetAssets::new().with_skills(catalog),
            ChannelSupport { interactive: true },
        );
        assert!(
            builder.skills_for_test().is_empty(),
            "a preset with no skill surface must clear the kernel's catalog"
        );
        let runtime = builder.build().expect("build");
        let mut names: Vec<String> = runtime
            .kernel()
            .tools()
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["Bash", "Edit", "Read", "Write"]);
    }

    /// Acceptance 1: measured, with a realistic project (project context file
    /// plus every local memory store seeded) — the `standard` session's
    /// system prompt is at least an order of magnitude heavier than the
    /// `minimal` one's.
    #[test]
    fn minimal_system_prompt_is_an_order_of_magnitude_smaller() {
        let _env_lock = crate::test_util::env_lock();
        let home = tempfile::tempdir().expect("home");
        let _pinned = crate::test_util::PinnedRecursiveHomeNoLock::new(home.path(), &_env_lock);
        let ws = tempfile::tempdir().expect("workspace");

        std::fs::write(
            ws.path().join("AGENTS.md"),
            format!("## AGENTS.md\n\n{}", "project lore. ".repeat(400)),
        )
        .expect("write AGENTS.md");
        let memory = crate::tools::memory::memory_path(ws.path());
        std::fs::create_dir_all(memory.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &memory,
            r#"{"notes":[{"id":"N1","tags":[],"text":"a remembered note","ts":"2026-07-09T00:00:00Z"}]}"#,
        )
        .expect("write memory");
        std::fs::write(
            crate::tools::memory::scratchpad_path(ws.path()),
            r#"{"entries":[{"key":"k","value":"a scratchpad value"}]}"#,
        )
        .expect("write scratchpad");
        let facts = crate::tools::facts::facts_path(ws.path(), "workspace");
        std::fs::create_dir_all(facts.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &facts,
            r#"{"id":"F1","text":"a workspace fact","tags":[],"source":null,"created_at":"2026-07-09T00:00:00Z","last_accessed":"2026-07-09T00:00:00Z","access_count":1,"superseded_by":null}
"#,
        )
        .expect("write facts");

        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "preset-test-model");
        std::env::set_var("RECURSIVE_WORKSPACE", ws.path());
        let config = Config::from_env().expect("config");
        assert!(
            config.system_prompt.contains("# Memory summary"),
            "fixture: the standard base must really carry the memory layers"
        );

        let skills = vec![demo_skill()];
        let standard = STANDARD.resolve(&config, &PresetEnv::default());
        let minimal = MINIMAL.resolve(&config, &PresetEnv::default());
        let standard_tokens = system_prompt_tokens(&config, &standard, &skills);
        let minimal_tokens = system_prompt_tokens(&config, &minimal, &skills);

        // The measurement itself, so a run with `--nocapture` leaves evidence
        // rather than only a pass/fail.
        eprintln!(
            "system prompt tokens — standard: {standard_tokens}, minimal: {minimal_tokens} \
             (ratio {:.1}x, standard base {} bytes, minimal {} bytes)",
            standard_tokens as f64 / minimal_tokens as f64,
            preview_system_prompt(&config, &standard, &skills)
                .full
                .len(),
            MINIMAL_PROMPT.len(),
        );

        assert_eq!(
            minimal_tokens,
            crate::llm::estimate_tokens(MINIMAL_PROMPT),
            "the minimal system prompt is the one-liner and nothing else"
        );
        assert!(
            standard_tokens >= minimal_tokens * 10,
            "measured: standard={standard_tokens} tokens vs minimal={minimal_tokens} tokens — \
             the whole point of the preset is that this differs by an order of magnitude"
        );
    }

    /// Acceptance 2, wiring half: the simple task shapes the minimal preset
    /// targets — read-modify-run, search, multi-step — still complete on a
    /// one-line prompt with four tools, exactly as they do under `standard`.
    ///
    /// The model here is scripted (`MockProvider` ignores the prompt), so this
    /// is NOT a model-quality measurement — the benchmark issue owns that half.
    /// What it pins is that the pruned, memory-less session is still a working
    /// agent: the core tools execute, the transcript stays paired, the turn
    /// ends with `NoMoreToolCalls`, and the workspace really changed.
    #[tokio::test]
    async fn the_simple_task_set_completes_under_both_presets() {
        use crate::llm::{Completion, ToolCall};

        let script = || {
            let call = |content: &str, id: &str, name: &str, args: serde_json::Value| Completion {
                content: content.to_string(),
                tool_calls: vec![ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: args,
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            };
            vec![
                // multi-step + write half of read-modify-run
                call(
                    "writing the note",
                    "c1",
                    "Write",
                    serde_json::json!({"path": "note.txt", "contents": "alpha\n"}),
                ),
                call(
                    "reading it back",
                    "c2",
                    "Read",
                    serde_json::json!({"path": "note.txt"}),
                ),
                call(
                    "editing it",
                    "c3",
                    "Edit",
                    serde_json::json!({"file_path": "note.txt", "old_string": "alpha", "new_string": "beta"}),
                ),
                // search half, through the shell the minimal preset is built on
                call(
                    "searching for the result",
                    "c4",
                    "Bash",
                    serde_json::json!({"command": "grep -n beta note.txt"}),
                ),
                call(
                    "confirming the run step",
                    "c5",
                    "Bash",
                    serde_json::json!({"command": "cat note.txt"}),
                ),
                Completion {
                    content: "note.txt now says beta".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                    reasoning_content: None,
                },
            ]
        };

        for preset in [&STANDARD, &MINIMAL] {
            let tmp = tempfile::tempdir().expect("workspace");
            let mut config = config_for("preset-test-model");
            config.workspace = tmp.path().to_path_buf();
            let resolved = preset.resolve(&config, &PresetEnv::default());
            let assembled = preview_system_prompt(&config, &resolved, &[]);
            let registry = crate::tools::build_standard_tools(tmp.path(), &[], 30);
            let assets = assets_from_registry(&registry, Vec::new());

            let builder = apply(
                AgentRuntimeBuilder::new()
                    .llm(Arc::new(MockProvider::new(script())))
                    .tools(registry)
                    .system_prompt(assembled.full),
                &resolved,
                &assets,
                ChannelSupport { interactive: false },
            );
            let mut runtime = builder.build().expect("build");
            let outcome = runtime
                .run("change alpha to beta in note.txt, then grep for beta")
                .await
                .expect("run");

            assert_eq!(
                outcome.finish_reason,
                crate::agent::FinishReason::NoMoreToolCalls,
                "{} session must finish the task, not stall",
                preset.id
            );
            assert_eq!(
                outcome.final_text.as_deref(),
                Some("note.txt now says beta"),
                "{} session must report the final answer",
                preset.id
            );
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("note.txt")).expect("read back"),
                "beta\n",
                "{} session must have really edited the file",
                preset.id
            );
        }
    }

    #[test]
    fn assets_are_taken_from_the_registry_read_state() {
        let registry = crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30);
        let assets = assets_from_registry(&registry, Vec::new());
        assert!(assets.read_state.is_some());
        let without = assets_from_registry(&crate::tools::ToolRegistry::default(), Vec::new());
        assert!(without.read_state.is_none());
    }

    /// Issue #134: `run_code` is mounted through the preset tier, off by
    /// default, with the env var as the operator's escape hatch. The row must
    /// also appear in the capability inventory (that is #127's discoverability
    /// contract for off-by-default capabilities).
    #[test]
    fn run_code_is_off_by_default_and_env_overridable() {
        let config = config_for("preset-test-model");
        assert!(
            !STANDARD
                .resolve(&config, &PresetEnv::default())
                .tools
                .run_code,
            "run_code must be opt-in"
        );

        let row = STANDARD
            .capabilities
            .iter()
            .find(|c| c.name == "programmatic-tool-calling")
            .expect("capability row must exist");
        assert_eq!(row.default, CapabilityDefault::Disabled);
        assert_eq!(row.toggle, RUN_CODE_ENV);

        for on in ["1", "on", "true", "yes"] {
            let env = PresetEnv {
                run_code: Some(on.to_string()),
                ..PresetEnv::default()
            };
            assert!(
                STANDARD.resolve(&config, &env).tools.run_code,
                "{on} must enable run_code"
            );
        }
        // A set-but-empty value is OFF, like every other toggle in this file:
        // `RECURSIVE_RUN_CODE=` in a Dockerfile must not opt a deployment into
        // host code execution.
        for off in ["0", "off", "false", "", " ", "  "] {
            let env = PresetEnv {
                run_code: Some(off.to_string()),
                ..PresetEnv::default()
            };
            assert!(
                !STANDARD.resolve(&config, &env).tools.run_code,
                "{off:?} must disable run_code"
            );
        }
    }
}
