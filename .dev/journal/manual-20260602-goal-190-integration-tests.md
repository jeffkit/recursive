# Manual edit: goal-190-integration-tests

**Date**: 2026-06-02
**Goal**: Goal 190 — CloudRuntime: 集成测试套件
**Files touched**:
- `.dev/goals/190-cloud-runtime-integration-tests.md` (new)
- `tests/v060_storage_integration.rs` (new)
- `src/lib.rs` (export `FsPolicy`, `PolicyConfig`, `ShellPolicy`)

**Tests added**:
- `v060::local_storage_backend_round_trip`
- `v060::local_storage_memory_round_trip`
- `v060::noop_session_store_save_load_does_not_error`
- `v060::checkpoint_state_serde_round_trip`
- `v060::kernel_builder_defaults_build_successfully`
- `v060::kernel_builder_accepts_explicit_storage_backend`
- `v060::policy_sandbox_blocks_forbidden_command`
- `v060::policy_sandbox_allows_permitted_command`
- `v060::restrictive_policy_preset_blocks_dangerous_commands`
- `v060::kernel_builder_with_policy_provider_builds`
- `v060::redis_session_store_construction_succeeds` (#[cfg(feature = "cloud-runtime")])
- `v060::checkpoint_state_json_is_stable` (#[cfg(feature = "cloud-runtime")])

**Notes**:
- Redis/S3 tests use lazy-connect or pure serialization checks; no external
  service required during CI.
- `transcript_key`/`key` are private methods; cloud-runtime tests validate
  construction and serialization instead of key format directly.
- All 12 tests pass under both default and `--features cloud-runtime` build.
