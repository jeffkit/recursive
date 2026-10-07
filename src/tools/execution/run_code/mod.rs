//! `run_code` — programmatic tool calling (issue #134, borrowed from DSH
//! `ptc-runtime`).
//!
//! The model writes a **program** that calls registered tools as async
//! functions and returns only its output; one run replaces many ReAct
//! round-trips (one LLM request + KV recompute each). Batch shapes —
//! ≥5 same-kind calls plus filtering / dedup / aggregation — collapse into a
//! single step.
//!
//! The runtime contract lives in [`runner`] (fresh process, empty env, three
//! budgets), the failure taxonomy in [`protocol`], the portable binding-name
//! rules in [`bindings`], and the ordered output budget in [`ledger`].
//!
//! This is **host execution, not a sandbox**: only the *environment* is
//! scrubbed — the program is a full runtime process whose globals are intact,
//! so it is not confined to the bindings it may call. Two consequences:
//! the tool is mounted only for a host-bound transport
//! ([`ToolTransport::executes_on_host`]) and never on the container / microVM
//! tiers, and it stays **off by default** — mounted through the issue #127
//! preset tier ([`crate::preset::ToolProfile`]), see `RECURSIVE_RUN_CODE`.
//!
//! Note on sub-agents: a worker's registry is assembled from the parent's
//! tools, so a manifest that *explicitly* lists `RunCode` in `allowed_tools`
//! gets a rebuild over the worker's own (allow-listed) registry — the
//! program's binding set is the worker's tool surface, never the parent's.
//! No built-in role lists it, so a worker only ever runs code if an operator
//! asks for it.
//!
//! Note on the `RECURSIVE_SANDBOX=policy` tier: it is host-bound as well
//! (`LocalTransport`), so the transport gate does not exclude it. There the
//! L1 policy is enforced by the tools that query it at call time — i.e. over
//! the program's *tool calls* — and says nothing about the program's own
//! filesystem or network access. An opted-in `run_code` is a full host
//! process on that tier too; the preset opt-in is the operator's gate.
//!
//! [`ToolTransport::executes_on_host`]: crate::tools::transport::ToolTransport::executes_on_host

pub mod bindings;
pub mod ledger;
pub mod protocol;
pub mod runner;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::tool_kind::ToolKind;
use crate::tools::Tool;
use crate::tools::ToolRegistry;

use bindings::BindingTable;
use runner::{RunCodeLimits, RunProgramRequest, ToolInvoker};

/// Primary tool name (PascalCase, like every other built-in tool).
pub const RUN_CODE_TOOL_NAME: &str = "RunCode";

/// Adapter that runs programmatic tool calls through the ordinary registry, so
/// they hit the same permission hook, permission pipeline, touched-files
/// collector and change-ledger accounting as a model-issued call.
///
/// The per-call [`AuditMeta`](crate::tools::AuditMeta) is built by
/// [`ToolRegistry::invoke_gated_with_audit`] and emitted as a tracing record.
/// A nested call has no transcript tool-call id, so there is no
/// `MessageAppendedWithAudit` slot to attach it to — `invoke` would simply
/// throw the record away and make a program's calls the one un-audited path
/// into the registry, while `invoke_with_audit` alone would drop the hook.
struct RegistryInvoker(ToolRegistry);

#[async_trait]
impl ToolInvoker for RegistryInvoker {
    async fn invoke(&self, tool: &str, args: Value) -> Result<String> {
        let dispatch = self.0.invoke_gated_with_audit(tool, args).await;
        let audit = &dispatch.audit;
        tracing::info!(
            target: "recursive::run_code",
            tool,
            step_id = %audit.step_id,
            side_effect = ?audit.side_effect,
            exit_status = ?audit.exit_status,
            started_at = audit.started_at,
            finished_at = audit.finished_at,
            "run_code: programmatic tool call",
        );
        dispatch.result
    }
}

/// The `run_code` tool.
pub struct RunCode {
    tools: ToolRegistry,
    limits: RunCodeLimits,
    node_bin: Option<PathBuf>,
}

impl RunCode {
    /// Build the tool over the registry whose tools the program may call.
    pub fn new(tools: ToolRegistry) -> Self {
        Self {
            tools,
            limits: RunCodeLimits::default(),
            node_bin: None,
        }
    }

    /// Override the three budgets (tests / operators).
    pub fn with_limits(mut self, limits: RunCodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Pin the runtime binary instead of locating `node` on `PATH`.
    pub fn with_node_bin(mut self, node: impl Into<PathBuf>) -> Self {
        self.node_bin = Some(node.into());
        self
    }

    /// The tool names exposed as program bindings.
    ///
    /// Only names that are portable identifiers ([`bindings`]) become
    /// bindings; a registry entry that cannot be one — a hyphenated / dotted
    /// client tool, an ECMAScript/Python reserved word — is **skipped**, never
    /// fatal. One unportable entry must not reject the whole table and kill
    /// every `run_code` call with an error naming a binding the program never
    /// used. An MCP name is not skipped as a class: `mcp__<server>__<tool>` is
    /// a valid identifier, so it does become a binding.
    ///
    /// [`RUN_CODE_TOOL_NAME`] is excluded: a program may not recurse into
    /// another run.
    pub fn binding_names(&self) -> Vec<String> {
        self.tools
            .names()
            .into_iter()
            .filter(|name| name != RUN_CODE_TOOL_NAME)
            .filter(|name| bindings::validate_binding_name(name).is_ok())
            .collect()
    }
}

#[async_trait]
impl Tool for RunCode {
    fn spec(&self) -> ToolSpec {
        let available = self.binding_names().join(", ");
        ToolSpec {
            name: RUN_CODE_TOOL_NAME.into(),
            description: format!(
                "Run a JavaScript program that calls tools as async functions — one run \
                 replaces many tool round-trips. The program body receives one binding per \
                 registered tool, each `async (args) => result` where `args` is the tool's \
                 argument object and the resolved value is the tool's text result; a failed \
                 tool call rejects the awaited promise. Await bindings, then filter / dedupe / \
                 aggregate in code and print with console.log(...); the program's output and \
                 its return value are the only things that come back. Each run is a fresh \
                 process on the host with an empty environment and fixed time / output / heap \
                 budgets (failures are reported as timeout / output-limit / heap-limit). \
                 Available bindings: {available}."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "JavaScript program body. Bindings are in scope as \
                                        `await <ToolName>({} args ...)`; use `return` for the \
                                        completion value and `console.log` for output."
                    }
                },
                "required": ["code"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let code = arguments
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::BadToolArgs {
                name: RUN_CODE_TOOL_NAME.into(),
                message: "missing required string field `code`".into(),
            })?;

        // `binding_names` only yields portable identifiers, so the table
        // always builds; the mapping is a defensive re-validation that keeps
        // an unusable name from ever reaching a spawned process.
        let bindings = BindingTable::from_tool_names(self.binding_names()).map_err(|err| {
            Error::ToolRejected {
                name: RUN_CODE_TOOL_NAME.into(),
                reason: err.to_string(),
            }
        })?;

        let report = runner::run_program(RunProgramRequest {
            source: code.to_string(),
            bindings,
            invoker: Arc::new(RegistryInvoker(self.tools.clone())),
            limits: self.limits,
            node_bin: self.node_bin.clone(),
            abort: None,
        })
        .await;
        Ok(report.render())
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::audit::ToolSideEffect;
    use std::time::Duration;

    /// A deterministic stand-in for a real tool: `Double({n})` → `n * 2`.
    struct Double;

    #[async_trait]
    impl Tool for Double {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "Double".into(),
                description: "Return twice the given n".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"n": {"type": "number"}},
                    "required": ["n"]
                }),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String> {
            let n = arguments.get("n").and_then(Value::as_i64).unwrap_or(0);
            Ok((n * 2).to_string())
        }

        fn side_effect_class(&self) -> ToolSideEffect {
            ToolSideEffect::ReadOnly
        }
    }

    /// A registry entry whose name cannot be a portable binding — a reserved
    /// word (`for`) or an MCP-style `mcp__<server>__<tool>` name.
    struct Unportable(&'static str);

    #[async_trait]
    impl Tool for Unportable {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "not a portable binding name".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, _arguments: Value) -> Result<String> {
            Ok("nope".into())
        }
    }

    fn registry_with(tool: Arc<dyn Tool>) -> ToolRegistry {
        ToolRegistry::local().register(tool)
    }

    /// Whether a `node` runtime is reachable for the integration tests. Uses
    /// a direct probe (not [`runner::locate_node`]) so the guard cannot mask a
    /// resolution bug: if `locate_node` fails to find the runtime these tests
    /// run — and fail loudly — instead of silently skipping.
    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn spec_advertises_every_binding() {
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let spec = tool.spec();
        assert_eq!(spec.name, RUN_CODE_TOOL_NAME);
        assert!(spec.description.contains("Double"));
        assert_eq!(tool.binding_names(), vec!["Double".to_string()]);
        assert_eq!(tool.kind(), ToolKind::Execute);
    }

    /// Issue #134 review: a registry entry that cannot be a portable binding
    /// (a reserved word, an MCP tool name with a hyphen) is skipped — it must
    /// not reject the whole table and fail every call, blaming a binding the
    /// program never asked for. The run proceeds to the runtime.
    #[tokio::test]
    async fn non_portable_registry_names_do_not_block_the_run() {
        let tool = RunCode::new(
            registry_with(Arc::new(Double))
                .register(Arc::new(Unportable("for")))
                .register(Arc::new(Unportable("mcp__filesystem-test__read"))),
        )
        .with_node_bin("/nonexistent/recursive-run-code-node");

        assert_eq!(tool.binding_names(), vec!["Double".to_string()]);
        let spec = tool.spec();
        assert!(spec.description.contains("Double"));
        assert!(
            !spec.description.contains("mcp__"),
            "an unportable name must not be advertised: {}",
            spec.description
        );

        let out = tool.execute(json!({"code": "return 1;"})).await.unwrap();
        assert!(out.contains("status=sandbox-unavailable"), "{out}");
    }

    /// Issue #134 review: a program's calls go through
    /// `invoke_gated_with_audit` — the hook gate *and* the audit record —
    /// rather than `invoke` (which throws the record away) — the tool result
    /// the program sees is unchanged, and a rejected call still surfaces as an
    /// `Err` the program can catch.
    #[tokio::test]
    async fn programmatic_calls_use_the_audited_dispatch() {
        let invoker = RegistryInvoker(registry_with(Arc::new(Double)));
        assert_eq!(
            invoker.invoke("Double", json!({"n": 21})).await.unwrap(),
            "42"
        );
        assert!(
            invoker.invoke("Nope", json!({})).await.is_err(),
            "an unknown binding must reject"
        );
    }

    /// Issue #134 review: a program's calls pass the session's runtime
    /// permission hook — the path a TUI/CLI approval prompt or an AG-UI client
    /// tool interrupt travels. No `permissions` config is installed: the hook
    /// gate must not depend on one (that is the CLI / TUI / HTTP default, and
    /// an earlier revision consulted the hook only inside the pipeline's
    /// pre-configured branch).
    #[tokio::test]
    async fn programmatic_calls_are_gated_by_the_permission_hook() {
        struct Deny;
        #[async_trait]
        impl crate::tools::PermissionHook for Deny {
            async fn check(&self, name: &str, _args: &Value) -> crate::agent::PermissionDecision {
                crate::agent::PermissionDecision::Deny(format!("denied {name}"))
            }
        }

        let registry = registry_with(Arc::new(Double)).with_permission_hook(Arc::new(Deny));
        let invoker = RegistryInvoker(registry);
        let err = invoker
            .invoke("Double", json!({"n": 1}))
            .await
            .expect_err("the hook must deny the programmatic call");
        assert!(
            matches!(err, Error::PermissionDenied { .. }),
            "expected a hook denial, got {err:?}"
        );
    }

    #[tokio::test]
    async fn missing_code_argument_is_a_bad_args_error() {
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        assert!(matches!(
            tool.execute(json!({})).await,
            Err(Error::BadToolArgs { .. })
        ));
    }

    /// A missing runtime is a sandbox *fact*, not a program error.
    #[tokio::test]
    async fn missing_runtime_reports_sandbox_unavailable() {
        let tool = RunCode::new(registry_with(Arc::new(Double)))
            .with_node_bin("/nonexistent/recursive-run-code-node");
        let out = tool.execute(json!({"code": "return 1;"})).await.unwrap();
        assert!(out.contains("status=sandbox-unavailable"), "{out}");
        assert!(out.contains("sandbox: runtime="), "{out}");
    }

    #[tokio::test]
    async fn program_calls_bindings_and_aggregates_in_one_run() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let code = r#"
let total = 0;
for (const n of [1, 2, 3, 4, 5]) {
  total += Number(await Double({ n }));
}
console.log(`total=${total}`);
return total;
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(out.contains("calls=5"), "{out}");
        assert!(out.contains("total=30"), "{out}");
        assert!(out.contains("--- value ---\n30"), "{out}");
    }

    #[tokio::test]
    async fn a_throwing_binding_rejects_the_awaited_call() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        // `Double` ignores a missing n, so force a failure with a tool that
        // does not exist: the program catches the rejection and reports.
        let code = r#"
try {
  await Nope();
} catch (err) {
  console.log("caught: " + err.message);
}
return "recovered";
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(out.contains("caught:"), "{out}");
        assert!(out.contains("recovered"), "{out}");
    }

    /// Acceptance (#134): the wall-clock budget trips and is classified
    /// `timeout`.
    #[tokio::test]
    async fn timeout_budget_is_enforced_and_classified() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_limits(RunCodeLimits {
            timeout: Duration::from_millis(400),
            ..RunCodeLimits::default()
        });
        let started = std::time::Instant::now();
        let out = tool
            .execute(json!({ "code": "await new Promise(() => {});" }))
            .await
            .unwrap();
        assert!(out.contains("status=timeout"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the run was not cut short: {:?}",
            started.elapsed()
        );
    }

    /// Issue #134 review: the wall-clock budget stays authoritative when the
    /// program leaves a detached process holding the inherited stdout/stderr
    /// pipes. The stderr reader used to be awaited unbounded, so a 0.4s budget
    /// produced a ~20s call after the runtime was killed (the leftover process
    /// kept the pipe open).
    #[tokio::test]
    async fn a_detached_pipe_holder_cannot_outlast_the_budget() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_limits(RunCodeLimits {
            timeout: Duration::from_millis(400),
            ..RunCodeLimits::default()
        });
        let code = r#"
const cp = await import('node:child_process');
cp.spawn(process.execPath, ['-e', 'setTimeout(() => {}, 20000)'], {
  detached: true,
  stdio: ['ignore', 'inherit', 'inherit'],
}).unref();
return 'spawned';
"#;
        let started = std::time::Instant::now();
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        let elapsed = started.elapsed();
        assert!(out.contains("status=timeout"), "{out}");
        assert!(
            elapsed < Duration::from_secs(15),
            "a leftover pipe holder must not stretch the call past its budget: {elapsed:?}"
        );
    }

    /// Acceptance (#134): the output budget trips, the largest prefix is
    /// retained, and the run is classified `output-limit`.
    #[tokio::test]
    async fn output_budget_is_enforced_and_classified() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_limits(RunCodeLimits {
            output_limit_bytes: 64,
            ..RunCodeLimits::default()
        });
        let code = r#"
for (let i = 0; i < 200; i += 1) console.log("payload-" + i);
return "tail";
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=output-limit"), "{out}");
        assert!(out.contains("truncated=true"), "{out}");
        assert!(out.contains("payload-0"), "{out}");
    }

    /// Issue #134 review: the output budget must bound the AGENT's memory,
    /// not just the ledger. A single newline-free blob far larger than the
    /// budget is dropped by the reader (so it is never buffered) and the run
    /// is still classified `output-limit`; the program keeps going.
    #[tokio::test]
    async fn an_oversized_single_line_is_dropped_not_buffered() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_limits(RunCodeLimits {
            output_limit_bytes: 64,
            ..RunCodeLimits::default()
        });
        let code = r#"
console.log("x".repeat(200000));
console.log("after");
return "tail";
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=output-limit"), "{out}");
        assert!(out.contains("truncated=true"), "{out}");
        assert!(out.contains("after"), "later lines must survive: {out}");
        assert!(out.contains("tail"), "{out}");
    }

    /// Issue #134 review: the completion value shares the output budget with
    /// the log lines. A value that does not fit is truncated and the run is
    /// reported `output-limit` — it must not be delivered in full (which would
    /// let the rendered observation reach ~2× the documented ceiling while
    /// still claiming `truncated=true`).
    #[tokio::test]
    async fn the_completion_value_shares_the_output_budget() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_limits(RunCodeLimits {
            output_limit_bytes: 64,
            ..RunCodeLimits::default()
        });
        let code = r#"return "v".repeat(400);"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=output-limit"), "{out}");
        assert!(out.contains("truncated=true"), "{out}");
        assert!(
            !out.contains(&"v".repeat(65)),
            "the value must be truncated to the budget: {} bytes",
            out.len()
        );
        assert!(
            out.len() < 4096,
            "the rendered observation must stay bounded: {} bytes",
            out.len()
        );
    }

    /// Issue #134 review: an unserializable completion value is the
    /// `invalid-output` failure. The bootstrap used to fall back to
    /// `String(value)` ("[object Object]"), which silently handed the model a
    /// wrong answer and made the whole classification unreachable.
    #[tokio::test]
    async fn an_unserializable_completion_value_is_invalid_output() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let code = r#"
const cyclic = {};
cyclic.self = cyclic;
return cyclic;
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=invalid-output"), "{out}");
        assert!(out.contains("could not be serialized"), "{out}");
    }

    /// A log argument that cannot be serialized stays lossy — a circular
    /// `console.log` must not fail the program.
    #[tokio::test]
    async fn an_unserializable_log_argument_does_not_fail_the_run() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let code = r#"
const cyclic = {};
cyclic.self = cyclic;
console.log(cyclic);
return "done";
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(out.contains("[object Object]"), "{out}");
    }

    /// Acceptance (#134): the heap budget trips and is classified `heap-limit`.
    #[tokio::test]
    async fn heap_budget_is_enforced_and_classified() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_limits(RunCodeLimits {
            heap_limit_mib: 16,
            ..RunCodeLimits::default()
        });
        let code = r#"
const ballast = [];
for (;;) ballast.push(new Array(100000).fill("xxxxxxxxxxxxxxxx"));
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=heap-limit"), "{out}");
        assert!(out.contains("heap budget"), "{out}");
    }

    /// The host-cancellation class: an external abort kills the run and is
    /// reported distinctly from a timeout.
    #[tokio::test]
    async fn host_abort_is_classified() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let token = tokio_util::sync::CancellationToken::new();
        let trigger = token.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let report = runner::run_program(RunProgramRequest {
            source: "await new Promise(() => {});".to_string(),
            bindings: BindingTable::empty(),
            invoker: Arc::new(RegistryInvoker(registry_with(Arc::new(Double)))),
            limits: RunCodeLimits::default(),
            node_bin: None,
            abort: Some(token),
        })
        .await;
        assert!(canceller.await.is_ok());
        assert_eq!(report.failure, Some(protocol::RunCodeFailure::Abort));
        assert!(report.render().contains("status=abort"));
    }
}
