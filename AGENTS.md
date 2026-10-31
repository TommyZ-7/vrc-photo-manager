## 0. Project profile (fill in per project)

- Project: `vrc-photo-manager`
- Stack / language: `Rust stable (edition 2024) + gpui-kit 0.8 + SQLite`
- Main branch: `main`
- Tag pattern: `v*.*.*`
- Local verify commands: see §3.

## 1. Branch operation

- Direct push to `main` is forbidden. Work on feature branches
  (`feat/*`, `fix/*`, `docs/*`, `ci/*`, `chore/*`).
- Basic flow: PR → `CI` green → merge into `main`.
- Every PR must carry a release-notes label: `enhancement` for features,
  `bug` for fixes. Release notes are generated from merged PRs and
  categorized by label (`.github/release.yml`, §7); an unlabeled PR falls
  into the catch-all category. Mapping details: `docs/release.md`.
- Keep `main` always releasable. Never merge a broken state.
- Releases are tag pushes only. Example: `git tag v0.2.0 && git push origin v0.2.0`
- AI must not perform release operations (tag create/push, manual Release run,
  `gh release` publish) without the user's explicit instruction.
- Creating/switching branches in the same workdir (`checkout -b` / `switch`)
  is forbidden. Always start new work with `git worktree`
  (enforced by `opencode.json` permissions):

  ```bash
  REPO=$(basename "$(git rev-parse --show-toplevel)")
  git worktree add ../${REPO}-<suffix> -b <branch>
  ```

  → open a separate session in the new worktree → after merge, `remove` + `prune`.
- Allowed operations: switching to `main`, path-scoped restore
  (`checkout -- <path>`), `worktree`. Details: `parallel-worktree` skill.
- After a merge, switch to `main` and pull. Do not stay on the work branch.
- Delete merged work branches locally and remotely
  (`git branch -d` + `git push origin --delete`).
- Always cut a new branch from an updated `main`
  (`checkout main && pull`, plus `status` clean check first).

## 2. CI trigger conditions (split to save Actions minutes)

- `.github/workflows/ci.yml` (light: lint + test + build check)
  - `pull_request`, `push: branches: [main]`, `workflow_dispatch`
  - Do NOT put `paths-ignore` on `pull_request`: required checks would stay
    "Expected" forever on docs-only PRs. Instead, detect docs-only changes in
    a `changes` job and fast-pass (echo green) — see `ci.yml.template`.
  - `push` may use `paths-ignore` for docs-only paths (`docs/**`, `**.md`,
    `LICENSE`, plus `packaging/**/*.md`).
  - Cancel superseded runs on the same ref (`cancel-in-progress: true`).
- `.github/workflows/release.yml` (heavy: bundle + Release publish)
  - `push: tags: ['v*.*.*']`, `pull_request` (job-side `changes` gate;
    no `paths` filter), `workflow_dispatch`
  - On PRs, run the bundle only when packaging-relevant files changed
    (`packaging/**`, `Packager.toml`, any `Cargo.toml`, `Cargo.lock`,
    `.github/workflows/release.yml`, `.github/release.yml`) or the PR
    carries the `bundle-check` label. `crates/**`-only changes do not
    trigger the bundle; code correctness is covered by CI.
  - Run it manually only when needed: `gh workflow run Release --ref <branch>`
  - Release publishing (`gh release create/upload`) runs on tags only.
    AI must not publish on its own (§1).

## 3. Pre-push local verification (reduce CI round-trips)

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

- Keep this list to what a typical dev machine can run in a few minutes.
  Full Linux package verification goes through the `Release` workflow.

## 4. Commit conventions

- Prefixes: `feat:`, `fix:`, `docs:`, `ci:`, `chore:`.
- Docs-only changes are assumed to skip heavy CI. Do not mix code and docs
  in the same PR.
- 計画ファイル (`docs/superpowers/plans/`) をコミットする場合、実行完了時に
  チェックボックスを `- [x]` に更新する。

## 5. CI result check obligation

- After every `push`, check CI results. Never leave red unattended and never
  start the next task while red.
- At PR creation and before merge, confirm all checks are green.
  Merging on non-green is forbidden.
- Follow the `ci-watch` skill for the procedure.

## 6. CI monitoring automation

- Prefer polling over `gh run watch` when annotations are unreliable in your
  environment (e.g. 403 on annotation fetch).
- Foreground shells time out (e.g. 120s limit) — always monitor in background.
- Never pin the run ID at start. A new push regenerates the run and the old
  one ends as `cancelled`; re-resolve the latest run on every poll.
- Implementation source of truth: `.opencode/skills/ci-watch/scripts/ci-watch.sh`
  (do not paste its body into this file — reference it).
- Rerun: relaunch the script in background with `BRANCH`/`WF` args. Monitors
  are read-only, so duplicate launches are harmless; merge/fix decisions stay
  centralized on completion notification.
- Default post-completion behavior: success → auto-merge
  (`gh pr merge <n> --merge`, then switch to `main` + pull);
  failure → analyze logs and push a fix on the branch.

## 7. Release procedure (v*.*.*)

1. Version bump (single source of truth: `Cargo.toml` (`workspace.package.version`) + `Cargo.lock`).
2. Branch → PR → `CI` + `Release` (bundle) green → merge into `main`
   (per §1, §5).
3. Tag the updated `main` (only with explicit user instruction):

   ```bash
   git checkout main && git pull
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

4. The tag-triggered `Release` workflow rebuilds artifacts from the tagged
   commit and publishes the GitHub Release (monitor per §6).
- The PR-time + tag-time double build is intentional (guarantees published
  artifacts come from the tagged commit; do not simplify).
- On PRs the bundle jobs (`build` / `windows`) are gated by the `changes`
  job: they run only when packaging-relevant files change (`packaging/**`,
  `Packager.toml`, any `Cargo.toml`, `Cargo.lock`, or the Release workflow
  files) or when the PR carries the `bundle-check` label. Otherwise they
  are skipped, which is the expected state; code correctness is covered
  by CI.
- If the bundle vendors third-party runtimes, record provenance + ship
  required license notices (add a staging check in `release.yml`).
- Release notes compare with the previous `v*.*.*` tag and are
  generated from merged PRs by GitHub's generate-notes API (categories
  are defined in `.github/release.yml`: `enhancement` -> `✨ Added`,
  `bug` -> `🐛 Fixed`, everything else -> `🔧 Improved`). The
  version-bump PR itself is dropped, and commits that belong to no PR
  are appended under `その他の変更`. The publish job regenerates and
  overwrites the body even when the Release was created manually
  beforehand.
- Verify publication:
  `gh release view <tag> --json assets --jq '.assets[].name'` and body check
  `gh release view <tag> --json body --jq .body`

## 8. External issues / PRs

- 新規 Issue は `triage` ラベルで受け、再現確認後に `needs-repro` /
  `platform:*` / `area:*` を付与する。
- 外部 fork の PR は初回のみ Actions の承認が必要。承認前にコードを実行しない。
- マージ条件は §5 と同じ。マージ後は head ブランチを削除する。
- セキュリティ報告は公開 Issue で扱わず、`SECURITY.md` の非公開レポートへ誘導する。
