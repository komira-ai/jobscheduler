#!/usr/bin/env bash
# Rolls out, to the control plane's js-server, the newest release when it contains a given commit
# (docs/runbooks/submit-rollout.md). A release reaches the js-server Swarm service through its own
# release run: the run publishes the release, then `cp-deploy` deploys its image and passes when the
# deploy converged (docs/release.md, "The control-plane deploy"). This script checks that the
# rollout happened and, when a release run stopped it, takes the documented recovery step:
# "Re-run failed jobs" of that run (docs/release.md, "Recovery"). It is a held step: owner-only, run
# on the Swarm node that runs the service's task (it reads the running container with `docker`),
# with `gh` logged in.
#
# It is a dry run by default. A dry run reads the newest release and whether it contains --commit.
# When it does not, it reads the commit's own release run; when it does, the version the running
# js-server container carries and the newest release's run. It prints what it found and what it
# would do, and changes nothing but the readings it writes to --out-dir. With `--apply` it re-runs
# the failed jobs of one run (`gh run rerun --failed`, which re-runs every failed job of the run, as
# the recovery does), only once that whole run has ended, and only:
#   - the commit's own release run, when it failed before it published and no release contains the
#     commit; or
#   - the newest release's run, when the service does not run that release and the run's
#     control-plane deploy failed, was cancelled or timed out, or was skipped because another job
#     of the run failed.
# It never re-runs the run of a release older than the newest (a re-run would move the service
# back), never starts a deploy of its own, never forces anything, and removes no file.
#
# Usage:
#   submit_rollout.sh --commit SHA --out-dir D [--repo OWNER/NAME] [--service NAME] [--apply]
#
# The readings go to --out-dir, a directory that must exist (the workspace scratch directory).
# It needs `gh` and `docker` on PATH.
#
# Exit status: 0 the release runs, or the dry run found a re-run to make, or --apply started it;
# 1 a state the script will not act on (said on stderr); 2 usage, --help or a refusal of the input;
# 3 not yet: a release run that would release or deploy the commit has not ended, or the commit has
# no release run (a later commit's release will carry it).
# A `gh` or `docker` call that fails (say, a re-run GitHub refuses) ends it with that call's status.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: submit_rollout.sh --commit SHA --out-dir D [--repo OWNER/NAME] [--service NAME] [--apply]
  dry run by default; --apply re-runs the failed jobs of a release run that stopped the rollout:
  the commit's own run when it failed before publishing, or the newest release's run when its
  control-plane deploy failed
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

# Reads run $1 into the file $2 of --out-dir: the run's status on the first line, then one line per
# job: its name, status and conclusion, separated by tabs.
read_run() {
  gh run view "$1" --repo "$repo" --json status,jobs \
    --jq '.status, (.jobs[] | [.name, .status, (.conclusion // "")] | @tsv)' > "$out_dir/$2"
}

# The failed jobs of the run read into the file $1 of --out-dir, joined by ", ".
failed_jobs() {
  local name status conclusion list=''
  while IFS=$'\t' read -r name status conclusion; do
    case "$conclusion" in
      failure | cancelled | timed_out | startup_failure) list="${list:+$list, }$name" ;;
    esac
  done < <(tail -n +2 "$out_dir/$1")
  printf '%s' "$list"
}

# Re-runs the failed jobs of run $1 (its status is $2), for the reason $3: the documented
# recovery. GitHub re-runs only a run that has ended. A dry run names the re-run; --apply makes it.
rerun() {
  local run=$1 status=$2 why=$3
  [ "$status" = completed ] ||
    stop "run $run is still $status: its failed jobs can be re-run once it ends; run this again then" 3
  printf 'rollout: gh run rerun %s --repo %s --failed   (%s; it re-runs every failed job of the run)\n' \
    "$run" "$repo" "$why"
  if [ "$apply" -eq 0 ]; then
    printf 'dry run: nothing was changed; run again with --apply to re-run the failed jobs\n'
    exit 0
  fi
  gh run rerun "$run" --repo "$repo" --failed
  printf 'APPLIED: the failed jobs of run %s run again; run this again once they end to confirm\n' "$run"
  exit 0
}

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
    # The commit's own release run: a run that failed before it published releases nothing until
    # its failed jobs run again (docs/release.md, "Recovery"); one still going is waited for.
    own=$(gh run list --repo "$repo" --workflow release.yml --commit "$commit" --event push \
      --json databaseId --jq '.[0].databaseId // empty')
    [ -n "$own" ] ||
      stop "no release contains $commit yet, and it has no release run of its own: the release of a later commit will carry it" 3
    read_run "$own" commit-run.tsv
    own_status=$(head -n 1 "$out_dir/commit-run.tsv")
    printf 'release run of %s: %s (%s)\n' "$commit" "$own" "$own_status"
    own_failed=$(failed_jobs commit-run.tsv)
    if [ -z "$own_failed" ]; then
      [ "$own_status" != completed ] ||
        stop "no release contains $commit, yet its release run $own ended with no failed job: read that run's plan before anything else" 1
      stop "no release contains $commit yet: its release run $own is still $own_status; wait for it, then run this again" 3
    fi
    published=$(tail -n +2 "$out_dir/commit-run.tsv" | while IFS=$'\t' read -r name status conclusion; do
      if [ "$name" = publish ] && [ "$conclusion" = success ]; then echo yes; fi
    done)
    [ -z "$published" ] ||
      stop "release run $own published, yet the newest release does not contain $commit: read the releases before anything else" 1
    rerun "$own" "$own_status" "its release run failed before publishing: $own_failed"
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

# 3. The release's run (the push run of the tag's commit): its own state, then every job's.
tag_sha=$(gh api "repos/$repo/commits/$tag" --jq .sha)
run=$(gh run list --repo "$repo" --workflow release.yml --commit "$tag_sha" --event push \
  --json databaseId --jq '.[0].databaseId // empty')
[ -n "$run" ] || stop "no release run of $tag_sha: look at the Actions page before anything else" 1
read_run "$run" run.tsv
run_status=$(head -n 1 "$out_dir/run.tsv")
printf 'release run: %s (%s)\n' "$run" "$run_status"

# The control-plane deploy's state, whatever order its jobs are listed in: a job not ended wins,
# then a failed one, then an unknown conclusion, then a skipped one; all passed is success.
rank() {
  case "$1" in
    pending) echo 5 ;;
    failed) echo 4 ;;
    other:*) echo 3 ;;
    skipped) echo 2 ;;
    success) echo 1 ;;
    *) echo 0 ;;
  esac
}
deploy_status=''
deploy=''
upstream=''
while IFS=$'\t' read -r name status conclusion; do
  [ -n "$name" ] || continue
  case "$conclusion" in
    failure | cancelled | timed_out | startup_failure) failed=1 ;;
    *) failed=0 ;;
  esac
  case "$name" in
    cp-deploy*) ;;
    *)
      [ "$failed" -eq 0 ] || upstream="${upstream:+$upstream, }$name"
      continue
      ;;
  esac
  printf 'job %s: %s %s\n' "$name" "$status" "$conclusion"
  if [ "$status" != completed ]; then
    this=pending
  elif [ "$failed" -eq 1 ]; then
    this=failed
  else
    case "$conclusion" in
      success) this=success ;;
      skipped) this=skipped ;;
      *) this="other:$conclusion" ;;
    esac
  fi
  if [ "$(rank "$this")" -gt "$(rank "$deploy_status")" ]; then
    deploy_status=$this
    if [ "$this" = failed ]; then
      deploy=$name
    fi
  fi
done < <(tail -n +2 "$out_dir/run.tsv")

case "$deploy_status" in
  '' | pending)
    stop "the control-plane deploy of run $run has not ended: wait for it, then run this again" 3
    ;;
  success)
    stop "run $run reports the deploy converged, yet $service runs ${running[*]}, not $version: read the service (docker service ps $service) before anything else" 1
    ;;
  skipped)
    [ -n "$upstream" ] ||
      stop "run $run skipped the control-plane deploy and no job of it failed (the deploy is off: docs/release.md): nothing here deploys" 1
    deploy="the deploy, skipped after $upstream failed"
    ;;
  failed)
    deploy="$deploy failed"
    ;;
  *)
    stop "run $run's deploy ended ${deploy_status#other:}: not a state this script acts on" 1
    ;;
esac

rerun "$run" "$run_status" "$deploy"
