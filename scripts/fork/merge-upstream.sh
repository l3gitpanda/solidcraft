#!/usr/bin/env bash
# The fork's nightly sync (.github/workflows/fork-upstream-sync.yml, docs/web-mcp.md): merges
# upstream's main into the branch checked out here, without pushing anything.
#
# Writes key=value lines to $GITHUB_OUTPUT (stdout when unset):
#   upstream=<sha>  behind=<commits this branch lacks>
#   status=current  nothing to merge
#   status=merged   merged and committed (lockfile=rebuilt when cargo had to settle Cargo.lock)
#   status=conflict nothing committed; conflicts=<the clashing files, space separated>
#
# The lockfile is left as cargo would write it. A clash in Cargo.lock alone is settled here:
# upstream's lockfile is taken and cargo adds the fork's own crates back to it, keeping every
# version upstream chose.
set -euo pipefail

url="${UPSTREAM_URL:-https://github.com/tangency-labs/solidcraft.git}"
ref="${UPSTREAM_REF:-main}"
out="${GITHUB_OUTPUT:-/dev/stdout}"

git fetch --no-tags --quiet "$url" "$ref"
upstream="$(git rev-parse FETCH_HEAD)"
behind="$(git rev-list --count "HEAD..$upstream")"
echo "upstream=$upstream" >>"$out"
echo "behind=$behind" >>"$out"

if [ "$behind" -eq 0 ]; then
  echo "status=current" >>"$out"
  exit 0
fi

message="Merge upstream $ref ($behind new commits, ${upstream:0:9})"
if git merge --quiet --no-edit -m "$message" "$upstream"; then
  # A merge can leave a lockfile cargo would still change (both sides touched one package's
  # entry); let cargo settle it now, so the checks' --locked judges the code, not the merge.
  cargo metadata --quiet --format-version 1 >/dev/null
  if ! git diff --quiet -- Cargo.lock; then
    git add Cargo.lock
    git commit --quiet --amend --no-edit
    echo "lockfile=rebuilt" >>"$out"
  fi
  echo "status=merged" >>"$out"
  exit 0
fi

mapfile -t clashes < <(git diff --name-only --diff-filter=U)
if [ "${#clashes[@]}" -eq 0 ]; then
  echo "git merge failed without a conflict; see the log above" >&2
  exit 1
fi

if [ "${#clashes[@]}" -eq 1 ] && [ "${clashes[0]}" = "Cargo.lock" ]; then
  git checkout --theirs -- Cargo.lock
  cargo metadata --quiet --format-version 1 >/dev/null
  git add Cargo.lock
  git commit --quiet --no-edit
  echo "status=merged" >>"$out"
  echo "lockfile=rebuilt" >>"$out"
  exit 0
fi

git merge --abort
echo "status=conflict" >>"$out"
echo "conflicts=${clashes[*]}" >>"$out"
