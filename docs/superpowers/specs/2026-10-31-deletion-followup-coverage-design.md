# Deletion Follow-up Coverage Design (#170/#171/#172/#174/#175)

> Status: approved 2026-10-31. Short design for the follow-ups deferred by
> PR #169.

## Intent

PR #169 fixed the deferred-deletion flow, but four review follow-ups only
pinned part of the behavior: the warning count still hard-codes job statuses,
the unknown-reason regression test checks only the original file, the persisted
`unknown` string is untested, and the live-snapshot apply path has no test that
proves a failed read preserves state. The 2026-10-29 plan also still shows every
task as unchecked.

This batch closes those gaps without changing the user-facing deletion flow.

## Goals

- Count every selected photo that has a non-terminal compression job, including
  statuses written by a newer version.
- Prove that an unknown deletion reason leaves derivatives untouched as well as
  the original.
- Pin `DeletionReason::Unknown` to the persisted string `"unknown"` and keep
  `parse("unknown")` returning `None`.
- Exercise the live-snapshot apply path with a missing deletion snapshot and
  prove all three retained values survive without a false blocked toast.
- Mark the completed 2026-10-29 plan and record the checkbox rule for future
  plans.

## Non-goals

- Implementing the #142 send outbox. `DeletionJobSummary.send` stays empty and
  its SQL stays compression-only until that table exists.
- Changing deletion ordering, blocking rules, or UI copy.
- Adding a gpui entity test harness or DB schema changes.

## Approaches Considered

1. **One PR for code and docs.** Rejected: `AGENTS.md` §4 requires docs-only
   changes to stay separate from code changes.
2. **Two PRs: code (170/171/172/174) and docs (175 + this design + plan).**
   Chosen. It keeps CI cost proportional and lets the docs PR carry the plan.
3. **#170, enumerate non-terminal statuses vs `status NOT IN
   ('completed','cancelled')`.** Chose `NOT IN`. `QueueStatus::parse` already
   treats unknown strings as `Pending`, so the body count and the heading must
   agree for statuses the current binary does not know.
4. **#174, build a gpui entity test vs extract the apply step.** Chose
   extraction. `App` owns a `Window`, inputs, and subscriptions; constructing it
   in a unit test would add a test harness for one assertion. A small helper
   called by `apply_live_snapshot` exercises the same state transition without
   changing production behavior.

## Decisions

### #170 affected-photo count

`deletion_job_summary` counts distinct photos whose compression job status is
not `completed` or `cancelled`. This matches `QueueStatus::parse`: unknown
statuses become `Pending` in the body, so they must also count as affected.

The send side is deferred. A follow-up issue will add send-job coverage when
#142 introduces the outbox, and #170 can close with the compression and
unknown-status behavior fixed.

### #171 derivative survival

The existing daemon test gains a real derivative file and row. After the
unknown-reason request runs, the test asserts the original file, the derivative
file, and the derivative row all survive, plus the existing `blocked` /
`unknown_reason` assertions.

### #172 persisted reason string

Extend the existing model round-trip test: `Unknown.as_str() == "unknown"`,
`parse("unknown") == None`, and `parse("future_reason") == None`.

### #174 live apply path

Extract the deletion branch of `apply_live_snapshot` into
`apply_deletion_snapshot(requests, counts, baseline, snapshot)`. It returns
`None` when the snapshot is missing, leaving all three values untouched, and
otherwise returns whether state changed and which blocked count (if any) should
be announced. `apply_live_snapshot` keeps its current toast and notify behavior.

The test seeds a blocked request, counts, and matching baseline, applies
`None`, asserts the three values are unchanged, then applies a successful
snapshot with the same counts and asserts no blocked toast.

### #175 plan status and rule

Mark every step in `docs/superpowers/plans/2026-10-29-deletion-followups.md`
as `- [x]` and add a status note that the document is a post-hoc record of work
merged in PR #169. Add the standing rule to `AGENTS.md` §4: when a plan file is
committed, update its checkboxes on completion.

## Constraints

- Rust stable (edition 2024), rusqlite, gpui-kit 0.8.
- No new dependencies and no schema changes.
- UI copy stays Japanese; the existing heading format is unchanged.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, and `cargo test --workspace` must pass before push.
- Code and docs land in separate PRs, each with the required release-notes
  label (`bug` for the code PR, docs PR is documentation).
