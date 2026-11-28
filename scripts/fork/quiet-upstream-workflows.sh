#!/usr/bin/env bash
# The fork's nightly sync (.github/workflows/fork-upstream-sync.yml, docs/web-mcp.md): turns off
# upstream's workflows in this fork and cancels their runs that are still waiting. They ask for
# upstream's own runners (Blacksmith), which a fork does not have, so every push to main left
# runs queued for a day before GitHub gave up on them.
#
# Kept running: the fork's own workflows (files named fork-*.yml) and any file named in the
# repository variable SYNC_KEEP_WORKFLOWS (space separated, for example "docker.yml").
# Needs GH_TOKEN with Actions read and write, and GITHUB_REPOSITORY (owner/name).
set -euo pipefail

repo="${GITHUB_REPOSITORY:?owner/name}"
keep=" ${SYNC_KEEP_WORKFLOWS:-} "

kept() {
  local file="${1##*/}"
  file="${file%%@*}"
  case "$file" in fork-*) return 0 ;; esac
  case "$keep" in *" $file "*) return 0 ;; esac
  return 1
}

gh api --paginate "repos/$repo/actions/workflows" --jq '.workflows[] | select(.state == "active") | "\(.id) \(.path)"' |
  while read -r id path; do
    kept "$path" && continue
    echo "turning off $path"
    gh api --silent -X PUT "repos/$repo/actions/workflows/$id/disable" || echo "::warning::could not turn off $path"
  done

for status in queued pending waiting requested; do
  gh api --paginate "repos/$repo/actions/runs?status=$status&per_page=100" --jq '.workflow_runs[] | "\(.id) \(.path)"' |
    while read -r id path; do
      kept "$path" && continue
      echo "cancelling run $id ($path, $status)"
      gh api --silent -X POST "repos/$repo/actions/runs/$id/cancel" || true
    done
done
