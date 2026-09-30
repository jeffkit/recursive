# Issue #50 — microvm 单租户文档/注释/启动 WARN 修正

- Date: 2026-09-30
- Goal: docs + 注释 + HTTP serve 启动 WARN 三处修正，使 issue #50 的 4 个源级测试转绿；不改运行时语义。
- Files touched:
  - `docs/architecture/execution-environments.md`（顶部部署警告、Data tenancy 行、Non-goals 交叉引用）
  - `src/tools/e2b_provider.rs`（模块头注释改为 per provider / per process）
  - `crates/recursive-cli/src/main.rs`（`Cmd::Http` 分支新增 MicroVm 判定 + WARN eprintln）
  - `tests/issue50_microvm_shared_vm_warning.rs`（调查产物，已存在，未改断言）
- Tests added: 无新增（本 issue 测试由调查阶段产出，本次仅使其转绿：`cargo test --test issue50_microvm_shared_vm_warning` 4/4 绿）。
- Notes: clippy `-p recursive-cli --all-targets -D warnings` 干净；`cargo fmt --all --check` 干净。全量质量门由管线 gate_runner 独立执行（未在本次手动运行）。
EOF
