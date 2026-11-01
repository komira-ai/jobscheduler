#!/usr/bin/env bash
# crun_host_setup.sh: sets up a Linux host to run js-agent with `--runtime crun`. Run it as root, on one host at a
# time, with the agent stopped and the host drained.
#
# What a host needs before `js-agent --runtime crun` can start (the agent's own start check lists the same pieces and
# exits 2 naming each one that is missing):
#   1. packages    crun and uidmap from the distribution (apt), /usr/bin/crun owned by the crun package (never a
#                  copy: the distribution's AppArmor profile attaches to that path), crun 1.14 or newer
#   2. job user    a system account (default jsjob), no login, no home: the agent stays root and runs every crun as it
#   3. subids      a block of 65536 ids in /etc/subuid and /etc/subgid for that user, found so that it overlaps no
#                  range and no passwd or group id on the host
#   4. scratch     the scratch filesystem's mount unit gets prjquota added to its Options= (the existing options are
#                  kept) and the filesystem is unmounted and mounted again through the unit, so the agent can
#                  put a project quota on a run's directory. Refused while anything holds the mount open
#   5. dirs        /etc/js-agent 0700, /var/lib/js-agent 0711, its state 0700, scratch 0711 and scratch/runs 0711 (all
#                  root's). One that exists, is root's and has another mode is chmod'ed to the listed mode (the old
#                  mode is recorded); one not root's, or a link, is refused
#   6. apparmor    where kernel.apparmor_restrict_unprivileged_userns is 1, the distribution's profile for
#                  /usr/bin/crun is loaded (loaded here when it is installed and not loaded; refused when absent)
#   7. unit        /etc/systemd/system/js-agent-crun.service and /etc/tmpfiles.d/js-agent-crun.conf, from the files
#                  packaging/js-agent/js-agent-crun.service and tmpfiles-crun.conf of the jobscheduler release, pinned
#                  by sha256; the unit is NOT enabled or started
#
# Usage:
#   crun_host_setup.sh [setup flags]                  dry run (the default): read-only, prints each step and what it
#                                                     would do. Exit 0 = a plan, 2 = refused (nothing would be
#                                                     changed), 3 = nothing to do (the host already has every piece)
#   crun_host_setup.sh --apply [setup flags]          the same checks, then the changes. Every refusal comes before
#                                                     any write. Never overwrites a file that differs, never edits an
#                                                     existing user, never uses a force flag of apt or dpkg. A rerun
#                                                     on a host that is set up changes nothing (exit 3)
#   crun_host_setup.sh --help
# Setup flags:
#   --job-user NAME           default jsjob (the unit names it: --job-user in its ExecStart)
#   --subid-start N           first id of the block (default: the first free block from 200000)
#   --unit FILE --unit-sha256 HEX            js-agent-crun.service (apply needs both)
#   --tmpfiles FILE --tmpfiles-sha256 HEX    tmpfiles-crun.conf (apply needs both)
#   --server URL --pool NAME  written into the unit in place of the placeholders (apply needs both)
#   --scratch-mount-unit NAME the systemd mount unit of /var/lib/js-agent/scratch, a file in /etc/systemd/system
#                             (default var-lib-js\x2dagent-scratch.mount)
#   --skip-scratch-quota      leave the scratch mount as it is (a host whose scratch is not a mount unit)
#   --root DIR                run against the tree DIR instead of /, as the user who owns DIR (for tests)
# Configuration is these flags only; the script reads no environment variable of its own.
#
# State: /var/lib/js-crun-host-setup/manifest (root, 0600): one line per thing this script created or changed, written
# after the step succeeded, kept as the record of what an apply did (there is no rollback verb: each line says what to
# undo). Exit codes: 0 done or planned, 1 a step failed (the manifest says what was done), 2 refused or bad usage,
# 3 nothing to do.

set -euo pipefail

JOB_USER=jsjob
SUBID_COUNT=65536
SUBID_MIN=200000
SUBID_START=""
CRUN=/usr/bin/crun
CRUN_MIN=1.14
KERNEL_MIN=5.11
# The unit's DelegateSubgroup= needs systemd 254 (Ubuntu 24.04 ships 255).
SYSTEMD_MIN=254
SCRATCH=/var/lib/js-agent/scratch
MOUNT_UNIT='var-lib-js\x2dagent-scratch.mount'
SKIP_QUOTA=0
QUOTA_OPTION=prjquota
# path mode, parents first
DIRS=("/etc/js-agent 0700" "/var/lib/js-agent 0711" "/var/lib/js-agent/state 0700" "/var/lib/js-agent/scratch 0711" "/var/lib/js-agent/scratch/runs 0711")
PLACEHOLDER_SERVER='--server SET-THE-SERVER-WITH-SYSTEMCTL-EDIT'
PLACEHOLDER_POOL='--pool default'
PLACEHOLDER_USER='--job-user jsjob'
# JS_AGENT is the installed agent. A build wired to run jobs with crun prints this exact line in --version; a build
# without crun prints `runtimes: docker`, an older one no runtimes line.
JS_AGENT=/usr/bin/js-agent
WIRED_LINE="runtimes: docker crun"

ROOT=""             # "" = the host itself; --root DIR = a tree (the tests' fake root)
EXPECT_UID=0        # the owner of what the script makes and checks
APPLY=0
UNIT_FILE="" UNIT_SHA="" TMPF_FILE="" TMPF_SHA="" SERVER="" POOL=""
RUN_MODE=plan       # plan: print what would be done; apply: do it
PLANNED=0
REFUSALS=0
FAILED=0
FILES_CHANGED=0
LAST_ACT=""
ABORT=0             # set when a step fails in a way that makes the steps after it unsafe

say() { printf '%s\n' "$*"; }
die() { printf 'STOP: %s\n' "$*" >&2; exit 2; }
refuse() { REFUSALS=$((REFUSALS + 1)); say "  REFUSE: $*"; }

# ---------------------------------------------------------------------------------------------------------------
# Pure functions (no I/O)
# ---------------------------------------------------------------------------------------------------------------

# valid_name NAME: a user name useradd accepts and the unit can carry.
valid_name() { [[ "$1" =~ ^[a-z_][a-z0-9_-]{0,30}$ ]]; }
# valid_url URL: the characters of a server address, nothing a unit line or sed could misread.
valid_url() { [[ "$1" =~ ^https?://[A-Za-z0-9._-]+(:[0-9]{1,5})?(/[A-Za-z0-9._/-]*)?$ ]]; }
valid_pool() { [[ "$1" =~ ^[A-Za-z0-9._-]{1,64}$ ]]; }
valid_sha() { [[ "$1" =~ ^[0-9a-f]{64}$ ]]; }
valid_number() { [[ "$1" =~ ^[0-9]{1,10}$ ]]; }

# version_ge A B: 0 when major.minor of A is at least that of B ("1.14.1" "1.14").
version_ge() {
  local a1 a2 b1 b2
  IFS=. read -r a1 a2 _ <<<"$1"
  IFS=. read -r b1 b2 _ <<<"$2"
  a2=${a2:-0} b2=${b2:-0}
  [[ "$a1" =~ ^[0-9]+$ && "$a2" =~ ^[0-9]+$ && "$b1" =~ ^[0-9]+$ && "$b2" =~ ^[0-9]+$ ]] || return 1
  [ "$a1" -gt "$b1" ] || { [ "$a1" -eq "$b1" ] && [ "$a2" -ge "$b2" ]; }
}

# crun_version_of TEXT: the version in the first line of `crun --version` ("crun version 1.14.1").
crun_version_of() {
  local line=${1%%$'\n'*}
  case $line in "crun version "*) ;; *) return 1 ;; esac
  line=${line#crun version }
  line=${line%% *}
  [[ "$line" =~ ^[0-9]+(\.[0-9]+)*$ ]] || return 1
  printf '%s\n' "$line"
}

# kernel_release_of TEXT: "6.8.0-142-generic" -> "6.8".
kernel_release_of() {
  local r=${1%%-*}
  [[ "$r" =~ ^([0-9]+)\.([0-9]+) ]] || return 1
  printf '%s.%s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
}

# subid_has NAME TEXT COUNT: 0 when one line of the subuid-format TEXT gives NAME at least COUNT ids from a start
# above 0 that ends below 2^32 (the agent reads the first such range and no sum of smaller ones).
subid_has() {
  local name=$1 text=$2 count=$3 line owner first n
  while IFS= read -r line; do
    IFS=: read -r owner first n _ <<<"$line"
    [ "$owner" = "$name" ] || continue
    [[ "$first" =~ ^[0-9]+$ && "$n" =~ ^[0-9]+$ ]] || continue
    if [ "$first" -gt 0 ] && [ "$n" -ge "$count" ] && [ $((first + n)) -le 4294967296 ]; then
      return 0
    fi
  done <<<"$text"
  return 1
}

# pick_subid_start MIN COUNT SUBUID SUBGID PASSWD GROUP: the first start at or above MIN of COUNT ids that overlaps no
# range of either subid file and holds no uid of PASSWD or gid of GROUP; fails when none fits below 2^32.
pick_subid_start() {
  local min=$1 count=$2 sub_u=$3 sub_g=$4 passwd=$5 group=$6
  local ranges=() line a b c cand moved r s e
  while IFS= read -r line; do
    IFS=: read -r _ a b _ <<<"$line"
    [[ "$a" =~ ^[0-9]+$ && "$b" =~ ^[0-9]+$ ]] && ranges+=("$a $((a + b))")
  done <<<"$sub_u"$'\n'"$sub_g"
  while IFS= read -r line; do
    IFS=: read -r _ _ a _ <<<"$line"
    [[ "$a" =~ ^[0-9]+$ ]] && ranges+=("$a $((a + 1))")
  done <<<"$passwd"
  while IFS= read -r line; do
    IFS=: read -r _ _ a _ <<<"$line"
    [[ "$a" =~ ^[0-9]+$ ]] && ranges+=("$a $((a + 1))")
  done <<<"$group"
  c=$count
  cand=$min
  while :; do
    moved=0
    for r in "${ranges[@]}"; do
      s=${r% *}
      e=${r#* }
      if [ "$cand" -lt "$e" ] && [ "$s" -lt $((cand + c)) ]; then
        cand=$e
        moved=1
      fi
    done
    [ "$moved" = 1 ] || break
  done
  [ $((cand + c)) -le 4294967296 ] || return 1
  printf '%s\n' "$cand"
}

# render_unit OUTVAR TEXT SERVER POOL USER: OUTVAR gets the unit with its placeholders replaced (exact bytes). A
# placeholder missing from TEXT is an error (the file is not the one this script was written for); an empty SERVER or
# POOL leaves that placeholder.
render_unit() {
  local out=$1 text=$2 server=$3 pool=$4 user=$5
  case $text in *"$PLACEHOLDER_SERVER"*) ;; *) return 1 ;; esac
  case $text in *"$PLACEHOLDER_POOL"*) ;; *) return 1 ;; esac
  case $text in *"$PLACEHOLDER_USER"*) ;; *) return 1 ;; esac
  if [ -n "$server" ]; then
    valid_url "$server" || return 1
    text=${text//"$PLACEHOLDER_SERVER"/--server $server}
  fi
  if [ -n "$pool" ]; then
    valid_pool "$pool" || return 1
    text=${text//"$PLACEHOLDER_POOL"/--pool $pool}
  fi
  valid_name "$user" || return 1
  text=${text//"$PLACEHOLDER_USER"/--job-user $user}
  printf -v "$out" '%s' "$text"
}

# profile_loaded TEXT NAME: 0 when a line of the apparmor profiles list ("name (mode)") names NAME.
profile_loaded() {
  local line name
  while IFS= read -r line; do
    name=${line% (*}
    [ "$name" = "$2" ] && return 0
  done <<<"$1"
  return 1
}

# profile_attaches TEXT: 0 when a profile in the source ATTACHES to the distribution's crun path: the header
# `profile NAME /path flags=(...) {` or `/path flags=(...) {` (the profile named by its path). A comment, a
# path inside a rule and a longer path (/usr/bin/crun2) do not count.
profile_attaches() {
  local line first second third
  while IFS= read -r line; do
    line=${line%%#*}
    # shellcheck disable=SC2086 # word splitting is the parse
    set -f; set -- $line; set +f
    first=${1:-}; second=${2:-}; third=${3:-}
    if [ "$first" = profile ]; then
      # `profile NAME /path ...` attaches to /path; `profile /path ...` is named by its path.
      if [ "${second#/}" != "$second" ] && [ "$second" = "$CRUN" ]; then return 0; fi
      if [ "${third#/}" != "$third" ] && [ "$third" = "$CRUN" ]; then return 0; fi
    elif [ "$first" = "$CRUN" ]; then
      # a profile header: the path, then its flags or the opening brace (a rule has a permission there)
      case $second in flags=* | "{") return 0 ;; esac
    fi
  done <<<"$1"
  return 1
}

# is_wired VERSION_TEXT: 0 when one line of the text is exactly the wired marker.
is_wired() {
  local line
  while IFS= read -r line; do
    [ "$line" = "$WIRED_LINE" ] && return 0
  done <<<"$1"
  return 1
}

# systemd_major_of TEXT: the major version in the first line of `systemctl --version` ("systemd 255 (255.4-1ubuntu8)").
systemd_major_of() {
  local line word num
  IFS= read -r line <<<"$1" || true
  IFS=' ' read -r word num _ <<<"$line"
  [ "$word" = systemd ] || return 1
  case $num in '' | *[!0-9]*) return 1 ;; esac
  printf '%s\n' "$num"
}

# dir_path / dir_mode "PATH MODE": the fields of a DIRS entry.
dir_path() { printf '%s\n' "${1% *}"; }
dir_mode() { printf '%s\n' "${1#* }"; }

# manifest_line KIND FIELD...: one tab-separated manifest line; a field with a tab or newline is refused.
manifest_line() {
  local f out=$1
  shift
  for f in "$@"; do
    case $f in *$'\t'* | *$'\n'*) return 1 ;; esac
    out+=$'\t'$f
  done
  printf '%s\n' "$out"
}

# sha256_str TEXT: sha256 of TEXT exactly (no newline added).
sha256_str() { local o; o=$(printf '%s' "$1" | sha256sum) || return 1; printf '%s\n' "${o%% *}"; }

# slurp VAR FILE: the exact bytes of FILE (a trailing newline kept) in VAR.
slurp() {
  local __t
  __t=$(cat -- "$2"; printf x) || return 1
  printf -v "$1" '%s' "${__t%x}"
}

# options_has LIST WORD: 0 when the comma-separated mount option LIST has WORD as one of its elements.
options_has() {
  local o
  local -a all
  IFS=, read -r -a all <<<"$1"
  for o in "${all[@]}"; do
    [ "$o" = "$2" ] && return 0
  done
  return 1
}

# ---------------------------------------------------------------------------------------------------------------
# Effects: everything that changes the host goes through act, so a dry run changes nothing.
# ---------------------------------------------------------------------------------------------------------------

# set_paths: the files the script reads and writes, under ROOT.
set_paths() {
  STATE_DIR=$ROOT/var/lib/js-crun-host-setup
  MANIFEST=$STATE_DIR/manifest
  UNIT_DST=$ROOT/etc/systemd/system/js-agent-crun.service
  TMPF_DST=$ROOT/etc/tmpfiles.d/js-agent-crun.conf
  MOUNT_UNIT_FILE=$ROOT/etc/systemd/system/$MOUNT_UNIT
  PROFILES=$ROOT/sys/kernel/security/apparmor/profiles
  RESTRICT=$ROOT/proc/sys/kernel/apparmor_restrict_unprivileged_userns
  APPARMOR_SRC=$ROOT/etc/apparmor.d/crun
}

# act DESCRIPTION CMD ARG...
act() {
  local what=$1
  shift
  case $RUN_MODE in
    plan)
      say "  would: $what"
      PLANNED=$((PLANNED + 1))
      ;;
    apply)
      say "  doing: $what"
      LAST_ACT=$what
      "$@" || { say "  FAILED: $what"; FAILED=1; return 1; }
      ;;
  esac
}

# record KIND FIELD...: appended to the manifest after the step succeeded (apply only).
record() {
  [ "$RUN_MODE" = apply ] || return 0
  local line
  line=$(manifest_line "$@") || { say "  FAILED: manifest line for $1"; FAILED=1; return 1; }
  printf '%s\n' "$line" >>"$MANIFEST" || { say "  FAILED: writing $MANIFEST"; FAILED=1; return 1; }
}

# write_file DST MODE CONTENT: atomic.
write_file() {
  local dst=$1 mode=$2 content=$3 tmp
  tmp=$dst.new.$$
  # errexit is off here (the callers run in || lists): every command is checked, and a half-written copy is removed.
  if ! (umask 077; printf '%s' "$content" >"$tmp"); then
    rm -f -- "$tmp"
    return 1
  fi
  if ! chmod "$mode" "$tmp"; then
    rm -f -- "$tmp"
    return 1
  fi
  if ! mv -- "$tmp" "$dst"; then
    rm -f -- "$tmp"
    return 1
  fi
}

# ---------------------------------------------------------------------------------------------------------------
# Reading the host (read-only)
# ---------------------------------------------------------------------------------------------------------------

is_root() { [ "$(id -u)" = 0 ]; }

pkg_installed() {
  local s
  # shellcheck disable=SC2016 # dpkg-query's own format, not a shell expansion
  s=$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null) || return 1
  case $s in "install ok installed") return 0 ;; *) return 1 ;; esac
}

# passwd_field NAME N: field N (3 = uid, 4 = gid) of NAME's line of the user database; fails when there is none.
passwd_field() {
  local all line
  local -a f
  all=$(getent passwd 2>/dev/null) || all=""
  while IFS= read -r line; do
    IFS=: read -r -a f <<<"$line"
    if [ "${f[0]:-}" = "$1" ]; then
      printf '%s\n' "${f[$(($2 - 1))]:-}"
      return 0
    fi
  done <<<"$all"
  return 1
}

# group_exists NAME: 0 when the group database has a group of that name.
group_exists() {
  local all line name
  all=$(getent group 2>/dev/null) || all=""
  while IFS= read -r line; do
    IFS=: read -r name _ <<<"$line"
    [ "$name" = "$1" ] && return 0
  done <<<"$all"
  return 1
}

# scratch_options: the options the scratch filesystem is mounted with, or nothing when it is not a mount point.
scratch_options() {
  local o
  o=$(findmnt -n -o OPTIONS --mountpoint "$SCRATCH" 2>/dev/null) || o=""
  printf '%s\n' "$o"
}

# ---------------------------------------------------------------------------------------------------------------
# Steps of the setup: each reads the host, refuses or plans, and (in apply mode) acts and records.
# ---------------------------------------------------------------------------------------------------------------

step_preflight() {
  say "preflight"
  local os kernel rel
  os=$(uname -s)
  [ "$os" = Linux ] || refuse "this is $os, not Linux"
  command -v dpkg-query >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1 \
    || refuse "no dpkg-query and apt-get: the packages are installed from a Debian or Ubuntu archive"
  kernel=$(uname -r)
  rel=$(kernel_release_of "$kernel") || rel=""
  if [ -z "$rel" ]; then
    refuse "cannot read the kernel release from '$kernel'"
  elif version_ge "$rel" "$KERNEL_MIN"; then
    say "  kernel $kernel (overlay in a user namespace needs $KERNEL_MIN or newer): ok"
  else
    refuse "kernel $kernel is older than $KERNEL_MIN (overlay mounts in a user namespace)"
  fi
  local sd sdv
  sd=$(systemctl --version 2>/dev/null) || sd=""
  sdv=$(systemd_major_of "$sd") || sdv=""
  if [ -z "$sdv" ]; then
    refuse "cannot read the systemd version from 'systemctl --version'"
  elif [ "$sdv" -ge "$SYSTEMD_MIN" ]; then
    say "  systemd $sdv (the unit's DelegateSubgroup= needs $SYSTEMD_MIN or newer): ok"
  else
    refuse "systemd $sdv is older than $SYSTEMD_MIN: the unit's DelegateSubgroup= is not known to it (Ubuntu 24.04 ships 255)"
  fi
  # The root of a cgroup version 2 hierarchy has this file; a version 1 one does not.
  if [ -e "$ROOT/sys/fs/cgroup/cgroup.controllers" ]; then
    say "  cgroup version 2 at /sys/fs/cgroup: ok"
  else
    refuse "/sys/fs/cgroup is not a cgroup version 2 hierarchy: the leases' limits need it"
  fi
  local unit state
  for unit in js-agent.service js-agent-crun.service; do
    state=$(systemctl show -p ActiveState --value -- "$unit" 2>/dev/null) || state=""
    case $state in
      inactive | failed) ;;
      *) refuse "$unit is '${state:-unknown}', not inactive or failed (active, activating, deactivating and reloading all count, and Restart= brings a failed unit back): drain and stop it first (the setup changes the modes of its directories and mounts the scratch filesystem again)" ;;
    esac
  done
  local have
  if [ -L "$STATE_DIR" ] || [ -e "$STATE_DIR" ]; then
    have=$(stat -c '%a %u' "$STATE_DIR" 2>/dev/null) || have=""
    if [ -L "$STATE_DIR" ] || [ ! -d "$STATE_DIR" ] || [ "$have" != "700 $EXPECT_UID" ]; then
      refuse "the state directory $STATE_DIR is a link, not a directory, not root's or not mode 0700 ('$have'): the record in it is trusted"
    fi
  fi
  if [ -L "$MANIFEST" ] || { [ -e "$MANIFEST" ] && [ ! -f "$MANIFEST" ]; }; then
    refuse "$MANIFEST is a link or not a regular file"
  fi
  if ! is_root; then
    if [ "$APPLY" = 1 ]; then refuse "not root"; else say "  NOTE: not root: a root-only read (a directory the agent owns) is shown as unknown"; fi
  fi
}

step_packages() {
  say "packages"
  local missing=() p v
  for p in crun uidmap; do
    if pkg_installed "$p"; then say "  $p: installed"; else say "  $p: not installed"; missing+=("$p"); fi
  done
  if ! pkg_installed crun && { [ -e "$ROOT$CRUN" ] || [ -L "$ROOT$CRUN" ]; }; then
    refuse "$CRUN exists and the crun package is not installed: a hand-made program, which installing the package would overwrite"
  fi
  if pkg_installed crun; then
    local owner
    owner=$(dpkg-query -S "$CRUN" 2>/dev/null) || owner=""
    case $owner in
      "crun: $CRUN"*) say "  $CRUN is owned by the crun package" ;;
      *) refuse "$CRUN is not owned by the crun package ('$owner'): it must be the distribution's file, never a copy" ;;
    esac
    v=$("$ROOT$CRUN" --version 2>/dev/null) || v=""
    v=$(crun_version_of "$v") || v=""
    if [ -z "$v" ]; then
      refuse "'$CRUN --version' did not print a version"
    elif version_ge "$v" "$CRUN_MIN"; then
      say "  crun $v (needs $CRUN_MIN or newer): ok"
    else
      refuse "crun $v is older than $CRUN_MIN"
    fi
  fi
  [ "${#missing[@]}" -gt 0 ] || return 0
  if act "apt-get install ${missing[*]}" apt-get install -y --no-install-recommends "${missing[@]}"; then
    for p in "${missing[@]}"; do record pkg "$p"; done
    if [ "$RUN_MODE" = apply ]; then
      v=$("$ROOT$CRUN" --version 2>/dev/null) || v=""
      v=$(crun_version_of "$v") || v=""
      if [ -n "$v" ] && version_ge "$v" "$CRUN_MIN"; then say "  crun $v: ok"; else say "  FAILED: the installed crun is '$v', not $CRUN_MIN or newer"; FAILED=1; fi
    fi
  fi
}

step_user() {
  say "job user $JOB_USER"
  local uid gid shell
  if uid=$(passwd_field "$JOB_USER" 3) && [ -n "$uid" ]; then
    gid=$(passwd_field "$JOB_USER" 4) || gid=""
    shell=$(passwd_field "$JOB_USER" 7) || shell=""
    if ! { valid_number "$uid" && valid_number "$gid" && [ "$uid" -gt 0 ] && [ "$gid" -gt 0 ]; }; then
      refuse "$JOB_USER exists with uid '$uid' and gid '$gid': a job never runs as root"
    elif [ "$uid" -gt 999 ]; then
      refuse "$JOB_USER exists with uid $uid, above 999: it is a person's account, not a system account for jobs"
    else
      case $shell in
        */nologin | /bin/false | /usr/bin/false) say "  exists (uid $uid, gid $gid): left as it is, not recorded" ;;
        *) refuse "$JOB_USER exists with the login shell '$shell': the job user cannot log in" ;;
      esac
    fi
    return 0
  fi
  if group_exists "$JOB_USER"; then
    refuse "a group $JOB_USER exists and the user does not: useradd --user-group would fail; name another --job-user"
    return 0
  fi
  if act "useradd a system account $JOB_USER (no login, no home)" \
    useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$JOB_USER"; then
    if [ "$RUN_MODE" = apply ]; then record user "$JOB_USER" "$(passwd_field "$JOB_USER" 3)"; fi
  fi
}

step_subids() {
  say "subuid and subgid block for $JOB_USER"
  local sub_u sub_g passwd group have_u=0 have_g=0 start end
  sub_u=$(cat "$ROOT/etc/subuid" 2>/dev/null) || sub_u=""
  sub_g=$(cat "$ROOT/etc/subgid" 2>/dev/null) || sub_g=""
  subid_has "$JOB_USER" "$sub_u" "$SUBID_COUNT" && have_u=1
  subid_has "$JOB_USER" "$sub_g" "$SUBID_COUNT" && have_g=1
  if [ "$have_u" = 1 ] && [ "$have_g" = 1 ]; then
    say "  both files give $JOB_USER $SUBID_COUNT ids or more: ok, left as they are"
    return 0
  fi
  passwd=$(getent passwd) || passwd=""
  group=$(getent group) || group=""
  if [ -n "$SUBID_START" ]; then
    start=$SUBID_START
    [ "$(pick_subid_start "$start" "$SUBID_COUNT" "$sub_u" "$sub_g" "$passwd" "$group" || true)" = "$start" ] \
      || { refuse "--subid-start $start overlaps a range of /etc/subuid or /etc/subgid or an id of passwd or group, or leaves no room below 2^32"; return 0; }
  else
    start=$(pick_subid_start "$SUBID_MIN" "$SUBID_COUNT" "$sub_u" "$sub_g" "$passwd" "$group") \
      || { refuse "no free block of $SUBID_COUNT ids below 2^32"; return 0; }
  fi
  end=$((start + SUBID_COUNT - 1))
  say "  block $start-$end ($SUBID_COUNT ids), clear of every other range, uid and gid"
  if [ "$have_u" = 0 ]; then
    act "usermod --add-subuids $start-$end $JOB_USER" usermod --add-subuids "$start-$end" "$JOB_USER" && record subuid "$JOB_USER" "$start" "$SUBID_COUNT"
  fi
  if [ "$have_g" = 0 ]; then
    act "usermod --add-subgids $start-$end $JOB_USER" usermod --add-subgids "$start-$end" "$JOB_USER" && record subgid "$JOB_USER" "$start" "$SUBID_COUNT"
  fi
}

# mount_unit_options TEXT: sets MU_COUNT (the Options= lines of the [Mount] section), MU_OPTIONS (the last one's
# value) and MU_TYPE (the Type= of the section, empty when there is none).
mount_unit_options() {
  local line section=""
  MU_COUNT=0 MU_OPTIONS="" MU_TYPE="" MU_CONTINUED=0
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in "["*"]") section=$line ;; esac
    [ "$section" = "[Mount]" ] || continue
    if [[ "$line" =~ ^Options[[:space:]]*=[[:space:]]*(.*[^[:space:]])?[[:space:]]*$ ]]; then
      MU_COUNT=$((MU_COUNT + 1))
      MU_OPTIONS=${BASH_REMATCH[1]}
      case $line in *\\) MU_CONTINUED=1 ;; esac
    elif [[ "$line" =~ ^Type[[:space:]]*=[[:space:]]*([^[:space:]]*) ]]; then
      MU_TYPE=${BASH_REMATCH[1]}
    fi
  done < <(printf '%s' "$1")
}

# with_quota_option OUTVAR TEXT: OUTVAR gets TEXT with prjquota added to the Options= line of the [Mount] section (call
# mount_unit_options on TEXT first); every other line as it was, each ending in a newline.
with_quota_option() {
  local line section="" out=""
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in "["*"]") section=$line ;; esac
    if [ "$section" = "[Mount]" ] && [[ "$line" =~ ^Options[[:space:]]*= ]]; then
      line="Options=${MU_OPTIONS:+$MU_OPTIONS,}$QUOTA_OPTION"
    fi
    out+=$line$'\n'
  done < <(printf '%s' "$2")
  printf -v "$1" '%s' "$out"
}

# quota_remount_checks: refuses (and fails) unless unmounting the scratch filesystem and mounting it again is safe:
# nothing holds it (a clean answer of fuser, not an error), no other mount of the same filesystem keeps it alive, none
# is mounted below it, and no other active unit depends on its mount unit.
quota_remount_checks() {
  local out rc=0 src targets children dep line state
  if ! command -v fuser >/dev/null 2>&1; then
    refuse "no fuser (package psmisc): cannot tell whether anything holds $SCRATCH open"
    return 1
  fi
  out=$(fuser -m -- "$SCRATCH" 2>&1) || rc=$?
  if [ "$rc" = 0 ]; then
    refuse "something holds $SCRATCH open (fuser -vm $SCRATCH lists it): the filesystem is unmounted to turn $QUOTA_OPTION on, so stop it first"
    return 1
  elif [ "$rc" != 1 ] || [ -n "$out" ]; then
    refuse "cannot tell whether anything holds $SCRATCH open (fuser exited $rc: ${out:-no output})"
    return 1
  fi
  src=$(findmnt -n -o SOURCE --mountpoint "$SCRATCH" 2>/dev/null) || src=""
  if [ -z "$src" ]; then
    refuse "cannot read the source of the mount at $SCRATCH"
    return 1
  fi
  targets=$(findmnt -n -o TARGET -S "$src" 2>/dev/null) || targets=""
  if [ "$targets" != "$SCRATCH" ]; then
    refuse "$src is mounted somewhere besides $SCRATCH (${targets//$'\n'/ }): a bind mount keeps the filesystem alive, so $QUOTA_OPTION would not take"
    return 1
  fi
  children=$(findmnt -n -R -o TARGET "$SCRATCH" 2>/dev/null) || children=""
  if [ "$children" != "$SCRATCH" ]; then
    refuse "mounts below $SCRATCH (${children//$'\n'/ }) would be unmounted with it and not mounted again"
    return 1
  fi
  if ! dep=$(systemctl list-dependencies --reverse --plain --no-pager "$MOUNT_UNIT" 2>/dev/null); then
    refuse "cannot list the units that depend on $MOUNT_UNIT"
    return 1
  fi
  while IFS= read -r line; do
    line=${line#"${line%%[![:space:]]*}"}
    case $line in "" | "$MOUNT_UNIT" | *.target) continue ;; esac
    state=$(systemctl show -p ActiveState --value -- "$line" 2>/dev/null) || state=""
    case $state in
      inactive | failed) ;;
      *) refuse "$line is '${state:-unknown}' and depends on $MOUNT_UNIT: stopping the mount stops it and nothing starts it again"; return 1 ;;
    esac
  done <<<"$dep"
}

step_scratch_quota() {
  say "scratch filesystem quota ($QUOTA_OPTION on $SCRATCH)"
  if [ "$SKIP_QUOTA" = 1 ]; then
    say "  skipped (--skip-scratch-quota)"
    return 0
  fi
  if [ ! -f "$MOUNT_UNIT_FILE" ] || [ -L "$MOUNT_UNIT_FILE" ]; then
    refuse "$MOUNT_UNIT_FILE is not a file: name the scratch mount unit with --scratch-mount-unit, or leave the quota out with --skip-scratch-quota"
    return 0
  fi
  if [ -e "$MOUNT_UNIT_FILE.d" ] || [ -L "$MOUNT_UNIT_FILE.d" ]; then
    refuse "$MOUNT_UNIT_FILE.d exists: a drop-in there can override Options=, and the edit would not reach it"
    return 0
  fi
  local text mounted edit=0 remount=0
  slurp text "$MOUNT_UNIT_FILE"
  mount_unit_options "$text"
  if [ "$MU_COUNT" != 1 ]; then
    refuse "$MOUNT_UNIT_FILE has $MU_COUNT Options= lines in its [Mount] section, not exactly one: the options to keep cannot be told"
    return 0
  fi
  if [ "$MU_CONTINUED" = 1 ]; then
    refuse "the Options= line of $MOUNT_UNIT_FILE is continued with a backslash: it cannot be extended safely"
    return 0
  fi
  if [ -n "$MU_TYPE" ] && [ "$MU_TYPE" != xfs ]; then
    refuse "$MOUNT_UNIT_FILE has Type=$MU_TYPE: the project quota here is XFS's"
    return 0
  fi
  mounted=$(scratch_options)
  if options_has "$MU_OPTIONS" "$QUOTA_OPTION"; then
    say "  Options=$MU_OPTIONS already has $QUOTA_OPTION"
  else
    edit=1
  fi
  if [ -n "$mounted" ] && ! options_has "$mounted" "$QUOTA_OPTION"; then
    remount=1
  elif [ -z "$mounted" ]; then
    say "  $SCRATCH is not mounted: the option applies at its next mount"
  else
    say "  mounted with $QUOTA_OPTION: ok"
  fi
  [ "$edit" = 1 ] || [ "$remount" = 1 ] || return 0
  if [ "$remount" = 1 ]; then
    quota_remount_checks || return 0
  fi
  if [ "$edit" = 1 ]; then
    local new
    with_quota_option new "$text"
    act "set Options=${MU_OPTIONS:+$MU_OPTIONS,}$QUOTA_OPTION in $MOUNT_UNIT_FILE (was Options=$MU_OPTIONS)" \
      write_file "$MOUNT_UNIT_FILE" "$(stat -c %a "$MOUNT_UNIT_FILE")" "$new" \
      && record mount-options "$MOUNT_UNIT_FILE" "$MU_OPTIONS" "$(sha256_str "$new")" \
      || { ABORT=1; return 0; }
  fi
  act "systemctl daemon-reload" systemctl daemon-reload || { ABORT=1; return 0; }
  if [ "$RUN_MODE" = apply ]; then
    local effective
    effective=$(systemctl show -p Options --value -- "$MOUNT_UNIT" 2>/dev/null) || effective=""
    if ! options_has "$effective" "$QUOTA_OPTION"; then
      say "  FAILED: the effective Options= of $MOUNT_UNIT is '$effective', without $QUOTA_OPTION (a file the edit missed): nothing was unmounted"
      FAILED=1
      ABORT=1
      return 0
    fi
  fi
  [ "$remount" = 1 ] || return 0
  act "unmount $SCRATCH (systemctl stop $MOUNT_UNIT)" systemctl stop "$MOUNT_UNIT" || { ABORT=1; return 0; }
  if ! act "mount it again with $QUOTA_OPTION (systemctl start $MOUNT_UNIT)" systemctl start "$MOUNT_UNIT"; then
    ABORT=1
    say "  STOP: $SCRATCH is unmounted and the host is left drained, with nothing after this step done."
    say "  Recover: systemctl start $MOUNT_UNIT   (then read the manifest, and run this script again)"
    return 0
  fi
  if [ "$RUN_MODE" = apply ]; then
    mounted=$(scratch_options)
    if options_has "$mounted" "$QUOTA_OPTION"; then
      say "  findmnt: $QUOTA_OPTION is active on $SCRATCH: ok"
    else
      say "  FAILED: $QUOTA_OPTION is not active on $SCRATCH after the mount (findmnt shows '${mounted:-not mounted}')"
      FAILED=1
      ABORT=1
    fi
  fi
}

step_dirs() {
  say "directories"
  local entry path mode have
  for entry in "${DIRS[@]}"; do
    path=$ROOT$(dir_path "$entry")
    mode=$(dir_mode "$entry")
    if [ -L "$path" ]; then refuse "$path is a symbolic link"; continue; fi
    if [ ! -e "$path" ] && [ -d "$(dirname "$path")" ] && [ ! -x "$(dirname "$path")" ]; then
      say "  $path: unknown (the directory above it is closed to this user: not root)"
      continue
    fi
    if [ ! -e "$path" ]; then
      if [ -d "$(dirname "$path")" ] || [ "$RUN_MODE" = plan ]; then
        if [ -n "$ROOT" ]; then
          act "make $path ($mode)" install -d -m "$mode" "$path" && record dir-created "$path"
        else
          act "make $path (root, $mode)" install -d -m "$mode" -o root -g root "$path" && record dir-created "$path"
        fi
      fi
      continue
    fi
    [ -d "$path" ] || { refuse "$path exists and is not a directory"; continue; }
    have=$(stat -c '%a %u' "$path" 2>/dev/null) || have=""
    if [ -z "$have" ]; then say "  $path: unknown (cannot stat: not root)"; continue; fi
    if [ "${have#* }" != "$EXPECT_UID" ]; then refuse "$path is not owned by root"; continue; fi
    if [ "$((8#${have% *}))" = "$((8#$mode))" ]; then
      say "  $path: mode $mode: ok"
    else
      act "chmod $mode $path (was ${have% *})" chmod "$mode" "$path" && record dir-mode "$path" "${have% *}"
    fi
  done
  if [ -d "$ROOT$SCRATCH" ] && [ -z "$(scratch_options)" ]; then
    say "  NOTE: $SCRATCH is not a mount point: the unit's ConditionPathIsMountPoint would skip the start"
  fi
}

step_apparmor() {
  say "apparmor profile for $CRUN"
  local restrict profiles src
  restrict=$(cat "$RESTRICT" 2>/dev/null) || restrict=""
  if [ "$restrict" != 1 ]; then
    say "  unprivileged user namespaces are not restricted here (${restrict:-no such switch}): no profile needed"
    return 0
  fi
  if ! profiles=$(cat "$PROFILES" 2>/dev/null); then
    if is_root; then refuse "cannot read $PROFILES"; else say "  unknown: cannot read $PROFILES (not root)"; fi
    return 0
  fi
  if profile_loaded "$profiles" crun; then
    say "  the profile crun is loaded: ok"
    return 0
  fi
  if [ ! -f "$APPARMOR_SRC" ]; then
    refuse "user namespaces are restricted, no profile crun is loaded and /etc/apparmor.d/crun is not installed: a decision for the owner (a profile for the pinned path, or the sysctl), not made here"
    return 0
  fi
  slurp src "$APPARMOR_SRC"
  if ! profile_attaches "$src"; then
    refuse "/etc/apparmor.d/crun does not name $CRUN"
    return 0
  fi
  act "load /etc/apparmor.d/crun (apparmor_parser -r)" apparmor_parser -r "$APPARMOR_SRC" && record apparmor-loaded /etc/apparmor.d/crun
}

# install_one LABEL SRC SHA DST CONTENT: checks the pin, compares with what is there, installs.
install_one() {
  local label=$1 src=$2 sha=$3 dst=$4 raw=$5 content=$6 have
  if [ -z "$src" ]; then
    if [ "$APPLY" = 1 ]; then refuse "$label: --$label FILE and --$label-sha256 are needed"; else say "  $label: no file given (apply needs it); would install $dst"; fi
    return 0
  fi
  [ -f "$src" ] || { refuse "$label: no file $src"; return 0; }
  valid_sha "$sha" || { refuse "$label: --$label-sha256 must be 64 lowercase hex digits"; return 0; }
  # The pin is of the bytes that were read once and are installed (rendered), not of a second read of the file.
  [ "$(sha256_str "$raw")" = "$sha" ] || { refuse "$label: $src does not have sha256 $sha"; return 0; }
  if [ -e "$dst" ] || [ -L "$dst" ]; then
    if [ -f "$dst" ] && [ ! -L "$dst" ] && slurp have "$dst" && [ "$have" = "$content" ]; then
      say "  $dst: already installed with this content: ok"
    else
      refuse "$dst exists and differs: never overwritten"
    fi
    return 0
  fi
  [ -d "$(dirname "$dst")" ] || { refuse "$(dirname "$dst") does not exist"; return 0; }
  act "install $dst (0644, root; from $src, sha256 $sha)" write_file "$dst" 0644 "$content" \
    && record file "$dst" "$(sha256_str "$content")" \
    && FILES_CHANGED=1
}

step_files() {
  say "unit and tmpfiles"
  local unit="" tmpf="" unit_raw=""
  if [ -n "$UNIT_FILE" ] && [ -f "$UNIT_FILE" ]; then
    slurp unit_raw "$UNIT_FILE"
    unit=$unit_raw
    render_unit unit "$unit_raw" "$SERVER" "$POOL" "$JOB_USER" \
      || { refuse "unit: $UNIT_FILE is not the unit this script renders (a placeholder is missing, or the server, pool or user is not valid)"; return 0; }
  fi
  if [ -n "$TMPF_FILE" ] && [ -f "$TMPF_FILE" ]; then slurp tmpf "$TMPF_FILE"; fi
  if [ "$APPLY" = 1 ] && { [ -z "$SERVER" ] || [ -z "$POOL" ]; }; then
    refuse "unit: --server and --pool are needed to write the unit"
  fi
  install_one unit "$UNIT_FILE" "$UNIT_SHA" "$UNIT_DST" "$unit_raw" "$unit"
  install_one tmpfiles "$TMPF_FILE" "$TMPF_SHA" "$TMPF_DST" "$tmpf" "$tmpf"
}

step_reload() {
  [ "$RUN_MODE" = apply ] && [ "$FILES_CHANGED" = 1 ] && [ -f "$UNIT_DST" ] || return 0
  say "systemd"
  if act "systemctl daemon-reload" systemctl daemon-reload; then
    record reload 1
    if systemd-analyze verify "$UNIT_DST" >/dev/null 2>&1; then say "  systemd-analyze verify $UNIT_DST: ok"; else say "  FAILED: systemd-analyze verify $UNIT_DST"; FAILED=1; fi
  fi
}

# One pass over the steps in the current RUN_MODE. The scratch mount comes before the directories, so the ones
# below it are made on the mounted filesystem.
setup_pass() {
  REFUSALS=0
  local step
  for step in step_preflight step_packages step_user step_subids step_scratch_quota step_dirs step_apparmor step_files step_reload; do
    "$step" || true
    if [ "$ABORT" = 1 ]; then
      say "aborted after $step: the steps after it did not run"
      break
    fi
  done
}

# ---------------------------------------------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------------------------------------------

# on_exit: an apply that ends in a failure or a signal writes that, and the step it was in, to the manifest.
on_exit() {
  local rc=$?
  trap - EXIT
  if [ "$rc" != 0 ] && [ "$rc" != 3 ] && [ -f "$MANIFEST" ]; then
    manifest_line incomplete "$rc" "${LAST_ACT:-}" >>"$MANIFEST" || true
  fi
  exit "$rc"
}

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

main() {
  while [ "$#" -gt 0 ]; do
    case $1 in
      --apply) APPLY=1 ;;
      --skip-scratch-quota) SKIP_QUOTA=1 ;;
      --help | -h) usage; exit 0 ;;
      --job-user | --subid-start | --unit | --unit-sha256 | --tmpfiles | --tmpfiles-sha256 | --server | --pool | --scratch-mount-unit | --root)
        [ "$#" -ge 2 ] || die "$1 needs a value"
        case $1 in
          --job-user) JOB_USER=$2 ;;
          --subid-start) SUBID_START=$2 ;;
          --unit) UNIT_FILE=$2 ;;
          --unit-sha256) UNIT_SHA=$2 ;;
          --tmpfiles) TMPF_FILE=$2 ;;
          --tmpfiles-sha256) TMPF_SHA=$2 ;;
          --server) SERVER=$2 ;;
          --pool) POOL=$2 ;;
          --scratch-mount-unit) MOUNT_UNIT=$2 ;;
          --root) ROOT=${2%/} ;;
        esac
        shift
        ;;
      *) die "unknown option $1 (see --help)" ;;
    esac
    shift
  done
  valid_name "$JOB_USER" || die "--job-user '$JOB_USER' is not a user name"
  if [ -n "$SUBID_START" ]; then
    valid_number "$SUBID_START" || die "--subid-start must be a number above 0"
    [ "$SUBID_START" -gt 0 ] || die "--subid-start must be a number above 0"
  fi
  [ -z "$SERVER" ] || valid_url "$SERVER" || die "--server '$SERVER' is not an http or https address"
  [ -z "$POOL" ] || valid_pool "$POOL" || die "--pool '$POOL' is not a pool name"
  case $MOUNT_UNIT in */* | "" | *.mount.*) die "--scratch-mount-unit '$MOUNT_UNIT' is not the name of a mount unit" ;; esac
  case $MOUNT_UNIT in *.mount) ;; *) die "--scratch-mount-unit '$MOUNT_UNIT' does not end in .mount" ;; esac
  if [ -n "$ROOT" ]; then
    case $ROOT in /*) ;; *) die "--root must be an absolute path" ;; esac
    [ -d "$ROOT" ] || die "--root $ROOT is not a directory"
    EXPECT_UID=$(stat -c %u "$ROOT")
  fi
  # A tree is for tests. The commands (systemctl, useradd, ...) still act on this host, so a root process must not
  # be given one to apply to. (`id -ur` is the real user; the root check below asks `id -u`.)
  if [ -n "$ROOT" ] && [ "$APPLY" = 1 ] && [ "$(id -ur)" = 0 ]; then
    die "--root with --apply must not run as root: its commands would act on this host, not on $ROOT"
  fi
  set_paths

  if [ "$APPLY" = 1 ]; then say "crun host setup: APPLY"; else say "crun host setup: DRY RUN (nothing is changed)"; fi
  RUN_MODE=plan
  setup_pass
  if [ "$REFUSALS" -gt 0 ]; then
    say "refused ($REFUSALS): nothing was changed"
    exit 2
  fi
  if [ "$PLANNED" = 0 ]; then
    say "nothing to do: the host already has every piece"
    exit 3
  fi
  if [ "$APPLY" = 0 ]; then
    say "dry run: nothing changed. --apply does the steps above."
    exit 0
  fi
  say "-- applying"
  mkdir -p -m 0700 "$STATE_DIR"
  if [ ! -e "$MANIFEST" ]; then
    : >"$MANIFEST"
    chmod 0600 "$MANIFEST"
  fi
  RUN_MODE=apply
  FILES_CHANGED=0
  trap on_exit EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
  setup_pass
  [ "$REFUSALS" = 0 ] || FAILED=1
  if [ "$FAILED" = 1 ]; then
    say "a step failed: $MANIFEST says what was done"
    exit 1
  fi
  say "done. The unit is installed and NOT started."
  local version=""
  version=$("$ROOT$JS_AGENT" --version 2>/dev/null) || version=""
  if is_wired "$version"; then
    say "  $JS_AGENT prints '$WIRED_LINE': a build wired for crun."
    say "NEXT: on this host, drain it, then systemctl stop js-agent and systemctl start js-agent-crun."
  else
    say "do NOT switch units yet: $JS_AGENT does not print '$WIRED_LINE' in --version (this build exits 2 at start with --runtime crun, or an older one does not know the flag)."
    say "NEXT: install an agent build wired for crun, run this script again, and only then switch units."
  fi
  exit 0
}

main "$@"
