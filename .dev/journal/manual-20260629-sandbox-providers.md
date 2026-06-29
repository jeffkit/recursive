# Manual edit: sandbox-providers

**Date**: 2026-06-29
**Goal**: Implement Goal-319 (MemFsToolSetProvider) and Goal-320 (FirecrackerToolSetProvider)
**Files touched**:
- `src/tools/memfs_provider.rs` (new)
- `src/tools/firecracker_provider.rs` (new)
- `src/tools/mod.rs` (register new modules + re-exports)
- `src/lib.rs` (re-export new public types)

**Tests added**:
- `memfs_provider::tests` — kv store ops, exec_shell simulation (ls, cat, echo, mkdir, grep), ToolSetProvider trait conformance, MemFsGlobTool, MemFsBashTool, MemFsGrepTool
- `firecracker_provider::tests` — kvm_available, FirecrackerConfig defaults, provider sandbox_mode, VsockResponse deserialization, VsockRequest serialization; Linux-only: api parse_status_code, api parse_body

**Notes**:
- MemFsToolSetProvider (L0): pure in-memory, zero-overhead, HashMap<PathBuf,Vec<u8>> backing. Simulates ls/pwd/cat/echo/mkdir/rm/grep. Good for testing, diagnostics, and scenarios not requiring real execution.
- FirecrackerToolSetProvider (L3): local Firecracker VMM integration. REST API over Unix socket for VM config; JSON-RPC over vsock for exec/file ops. Gated on `#[cfg(target_os = "linux")]`. kvm_available() check at runtime.
- No new Cargo dependencies added (vsock protocol uses UTF-8 strings to avoid base64 crate).
- `config_file::tests::test_load_layered_permissions_loads_user_and_project` is a pre-existing flaky test (races on HOME env var in parallel test runs); passes when run in isolation.
