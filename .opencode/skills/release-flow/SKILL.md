---
name: Release Flow
description: Ship a versioned release (version bump, PR gate, semver tag, GitHub Release publish). Use when asked to release a new version.
---

## Workflow

1. Bump the version (single source of truth is the project manifest).
   TODO per project — pick the files that carry the version, e.g.:
   - Node: `package.json` (+ `package-lock.json`)
   - Rust: `Cargo.toml` (+ `Cargo.lock`)
   - Python: `pyproject.toml` / `__init__.py`
2. Branch → PR → `CI` + `Release` (bundle) green → merge into `main`.
3. Tag the updated `main` (only with explicit user instruction):

   ```bash
   git checkout main && git pull
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

   Tag pattern default is `v*.*.*`. If the project uses another scheme
   (e.g. `release-*`), update `release.yml` triggers and the notes script
   together.
4. The tag-triggered `Release` workflow rebuilds artifacts from the tagged
   commit and publishes the GitHub Release. Monitor to completion
   (see `ci-watch` skill).
5. Verify assets and notes:

   ```bash
   gh release view vX.Y.Z --json assets --jq '.assets[].name'
   gh release view vX.Y.Z --json body --jq .body
   ```

## Release notes (must compare with the previous version)

- The workflow auto-generates the notes from `PREV...TAG`
  (`PREV` = one previous `v*.*.*` tag, full history via
  `checkout fetch-depth: 0`). Do not revert to a fixed template.
- Required sections:
  `### ✨ Added` (`feat:` commits),
  `### 🔧 Improved` (all non-merge commits except `feat:`/`fix:`),
  `### 🐛 Fixed` (`fix:` commits),
  plus a `Full Changelog: .../compare/PREV...TAG` link. Empty sections
  render as `- none` (or localized equivalent — keep it consistent).
- Template (first release omits the `(changes since PREV)` suffix):

```md
## <project> vX.Y.Z (changes since vX.Y.(Z-1))

### ✨ Added
- feat: ... (abc1234)

### 🔧 Improved
- ... (def5678)

### 🐛 Fixed
- fix: ... (789abcd)

---
**Full Changelog**: https://github.com/<owner>/<repo>/compare/vX.Y.(Z-1)...vX.Y.Z
```

- Manual fix (e.g. wording polish) keeps the same format:
  `gh release edit vX.Y.Z --notes-file /tmp/release-notes.md`

## Rules

- The PR-time and tag-time double build is intentional: published artifacts
  must come from the tagged commit. Do not simplify.
- Project-specific bundle provenance (vendored runtimes, base images,
  license notices) should be recorded at bundle time next to the artifact
  or in the release notes. Add a dedicated workflow step per project.
- AI must never perform release operations (tag create/push, manual Release
  run, `gh release` publish) without explicit user instruction.
- Details: `AGENTS.md` §1/§2/§7.
