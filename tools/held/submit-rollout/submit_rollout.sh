#!/usr/bin/env bash
# Rolls out, to the control plane's js-server, the newest release when it contains a given commit
# (docs/runbooks/submit-rollout.md). A release reaches the js-server Swarm service through its own
# release run: `cp-deploy` deploys the release's image and passes when the deploy converged
# (docs/release.md, "The control-plane deploy"). This script checks that the rollout happened and,
# only when that deploy failed, takes the documented recovery step: "Re-run failed jobs" of that
# run (docs/release.md, "Recovery"). It is a held step: owner-only, run on the Swarm node that runs
# the service's task (it reads the running container with `docker`), with `gh` logged in.
#
# It is a dry run by default. A dry run reads the newest release, whether it contains --commit, the
# version the running js-server container carries, and the jobs of the release's run, prints what
# it found and what it would do, and changes nothing. With `--apply` it re-runs the failed jobs of
# that run, and only when the run's control-plane deploy ended failed, cancelled or timed out. It
# never re-runs the run of an older release (a re-run would move the service back), never starts
# a deploy of its own, never forces anything, and removes no file.
#
# Usage:
#   submit_rollout.sh --commit SHA --out-dir D [--repo OWNER/NAME] [--service NAME] [--apply]
#
# The readings go to --out-dir, a directory that must exist (the workspace scratch directory).
# It needs `gh` and `docker` on PATH.
#
# Exit status: 0 the release runs, or the dry run found a re-run to make, or --apply started it;
# 1 a state the script will not act on (said on stderr); 2 usage or a refusal of the input;
# 3 not yet: no release contains the commit, or the release's deploy has not ended.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: submit_rollout.sh --commit SHA --out-dir D [--repo OWNER/NAME] [--service NAME] [--apply]
  dry run by default; --apply re-runs the failed jobs of the newest release's run, and only when
  its control-plane deploy failed
  --commit   the commit the running js-server must contain (a full 40-hex SHA)
  --repo     the repository of the releases (komira-ai/jobscheduler)
  --service  the Swarm service of the server, whose running container is read (js-server_server)
USAGE
  exit 2
}

fail() {
  printf 'submit_rollout.sh: %s\n' "$1" >&2
  usage
}

# Ends the run with status `$2` and the reason `$1` on stderr (a state, not a usage error).
stop() {
  printf 'submit_rollout.sh: %s\n' "$1" >&2
  exit "$2"
}

commit=''
out_dir=''
repo='komira-ai/jobscheduler'
service='js-server_server'
apply=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help | -h)
      usage
      ;;
    --apply)
      apply=1
      shift
      ;;
    --commit | --out-dir | --repo | --service)
      [ "$#" -ge 2 ] || fail "$1 needs a value"
      case "$1" in
        --commit) commit=$2 ;;
        --out-dir) out_dir=$2 ;;
        --repo) repo=$2 ;;
        --service) service=$2 ;;
      esac
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[ -n "$commit" ] || fail '--commit is required'
[ -n "$out_dir" ] || fail '--out-dir is required'
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail "--commit must be a full 40-hex SHA: $commit"
[[ "$repo" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || fail "--repo must be OWNER/NAME: $repo"
[ -n "$service" ] || fail '--service must not be empty'
[ -d "$out_dir" ] || fail "--out-dir is not a directory: $out_dir"

# 1. The newest release, and whether it contains the commit. Only the newest is ever rolled out:
# re-running the deploy of an older one would move the service back.
tag=$(gh release view --repo "$repo" --json tagName --jq .tagName)
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || stop "the newest release's tag is not vX.Y.Z: $tag" 1
version=${tag#v}
contains=$(gh api "repos/$repo/compare/$commit...$tag" --jq .status)
printf '%s %s %s\n' "$tag" "$commit" "$contains" > "$out_dir/release.txt"
case "$contains" in
  identical | ahead)
    printf 'newest release: %s (contains %s)\n' "$tag" "$commit"
    ;;
  *)
    printf 'newest release: %s (does not contain %s: %s)\n' "$tag" "$commit" "$contains"
    stop "no release contains $commit yet: wait for the release run of a commit that does" 3
    ;;
esac

# 2. What runs: the version label of each running container of the service on this node.
docker ps --no-trunc --filter "name=$service" --format '{{.ID}}' > "$out_dir/containers.txt"
running=()
while IFS= read -r id; do
  [ -n "$id" ] || continue
  running+=("$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$id")")
done < "$out_dir/containers.txt"
[ "${#running[@]}" -gt 0 ] ||
  stop "no running container of $service on this node: run the script on the node that runs it" 1
printf '%s\n' "${running[@]}" > "$out_dir/running.txt"
printf 'running %s: %s\n' "$service" "${running[*]}"
behind=0
for v in "${running[@]}"; do
  [ "$v" = "$version" ] || behind=1
done
if [ "$behind" -eq 0 ]; then
  printf 'ROLLED OUT: %s runs %s, which contains %s\n' "$service" "$version" "$commit"
  exit 0
fi

# 3. The release's run (the push run of the tag's commit) and its control-plane deploy.
tag_sha=$(gh api "repos/$repo/commits/$tag" --jq .sha)
run=$(gh run list --repo "$repo" --workflow release.yml --commit "$tag_sha" --event push \
  --json databaseId --jq '.[0].databaseId // empty')
[ -n "$run" ] || stop "no release run of $tag_sha: look at the Actions page before anything else" 1
gh run view "$run" --repo "$repo" --json jobs \
  --jq '.jobs[] | select(.name | startswith("cp-deploy")) | [.name, .status, (.conclusion // "")] | @tsv' \
  > "$out_dir/deploy-jobs.tsv"
printf 'release run: %s\n' "$run"
deploy=''
deploy_status=''
while IFS=$'\t' read -r name status conclusion; do
  [ -n "$name" ] || continue
  printf 'job %s: %s %s\n' "$name" "$status" "$conclusion"
  if [ "$status" != completed ]; then
    deploy_status=pending
  elif [ -z "$deploy_status" ] || [ "$deploy_status" = success ]; then
    case "$conclusion" in
      success) deploy_status=success ;;
      skipped) deploy_status=skipped ;;
      failure | cancelled | timed_out) deploy_status=failed deploy=$name ;;
      *) deploy_status="other:$conclusion" ;;
    esac
  fi
done < "$out_dir/deploy-jobs.tsv"

case "$deploy_status" in
  '' | pending)
    stop "the control-plane deploy of run $run has not ended: wait for it, then run this again" 3
    ;;
  success)
    stop "run $run reports the deploy converged, yet $service runs ${running[*]}, not $version: read the service (docker service ps $service) before anything else" 1
    ;;
  skipped)
    stop "run $run skipped the control-plane deploy (it is off: docs/release.md): nothing here deploys" 1
    ;;
  failed)
    ;;
  *)
    stop "run $run's deploy ended ${deploy_status#other:}: not a state this script acts on" 1
    ;;
esac

printf 'rollout: gh run rerun %s --repo %s --failed   (%s ended failed)\n' "$run" "$repo" "$deploy"
if [ "$apply" -eq 0 ]; then
  printf 'dry run: nothing was changed; run again with --apply to re-run the failed jobs\n'
  exit 0
fi
gh run rerun "$run" --repo "$repo" --failed
printf 'APPLIED: the failed jobs of run %s run again; run this again once they end to confirm\n' "$run"
