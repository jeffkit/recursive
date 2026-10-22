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
use protocol::SandboxFacts;
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
    /// Goal 416: the directory the program may read under the runtime
    /// permission model (the workspace root). `None` resolves to the host
    /// process's current directory at spawn time.
    read_root: Option<PathBuf>,
    /// Issue #134 W2: the session's event sink, held **weakly** (the
    /// [`WorkerEventSink`](crate::event::WorkerEventSink) precedent, issue
    /// #119). A strong handle would pin the run-scoped sink — and with it a
    /// `ChannelSink` sender — for as long as the registry clone lives,
    /// keeping every consumer that drains until the channel closes (the
    /// CLI printer) from ever finishing. The parent runtime publishes its
    /// sink through the telemetry-style slot below; once it drops the
    /// sink, the tool's events are discarded (the run that would have
    /// consumed them is over).
    sink_slot: std::sync::Mutex<Option<std::sync::Weak<dyn crate::event::EventSink>>>,
}

impl RunCode {
    /// Build the tool over the registry whose tools the program may call.
    pub fn new(tools: ToolRegistry) -> Self {
        Self {
            tools,
            limits: RunCodeLimits::default(),
            node_bin: None,
            read_root: None,
            sink_slot: std::sync::Mutex::new(None),
        }
    }

    /// Publish the session's event sink (issue #134 W2): each program run
    /// emits one [`AgentEvent::ProgrammaticRunStarted`] /
    /// [`AgentEvent::ProgrammaticRunFinished`] pair through it. Held weakly
    /// — a dead parent sink simply drops the events.
    pub fn with_event_sink(self, sink: std::sync::Arc<dyn crate::event::EventSink>) -> Self {
        *self
            .sink_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(std::sync::Arc::downgrade(&sink));
        self
    }

    /// Emit one PTC lifecycle event through the published sink, when it is
    /// still alive. Awaited inline (`execute` is already async): the sinks
    /// in use are non-blocking sends, and inline delivery keeps the
    /// started→finished ordering trivially monotonic.
    async fn emit(&self, event: crate::event::AgentEvent) {
        let sink = self
            .sink_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(sink) = sink {
            sink.emit(event).await;
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

    /// Pin the directory the program may read under the runtime permission
    /// model (the workspace root). Unpinned, the runner falls back to the
    /// host process's current directory — the same root the runtime process
    /// inherited before the permission model existed.
    pub fn with_read_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.read_root = Some(root.into());
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

/// Issue #134 W2: the sandbox facts as the `key=value` line a consumer
/// (SSE / collector) can render without re-deriving them from the model's
/// observation. Mirrors [`protocol::RunReport::render`]'s sandbox block,
/// minus the program output.
pub fn sandbox_summary(facts: &SandboxFacts) -> String {
    let mut out = format!(
        "runtime={} env={} heap_limit_mib={} output_limit_bytes={} timeout_secs={}",
        facts.runtime.as_deref().unwrap_or("unavailable"),
        facts.env_policy,
        facts.heap_limit_mib,
        facts.output_limit_bytes,
        facts.timeout_secs,
    );
    if !facts.fs_policy.is_empty() {
        out.push_str(&format!(" fs={}", facts.fs_policy));
    }
    if let Some(flag) = &facts.permission_flag {
        out.push_str(&format!(" permission_flag={flag}"));
    }
    if let Some(dir) = &facts.scratch_dir {
        out.push_str(&format!(" scratch={dir}"));
    }
    if !facts.resource_guards.is_empty() {
        out.push_str(&format!(" guards={}", facts.resource_guards));
    }
    out
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

        // Issue #134 W2: one started/finished pair per run — the visible
        // surface of a black-box step. No per-inner-call events (that would
        // dilute the batching into ToolCall noise).
        self.emit(crate::event::AgentEvent::ProgrammaticRunStarted {
            step: 0,
            program_chars: code.len(),
        })
        .await;
        let started = std::time::Instant::now();
        let report = runner::run_program(RunProgramRequest {
            source: code.to_string(),
            bindings,
            invoker: Arc::new(RegistryInvoker(self.tools.clone())),
            limits: self.limits,
            node_bin: self.node_bin.clone(),
            abort: None,
            read_root: self.read_root.clone(),
            min_free_bytes: None,
        })
        .await;
        let status = report
            .failure
            .map(|f| f.as_str().to_string())
            .unwrap_or_else(|| "ok".to_string());
        let sandbox_summary = crate::tools::run_code::sandbox_summary(&report.sandbox);
        self.emit(crate::event::AgentEvent::ProgrammaticRunFinished {
            step: 0,
            success: report.failure.is_none(),
            status,
            calls: report.calls,
            duration_ms: started.elapsed().as_millis() as u64,
            sandbox_summary,
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

    /// A binding that stays pending long enough for the runtime to exit
    /// mid-call — the issue #218 regression harness.
    struct Slow(Duration);

    #[async_trait]
    impl Tool for Slow {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "Slow".into(),
                description: "Resolve after a fixed delay".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, _arguments: Value) -> Result<String> {
            tokio::time::sleep(self.0).await;
            Ok("slow".into())
        }

        fn side_effect_class(&self) -> ToolSideEffect {
            ToolSideEffect::ReadOnly
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

    /// Issue #134 W2: a run with a published (live) sink emits exactly one
    /// started/finished pair carrying the aggregate (`calls`, duration,
    /// status, sandbox summary) — and never one event per inner tool call.
    #[tokio::test]
    async fn a_run_emits_started_and_finished_events() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let (sink, mut rx) = crate::event::ChannelSink::new();
        // Hold the sink Arc for the whole test: the tool takes the sink
        // **weakly** (the runtime owns the strong handle in production), so
        // a temporary Arc would die here and the weak upgrade would
        // legitimately drop every event.
        let sink = std::sync::Arc::new(sink);
        let tool = RunCode::new(registry_with(Arc::new(Double))).with_event_sink(sink.clone());
        let out = tool
            .execute(json!({
                "code": "let t = 0; for (const n of [1,2,3]) t += Number(await Double({n})); return t;"
            }))
            .await
            .unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(out.contains("calls=3"), "{out}");

        let mut started = None;
        let mut finished = None;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                crate::event::AgentEvent::ProgrammaticRunStarted { .. } => {
                    assert!(started.is_none(), "at most one started per run");
                    started = Some(());
                }
                crate::event::AgentEvent::ProgrammaticRunFinished {
                    success,
                    status,
                    calls,
                    duration_ms,
                    sandbox_summary,
                    ..
                } => {
                    assert!(finished.is_none(), "at most one finished per run");
                    assert!(success);
                    assert_eq!(status, "ok");
                    assert_eq!(calls, 3, "the aggregate counts the inner calls");
                    assert!(duration_ms > 0);
                    assert!(
                        sandbox_summary.contains("runtime="),
                        "the summary must carry the sandbox facts: {sandbox_summary}"
                    );
                    finished = Some(());
                }
                // A tool call inside the program must NOT surface as a
                // ToolCall event — no per-inner-call dilution.
                other => panic!("unexpected event from a run: {other:?}"),
            }
        }
        assert!(started.is_some(), "a started event must be emitted");
        assert!(finished.is_some(), "a finished event must be emitted");
    }

    /// Issue #134 W2: the failing classes are reported with their taxonomy
    /// string, exactly as the model's own observation reads.
    #[tokio::test]
    async fn a_failed_run_reports_its_taxonomy_status() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let (sink, mut rx) = crate::event::ChannelSink::new();
        // Held for the whole test — the tool's handle is weak (see the
        // emit test above).
        let sink = std::sync::Arc::new(sink);
        let tool = RunCode::new(registry_with(Arc::new(Double)))
            .with_event_sink(sink.clone())
            .with_limits(RunCodeLimits {
                timeout: Duration::from_millis(300),
                ..RunCodeLimits::default()
            });
        let out = tool
            .execute(json!({ "code": "await new Promise(() => {});" }))
            .await
            .unwrap();
        assert!(out.contains("status=timeout"), "{out}");
        let mut saw_finished = false;
        while let Ok(ev) = rx.try_recv() {
            if let crate::event::AgentEvent::ProgrammaticRunFinished {
                success, status, ..
            } = ev
            {
                saw_finished = true;
                assert!(!success, "a timeout is not a success");
                assert_eq!(status, "timeout", "the taxonomy string must match");
            }
        }
        assert!(
            saw_finished,
            "the finish event must actually reach the sink"
        );
    }

    /// Issue #134 W2 (negative control): no published sink (or a dropped
    /// parent) means no events and no failure — the tool result is unchanged.
    #[tokio::test]
    async fn a_run_without_a_sink_still_runs() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let out = tool.execute(json!({"code": "return 1;"})).await.unwrap();
        assert!(out.contains("status=ok"), "{out}");
    }

    /// The sandbox summary mirrors the model's own observation block so a
    /// consumer never re-derives facts from rendered text.
    #[test]
    fn sandbox_summary_mirrors_the_rendered_facts() {
        let facts = SandboxFacts {
            runtime: Some("/usr/bin/node".into()),
            env_policy: "cleared".into(),
            heap_limit_mib: 512,
            output_limit_bytes: 2 * 1024 * 1024,
            timeout_secs: 120,
            fs_policy: "fs-write=scratch-only".into(),
            permission_flag: Some("--permission".into()),
            scratch_dir: Some("/tmp/scratch".into()),
            resource_guards: "rlimit_as=enforced(1024MiB)".into(),
        };
        let summary = sandbox_summary(&facts);
        assert!(summary.contains("runtime=/usr/bin/node"), "{summary}");
        assert!(summary.contains("env=cleared"), "{summary}");
        assert!(summary.contains("heap_limit_mib=512"), "{summary}");
        assert!(summary.contains("fs=fs-write=scratch-only"), "{summary}");
        assert!(
            summary.contains("permission_flag=--permission"),
            "{summary}"
        );
        assert!(summary.contains("scratch=/tmp/scratch"), "{summary}");
        assert!(summary.contains("guards=rlimit_as=enforced"), "{summary}");

        // Unavailable sandbox: the facts stay honest.
        let summary = sandbox_summary(&SandboxFacts::default());
        assert!(summary.contains("runtime=unavailable"), "{summary}");
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

    /// Issue #218: a runtime that exits while a call-result is still being
    /// written back must not swallow the failure — the write error is
    /// classified `protocol` and names the call, instead of surfacing as a
    /// bare `worker-exit` while the program side waits for a result that can
    /// never arrive.
    // windows（#224）：产品行为正确（status=protocol、点名 call 1），差异只在错误浮现的步骤——
    // unix 的 write_all 对死管道即 EPIPE（"writing call-result"），windows 匿名管道先吞下
    // write、flush 才报 ERROR_NO_DATA 232（"flushing call-result"）；子串断言钉的是 unix 措辞。
    #[cfg_attr(
        windows,
        ignore = "#224: on windows the dead-pipe error surfaces at flush (ERROR_NO_DATA 232, 'flushing call-result') rather than at write (unix EPIPE, 'writing call-result'); the protocol classification itself holds"
    )]
    #[tokio::test]
    async fn a_lost_call_result_write_is_reported_as_protocol_error() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Slow(Duration::from_millis(400)))));
        // The runtime issues the call, then exits before the result can be
        // written back: the host's write lands on a dead pipe.
        let code = r#"
setTimeout(() => process.exit(7), 30);
return await Slow();
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=protocol"), "{out}");
        assert!(
            out.contains("writing call-result 1 to runtime stdin failed"),
            "{out}"
        );
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

    /// Issue #134 review: the wall-clock budget stays authoritative even when
    /// something could hold the inherited stdout/stderr pipes past the kill.
    /// The stderr reader used to be awaited unbounded, so a 0.4s budget
    /// produced a ~20s call after the runtime was killed.
    ///
    /// Goal 416 re-point: the runtime permission model now denies
    /// `child_process` outright (that is the security baseline this goal
    /// adds), so the detached-holder *scenario* is prevented at the source —
    /// the program's spawn attempt throws. The host-side guarantee the test
    /// pins is unchanged and still exercised end-to-end: the run hits its
    /// wall-clock budget, the group kill fires, and the bounded stderr join
    /// returns the call in bounded time. The program parks after the denial
    /// so the timeout path (kill + bounded joins) is what actually runs.
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
try {
  const cp = await import('node:child_process');
  cp.spawn(process.execPath, ['-e', 'setTimeout(() => {}, 20000)'], {
    detached: true,
    stdio: ['ignore', 'inherit', 'inherit'],
  }).unref();
  console.log("spawn unexpectedly allowed");
} catch (err) {
  console.log("spawn denied by the permission model");
}
await new Promise(() => {});
"#;
        let started = std::time::Instant::now();
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        let elapsed = started.elapsed();
        assert!(out.contains("status=timeout"), "{out}");
        assert!(
            out.contains("spawn denied by the permission model"),
            "the permission model must deny the spawn: {out}"
        );
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
            read_root: None,
            min_free_bytes: None,
        })
        .await;
        assert!(canceller.await.is_ok());
        assert_eq!(report.failure, Some(protocol::RunCodeFailure::Abort));
        assert!(report.render().contains("status=abort"));
    }

    // ── Goal 416 — live-node security baseline ─────────────────────────────

    /// Live-node (#134 W1): `child_process` is denied by the runtime
    /// permission model — the mod.rs:458 detach-holder escape (spawning a
    /// grandchild process) is dead, loudly, inside the program.
    #[tokio::test]
    async fn child_process_import_is_denied() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let code = r#"
const cp = await import('node:child_process');
try {
  cp.spawnSync('echo', ['pwned']);
  console.log("SPAWN ALLOWED");
} catch (err) {
  console.log("spawn denied: " + err.message.slice(0, 40));
}
try {
  cp.execSync('echo pwned');
  console.log("EXEC ALLOWED");
} catch (err) {
  console.log("exec denied: " + err.message.slice(0, 40));
}
return "checked";
"#;
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(out.contains("spawn denied:"), "{out}");
        assert!(out.contains("exec denied:"), "{out}");
        assert!(!out.contains("SPAWN ALLOWED"), "{out}");
        assert!(!out.contains("EXEC ALLOWED"), "{out}");
    }

    /// Live-node (#134 W1): a direct workspace write outside the scratch dir
    /// is denied by the permission model. The program's only writable
    /// location is its per-run scratch dir; the read root stays read-only.
    #[tokio::test]
    async fn workspace_direct_write_outside_scratch_is_denied() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let ws = tempfile::TempDir::new().expect("workspace tempdir");
        let tool = RunCode::new(registry_with(Arc::new(Double)))
            .with_read_root(ws.path())
            .with_node_bin(runner::locate_node().expect("node must be on PATH for this test"));
        let ws_path = ws.path().display().to_string().replace('\\', "\\\\");
        let code = format!(
            r#"
const fs = require('fs');
try {{
  fs.writeFileSync("{ws_path}/escalated.txt", "pwned");
  console.log("WORKSPACE WRITE ALLOWED");
}} catch (err) {{
  console.log("workspace write denied: " + err.message.slice(0, 40));
}}
// The scratch dir (the spawn cwd) is writable.
fs.writeFileSync("in-scratch.txt", "fine");
return "checked";
"#
        );
        let out = tool.execute(json!({ "code": code })).await.unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(out.contains("workspace write denied:"), "{out}");
        assert!(!out.contains("WORKSPACE WRITE ALLOWED"), "{out}");
        assert!(
            !ws.path().join("escalated.txt").exists(),
            "nothing may land in the read root: {}",
            ws.path().display()
        );
    }

    /// Live-node (#134 W1): the rendered fact is honest about what the
    /// permission model is — a seat belt, not a malicious-code boundary.
    /// `node:sqlite` is Node's own documented bypass ("does not guarantee
    /// that users will not be able to access the file system through other
    /// means"); verified live on node 26.10 (macOS and Linux):
    /// `new DatabaseSync('<outside>')` + `exec('CREATE TABLE …')` writes a
    /// file outside every fs grant while `fs.writeFileSync` to the same path
    /// is denied. The fact string must say so — a model reading this
    /// observation must never believe `scratch-only` means it cannot write
    /// elsewhere, and the operator must never read it as hostile-code
    /// containment (that is the container/microVM tier's job).
    #[tokio::test]
    async fn fs_policy_fact_names_the_node_sqlite_bypass() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        let out = tool
            .execute(json!({ "code": "return 'probe';" }))
            .await
            .unwrap();
        assert!(out.contains("status=ok"), "{out}");
        assert!(
            out.contains("fs-write=scratch-only"),
            "the fact keeps its headline: {out}"
        );
        assert!(
            out.contains("node:sqlite bypasses fs grants"),
            "the fact must disclose the sqlite bypass, not claim hard confinement: {out}"
        );
        assert!(
            out.contains("seat-belt"),
            "the fact must carry the seat-belt qualifier: {out}"
        );
    }

    /// Live-node (#134 W1): the address-space guard is what the sandbox facts
    /// claim it is. On a Linux kernel (which enforces RLIMIT_AS) the child
    /// sees the lowered limit — it reads `/proc/self/limits`, which the
    /// runner grants explicitly (`--allow-fs-read=/proc`; verified: the
    /// permission model has no /proc carve-out and denies the read with
    /// `ERR_ACCESS_DENIED` otherwise). On a kernel that refuses to lower it
    /// (macOS) the fact string must say `not-enforced-by-kernel` rather than
    /// claim a cap that does not exist.
    // windows（#224）：同 runner::tests::resource_guard_fact_matches_the_enforcement_completeness
    // ——cfg(not(unix)) 事实串为 "rlimit_as=not-probed(platform)"，else 分支认的是 macOS 的
    // "not-enforced-by-kernel"。windows 无 setrlimit/RLIMIT_AS 等价。
    #[cfg_attr(
        windows,
        ignore = "#224: windows has no RLIMIT_AS; the cfg(not(unix)) fact is 'not-probed(platform)', not the unix 'not-enforced-by-kernel' this test's else branch encodes"
    )]
    #[tokio::test]
    async fn address_space_guard_fact_matches_the_child_view() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let tool = RunCode::new(registry_with(Arc::new(Double)));
        // Read the limit the child inherited: glibc exposes getrlimit only
        // natively, so go through /proc on Linux and assert the fact string
        // elsewhere.
        if cfg!(target_os = "linux") {
            let code = r#"
const text = require('fs').readFileSync('/proc/self/limits', 'utf8');
const line = text.split('\n').find((l) => l.includes('Max address space'));
console.log(line);
return line;
"#;
            let out = tool.execute(json!({ "code": code })).await.unwrap();
            assert!(
                out.contains("status=ok"),
                "the child must be able to read /proc/self/limits: {out}"
            );
            assert!(
                out.contains("Max address space"),
                "the child must report its own limit: {out}"
            );
            assert!(
                out.contains("rlimit_as=enforced("),
                "Linux facts must claim enforcement: {out}"
            );
        } else {
            let out = tool
                .execute(json!({ "code": "return 'probe';" }))
                .await
                .unwrap();
            assert!(out.contains("status=ok"), "{out}");
            assert!(
                out.contains("rlimit_as=not-enforced-by-kernel"),
                "a kernel that cannot lower RLIMIT_AS must be labeled, not lied about: {out}"
            );
        }
    }
}
