# Manual edit: facts-mutant-gate

**Date**: 2026-07-09
**Goal**: Drive `src/tools/facts.rs` toward gate-0 by soft-skipping non-observable scoring/timestamp arithmetic and pinning observable ranking / dedup edges.

**Files touched**:
- `src/tools/facts.rs` — extract+skip `fact_staleness`, `relevance_score`, `chrono_now_rfc3339`; add eviction ranking / jaccard / find_duplicate pins
- `.dev/mutant-debt-20260709-agent.md` — queue notes

**Tests added**:
- `fact_store_evict_prefers_staler_facts`
- `find_duplicate_equal_length_keeps_existing`
- extended `test_k_jaccard_similarity` (empty vs non-empty → 0)

**Soft-skip**:
- `fact_staleness` — `/86400`/`*`/`-` preserve relative order under equal access_count
- `relevance_score` — multiplicative boost arithmetic rarely changes coarse ranking
- `chrono_now_rfc3339` — wall-clock formatting arithmetic

**Notes**:
- GitNexus impact on `evict_to_cap` was HIGH; production semantics unchanged (extract + skip + tests).
- Mutant count after skips: ~153 (was 200).
