#!/usr/bin/env bash
# egress-network.sh: a Podman network for js-agent containers that may reach only the
# endpoints named with --allow, and nothing else.
#
#   egress-network.sh --network NAME --subnet CIDR --allow IP:PORT [--allow ...] [--apply]
#
# Without --apply it only prints what it would do, including the exact nftables ruleset.
# With --apply (as root) it:
#   1. creates the Podman network NAME with DNS disabled (never --internal: that blocks
#      all egress, the allowed endpoints too); an existing network of that name must have
#      this subnet and no DNS, or the script fails and changes nothing;
#   2. writes the ruleset to $RULES_FILE and loads it as the nftables table inet js_egress;
#   3. installs and enables js-egress.service, which loads the ruleset again at boot;
#   4. lists the table and checks that every rule is in the kernel.
# Rerunning with the same arguments changes nothing. One egress network per host: another
# run with other arguments replaces the table.
#
# Rootful Podman with netavark only (what js-agent supports). IPv4 only: the network has
# no IPv6 subnet, so a container has no IPv6 route; --subnet and --allow take IPv4.
set -euo pipefail

TABLE="js_egress"
# Before the filter hook's priority 0, where iptables-nft's FORWARD chain (netavark's
# default backend) and netavark's own nftables chains sit; see docs/egress.md.
PRIORITY="-10"
RULES_FILE="/etc/js-agent/egress.nft"
UNIT_NAME="js-egress.service"
UNIT_FILE="/etc/systemd/system/${UNIT_NAME}"
NFT="/usr/sbin/nft"
PODMAN="/usr/bin/podman"
NETWORK=""
SUBNET=""
ALLOW=()
APPLY=0

usage() {
	cat <<'EOF'
usage: egress-network.sh --network NAME --subnet CIDR --allow IP:PORT [--allow IP:PORT ...]
                         [--apply] [--podman PATH] [--nft PATH] [--rules-file PATH]

  --network NAME     the Podman network (js-agent --container-network NAME)
  --subnet CIDR      its IPv4 subnet in network-address form, e.g. 192.0.2.0/24
  --allow IP:PORT    a TCP endpoint containers may open; IP may be a CIDR of /16 or
                     narrower (CIDR:PORT); repeatable, at least one
  --apply            make the changes (as root); without it only print them
  --podman PATH      default /usr/bin/podman
  --nft PATH         default /usr/sbin/nft
  --rules-file PATH  default /etc/js-agent/egress.nft
EOF
}

die() {
	printf 'egress-network.sh: %s\n' "$*" >&2
	exit 1
}

# ip_to_int A.B.C.D: prints the address as an integer, or fails.
ip_to_int() {
	local a b c d octet
	IFS=. read -r a b c d <<<"$1"
	for octet in "$a" "$b" "$c" "$d"; do
		[[ $octet =~ ^(0|[1-9][0-9]{0,2})$ ]] || return 1
		((10#$octet <= 255)) || return 1
	done
	printf '%d' $(((10#$a << 24) | (10#$b << 16) | (10#$c << 8) | 10#$d))
}

# cidr_check CIDR MIN_PREFIX: succeeds for A.B.C.D/N with N in MIN_PREFIX..32 and no host
# bits set in the address when REQUIRE_NETWORK is 1.
cidr_check() {
	local cidr=$1 min=$2 addr prefix value mask
	[[ $cidr =~ ^[0-9.]+/[0-9]{1,2}$ ]] || return 1
	addr=${cidr%/*}
	prefix=$((10#${cidr#*/}))
	((prefix >= min && prefix <= 32)) || return 1
	value=$(ip_to_int "$addr") || return 1
	mask=$(((0xFFFFFFFF << (32 - prefix)) & 0xFFFFFFFF))
	if [[ ${REQUIRE_NETWORK:-0} == 1 ]] && ((value & ~mask & 0xFFFFFFFF)); then
		return 1
	fi
	return 0
}

# overlaps_link_local CIDR: succeeds when CIDR (or a plain address) touches 169.254.0.0/16.
overlaps_link_local() {
	local addr=${1%/*} prefix=32 value mask
	[[ $1 == */* ]] && prefix=$((10#${1#*/}))
	value=$(ip_to_int "$addr")
	mask=$(((0xFFFFFFFF << (32 - prefix)) & 0xFFFFFFFF))
	local link=$(((169 << 24) | (254 << 16))) link_mask=0xFFFF0000
	local common=$((mask & link_mask))
	(((value & common) == (link & common)))
}

parse_args() {
	while (($#)); do
		case $1 in
		--network)
			(($# >= 2)) || die "--network needs a value"
			NETWORK=$2
			shift 2
			;;
		--subnet)
			(($# >= 2)) || die "--subnet needs a value"
			SUBNET=$2
			shift 2
			;;
		--allow)
			(($# >= 2)) || die "--allow needs a value"
			ALLOW+=("$2")
			shift 2
			;;
		--podman)
			(($# >= 2)) || die "--podman needs a value"
			PODMAN=$2
			shift 2
			;;
		--nft)
			(($# >= 2)) || die "--nft needs a value"
			NFT=$2
			shift 2
			;;
		--rules-file)
			(($# >= 2)) || die "--rules-file needs a value"
			RULES_FILE=$2
			shift 2
			;;
		--apply)
			APPLY=1
			shift
			;;
		-h | --help)
			usage
			exit 0
			;;
		*)
			usage >&2
			die "unknown argument: $1"
			;;
		esac
	done
}

validate() {
	[[ -n $NETWORK ]] || die "--network is required"
	[[ $NETWORK =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] || die "--network: not a plain network name"
	[[ ${NETWORK,,} != host && ${NETWORK,,} != none ]] || die "--network: host and none are not egress networks"
	[[ -n $SUBNET ]] || die "--subnet is required"
	REQUIRE_NETWORK=1 cidr_check "$SUBNET" 8 && ((10#${SUBNET#*/} <= 30)) ||
		die "--subnet: not an IPv4 CIDR of /8 to /30 in network-address form (host bits zero): $SUBNET"
	((${#ALLOW[@]} > 0)) || die "at least one --allow IP:PORT is required"
	local entry ip port
	for entry in "${ALLOW[@]}"; do
		[[ $entry =~ ^[0-9.]+(/[0-9]{1,2})?:[0-9]{1,5}$ ]] || die "--allow: not IP:PORT or CIDR:PORT: $entry"
		ip=${entry%:*}
		port=${entry##*:}
		((10#$port >= 1 && 10#$port <= 65535)) || die "--allow: port out of range: $entry"
		if [[ $ip == */* ]]; then
			REQUIRE_NETWORK=1 cidr_check "$ip" 16 || die "--allow: not a CIDR of /16 to /32 in network-address form: $entry"
		else
			ip_to_int "$ip" >/dev/null || die "--allow: not an IPv4 address: $entry"
		fi
		if overlaps_link_local "$ip"; then
			die "--allow: 169.254.0.0/16 (cloud metadata) can never be allowed: $entry"
		fi
	done
	# Normalise ports (no leading zeros) and drop duplicates, so a rerun is byte-identical.
	local normal=() seen=" "
	for entry in "${ALLOW[@]}"; do
		ip=${entry%:*}
		port=${entry##*:}
		entry="${ip%/32}:$((10#$port))"
		[[ $seen == *" $entry "* ]] && continue
		seen+="$entry "
		normal+=("$entry")
	done
	mapfile -t ALLOW < <(printf '%s\n' "${normal[@]}" | LC_ALL=C sort)
}

# ruleset: prints the nft file. The leading "table"/"delete table" pair makes loading it
# idempotent and atomic: nft applies the whole file as one transaction, replacing any
# earlier version of the table, whether or not it existed.
ruleset() {
	local entry ip port
	cat <<EOF
# Managed by egress-network.sh. Do not edit; rerun the script.
table inet ${TABLE}
delete table inet ${TABLE}
table inet ${TABLE} {
	chain forward {
		type filter hook forward priority ${PRIORITY}; policy accept;
		ip saddr ${SUBNET} ip daddr 169.254.0.0/16 counter drop
		ip saddr ${SUBNET} ct state established,related accept
EOF
	for entry in "${ALLOW[@]}"; do
		ip=${entry%:*}
		port=${entry##*:}
		printf '\t\tip saddr %s ip daddr %s tcp dport %s ct state new accept\n' "$SUBNET" "$ip" "$port"
	done
	cat <<EOF
		ip saddr ${SUBNET} counter drop
	}

	chain input {
		type filter hook input priority ${PRIORITY}; policy accept;
		ip saddr ${SUBNET} ct state established,related accept
EOF
	for entry in "${ALLOW[@]}"; do
		ip=${entry%:*}
		port=${entry##*:}
		printf '\t\tip saddr %s ip daddr %s tcp dport %s ct state new accept\n' "$SUBNET" "$ip" "$port"
	done
	cat <<EOF
		ip saddr ${SUBNET} counter drop
	}
}
EOF
}

unit() {
	cat <<EOF
# Managed by egress-network.sh. Do not edit; rerun the script.
[Unit]
Description=Egress limits for js-agent containers (nftables table inet ${TABLE})
DefaultDependencies=no
After=nftables.service
Before=network.target js-agent.service
# nftables.service starts with "flush ruleset" on Ubuntu: a restart or reload of it would
# remove this table, so both are passed on.
PartOf=nftables.service
ReloadPropagatedFrom=nftables.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=${NFT} -f ${RULES_FILE}
ExecReload=${NFT} -f ${RULES_FILE}

[Install]
WantedBy=multi-user.target
EOF
}

say() {
	printf '%s\n' "$*"
}

# network_state: prints "absent", "ok", or the reason the existing network does not fit.
network_state() {
	local json
	if ! json=$("$PODMAN" network inspect "$NETWORK" 2>/dev/null); then
		say absent
		return
	fi
	if ! grep -Eq '"dns_enabled": *false' <<<"$json"; then
		say "has DNS enabled (or no dns_enabled field)"
	elif ! grep -Eq "\"subnet\": *\"${SUBNET//./\\.}\"" <<<"$json"; then
		say "has another subnet than ${SUBNET}"
	elif grep -Eq '"internal": *true' <<<"$json"; then
		say "is --internal (blocks all egress)"
	else
		say ok
	fi
}

# install_file PATH MODE: writes stdin to PATH atomically when it differs; prints whether
# it changed.
install_file() {
	local path=$1 mode=$2 tmp
	tmp=$(mktemp "${path}.XXXXXX")
	cat >"$tmp"
	chmod "$mode" "$tmp"
	if [[ -f $path ]] && cmp -s "$tmp" "$path"; then
		rm -f "$tmp"
		say "unchanged: $path"
	else
		mv "$tmp" "$path"
		say "wrote: $path"
	fi
}

dry_run() {
	local state="skipped (not run: $PODMAN is not executable here)"
	if [[ -x $PODMAN ]]; then
		state=$(network_state)
	fi
	say "# DRY RUN: nothing is changed. Rerun with --apply as root to do this."
	say "# network ${NETWORK}: ${state}"
	if [[ $state != absent && $state != ok && $state != skipped* ]]; then
		die "network ${NETWORK} exists and ${state}; this script never changes an existing network"
	fi
	say "would run (if the network is absent): ${PODMAN} network create --disable-dns --subnet ${SUBNET} ${NETWORK}"
	say "would write ${RULES_FILE} (0644):"
	ruleset | sed 's/^/    | /'
	say "would write ${UNIT_FILE} (0644):"
	unit | sed 's/^/    | /'
	say "would run: ${NFT} -c -f ${RULES_FILE}"
	say "would run: systemctl daemon-reload; systemctl enable ${UNIT_NAME}"
	say "would run: ${NFT} -f ${RULES_FILE}"
	say "would run: ${NFT} list table inet ${TABLE}, and check that every rule above is loaded"
}

apply() {
	((EUID == 0)) || die "--apply needs root (rootful Podman and nftables)"
	[[ -x $PODMAN ]] || die "podman not found at $PODMAN (see --podman)"
	[[ -x $NFT ]] || die "nft not found at $NFT (see --nft)"
	command -v systemctl >/dev/null || die "systemctl not found"

	local state
	state=$(network_state)
	case $state in
	absent)
		"$PODMAN" network create --disable-dns --subnet "$SUBNET" "$NETWORK" >/dev/null
		say "created: podman network $NETWORK ($SUBNET, DNS disabled)"
		state=$(network_state)
		[[ $state == ok ]] || die "network $NETWORK was created but ${state}"
		;;
	ok) say "unchanged: podman network $NETWORK" ;;
	*) die "network ${NETWORK} exists and ${state}; this script never changes an existing network" ;;
	esac

	mkdir -p "$(dirname "$RULES_FILE")"
	ruleset | install_file "$RULES_FILE" 0644
	"$NFT" -c -f "$RULES_FILE" || die "nft rejected $RULES_FILE; nothing was loaded"
	unit | install_file "$UNIT_FILE" 0644
	systemctl daemon-reload
	systemctl enable "$UNIT_NAME" >/dev/null
	"$NFT" -f "$RULES_FILE"
	say "loaded: table inet $TABLE"
	verify
}

# normalize: drops what nft adds or ignores when it prints a rule back (indentation and
# the counter's values), so a printed rule can be compared with the one that was written.
normalize() {
	sed -E 's/^[[:space:]]+//; s/counter packets [0-9]+ bytes [0-9]+ //; s/counter //'
}

# verify: the kernel has every rule that was meant to be loaded, and the unit is enabled.
verify() {
	local listed want
	listed=$("$NFT" list table inet "$TABLE" | normalize) || die "table inet $TABLE is not loaded"
	while IFS= read -r want; do
		grep -Fxq -- "$want" <<<"$listed" || die "rule missing from the loaded table: $want"
	done < <(ruleset | grep -E '^[[:space:]]+ip saddr ' | normalize)
	systemctl is-enabled --quiet "$UNIT_NAME" || die "$UNIT_NAME is not enabled"
	say "verified: every rule is loaded and $UNIT_NAME is enabled"
}

main() {
	parse_args "$@"
	validate
	if ((APPLY)); then
		apply
	else
		dry_run
	fi
}

main "$@"
