#!/usr/bin/env bash
# Runs the live conformance suite of one machine provider (ec2, gce or lambdalabs) against the
# real cloud, by hand only (docs/runbooks/live-conformance.md). It is a held step: owner-only,
# and spending needs the CEO's go.
#
# It is a dry run by default. A dry run reads the provider's catalog for the shape, prints the
# plan with a plan hash, lists what an earlier run with the same id left behind, and launches
# nothing. It spends only with `--spend --plan-hash H`, where H is the hash a dry run printed for
# the same plan; the suite itself refuses when the plan it computes now hashes to something else
# (a changed price or shape). It never forces anything: there is no --force, and it removes no
# file. The suite ends every spending run with a cleanup pass that terminates what the run left.
#
# Usage:
#   provider_live.sh --provider ec2|gce|lambdalabs --run-id ID --location L --shape S
#                    [--image I] [--arch A] [--project P] [--network N]
#                    [--subnetwork N] [--credential-file F] [--capture-dir D]
#                    [--spend --plan-hash H]
#
# --location is a region (ec2, lambdalabs) or a zone (gce). A flag whose value is empty is
# dropped, so a caller can pass every flag and leave the unused ones empty. --credential-file is
# the service account key for gce (exported as GOOGLE_APPLICATION_CREDENTIALS) and the API key
# file for lambdalabs; ec2 takes none and reads AWS_* or the instance role. The credential's
# content is never printed, only the path.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: provider_live.sh --provider ec2|gce|lambdalabs --run-id ID --location L --shape S
                        [--image I] [--arch A] [--project P] [--network N]
                        [--subnetwork N] [--credential-file F] [--capture-dir D]
                        [--spend --plan-hash H]
  dry run by default; it spends only with --spend --plan-hash H, H printed by a dry run
  ec2:         --image required; --arch x86_64|arm64; --capture-dir
  gce:         --project and --image required; --arch; --network; --subnetwork; --capture-dir;
               --credential-file (a service account key)
  lambdalabs:  --credential-file (the API key file) required
USAGE
  exit 2
}

fail() {
  printf 'provider_live.sh: %s\n' "$1" >&2
  usage
}

provider=''
run_id=''
location=''
shape=''
image=''
arch=''
project=''
network=''
subnetwork=''
credential_file=''
capture_dir=''
plan_hash=''
spend=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help | -h)
      usage
      ;;
    --spend)
      spend=1
      shift
      ;;
    --provider | --run-id | --location | --shape | --image | --arch | --project | --network | \
      --subnetwork | --credential-file | --capture-dir | --plan-hash)
      [ "$#" -ge 2 ] || fail "$1 needs a value"
      case "$1" in
        --provider) provider=$2 ;;
        --run-id) run_id=$2 ;;
        --location) location=$2 ;;
        --shape) shape=$2 ;;
        --image) image=$2 ;;
        --arch) arch=$2 ;;
        --project) project=$2 ;;
        --network) network=$2 ;;
        --subnetwork) subnetwork=$2 ;;
        --credential-file) credential_file=$2 ;;
        --capture-dir) capture_dir=$2 ;;
        --plan-hash) plan_hash=$2 ;;
      esac
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

case "$provider" in
  ec2 | gce | lambdalabs) ;;
  '') fail "--provider is required" ;;
  *) fail "--provider is ec2, gce or lambdalabs, not $provider" ;;
esac
[ -n "$run_id" ] || fail "--run-id is required"
[ -n "$location" ] || fail "--location is required"

if ! [[ $run_id =~ ^[a-z0-9-]{1,40}$ ]]; then
  fail "--run-id is 1 to 40 characters of a-z, 0-9 and -"
fi
if [ "$spend" -eq 1 ] && [ -z "$plan_hash" ]; then
  fail "--spend needs --plan-hash, the hash a dry run printed"
fi
if [ "$spend" -eq 0 ] && [ -n "$plan_hash" ]; then
  fail "--plan-hash is only for --spend"
fi
if [ -n "$plan_hash" ] && ! [[ $plan_hash =~ ^[0-9a-f]{64}$ ]]; then
  fail "--plan-hash is 64 lowercase hex digits"
fi

# The flags each provider takes; a value for any other is a mistake, not something to drop.
refuse_unless() {
  local name=$1 value=$2
  shift 2
  local allowed
  for allowed in "$@"; do
    [ "$allowed" != "$provider" ] || return 0
  done
  [ -z "$value" ] || fail "$name is not a flag of $provider"
}
refuse_unless --image "$image" ec2 gce
refuse_unless --arch "$arch" ec2 gce
refuse_unless --capture-dir "$capture_dir" ec2 gce
refuse_unless --project "$project" gce
refuse_unless --network "$network" gce
refuse_unless --subnetwork "$subnetwork" gce
if [ "$provider" = ec2 ] && [ -n "$credential_file" ]; then
  fail "--credential-file is not a flag of ec2: it reads AWS_* or the instance role"
fi

[ -n "$shape" ] || fail "--shape is required: the suite launches only the shape it is told"
case "$provider" in
  ec2)
    [ -n "$image" ] || fail "--image is required for ec2"
    ;;
  gce)
    [ -n "$project" ] || fail "--project is required for gce"
    [ -n "$image" ] || fail "--image is required for gce"
    ;;
  lambdalabs)
    [ -n "$credential_file" ] || fail "--credential-file is required for lambdalabs"
    ;;
esac
if [ -n "$credential_file" ] && ! [ -r "$credential_file" ]; then
  fail "--credential-file $credential_file is not a readable file"
fi

# The suite runs from the repository root, so a relative path is made absolute against the
# directory the script was started in.
absolute() {
  case $1 in
    /*) printf '%s' "$1" ;;
    *) printf '%s/%s' "$PWD" "$1" ;;
  esac
}
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null || (cd "$script_dir/../../.." && pwd))
if [ -n "$credential_file" ]; then
  credential_file=$(absolute "$credential_file")
fi
if [ -n "$capture_dir" ]; then
  capture_dir=$(absolute "$capture_dir")
  # Raw answers hold real ids and addresses: keep them out of the checkout, so they are never
  # committed by mistake.
  case $capture_dir/ in
    "$root"/*) fail "--capture-dir $capture_dir is inside the repository; choose a directory outside it" ;;
  esac
fi

command=(cargo test --locked -p "js-provider-$provider" --features live --test live --
  --run-id "$run_id")
if [ "$provider" = gce ]; then
  command+=(--zone "$location")
else
  command+=(--region "$location")
fi
add() {
  local name=$1 value=$2
  if [ -n "$value" ]; then
    command+=("$name" "$value")
  fi
}
add --shape "$shape"
add --image "$image"
add --arch "$arch"
add --project "$project"
add --network "$network"
add --subnetwork "$subnetwork"
add --capture-dir "$capture_dir"
if [ "$provider" = lambdalabs ]; then
  add --key-file "$credential_file"
fi
if [ "$spend" -eq 1 ]; then
  command+=(--spend --plan-hash "$plan_hash")
fi

cd "$root"

if [ "$spend" -eq 1 ]; then
  echo "SPEND: this run launches machines and costs money"
else
  echo "DRY RUN: nothing is launched"
fi
if [ "$provider" = gce ] && [ -n "$credential_file" ]; then
  export GOOGLE_APPLICATION_CREDENTIALS=$credential_file
  printf 'GOOGLE_APPLICATION_CREDENTIALS=%q\n' "$credential_file"
fi
printf 'cd %q\n' "$root"
printf '%q ' "${command[@]}"
printf '\n'
exec "${command[@]}"
