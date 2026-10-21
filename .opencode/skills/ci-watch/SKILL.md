---
name: CI Watch
description: Monitor GitHub Actions runs for a branch to completion with dynamic latest-run tracking. Use when waiting on CI or Release workflow results after a push or PR creation.
---

## Workflow

1. Identify target: `BRANCH` (e.g. `feat/xxx`) and `WF` (workflow name, e.g. `CI`).
2. Start `scripts/ci-watch.sh <branch> [workflow]` in a background shell
   (foreground shells time out, so background execution is required).

   ```bash
   .opencode/skills/ci-watch/scripts/ci-watch.sh <branch> <workflow> > /tmp/opencode/ci-watch.log 2>&1
   ```

   - `<workflow>` defaults to `CI` when omitted. Tail the log for progress:
     `tail /tmp/opencode/ci-watch.log`.
   - Do not append `&`/`nohup` to the command. In-shell backgrounding escapes
     tool management, so no completion notification is delivered.
   - Do not combine `export` and invocation on one line
     (`BRANCH=... cmd "$BRANCH"` leaves `"$VAR"` empty at expansion time;
     the script's env fallback exists but should not be relied on).
3. On completion notification, check the final summary and act:
   - success → merge flow (`gh pr merge <n> --merge`, switch to `main`, pull).
   - failure → fetch logs, analyze, fix on the branch.
4. Judgment on merge/fix stays centralized on notification; monitors are read-only.

## Rules

- Never pin the run ID at start. A new push regenerates the run and the old
  one ends as `cancelled`; re-resolve the latest run on every poll.
- Prefer `gh run list` / `gh run view` for status checks. `gh pr checks` may
  fail with limited tokens depending on the environment.
- If `gh run watch` returns annotation errors (e.g. 403) in your environment,
  use this polling script instead.
- Details and recovery: `AGENTS.md` §5/§6.
