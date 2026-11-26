#!/usr/bin/env bash
# Turns on GitHub's merge queue for main of komira-ai/jobscheduler: the ruleset "main" of
# docs/ci.md ("The merge queue") and the two squash message settings that section names. By hand
# only, by an admin of the repository; it is a held step.
#
# It is a dry run by default. A dry run reads the repository, ci.yml on main and the rules of
# main, prints the settings and the ruleset now, the ones it would write and the exact API calls,
# and changes nothing. With --apply it makes those calls (the squash settings first, so the queue
# never squashes with the default message, which copies every branch commit), reads both back,
# checks them and prints the calls that back the change out. It never forces: there is no
# --force, it deletes nothing and bypasses nothing, keeps every other rule of an existing ruleset
# "main", and stops at the first call that fails, saying what it had changed by then.
#
# It stops (exit 1) before any write when gh is not logged in, when the repository is not public
# (a private repository has a merge queue only on GitHub Enterprise Cloud), when ci.yml on main
# does not start on merge_group or has no job all-passed (the queue would wait for a check that
# never reports), or when a ruleset other than "main" already puts a merge queue or required
# checks on main.
#
# Usage: merge_queue.sh [--apply]
# It needs gh and jq on PATH.
set -euo pipefail

REPO=komira-ai/jobscheduler
NAME=main
TITLE=PR_TITLE
MESSAGE=BLANK

# The ruleset of docs/ci.md, word for word (crates/xtask/tests/merge_queue holds the two equal).
WANT=$(
  cat <<'JSON'
{
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": {
      "include": ["refs/heads/main"],
      "exclude": []
    }
  },
  "rules": [
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": false,
        "do_not_enforce_on_create": false,
        "required_status_checks": [
          { "context": "all-passed", "integration_id": 15368 }
        ]
      }
    },
    {
      "type": "merge_queue",
      "parameters": {
        "merge_method": "SQUASH",
        "grouping_strategy": "ALLGREEN",
        "max_entries_to_build": 5,
        "min_entries_to_merge": 1,
        "max_entries_to_merge": 5,
        "min_entries_to_merge_wait_minutes": 5,
        "check_response_timeout_minutes": 360
      }
    }
  ]
}
JSON
)

usage() {
  echo "usage: merge_queue.sh [--apply]   (a dry run without --apply; there is no --force)" >&2
  exit 2
}

APPLY=0
for arg in "$@"; do
  case $arg in
    --apply) APPLY=1 ;;
    *) usage ;;
  esac
done

stop() {
  printf 'STOP: %s\n' "$*" >&2
  exit 1
}

# The fields of a ruleset that a write sets, its rules in a fixed order, so two read the same.
shape() {
  jq -S '{name, target, enforcement, conditions, bypass_actors: (.bypass_actors // []), rules: (.rules | sort_by(.type))}'
}

if [ "$APPLY" = 1 ]; then echo "mode: APPLY"; else echo "mode: DRY RUN (run again with --apply to write)"; fi

gh auth status >/dev/null 2>&1 || stop "gh is not logged in"

repo=$(gh api "repos/$REPO")
visibility=$(jq -r .visibility <<<"$repo")
[ "$visibility" = public ] ||
  stop "$REPO is $visibility: a private repository has a merge queue only on GitHub Enterprise Cloud; nothing changed"
echo "== $REPO is public: its plan allows a merge queue"

ci=$(gh api -H "Accept: application/vnd.github.raw+json" "repos/$REPO/contents/.github/workflows/ci.yml?ref=main")
grep -q '^  merge_group:' <<<"$ci" ||
  stop "ci.yml on main does not start on merge_group: the queue would wait for a check that never reports; nothing changed"
grep -q '^  all-passed:' <<<"$ci" ||
  stop "ci.yml on main has no job all-passed, the check the ruleset requires; nothing changed"
echo "== ci.yml on main starts on merge_group and has the job all-passed"

# The ruleset "main" of the repository itself, if there is one (the list also holds the
# organization's rulesets; one of those named "main" is another ruleset).
id=$(gh api "repos/$REPO/rulesets" | jq -r --arg n "$NAME" '[.[] | select(.name == $n and .target == "branch" and .source_type == "Repository")][0].id // empty')

# Rules from any other ruleset (of this repository or of the organization) on main.
effect=$(gh api "repos/$REPO/rules/branches/main")
others=$(jq -r --arg id "${id:-none}" '[.[] | select((.type == "merge_queue" or .type == "required_status_checks") and ((.ruleset_id | tostring) != $id)) | "\(.type) (ruleset \(.ruleset_id))"] | join(", ")' <<<"$effect")
[ -z "$others" ] || stop "another ruleset already puts rules on main: $others; nothing changed"

settings_now=$(jq -c '{squash_merge_commit_title, squash_merge_commit_message}' <<<"$repo")
settings_want=$(jq -nc --arg t "$TITLE" --arg m "$MESSAGE" '{squash_merge_commit_title: $t, squash_merge_commit_message: $m}')
old_title=$(jq -r .squash_merge_commit_title <<<"$repo")
old_message=$(jq -r .squash_merge_commit_message <<<"$repo")

if [ -n "$id" ]; then
  now=$(gh api "repos/$REPO/rulesets/$id" | shape)
  new=$(jq -n --argjson cur "$now" --argjson want "$WANT" \
    '$want + {bypass_actors: $cur.bypass_actors, rules: ([$cur.rules[] | select(.type != "required_status_checks" and .type != "merge_queue")] + $want.rules)}')
  method=PUT
  path="repos/$REPO/rulesets/$id"
else
  now=none
  new=$WANT
  method=POST
  path="repos/$REPO/rulesets"
fi

echo "== the rules in effect on main now:"
jq . <<<"$effect"
echo "== squash message settings now:   $settings_now"
echo "== squash message settings after: $settings_want"
echo "== ruleset \"$NAME\" now:"
if [ "$now" = none ]; then echo none; else jq . <<<"$now"; fi
echo "== ruleset \"$NAME\" after:"
jq . <<<"$new"

patch_settings=0
[ "$settings_now" = "$settings_want" ] || patch_settings=1
write_ruleset=1
[ "$now" = none ] || [ "$(shape <<<"$now")" != "$(shape <<<"$new")" ] || write_ruleset=0

echo "== the calls:"
[ "$patch_settings" = 0 ] ||
  echo "gh api --method PATCH repos/$REPO -f squash_merge_commit_title=$TITLE -f squash_merge_commit_message=$MESSAGE"
[ "$write_ruleset" = 0 ] ||
  echo "gh api --method $method $path --input -   (the body: ruleset \"$NAME\" after, above)"
if [ "$patch_settings" = 0 ] && [ "$write_ruleset" = 0 ]; then
  echo "none: the merge queue is already on as docs/ci.md gives it"
  exit 0
fi
[ "$APPLY" = 1 ] || {
  echo "DRY RUN: nothing changed."
  exit 0
}

back_settings="gh api --method PATCH repos/$REPO -f squash_merge_commit_title=$old_title -f squash_merge_commit_message=$old_message"

if [ "$patch_settings" = 1 ]; then
  gh api --method PATCH "repos/$REPO" -f "squash_merge_commit_title=$TITLE" -f "squash_merge_commit_message=$MESSAGE" --silent ||
    stop "the squash settings call failed; nothing changed"
  echo "== the squash message settings are written"
fi

if [ "$write_ruleset" = 1 ]; then
  if ! written=$(gh api --method "$method" "$path" --input - <<<"$new"); then
    [ "$patch_settings" = 0 ] || echo "the squash message settings were written; to back them out: $back_settings" >&2
    stop "the ruleset call failed; the ruleset is as it was"
  fi
  [ -n "$id" ] || id=$(jq -r .id <<<"$written")
  echo "== ruleset \"$NAME\" is written: id $id"
fi

echo "== to back out:"
[ "$patch_settings" = 0 ] || echo "$back_settings"
if [ "$write_ruleset" = 1 ] && [ "$method" = POST ]; then
  echo "gh api --method DELETE repos/$REPO/rulesets/$id"
elif [ "$write_ruleset" = 1 ]; then
  echo "gh api --method PUT repos/$REPO/rulesets/$id --input -   (the body: ruleset \"$NAME\" now, above)"
fi

got_settings=$(gh api "repos/$REPO" | jq -c '{squash_merge_commit_title, squash_merge_commit_message}')
[ "$got_settings" = "$settings_want" ] || stop "the squash message settings read back as $got_settings"
got=$(gh api "repos/$REPO/rulesets/$id" | shape)
[ "$got" = "$(shape <<<"$new")" ] || stop "ruleset $id reads back as $(jq -c . <<<"$got")"
echo "DONE: main merges through the merge queue (ruleset $id), the required check is all-passed"
