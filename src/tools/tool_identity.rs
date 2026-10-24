//! Structured tool identity for permission decisions (issue #199, owner
//! ruling ①: "identity is an object, derivation is a single function").
//!
//! Before this module, the permission surface classified tools by *raw
//! strings* with the derivation scattered across ≥7 sites: three separate
//! hardcoded name sets (`"Write"|"Read"` for safety content,
//! `"Write"|"Read"|"Edit"` for protected-path extraction,
//! `"Write"|"Edit"|"StrReplace"` for L1 write classification) duplicated
//! the same concept with inconsistent membership, so every new FS tool or
//! rename had to be synced in N places — the root cause behind the #187
//! bypass cluster. This module collapses all of them into **one table**
//! ([`SAFETY_SHAPE_TABLE`]) plus **one registry-composed derivation**
//! (`ToolRegistry::identity_for`); every other consumer must read the
//! table through the primitives below.
//!
//! # What lives where
//!
//! - [`SafetyShape`] / [`PathArg`] — the name-keyed safety classification
//!   (path-argument position + read/write class). Looked up via
//!   [`builtin_safety_shape`], the only place a tool-name string may map
//!   to identity-shaped data.
//! - [`ToolOrigin`] — where a registered tool came from
//!   ([`ToolOrigin::Builtin`] vs [`ToolOrigin::Mcp`]). Stamped explicitly
//!   into the registry at insert time; no longer encoded in the
//!   `mcp__<server>__<tool>` naming convention.
//! - [`ToolIdentity`] — the composite object (`canonical_name`, `origin`
//!   and `shape`) produced by `ToolRegistry::identity_for`. Registry-less
//!   callers (rule layers, deep-defense checks) consume [`SafetyShape`]
//!   directly via [`builtin_safety_shape`].
//!
//! # Rule-file compatibility (S2, zero migration)
//!
//! Identity structuring is a *pipeline-internal* change of the decision
//! subject, not a rule-file format change. `[permissions]` layers keep
//! their string-pattern lists (exact or `prefix*`); rule matching
//! continues to consume the identity's canonical name and keeps the
//! issue-#153 dual-hit semantics (`name_matches` fires a pattern written
//! against either the canonical name or an alias, in one pass so
//! deny-union still beats allow-union). Old rule files need no edits.
//! MCP tools are server-addressable today via the existing `prefix*`
//! form (`"mcp__github__*"`); structured rule keys (objects pinning the
//! origin) would be a separate decision.
//!
//! # Naming note
//!
//! `RuleSource` (in `crate::permissions`) is a *different* "source": the
//! config layer a rule was loaded from (User/Project/Session). This
//! module's [`ToolOrigin`] is the *tool's* registration source. The two
//! concepts deliberately do not share a name.
//!
//! `ToolOrigin` is **not** tenant identity (owner rulings on #146/#152):
//! it answers "where does this tool come from", never "who is calling".
//! No request-subject field exists here by design.

use serde_json::Value;

/// Which argument field of a tool call carries the file path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathArg {
    /// `args["path"]` — `Write`, `Read`.
    Path,
    /// `args["file_path"]` — `Edit`.
    FilePath,
}

/// Name-keyed safety classification of a tool — the single table that
/// replaced the three divergent hardcoded name sets (issue #199).
///
/// Membership notes that preserve pre-refactor behavior exactly:
///
/// - `Write` / `Read` carry the path in `args["path"]`; `Edit` in
///   `args["file_path"]` (previously set A in `safety_content_for_tool`
///   and set B in `extract_file_path_from_content`).
/// - `StrReplace` is write-classified (previously set C) but declares no
///   path argument, so it contributes no safety content and gets no
///   protected-path extraction — exactly as before, where it appeared in
///   the write set only.
/// - Every other name — including FS-adjacent tools like `Glob`/`Grep`,
///   whose read-only-ness comes from the tool object, not this table —
///   is [`SafetyShape::NonFs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyShape {
    /// No file-path argument; no read/write classification.
    NonFs,
    /// Read-only FS tool; reading never trips the write policy nor the
    /// protected-path check.
    Read { path_arg: PathArg },
    /// Write-classified FS tool. `path_arg` is `None` for tools whose
    /// path extraction is handled elsewhere (`StrReplace`).
    Write { path_arg: Option<PathArg> },
}

impl SafetyShape {
    /// `true` for write-classified tools (L1 `fs.write_allow` policy and
    /// plan-mode write blocking key on this).
    pub fn is_write(&self) -> bool {
        matches!(self, SafetyShape::Write { .. })
    }

    /// The declared path-argument position, `None` for non-FS tools and
    /// tools without a declared path argument.
    pub fn path_arg(&self) -> Option<PathArg> {
        match self {
            SafetyShape::NonFs => None,
            SafetyShape::Read { path_arg } => Some(*path_arg),
            SafetyShape::Write { path_arg } => *path_arg,
        }
    }

    /// Extract the safety "content" (the file path) from call arguments.
    ///
    /// This is the single-table replacement for `safety_content_for_tool`
    /// (permission pipeline, former set A): `Write`/`Read` read
    /// `args["path"]`, `Edit` reads `args["file_path"]`, everything else
    /// returns `None`.
    pub fn extract_path(&self, args: &Value) -> Option<String> {
        let field = self.path_arg()?;
        match field {
            PathArg::Path => args.get("path"),
            PathArg::FilePath => args.get("file_path"),
        }
        .and_then(|v| v.as_str())
        .map(String::from)
    }
}

/// THE single table: canonical builtin tool name → [`SafetyShape`].
///
/// The complete membership of the FS-identity sets is enumerated here and
/// nowhere else. Adding an FS tool or renaming one is a one-line change
/// in this table, not a sweep across the permission surface
/// (`tests/tool_identity_contract.rs` statically asserts no second
/// table exists outside this module).
const SAFETY_SHAPE_TABLE: &[(&str, SafetyShape)] = &[
    (
        "Write",
        SafetyShape::Write {
            path_arg: Some(PathArg::Path),
        },
    ),
    (
        "Read",
        SafetyShape::Read {
            path_arg: PathArg::Path,
        },
    ),
    (
        "Edit",
        SafetyShape::Write {
            path_arg: Some(PathArg::FilePath),
        },
    ),
    // Write-classified (L1 policy) but no declared path argument: the
    // pipeline's L1 recheck reads `path`-or-`file_path` generically for
    // every tool, which is unchanged behavior.
    ("StrReplace", SafetyShape::Write { path_arg: None }),
];

/// Look up the safety shape of a canonical tool name.
///
/// Registry-less consumers (the rule layer's protected-path check, the
/// fs deep-defense assertion, the touched-files recorder) read the table
/// through this primitive; unknown names are [`SafetyShape::NonFs`].
/// The name-keyed fallback for *unregistered* names is intentional and
/// preserves pre-refactor behavior: the former sets were string-keyed
/// regardless of registration, so e.g. a `Write` call into a registry
/// that filtered the tool out still gets its protected-path check.
pub fn builtin_safety_shape(canonical_name: &str) -> SafetyShape {
    SAFETY_SHAPE_TABLE
        .iter()
        .find(|(name, _)| *name == canonical_name)
        .map(|(_, shape)| *shape)
        .unwrap_or(SafetyShape::NonFs)
}

/// Where a registered tool came from (issue #199 owner ruling ①: the
/// registration source is explicit registry state, not a naming
/// convention encoded in the tool name).
///
/// Deliberately minimal and **not** extensible toward request/tenant
/// identity: multi-tenancy is resolved at the serving edge (owner
/// rulings on #146/#152), and `TurnContext` carries no identity for the
/// same reason. Sandbox replacements register under the same primary
/// name and re-stamp the same origin — the origin belongs to the name's
/// registration, not to the implementation behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOrigin {
    /// Registered as part of the agent's native tool set.
    Builtin,
    /// Proxied from an MCP server (previously only recoverable from the
    /// `mcp__<server>__<tool>` name prefix).
    Mcp { server: String },
}

/// The structured identity a permission decision is made against
/// (issue #199): canonical name, registration origin, and safety shape.
///
/// Produced only by `ToolRegistry::identity_for` — the single derivation
/// function. Every phase of the permission pipeline and every identity
/// consumer reads its inputs from this object instead of re-deriving
/// them from raw strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolIdentity {
    /// The registry's primary name (alias resolved). Decisions are keyed
    /// on this; the raw call name stays available to callers for
    /// user-facing text and the issue-#153 dual-hit rule matching.
    pub canonical_name: String,
    /// Registration source (stamped at insert time).
    pub origin: ToolOrigin,
    /// Safety classification from the single table.
    pub shape: SafetyShape,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── The single table is the only name→shape mapping ─────────────────

    /// Exact table membership: the four FS-relevant names, everything
    /// else NonFs. These expectations ARE the pre-refactor snapshot of
    /// the three former name sets (sets A/B: Write+Read→path, Edit→
    /// file_path; set C: Write/Edit/StrReplace write-classified).
    #[test]
    fn table_membership_matches_pre_refactor_sets() {
        assert_eq!(
            builtin_safety_shape("Write"),
            SafetyShape::Write {
                path_arg: Some(PathArg::Path)
            }
        );
        assert_eq!(
            builtin_safety_shape("Read"),
            SafetyShape::Read {
                path_arg: PathArg::Path
            }
        );
        assert_eq!(
            builtin_safety_shape("Edit"),
            SafetyShape::Write {
                path_arg: Some(PathArg::FilePath)
            }
        );
        assert_eq!(
            builtin_safety_shape("StrReplace"),
            SafetyShape::Write { path_arg: None }
        );
        for name in ["Bash", "Glob", "Grep", "WebFetch", "mcp__srv__t", "Unknown"] {
            assert_eq!(
                builtin_safety_shape(name),
                SafetyShape::NonFs,
                "`{name}` must stay NonFs in the single table"
            );
        }
    }

    // ── extract_path: the former set-A semantics ────────────────────────

    /// Kills a mutation dropping `Write` from the table or swapping its
    /// path-arg position.
    #[test]
    fn extract_path_write_reads_path_field() {
        let args = json!({"path": "/tmp/out.txt", "file_path": "/decoy"});
        assert_eq!(
            builtin_safety_shape("Write").extract_path(&args).as_deref(),
            Some("/tmp/out.txt")
        );
    }

    /// Kills a mutation swapping `Read`'s extraction to `file_path`.
    #[test]
    fn extract_path_read_reads_path_field() {
        let args = json!({"path": "/tmp/file.txt", "file_path": "/decoy"});
        assert_eq!(
            builtin_safety_shape("Read").extract_path(&args).as_deref(),
            Some("/tmp/file.txt")
        );
    }

    /// Kills a mutation swapping `Edit`'s extraction to `path`.
    #[test]
    fn extract_path_edit_reads_file_path_field() {
        let args = json!({"path": "/decoy", "file_path": "/src/main.rs"});
        assert_eq!(
            builtin_safety_shape("Edit").extract_path(&args).as_deref(),
            Some("/src/main.rs")
        );
    }

    /// Kills `_ => Some(...)` table mutations: non-FS tools must extract
    /// nothing, and `StrReplace` (write-classified, no declared path
    /// argument) must extract nothing either.
    #[test]
    fn extract_path_non_fs_and_str_replace_return_none() {
        let args = json!({"path": "/tmp/x", "file_path": "/tmp/y", "command": "ls"});
        for name in ["Bash", "Glob", "StrReplace", "NeverHeard"] {
            assert!(
                builtin_safety_shape(name).extract_path(&args).is_none(),
                "`{name}` must extract no safety content"
            );
        }
    }

    // ── is_write: the former set-C semantics ────────────────────────────

    /// Kills mutations flipping the write classification (e.g. dropping
    /// `StrReplace` or adding `Read`).
    #[test]
    fn is_write_exactly_write_edit_str_replace() {
        for (name, expected) in [
            ("Write", true),
            ("Edit", true),
            ("StrReplace", true),
            ("Read", false),
            ("Bash", false),
            ("Grep", false),
        ] {
            assert_eq!(
                builtin_safety_shape(name).is_write(),
                expected,
                "write classification of `{name}` diverged"
            );
        }
    }
}
