// Invariant guard tests — automated checks for the 10 invariants in
// .dev/AGENTS.md.
//
// Each module below corresponds to one or more invariants:
//   loop_size_orthogonality:  #1 (loop size) + #2 (orthogonality)
//   sandbox:                  #3 (sandbox path resolution)
//   test_coverage:            #4 (test coverage)
//   finish_reason_data:       #7 (finish reasons are data, not errors)
//   tool_call_pairing:        #8 (tool-call ↔ tool-result pairing)
//   dep_justification:        #6 (no new deps without justification)
//   invariant_registry:       #9 (new tool → new file) + #10 (new provider
//                             → new file + trait), plus the numbering guard
//                             that keeps .dev/AGENTS.md and
//                             docs/architecture/invariants.md in sync.
//
//   #5 (no unwrap/expect) is covered by clippy::unwrap_used deny (Goal 224).
//
// Run with: `cargo test --test invariants`

#[path = "invariants/dep_justification.rs"]
mod dep_justification;
#[path = "invariants/finish_reason_data.rs"]
mod finish_reason_data;
#[path = "invariants/invariant_registry.rs"]
mod invariant_registry;
#[path = "invariants/loop_size_orthogonality.rs"]
mod loop_size_orthogonality;
#[path = "invariants/sandbox.rs"]
mod sandbox;
#[path = "invariants/test_coverage.rs"]
mod test_coverage;
#[path = "invariants/tool_call_pairing.rs"]
mod tool_call_pairing;
