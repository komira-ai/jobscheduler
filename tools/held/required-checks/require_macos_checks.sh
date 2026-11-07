#!/usr/bin/env bash
# Makes the macOS checks of a pull request required checks of main: `macos` (ci.yml) and
# `js-agent-macos-pkg` (js-agent-macos-pkg.yml). A held step: it changes the repository's branch
# protection, which only the owner sets (docs/release.md, "Configuration the project owner must set"). Run it
# after the pull request that makes every pull request run those two checks is merged, so that
# no open pull request waits on a check it does not run.
#
# It is a dry run by default: it reads the protection of main and prints what it would change,
# and changes nothing. It changes the protection only with `--apply`, and then only adds the two
# checks: when main has a protection, the two contexts are added to its required status checks
# (the ones already there stay); when it has none, the protection set is required status checks
# of exactly those two contexts, nothing else (no review rule, no push restriction, admins not
# bound). It never removes a check, never forces and touches nothing but main.
#
# Usage:
#   require_macos_checks.sh [--repo OWNER/NAME] [--apply]
#
# --repo defaults to komira-ai/jobscheduler. It calls `gh api` with the caller's own login.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: require_macos_checks.sh [--repo OWNER/NAME] [--apply]
  dry run by default; it changes main's branch protection only with --apply
USAGE
  exit 2
}

repo=komira-ai/jobscheduler
apply=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo)
      [ "$#" -ge 2 ] && [ -n "$2" ] || usage
      repo="$2"
      shift 2
      ;;
    --apply)
      apply=true
      shift
      ;;
    *)
      usage
      ;;
  esac
done
case "$repo" in
  */*) ;;
  *) usage ;;
esac

checks=(macos js-agent-macos-pkg)
base="repos/$repo/branches/main/protection"

# The required status checks of main: absent (404, no protection or none of its checks), or
# the contexts listed. Any other failure stops the script before it changes anything.
if out="$(gh api "$base/required_status_checks" --jq '.contexts[]' 2>&1)"; then
  state=present
  current="$out"
elif printf '%s\n' "$out" | grep -q 'HTTP 404'; then
  state=absent
  current=""
else
  printf 'reading the protection of main in %s failed:\n%s\n' "$repo" "$out" >&2
  exit 1
fi

missing=()
for check in "${checks[@]}"; do
  if ! printf '%s\n' "$current" | grep -qxF "$check"; then
    missing+=("$check")
  fi
done

echo "repository: $repo, branch main"
if [ "$state" = present ]; then
  echo "required status checks now: ${current//$'\n'/ }"
else
  echo "required status checks now: none (main has no protection of its status checks)"
fi
if [ "${#missing[@]}" -eq 0 ]; then
  echo "nothing to do: ${checks[*]} are required already"
  exit 0
fi
echo "to add: ${missing[*]}"

if [ "$apply" != true ]; then
  echo "dry run: nothing changed; run again with --apply to add them"
  exit 0
fi

if [ "$state" = present ]; then
  args=()
  for check in "${missing[@]}"; do
    args+=(-f "contexts[]=$check")
  done
  gh api --method POST "$base/required_status_checks/contexts" "${args[@]}" > /dev/null
else
  gh api --method PUT "$base" --input - > /dev/null <<JSON
{"required_status_checks":{"strict":false,"contexts":["macos","js-agent-macos-pkg"]},"enforce_admins":false,"required_pull_request_reviews":null,"restrictions":null}
JSON
fi
echo "added: ${missing[*]}"
