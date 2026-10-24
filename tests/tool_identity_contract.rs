//! Goal 426 — issue #199 tool-identity contract: one identity object, one
//! derivation function, one name→shape table.
//!
//! Every permission decision (safety content, write classification,
//! protected paths, readonly, interactive gating, static rules) must be
//! made against the structured [`ToolIdentity`] from
//! `ToolRegistry::identity_for`, with the name-keyed classification
//! enumerated **only** in `src/tools/tool_identity.rs`. These tests pin:
//!
//! 1. the static half — no second identity table outside the single-table
//!    module (411 `single-path` style scan);
//! 2. the consistency half — for every registered tool and alias, the six
//!    decision facets agree between the canonical and the aliased call,
//!    and match the pre-refactor snapshot (behavior-preservation red line);
//! 3. named negative controls — reverting any consumer to raw-name string
//!    comparison turns the matching test red (each control names its
//!    mutation);
//! 4. the origin stamp — `ToolOrigin` is explicit registry state (builtin
//!    vs MCP server), not a naming convention; sandbox replacements keep
//!    the origin of the name they replace.
//!
//! Negative-control protocol (236defd3/fa0a1332 precedent): the scanner
//! control below proves the static scan detects a reintroduced name set;
//! the runtime controls were verified by temporarily restoring the former
//! string comparisons (results recorded in the commit message).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use recursive::agent::PermissionDecision;
use recursive::error::Result;
use recursive::llm::ToolSpec;
use recursive::permissions::{
    DecisionReason, LayeredPermissionsConfig, PermissionLayer, PermissionMode, RuleSource,
};
use recursive::test_util::IsolatedWorkspace;
use recursive::tools::edit::EditTool;
use recursive::tools::fs::{ReadFile, WriteFile};
use recursive::tools::glob::GlobTool;
use recursive::tools::policy_sandbox::{FsPolicy, PolicyConfig, ShellPolicy};
use recursive::tools::tool_identity::{builtin_safety_shape, PathArg, SafetyShape, ToolOrigin};
use recursive::tools::{PermissionHook, PermissionPipeline, Tool, ToolRegistry};

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Registry exposing the real FS tools under their production names and
/// aliases (Read/Write aliased as in `build_standard_tools`, Glob with its
/// two aliases, Edit primary-only — mirroring the factory).
fn registry_with_fs_tools(root: &Path) -> ToolRegistry {
    ToolRegistry::local()
        .register_with_aliases(Arc::new(ReadFile::new(root)), &["read_file"])
        .register_with_aliases(Arc::new(WriteFile::new(root)), &["write_file"])
        .register(Arc::new(EditTool::new(root)))
        .register_with_aliases(Arc::new(GlobTool::new(root)), &["list_dir", "glob"])
}

fn deny_only(patterns: &[&str]) -> LayeredPermissionsConfig {
    LayeredPermissionsConfig {
        mode: PermissionMode::Default,
        layers: vec![PermissionLayer {
            source: RuleSource::User,
            deny: patterns.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }],
    }
}

fn write_allow_policy() -> PolicyConfig {
    PolicyConfig {
        shell: ShellPolicy::default(),
        fs: FsPolicy {
            write_allow: vec!["allowed/".into()],
            ..Default::default()
        },
    }
}

/// Minimal MCP-proxied tool: origin flows from `mcp_server_name`, exactly
/// like `crate::mcp::McpTool` (kept local so the contract does not depend
/// on the mcp module's test-assertion surface).
struct FakeMcpTool {
    tool: String,
    server: String,
}

#[async_trait]
impl Tool for FakeMcpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp__{}__{}", self.server, self.tool),
            description: "fake mcp tool".into(),
            parameters: serde_json::json!({}),
        }
    }
    async fn execute(&self, _arguments: serde_json::Value) -> Result<String> {
        Ok("ok".into())
    }
    fn mcp_server_name(&self) -> Option<&str> {
        Some(&self.server)
    }
}

// ── 1. Static half: the single table is the only one ─────────────────────

/// FS tool names whose set-membership must live in exactly one place.
const FS_TOOL_NAMES: [&str; 4] = ["Write", "Read", "Edit", "StrReplace"];

/// All `"X" | "Y"` pipe-joined pairs of FS tool names (both spacing
/// styles), the textual signature of a hardcoded identity set.
fn identity_name_set_needles() -> Vec<String> {
    let mut needles = Vec::new();
    for a in FS_TOOL_NAMES {
        for b in FS_TOOL_NAMES {
            if a != b {
                needles.push(format!("\"{a}\" | \"{b}\""));
                needles.push(format!("\"{a}\"|\"{b}\""));
            }
        }
    }
    needles
}

/// 411-style walk over `src/`: flag production code (text above the
/// `#[cfg(test)]` module, doc comments excluded) outside
/// `src/tools/tool_identity.rs` containing an FS-name set. This is the
/// static assertion that no second identity table appears.
fn scan_identity_tables(dir: &Path, src_root: &Path, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir)
        .expect("src must be listable")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            scan_identity_tables(&path, src_root, offenders);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // The sanctioned single table module.
        if path == src_root.join("tools").join("tool_identity.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("source file must be readable");
        // Only production code: everything above the test module (this
        // repo's convention places `#[cfg(test)] mod tests` at the bottom).
        let production = text.split("#[cfg(test)]").next().unwrap_or(&text);
        for line in production.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue; // rustdoc/comments may legitimately cite the old sets
            }
            for needle in identity_name_set_needles() {
                if line.contains(&needle) {
                    offenders.push(format!("{} contains `{needle}`", path.display()));
                    break;
                }
            }
        }
    }
}

/// The single-table contract, static half: only `tool_identity.rs` may map
/// FS tool-name strings to identity-shaped data.
#[test]
fn static_no_second_identity_table_outside_tool_identity() {
    let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    scan_identity_tables(&src_root, &src_root, &mut offenders);
    assert!(
        offenders.is_empty(),
        "tool-name identity sets may only live in src/tools/tool_identity.rs; \
         offenders: {offenders:#?}"
    );
}

/// Negative control for the scanner itself (mutation: reintroduce a
/// hardcoded set in a consumer file). Writes an offender into a scratch
/// tree and asserts the scan turns red — proving the empty scan above
/// detects the pattern rather than scanning nothing.
#[test]
fn negative_control_scanner_detects_reintroduced_name_set() {
    let tmp = IsolatedWorkspace::new();
    let consumer = tmp.path().join("fake_consumer.rs");
    std::fs::write(
        &consumer,
        "fn is_write(name: &str) -> bool {\n    matches!(name, \"Write\" | \"Edit\" | \"StrReplace\")\n}\n",
    )
    .unwrap();
    // A doc-comment citing the old set must NOT trip the scan.
    std::fs::write(
        tmp.path().join("fake_docs.rs"),
        "//! the old set was `\"Write\" | \"Read\"`\n",
    )
    .unwrap();
    let mut offenders = Vec::new();
    scan_identity_tables(tmp.path(), tmp.path(), &mut offenders);
    assert_eq!(
        offenders.len(),
        1,
        "scanner must flag exactly the code offender, got: {offenders:#?}"
    );
    assert!(offenders[0].contains("fake_consumer.rs"));
}

// ── 2. Consistency half: six facets, primary ≡ alias, snapshot-pinned ────

/// For every registered (primary, alias) pair, the identity-derived facets
/// agree between the two names — and for the FS tools the values equal the
/// pre-refactor snapshot (behavior-preservation red line of the goal):
///
/// 1. safety shape (former set A: Write/Read→`path`, Edit→`file_path`)
/// 2. readonly classification (tool-object derived, alias-resolved)
/// 3. protected-path check (former set B via the rule layer)
/// 4. L1 write classification (former set C)
/// 5. static rule dual-hit (a primary-keyed deny fires for the alias too)
/// 6. interactive gating parity (hook consulted identically for both)
#[tokio::test]
async fn identity_consistency_primary_alias_across_six_facets() {
    let tmp = IsolatedWorkspace::new();
    let registry = registry_with_fs_tools(tmp.path());
    let pipeline = PermissionPipeline::new(&registry);

    // (primary, alias) pairs actually registered above.
    let pairs = [
        ("Write", "write_file"),
        ("Read", "read_file"),
        ("Glob", "list_dir"),
        ("Glob", "glob"),
    ];

    // Pre-refactor snapshot of the shape facets (former set A membership).
    let snapshot_shape = [
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
        ("Glob", SafetyShape::NonFs),
    ];
    // Pre-refactor snapshot of the readonly facet.
    let snapshot_readonly = [("Write", false), ("Read", true), ("Glob", true)];

    for (primary, alias) in pairs {
        // Facet 1 — shape: alias resolves to the canonical identity, and
        // both match the single-table snapshot.
        let id_primary = registry
            .identity_for(primary)
            .unwrap_or_else(|| panic!("`{primary}` must resolve to an identity"));
        let id_alias = registry
            .identity_for(alias)
            .unwrap_or_else(|| panic!("alias `{alias}` must resolve to the canonical identity"));
        assert_eq!(
            id_primary.canonical_name, primary,
            "primary name must resolve to itself"
        );
        assert_eq!(
            id_alias.canonical_name, id_primary.canonical_name,
            "alias `{alias}` must resolve to `{primary}`"
        );
        assert_eq!(id_primary.shape, id_alias.shape);
        assert_eq!(
            id_primary.shape,
            builtin_safety_shape(primary),
            "`{primary}` shape diverged from the single table"
        );
        for (name, expected) in snapshot_shape {
            if name == primary {
                assert_eq!(
                    id_primary.shape, expected,
                    "`{primary}` shape diverged from the pre-refactor snapshot"
                );
            }
        }

        // Facet 2 — readonly classification agrees across names.
        assert_eq!(
            registry.is_readonly(primary),
            registry.is_readonly(alias),
            "readonly classification of `{primary}` vs `{alias}` diverged"
        );
        for (name, expected) in snapshot_readonly {
            if name == primary {
                assert_eq!(
                    registry.is_readonly(primary),
                    expected,
                    "`{primary}` readonly diverged from the pre-refactor snapshot"
                );
            }
        }

        // Facet 3 — protected-path check: identical verdict for both names
        // on a `.git` target (denied SafetyCheck for write-classified FS
        // tools, pass-through otherwise).
        let target = if primary == "Edit" {
            serde_json::json!({"file_path": ".git/config", "contents": "x"})
        } else {
            serde_json::json!({"path": ".git/config"})
        };
        assert_eq!(
            pipeline.check(primary, target.clone()).await.is_deny(),
            pipeline.check(alias, target).await.is_deny(),
            "protected-path verdict of `{primary}` vs `{alias}` diverged"
        );

        // Facet 4 — L1 write classification: identical verdict outside the
        // write_allow prefix for both names.
        let reg_pol = registry_with_fs_tools(tmp.path()).with_policy(write_allow_policy());
        let pol_pipeline = PermissionPipeline::new(&reg_pol);
        assert_eq!(
            pol_pipeline
                .recheck_policy(primary, &serde_json::json!({"path": "elsewhere/x"}))
                .is_err(),
            pol_pipeline
                .recheck_policy(alias, &serde_json::json!({"path": "elsewhere/x"}))
                .is_err(),
            "L1 write classification of `{primary}` vs `{alias}` diverged"
        );

        // Facet 5 — static rule dual-hit: a deny written against the
        // primary name fires for the alias call too (issue #153, kept by
        // the identity pipeline).
        let reg_deny = registry_with_fs_tools(tmp.path()).with_permissions(deny_only(&[primary]));
        let deny_pipeline = PermissionPipeline::new(&reg_deny);
        let args = serde_json::json!({"path": "out.txt"});
        assert!(
            deny_pipeline.check(primary, args.clone()).await.is_deny(),
            "primary `{primary}` must be denied by deny=[\"{primary}\"]"
        );
        assert!(
            deny_pipeline.check(alias, args).await.is_deny(),
            "alias `{alias}` must not bypass deny=[\"{primary}\"]"
        );
    }

    // Facet snapshot details for Edit (no alias registered — pinned directly
    // against the single table): write-classified via `file_path`.
    assert_eq!(
        builtin_safety_shape("Edit"),
        SafetyShape::Write {
            path_arg: Some(PathArg::FilePath)
        }
    );
}

/// Facet 6 — interactive gating parity: with `Read` marked interactive, a
/// registered permission hook is consulted — and can deny — for both the
/// canonical and the aliased call. Mutation caught: re-deriving the
/// interactive decision from the raw name only (dropping the pipeline's
/// canonical OR raw-name gate) leaves the alias call unhooked.
#[tokio::test]
async fn interactive_gate_fires_identically_for_alias() {
    struct DenyHook;
    #[async_trait]
    impl PermissionHook for DenyHook {
        async fn check(&self, name: &str, _args: &serde_json::Value) -> PermissionDecision {
            PermissionDecision::Deny(format!("hook denied {name}"))
        }
    }

    let tmp = IsolatedWorkspace::new();
    let config = LayeredPermissionsConfig {
        mode: PermissionMode::Default,
        layers: vec![PermissionLayer {
            source: RuleSource::User,
            interactive: vec!["Read".into()],
            ..Default::default()
        }],
    };
    let registry = ToolRegistry::local()
        .register_with_aliases(Arc::new(ReadFile::new(tmp.path())), &["read_file"])
        .with_permissions(config)
        .with_permission_hook(Arc::new(DenyHook));
    let pipeline = PermissionPipeline::new(&registry);

    for name in ["Read", "read_file"] {
        match pipeline
            .check(name, serde_json::json!({"path": "src/lib.rs"}))
            .await
        {
            recursive::tools::permission_pipeline::CheckOutcome::Deny { error, .. } => assert!(
                matches!(
                    error,
                    recursive::error::Error::PermissionDenied {
                        reason: DecisionReason::Hook { .. },
                        ..
                    }
                ),
                "`{name}` must be denied by the interactive hook, got: {error:?}"
            ),
            recursive::tools::permission_pipeline::CheckOutcome::Allow { .. } => {
                panic!("`{name}` must not bypass the interactive gate")
            }
        }
    }
}

// ── 3. Named negative controls (mutation: revert a consumer to raw-name
//       string comparison; the matching control turns red) ─────────────────

/// Control 1 of 3 — deny-rule face. Mutation caught: deriving the safety
/// content from the RAW call name instead of the identity's canonical name
/// (reverting `check`'s entry derivation) lets a call under
/// `Write`-shaped alias arguments dodge... nothing — but reverting the
/// RULE decision subject to the raw name drops the dual-hit semantics and
/// the alias sails through a `deny=["Write"]` rule.
#[tokio::test]
async fn negative_control_alias_cannot_bypass_deny_rule() {
    let tmp = IsolatedWorkspace::new();
    let registry = registry_with_fs_tools(tmp.path()).with_permissions(deny_only(&["Write"]));
    let pipeline = PermissionPipeline::new(&registry);
    let args = serde_json::json!({"path": "out.txt", "contents": "x"});
    assert!(
        pipeline.check("write_file", args).await.is_deny(),
        "alias `write_file` must not bypass deny=[\"Write\"] — if this is \
         red-green-flipped, the pipeline decision subject was reverted to \
         the raw call name"
    );
}

/// Control 2 of 3 — protected-path face. Mutation caught: reverting
/// `check_protected_path` to a name-string gate (former set B as a
/// parallel map) keyed on the raw call name — the alias call would then
/// extract no path and write `.git/hooks/pre-commit`.
#[tokio::test]
async fn negative_control_alias_cannot_bypass_protected_path() {
    let tmp = IsolatedWorkspace::new();
    let registry = registry_with_fs_tools(tmp.path());
    let pipeline = PermissionPipeline::new(&registry);
    let args = serde_json::json!({"path": ".git/hooks/pre-commit", "contents": "x"});
    match pipeline.check("write_file", args).await {
        recursive::tools::permission_pipeline::CheckOutcome::Deny { error, .. } => assert!(
            matches!(
                error,
                recursive::error::Error::PermissionDenied {
                    reason: DecisionReason::SafetyCheck { .. },
                    ..
                }
            ),
            "alias write to a protected path must be a SafetyCheck denial, got: {error:?}"
        ),
        recursive::tools::permission_pipeline::CheckOutcome::Allow { .. } => {
            panic!("alias `write_file` must not bypass the protected-path check")
        }
    }
}

/// Control 3 of 3 — L1 write-policy face. Mutation caught: reverting
/// `recheck_policy`'s write classification to a raw-name `matches!`
/// (former set C without alias resolution) classifies `write_file` as a
/// non-write and skips `fs.write_allow`.
#[tokio::test]
async fn negative_control_alias_is_write_for_l1_policy() {
    let tmp = IsolatedWorkspace::new();
    let registry = registry_with_fs_tools(tmp.path()).with_policy(write_allow_policy());
    let pipeline = PermissionPipeline::new(&registry);
    assert!(
        pipeline
            .recheck_policy(
                "write_file",
                &serde_json::json!({"path": "elsewhere/out.txt"})
            )
            .is_err(),
        "write_file outside write_allow must be denied — if this is red, \
         recheck_policy's write classification was reverted to raw-name \
         string matching"
    );
}

// ── 4. Origin stamping: explicit registry state ──────────────────────────

/// `ToolOrigin` is stamped at insert time from the tool object: builtin
/// tools stamp `Builtin` (also through aliases), MCP-proxied tools stamp
/// `Mcp { server }`, and a sandbox replacement re-registering under the
/// same primary name keeps that name's origin (the origin belongs to the
/// registration, not the implementation behind it).
#[test]
fn origin_stamped_builtin_mcp_and_replacement() {
    let tmp = IsolatedWorkspace::new();
    let registry = registry_with_fs_tools(tmp.path())
        .register(Arc::new(FakeMcpTool {
            tool: "deploy".into(),
            server: "github".into(),
        }))
        .register_with_aliases(Arc::new(GlobTool::new(tmp.path())), &["glob2"]);

    // Builtin, via primary name.
    let read = registry.identity_for("Read").expect("Read registered");
    assert_eq!(read.origin, ToolOrigin::Builtin);
    // Origin follows the CANONICAL name: the alias resolves first.
    let read_alias = registry.identity_for("read_file").expect("alias resolves");
    assert_eq!(read_alias.origin, ToolOrigin::Builtin);
    assert_eq!(read_alias.canonical_name, "Read");

    // MCP: explicit origin, no longer inferred from the name prefix.
    let mcp = registry
        .identity_for("mcp__github__deploy")
        .expect("mcp tool registered");
    assert_eq!(
        mcp.origin,
        ToolOrigin::Mcp {
            server: "github".into()
        }
    );
    assert_eq!(mcp.canonical_name, "mcp__github__deploy");
    assert_eq!(mcp.shape, SafetyShape::NonFs);

    // Unregistered names have no identity at all (no fabricated origin).
    assert!(registry.identity_for("NeverRegistered").is_none());

    // Sandbox replacement: re-register under the same primary name (the
    // container tier's same-name swap) — origin of the name is unchanged.
    let replaced = registry_with_fs_tools(tmp.path())
        .register(Arc::new(WriteFile::new(tmp.path())))
        .register(Arc::new(FakeMcpTool {
            tool: "deploy2".into(),
            server: "github".into(),
        }))
        .register(Arc::new(WriteFile::new(tmp.path())));
    let write = replaced.identity_for("Write").expect("Write registered");
    assert_eq!(write.origin, ToolOrigin::Builtin);
    assert_eq!(
        replaced
            .identity_for("mcp__github__deploy2")
            .expect("")
            .origin,
        ToolOrigin::Mcp {
            server: "github".into()
        }
    );
}

/// Unregistered-but-table-named FS tools keep the pre-refactor name-keyed
/// semantics: the shape fallback still classifies `StrReplace` as a write
/// even when nothing is registered under that name (the L1 recheck and
/// protected-path behavior for filtered-out tools is unchanged).
#[test]
fn unregistered_fs_names_keep_table_semantics() {
    let registry = ToolRegistry::local().with_policy(write_allow_policy());
    let pipeline = PermissionPipeline::new(&registry);
    assert!(registry.identity_for("StrReplace").is_none());
    assert!(builtin_safety_shape("StrReplace").is_write());
    assert!(
        pipeline
            .recheck_policy(
                "StrReplace",
                &serde_json::json!({"path": "/other/data.json"})
            )
            .is_err(),
        "StrReplace outside write_allow must stay denied via the table fallback"
    );
}
