---
name: Parallel Worktree
description: Work on another branch in a separate session without disturbing the current checkout. Use when starting parallel work while CI runs or another session is active.
---

## Workflow

1. Create one worktree per session/branch as a sibling of the repo root.
   Replace `<repo>` with the repository directory basename (e.g. `my-app`):

   ```bash
   REPO=$(basename "$(git rev-parse --show-toplevel)")
   git worktree add ../${REPO}-<suffix> -b <branch>
   ```

   For an existing branch: `git worktree add ../${REPO}-<suffix> <branch>`
2. Open the new session in the new worktree directory.
3. Keep a 1:1 session-to-branch mapping. Never check out the same branch in
   two worktrees (git forbids it) and never switch branches in a worktree
   owned by another session. Branch create/switch shell commands should be
   denied by project `opencode.json` permissions (except switching to `main`
   and path restore) — create branches only via `git worktree add -b`.
   Note: shell permission matching is whole-command text, so compound
   commands smuggling a checkout (e.g. via `&&`) fall back to ask — stay in
   the worktree flow regardless.
4. Clean up after merge: `git worktree remove ../${REPO}-<suffix>`,
   then `git worktree prune`. Verify with `git worktree list`.

## Notes

- Build artifacts are per-worktree (e.g. `target/`, `node_modules/`,
  `.venv/`); each worktree needs its first build. Sharing a build cache
  across simultaneous builds may block on a lock — prefer per-worktree
  artifacts unless the toolchain documents safe sharing.
- Branches, tags and remotes are shared across worktrees. Merge into `main`
  one at a time per `AGENTS.md` §1/§5.
- Template variable: `<suffix>` is typically the branch short name
  (e.g. branch `feat/login` → worktree `../<repo>-login`).
