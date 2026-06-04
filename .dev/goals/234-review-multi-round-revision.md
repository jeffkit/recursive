# Goal 234: Multi-round Revision in self-improve.sh

## Summary

Allow up to N revision rounds (default N=2) in the self-improve loop's
automated review pipeline. Currently only one revision is attempted; if the
revision still has issues, the run commits as-is. This change loops review →
revision until the reviewer approves or the round limit is hit.

## Motivation

Complex goals often need more than one revision pass. The current single-round
limit means the orchestrator must manually catch residual issues in §3.4.1.
With N=2, the automated pipeline can resolve two layers of problems without
human intervention — at the cost of one additional LLM call pair per failing
goal.

## What to implement

In `.dev/scripts/self-improve.sh`, replace the current single-revision block
with a loop:

```bash
MAX_REVISION_ROUNDS="${RECURSIVE_MAX_REVISION_ROUNDS:-2}"
REVISION_ROUND=0

while [[ "$REVISION_ROUND" -lt "$MAX_REVISION_ROUNDS" ]]; do
  # Run review
  REVIEW_JSON=$(...)
  VERDICT=$(...)
  
  if [[ "$VERDICT" != "request_changes" ]]; then
    break   # approved — exit loop
  fi
  
  REVISION_ROUND=$(( REVISION_ROUND + 1 ))
  echo "[self-improve] review rejected (round $REVISION_ROUND/$MAX_REVISION_ROUNDS), feeding back..."
  
  # Extract issues, build revision prompt, run revision agent
  # (same logic as current single-revision block)
  
  # Re-run cargo test after revision
  # If test fails, roll back (don't try another round)
done

if [[ "$VERDICT" == "request_changes" ]]; then
  echo "[self-improve] review still rejecting after $MAX_REVISION_ROUNDS rounds — committing with warnings"
fi
```

Key constraints:
- `RECURSIVE_MAX_REVISION_ROUNDS` env var controls the limit (default 2).
  Setting it to 1 restores the current behaviour.
- After each revision, re-run `cargo test`. If tests fail, roll back
  immediately — don't attempt another round.
- Each revision round's transcript is saved as
  `${TRANSCRIPT_OUT%.json}-revision-${ROUND}.json`.
- If all rounds exhaust and the reviewer still rejects, **commit anyway** (same
  behaviour as today — orchestrator does final review).
- Log clearly: `[self-improve] revision round N/M`.

## Implementation notes

- This changes `.dev/scripts/self-improve.sh` only (meta-tooling, not `src/`).
- The review script call and issue extraction logic is already present in the
  current single-revision block — refactor it into the loop, not duplicate it.
- Keep the loop readable: extract the review-call and revision-run into
  clearly named local variables.

## Acceptance

```bash
# With a goal that the reviewer will reject twice:
# Verify the log shows "revision round 1/2" and "revision round 2/2"
# Verify two revision transcript files are created
# Verify cargo test is run after each revision
```

Set `RECURSIVE_MAX_REVISION_ROUNDS=1` and confirm it behaves identically to
the current single-revision logic.
