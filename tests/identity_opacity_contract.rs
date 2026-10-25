//! Goal 430 — issue #244 W1: the identity-opacity contract.
//!
//! Owner ruling (AGENTS.md "运行时租户透明", 2026-10-18; #244 ruling,
//! 2026-10-25): the kernel never sees identity; the application edge may
//! forward opaque labels for observability, and the layers below carry them
//! **without reading a single key**; what a key means is decided only at the
//! edge (HTTP handlers, observability backends, skill sources). Until this
//! file, that sentence lived in a document; the most natural first step for
//! anyone implementing #146's asks was still "add a field to `TurnContext`".
//! These tests make the ruling red/green:
//!
//! 1. the red zone — kernel / runtime / agent loop, tool registry & dispatch,
//!    observability carriers, outbound & skill tools — has **zero**
//!    production mentions of `tenant` / `subject`. Strict zero, not "must
//!    not increase": the baseline was measured at 0 on `caaee099`, and an
//!    upper bound would both trip on a stray comment and leave a grey zone;
//! 2. named negative controls prove the scanner sees a field (and ignores a
//!    comment), sees a key-name branch, and reads past a mid-file
//!    `#[cfg(test)]` attribute (the vacuity trap that bit the sibling
//!    scanner's split — see [`production_half`]);
//! 3. the `TurnContext` field list is pinned: adding any field must first
//!    come here and say why it is not identity.
//!
//! Identity's legitimate homes — `src/http/` (`AuthIdentity`, `labels.rs`),
//! `src/session/`, `src/agui_session.rs` — are deliberately **not** scanned.
//!
//! Negative-control protocol (#243 / Goal 429 precedent): temporarily adding
//! `pub tenant: Option<String>,` to `TurnContext` turns tests 1 and 4 red
//! together; adding `if key == "tenant" { … }` to the collector's label loop
//! turns test 1 red. Results are recorded in the landing commit message; both
//! files were restored byte-for-byte (`cp` + `cmp`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const CFG_TEST: &str = "#[cfg(test)]";

/// Identity vocabulary that must not appear in red-zone production code.
/// Matched as a case-insensitive substring — stronger than #244's literal
/// `tenant|subject` grep, so `TenantId` / `Subject` cannot slip through.
/// `identity` is deliberately absent: `tools/tool_identity.rs` is about
/// *tool* identity (Goal 426) and would drown the scan.
const NEEDLES: [&str; 2] = ["tenant", "subject"];

/// Red-zone files as `(path under src/, production anchor, why)`.
///
/// The anchor is a production-only symbol the scan must reach. It proves the
/// production half was actually read: a plain `split("#[cfg(test)]")` reads
/// 110 of `run_core.rs`'s 5 400 lines (a `#[cfg(test)] use` sits at line
/// 111) and 148 of `a2a.rs`'s 2 000 (test-only struct fields).
const RED_ZONE: &[(&str, &str, &str)] = &[
    // Ruling ①: the kernel / runtime never sees identity.
    (
        "kernel.rs",
        "pub struct TurnContext",
        "kernel — identity never enters the step loop (ruling ①)",
    ),
    (
        "runtime.rs",
        "pub struct AgentRuntime",
        "runtime — builds turns from plain per-session inputs (ruling ①)",
    ),
    (
        "run_core.rs",
        "fn run_inner",
        "agent loop — stays small, never branches on who is calling (ruling ①)",
    ),
    (
        "agent/mod.rs",
        "pub mod types",
        "agent facade — re-exports only (ruling ①)",
    ),
    (
        "agent/types.rs",
        "pub enum FinishReason",
        "agent types — finish reasons / permission decisions carry no caller (ruling ①)",
    ),
    // #244 red line: the tool registry and dispatch stay identity-free.
    (
        "tools/mod.rs",
        "pub use dispatch::resolve_within",
        "tool factory — tools are built from workspace paths, not callers (#244 red line)",
    ),
    (
        "tools/registry.rs",
        "pub struct ToolRegistry",
        "tool registry — per-session instances are swapped in, never looked up by caller (#244 red line)",
    ),
    (
        "tools/dispatch.rs",
        "pub struct ToolDispatch",
        "tool dispatch — the sandbox gate knows paths, not people (#244 red line)",
    ),
    (
        "tools/tool_identity.rs",
        "pub fn builtin_safety_shape",
        "tool identity is *tool* identity (Goal 426), never caller identity",
    ),
    // Opaque carriers: observability forwards labels, never reads a key.
    (
        "observability/mod.rs",
        "pub struct RunMeta",
        "run metadata — `labels` is an opaque map, no key is named here",
    ),
    (
        "observability/collector.rs",
        "fn root_record",
        "collector — `for (k, v) in labels` only; no key literal, no branch",
    ),
    (
        "observability/exporter.rs",
        "pub struct ActiveRun",
        "exporter — attribute plumbing only",
    ),
    (
        "observability/config.rs",
        "pub struct LangfuseConfig",
        "exporter config — endpoint / keys / sampling, no caller",
    ),
    (
        "event.rs",
        "pub enum AgentEvent",
        "event stream — the kernel's output vocabulary carries no caller",
    ),
    // Outbound and skill tools receive concrete, pre-expanded values only.
    (
        "tools/http_call.rs",
        "pub struct EndpointSpec",
        "outbound HTTP — headers arrive expanded; the tool sends `(k, v)` pairs blind (W2b, Goal 431)",
    ),
    (
        "tools/a2a.rs",
        "pub struct A2aCallTool",
        "A2A — `auth` is a model-supplied parameter, never derived identity",
    ),
    (
        "tools/load_skill.rs",
        "pub struct LoadSkill",
        "skill loader — receives a concrete catalog, never picks one by caller (W2a, Goal 431)",
    ),
    (
        "skills.rs",
        "pub struct HttpSkillSource",
        "skill sources — fetch a concrete URL; template expansion happens at the edge (W2a, Goal 431)",
    ),
];

/// Directories whose every `.rs` file is red, so a new file under them is
/// covered without editing this list.
const RED_ZONE_DIRS: &[&str] = &["agent"];

/// Field names of `pub struct TurnContext { … }` in `src/kernel.rs`, in
/// declaration order (Goal 430 snapshot, 13 fields). A new field must be
/// added here deliberately — and if it is identity-shaped, stop: ruling ①.
const TURN_CONTEXT_FIELDS: [&str; 13] = [
    "messages",
    "step_events_tx",
    "tool_specs",
    "streaming",
    "permission_hook",
    "exploring_plan_mode",
    "permission_mode",
    "mailbox",
    "turn",
    "audit_session",
    "prompt_segments",
    "wall_timeout_secs",
    "failure_outcome",
];

// ── Scanner ──────────────────────────────────────────────────────────────

/// Whether `s` (already trimmed) opens a module declaration — `mod x`,
/// `pub mod x`, `pub(crate) mod x`.
fn starts_module(s: &str) -> bool {
    let s = s.strip_prefix("pub").map(str::trim_start).unwrap_or(s);
    let s = match s.strip_prefix('(') {
        Some(rest) => rest.split_once(')').map_or(s, |(_, r)| r.trim_start()),
        None => s,
    };
    s.starts_with("mod ")
}

/// Everything above the test **module** — the `#[cfg(test)]` that is
/// followed (after any further attributes) by `mod …`, not merely the first
/// `#[cfg(test)]` attribute in the file. `run_core.rs` has a
/// `#[cfg(test)] use` at line 111, `kernel.rs` / `runtime.rs` test-only
/// helper fns, `a2a.rs` test-only struct fields — splitting at any of those
/// would skip most of the production code. A file with no test module is
/// scanned whole.
fn production_half(text: &str) -> &str {
    let mut from = 0;
    while let Some(rel) = text[from..].find(CFG_TEST) {
        let at = from + rel;
        let mut after = text[at + CFG_TEST.len()..].trim_start();
        while after.starts_with("#[") {
            after = match after.find(']') {
                Some(close) => after[close + 1..].trim_start(),
                None => break,
            };
        }
        if starts_module(after) {
            return &text[..at];
        }
        from = at + CFG_TEST.len();
    }
    text
}

/// Scan one file's production half; push `display:line` offenders. Lines
/// that start with `//` (comments, rustdoc) are exempt — prose may say
/// "not tenant identity" (registry.rs) or "remote/tenant-configured"
/// (skills.rs); code may not.
fn scan_source(display: &str, why: &str, text: &str, offenders: &mut Vec<String>) {
    for (idx, line) in production_half(text).lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(needle) = NEEDLES.iter().find(|n| lower.contains(**n)) {
            offenders.push(format!(
                "{display}:{} mentions `{needle}` [{why}]: {}",
                idx + 1,
                line.trim()
            ));
        }
    }
}

/// `"a/b/c.rs"` → `root/a/b/c.rs`, platform-neutral.
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .fold(root.to_path_buf(), |p, seg| p.join(seg))
}

/// `root/a/b/c.rs` → `"a/b/c.rs"` for stable messages on every platform.
fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Recursively scan every `.rs` under `dir` (used for [`RED_ZONE_DIRS`] and
/// the scratch trees of the negative controls). Returns the number of `.rs`
/// files visited, listed-and-skipped ones included, so a caller can prove
/// the walk reached a populated directory.
fn scan_dir(
    dir: &Path,
    root: &Path,
    why: &str,
    skip: &BTreeSet<String>,
    offenders: &mut Vec<String>,
) -> usize {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{} must be listable: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    let mut visited = 0;
    for path in entries {
        if path.is_dir() {
            visited += scan_dir(&path, root, why, skip, offenders);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        visited += 1;
        let display = display_rel(root, &path);
        if skip.contains(&display) {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));
        scan_source(&display, why, &text, offenders);
    }
    visited
}

/// Scan the whole red zone; panics if a listed file is missing, its anchor
/// is not in the production half, or a red-zone directory is empty.
fn scan_red_zone() -> Vec<String> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut listed = BTreeSet::new();
    for (rel, anchor, why) in RED_ZONE {
        let path = join_rel(&src, rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("red-zone file {rel} must exist: {e}"));
        assert!(
            production_half(&text).contains(anchor),
            "red-zone scan of {rel} never reached `{anchor}` — the production/test \
             split moved; fix `production_half` before trusting the zero"
        );
        scan_source(rel, why, &text, &mut offenders);
        listed.insert((*rel).to_string());
    }
    for dir in RED_ZONE_DIRS {
        let visited = scan_dir(
            &src.join(dir),
            &src,
            &format!("under src/{dir}/ (ruling ①)"),
            &listed,
            &mut offenders,
        );
        assert!(
            visited > 0,
            "red-zone directory src/{dir}/ has no .rs files"
        );
    }
    offenders
}

// ── 1. The zero ───────────────────────────────────────────────────────────

/// Main pin: no red-zone production line mentions identity vocabulary.
#[test]
fn red_zone_has_zero_identity_mentions() {
    let offenders = scan_red_zone();
    assert!(
        offenders.is_empty(),
        "identity vocabulary belongs at the edge (src/http, src/session, \
         src/agui_session.rs) — the kernel, runtime, tool registry and \
         observability carriers must stay opaque (#244 ruling). Offenders:\n{}",
        offenders.join("\n")
    );
}

// ── 2./3. Negative controls for the scanner itself ────────────────────────

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("scratch dir")
}

fn scan_scratch(dir: &Path) -> Vec<String> {
    let mut offenders = Vec::new();
    scan_dir(dir, dir, "scratch", &BTreeSet::new(), &mut offenders);
    offenders
}

/// Control A — field shape. A struct field named after identity is exactly
/// one hit (with its line); a comment saying the same words is none, so the
/// comment exemption is proven, not assumed.
#[test]
fn negative_control_scanner_flags_a_field_and_exempts_a_comment() {
    let tmp = scratch();
    std::fs::write(
        tmp.path().join("ctx.rs"),
        "pub struct Ctx {\n    pub messages: Vec<u8>,\n    pub tenant: Option<String>,\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("notes.rs"),
        "// tenant is resolved at the edge\n/// the subject never reaches here\nfn edge() {}\n",
    )
    .unwrap();
    let offenders = scan_scratch(tmp.path());
    assert_eq!(
        offenders.len(),
        1,
        "exactly the field, not the comments: {offenders:#?}"
    );
    assert!(
        offenders[0].starts_with("ctx.rs:3 mentions `tenant`"),
        "offender names file, line and needle: {}",
        offenders[0]
    );
}

/// Control B — key-name branch. The ruling forbids the lower layers from
/// *interpreting* a key, not only from storing identity: a
/// `labels.get("tenant")` branch in a carrier is one hit.
#[test]
fn negative_control_scanner_flags_a_key_name_branch() {
    let tmp = scratch();
    std::fs::write(
        tmp.path().join("collector.rs"),
        "fn root(labels: &std::collections::BTreeMap<String, String>) -> bool {\n    \
         if labels.get(\"tenant\").is_some() {\n        return true;\n    }\n    false\n}\n",
    )
    .unwrap();
    let offenders = scan_scratch(tmp.path());
    assert_eq!(offenders.len(), 1, "{offenders:#?}");
    assert!(
        offenders[0].starts_with("collector.rs:2 mentions `tenant`"),
        "{}",
        offenders[0]
    );
}

/// Control C — the production/test split. A mid-file `#[cfg(test)]` on a
/// `use` / fn / field must **not** end the production half (the sibling
/// scanner's `split("#[cfg(test)]")` would have gone blind here), while the
/// real `#[cfg(test)] mod tests` — with or without attributes in between,
/// inline or `mod tests;` — must. Case-insensitivity is pinned alongside.
#[test]
fn negative_control_production_half_ends_at_the_test_module_only() {
    let tmp = scratch();
    std::fs::write(
        tmp.path().join("mid_cfg.rs"),
        "#[cfg(test)]\nuse crate::helper;\n\npub struct S {\n    #[cfg(test)]\n    pub probe: bool,\n    \
         pub tenant_id: u64,\n}\n\n#[cfg(test)]\n#[allow(clippy::all)]\nmod tests {\n    \
         fn t() { let subject = 1; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("ext_tests.rs"),
        "pub struct TenantScoped;\n\n#[cfg(test)]\nmod tests;\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("no_tests.rs"),
        "pub fn subject_of(x: u8) -> u8 { x }\n",
    )
    .unwrap();
    let offenders = scan_scratch(tmp.path());
    assert_eq!(
        offenders.len(),
        3,
        "one production hit per file, none from test modules: {offenders:#?}"
    );
    assert!(
        offenders[0].starts_with("ext_tests.rs:1 mentions `tenant`"),
        "{}",
        offenders[0]
    );
    assert!(
        offenders[1].starts_with("mid_cfg.rs:7 mentions `tenant`"),
        "{}",
        offenders[1]
    );
    assert!(
        offenders[2].starts_with("no_tests.rs:1 mentions `subject`"),
        "{}",
        offenders[2]
    );
}

// ── 4. TurnContext structure pin ─────────────────────────────────────────

/// `pub foo: T,` / `pub(crate) foo: T,` / `foo: T,` → `foo`. Lines that are
/// not field declarations (doc/attribute lines are filtered before, type
/// continuations like `std::sync::Arc<…>,` here) yield `None`.
fn field_name(line: &str) -> Option<&str> {
    let rest = if let Some(after_vis) = line.strip_prefix("pub(") {
        after_vis.split_once(')')?.1
    } else if let Some(after_pub) = line.strip_prefix("pub ") {
        after_pub
    } else {
        line
    };
    let (name, after) = rest.trim_start().split_once(':')?;
    let name = name.trim_end();
    let is_ident = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    (is_ident && !after.starts_with(':')).then_some(name)
}

/// Field names of `pub struct TurnContext { … }` in declaration order.
/// Line-based and `trim`med, so a CRLF checkout reads the same (#224).
fn turn_context_fields(kernel_src: &str) -> Vec<&str> {
    let mut lines = kernel_src.lines().map(str::trim);
    assert!(
        lines.any(|l| l.starts_with("pub struct TurnContext {")),
        "src/kernel.rs must define `pub struct TurnContext {{`"
    );
    let mut fields = Vec::new();
    for line in lines {
        if line == "}" {
            return fields;
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if let Some(name) = field_name(line) {
            fields.push(name);
        }
    }
    panic!("`pub struct TurnContext {{` body never closed");
}

/// The kernel's turn input is exactly the 13 pinned fields. Growing it is
/// allowed — but only after coming here and saying why the new field is not
/// identity (ruling ①: labels live in `RunMeta`, derived at the HTTP edge,
/// never in `TurnContext`).
#[test]
fn turn_context_field_set_is_pinned() {
    let kernel = include_str!("../src/kernel.rs");
    let actual = turn_context_fields(kernel);
    assert_eq!(
        actual, TURN_CONTEXT_FIELDS,
        "TurnContext changed shape. If the new field carries who is calling \
         (tenant / subject / owner / identity / labels), stop — ruling ① says \
         identity never enters the kernel; derive it at the edge into plain \
         per-session inputs instead. Otherwise extend TURN_CONTEXT_FIELDS and \
         explain in the commit."
    );
}

/// Positive control for the field parser: visibility forms, a field whose
/// name starts with `pub`, and the two continuation shapes it must ignore.
#[test]
fn field_name_parser_handles_visibility_and_continuations() {
    assert_eq!(
        field_name("pub messages: Arc<Vec<Message>>,"),
        Some("messages")
    );
    assert_eq!(field_name("pub(crate) slot: u8,"), Some("slot"));
    assert_eq!(field_name("pub(in crate::x) slot: u8,"), Some("slot"));
    assert_eq!(field_name("plain: bool,"), Some("plain"));
    assert_eq!(field_name("public_key: String,"), Some("public_key"));
    assert_eq!(field_name("Box<dyn Fn() + Send>,"), None);
    assert_eq!(field_name("std::sync::Arc<X>,"), None);
    assert_eq!(field_name("}"), None);
}

/// Positive control for the snapshot parser against a synthetic struct with
/// doc comments, attributes, blank lines and a multi-line field type.
#[test]
fn turn_context_parser_reads_a_synthetic_struct() {
    let src = "use x;\n\npub struct TurnContext {\n    /// doc\n    pub a: u8,\n\n    #[allow(dead_code)]\n    \
               pub(crate) b: Option<\n        std::sync::Arc<X>,\n    >,\n    c: bool,\n}\n\nfn after() {}\n";
    assert_eq!(turn_context_fields(src), ["a", "b", "c"]);
}
