#!/usr/bin/env bash
# Watch GitHub Actions runs for a branch to completion, tracking the latest
# run dynamically (a new push supersedes the old run as `cancelled`).
# Usage: ci-watch.sh <branch> [workflow]   (workflow default: CI)
set -u
# Fall back to env BRANCH/WF when $1/$2 are empty.
BRANCH="${1:-${BRANCH:-}}"
WF="${2:-${WF:-CI}}"
[ -n "$BRANCH" ] || { echo "usage: ci-watch.sh <branch> [workflow]" >&2; exit 2; }
TRACK=""
resolve() { gh run list --branch "$BRANCH" --workflow "$WF" --limit 5 --json databaseId,status,conclusion --jq '[.[] | select(.conclusion != "cancelled")] | ((map(select(.status=="in_progress" or .status=="queued" or .status=="waiting" or .status=="requested")) | sort_by(.databaseId) | last) // (sort_by(.databaseId) | last)) | .databaseId // empty'; }
for i in $(seq 1 90); do
  id=$(resolve)
  if [ -z "$id" ]; then echo "$(date -u +%H:%M:%S) waiting for run..."; sleep 30; continue; fi
  [ "$id" != "$TRACK" ] && { [ -n "$TRACK" ] && echo "switch $TRACK -> $id"; TRACK="$id"; }
  s=$(gh run view "$TRACK" --json status,conclusion --jq '.status + "/" + .conclusion')
  echo "$(date -u +%H:%M:%S) $TRACK $s"
  case "$s" in */cancelled) TRACK="";; completed/*) break;; *) sleep 60;; esac
done
gh run view "$TRACK" --json status,conclusion,jobs --jq '{status, conclusion, jobs: [.jobs[] | {name, conclusion}]}'
