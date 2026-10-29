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
#   2. writes the ruleset to $RULES_FILE and loads it as the nftables table inet js_egress,
#      after `nft -c` has accepted it as a temporary file (an invalid file never replaces
#      the copy the next boot loads, and the network is not created before that);
#   3. installs and enables js-egress.service, which loads the ruleset again at boot, is
#      required by js-agent.service and removes only its own table when it stops;
#   4. lists the table and checks that every rule and both hooks are in the kernel.
# The agent (--container-egress-allow) reads all of this back before every job.
# Rerunning with the same arguments changes nothing. One egress network per host: another
# run with other arguments replaces the table.
#
# Rootful Podman with netavark only (what js-agent supports). IPv4 only: --subnet and
# --allow take IPv4, the network has no IPv6 subnet, and every IPv6 packet from the bridge
# is dropped. br_netfilter must be loaded (sysctl net.bridge.bridge-nf-call-iptables=1),
# or traffic between two containers of the bridge never reaches the hooks; the agent
# refuses jobs without it.
set -euo pipefail

cleanup() {
	local f
	for f in "${TMPFILES[@]}"; do
		rm -f -- "$f"
	done
}
trap cleanup EXIT

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
IFACE="jsbr0"
TMPFILES=()
ALLOW=()
APPLY=0

usage() {
	cat <<'EOF'
usage: egress-network.sh --network NAME --subnet CIDR --allow IP:PORT [--allow IP:PORT ...]
                         [--interface IFNAME] [--apply] [--podman PATH] [--nft PATH]
                         [--rules-file PATH]

  --network NAME     the Podman network (js-agent --container-network NAME)
  --subnet CIDR      its IPv4 subnet in network-address form, e.g. 192.0.2.0/24
  --interface IFNAME the bridge interface the network gets, at most 15 characters
                     (default jsbr0); the rules match it
  --allow IP:PORT    a TCP endpoint containers may open; IP may be a CIDR of /24 or
                     narrower (CIDR:PORT); repeatable, at least one; never inside the
                     network's subnet (the gateway included) or 169.254.0.0/16. A host
                     address of this machine is fine (the store may be local)
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

# overlaps A B: succeeds when the address or CIDR A and the CIDR B share an address.
overlaps() {
	local a_addr=${1%/*} a_len=32 b_addr=${2%/*} b_len=${2#*/} a_lo b_lo a_hi b_hi
	[[ $1 == */* ]] && a_len=$((10#${1#*/}))
	a_lo=$(ip_to_int "$a_addr")
	b_lo=$(ip_to_int "$b_addr")
	a_hi=$((a_lo | (0xFFFFFFFF >> a_len)))
	b_hi=$((b_lo | (0xFFFFFFFF >> b_len)))
	((a_lo <= b_hi && b_lo <= a_hi))
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
		--interface)
			(($# >= 2)) || die "--interface needs a value"
			IFACE=$2
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
	[[ $IFACE =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,14}$ ]] || die "--interface: not a plain interface name of at most 15 characters"
	local path
	for path in "$PODMAN" "$NFT" "$RULES_FILE"; do
		[[ $path =~ ^/[A-Za-z0-9_./+-]*$ ]] || die "paths must be absolute and use only letters, digits and _ . / + -: $path"
	done
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
			REQUIRE_NETWORK=1 cidr_check "$ip" 24 || die "--allow: not a CIDR of /24 to /32 in network-address form: $entry"
		else
			ip_to_int "$ip" >/dev/null || die "--allow: not an IPv4 address: $entry"
		fi
		if overlaps "$ip" "$SUBNET"; then
			die "--allow: lies in the network's subnet (a neighbour or the gateway): $entry"
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
# earlier version of the table, whether or not it existed. Every rule matches the
# network's bridge (iifname), so a network of another kind or subnet is outside them, and
# IPv6 from the bridge is dropped first. With br_netfilter, container-to-container
# traffic of the bridge also runs the forward hook: its destination is not an allowed
# endpoint (an endpoint in the subnet is refused), so the final drop takes it.
ruleset() {
	local entry ip port
	cat <<EOF
# Managed by egress-network.sh. Do not edit; rerun the script.
table inet ${TABLE}
delete table inet ${TABLE}
table inet ${TABLE} {
	chain forward {
		type filter hook forward priority ${PRIORITY}; policy accept;
		iifname "${IFACE}" meta nfproto ipv6 counter drop
		iifname "${IFACE}" ip daddr 169.254.0.0/16 counter drop
		iifname "${IFACE}" ct state established,related accept
EOF
	for entry in "${ALLOW[@]}"; do
		ip=${entry%:*}
		port=${entry##*:}
		printf '\t\tiifname "%s" ip saddr %s ip daddr %s tcp dport %s ct state new accept\n' "$IFACE" "$SUBNET" "$ip" "$port"
	done
	cat <<EOF
		iifname "${IFACE}" counter drop
	}

	chain input {
		type filter hook input priority ${PRIORITY}; policy accept;
		iifname "${IFACE}" meta nfproto ipv6 counter drop
		iifname "${IFACE}" ct state established,related accept
EOF
	for entry in "${ALLOW[@]}"; do
		ip=${entry%:*}
		port=${entry##*:}
		printf '\t\tiifname "%s" ip saddr %s ip daddr %s tcp dport %s ct state new accept\n' "$IFACE" "$SUBNET" "$ip" "$port"
	done
	cat <<EOF
		iifname "${IFACE}" counter drop
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
ExecStart="${NFT}" -f "${RULES_FILE}"
ExecReload="${NFT}" -f "${RULES_FILE}"
# Only this table: other tables (netavark's, the host's) are left alone.
ExecStop=-"${NFT}" delete table inet ${TABLE}

[Install]
WantedBy=multi-user.target
# The agent does not run without these rules, and stops when they are stopped.
RequiredBy=js-agent.service
EOF
}

say() {
	printf '%s\n' "$*"
}

# network_state: prints "absent", "ok", or the reason the existing network does not fit:
# a bridge named IFACE with DNS off, no IPv6, and SUBNET as its only subnet.
network_state() {
	local json
	if ! json=$("$PODMAN" network inspect "$NETWORK" 2>/dev/null); then
		say absent
		return
	fi
	if ! grep -Eq '"driver": *"bridge"' <<<"$json"; then
		say "is not a bridge network"
	elif ! grep -Eq '"dns_enabled": *false' <<<"$json"; then
		say "has DNS enabled (or no dns_enabled field)"
	elif ! grep -Eq '"ipv6_enabled": *false' <<<"$json" || grep -Eq '"subnet": *"[^"]*:' <<<"$json"; then
		say "has IPv6 (or no ipv6_enabled field)"
	elif ! grep -Eq "\"network_interface\": *\"${IFACE}\"" <<<"$json"; then
		say "has another interface than ${IFACE}"
	elif ! grep -Eq "\"subnet\": *\"${SUBNET//./\\.}\"" <<<"$json"; then
		say "has another subnet than ${SUBNET}"
	elif [[ $(grep -Ec '"subnet":' <<<"$json") != 1 ]]; then
		say "has more than one subnet"
	elif grep -Eq '"internal": *true' <<<"$json"; then
		say "is --internal (blocks all egress)"
	else
		say ok
	fi
}

# mktmp_for PATH: a temporary file next to PATH, removed when the script ends.
mktmp_for() {
	local tmp
	tmp=$(mktemp "$1.XXXXXX")
	TMPFILES+=("$tmp")
	printf '%s' "$tmp"
}

# install_file TMP PATH MODE: moves TMP to PATH when it differs, and prints whether it did.
install_file() {
	local tmp=$1 path=$2 mode=$3
	chmod "$mode" "$tmp"
	if [[ -f $path ]] && cmp -s "$tmp" "$path"; then
		say "unchanged: $path"
	else
		mv -- "$tmp" "$path"
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
	say "would write ${RULES_FILE} (0644), after nft -c accepted it as a temporary file:"
	ruleset | sed 's/^/    | /'
	say "would run (if the network is absent): ${PODMAN} network create --disable-dns --subnet ${SUBNET} --interface-name ${IFACE} ${NETWORK}"
	say "would write ${UNIT_FILE} (0644):"
	unit | sed 's/^/    | /'
	say "would run: systemctl daemon-reload; systemctl enable ${UNIT_NAME}"
	say "would run: ${NFT} -f ${RULES_FILE}"
	say "would run: ${NFT} list table inet ${TABLE}, and check that every rule and both hooks are loaded"
	say "note: the agent also needs br_netfilter (sysctl net.bridge.bridge-nf-call-iptables=1)"
}

apply() {
	((EUID == 0)) || die "--apply needs root (rootful Podman and nftables)"
	[[ -x $PODMAN ]] || die "podman not found at $PODMAN (see --podman)"
	[[ -x $NFT ]] || die "nft not found at $NFT (see --nft)"
	command -v systemctl >/dev/null || die "systemctl not found"

	# Everything that can fail without a side effect comes first: the network's state, and
	# the ruleset as a temporary file that nft accepts. Only then is anything created or
	# replaced, so a failed run leaves neither a network without rules nor a boot copy
	# that does not load.
	local state rules_tmp unit_tmp
	state=$(network_state)
	case $state in
	absent | ok) ;;
	*) die "network ${NETWORK} exists and ${state}; this script never changes an existing network" ;;
	esac
	mkdir -p "$(dirname "$RULES_FILE")"
	rules_tmp=$(mktmp_for "$RULES_FILE")
	ruleset >"$rules_tmp"
	"$NFT" -c -f "$rules_tmp" || die "nft rejected the ruleset; nothing was changed"
	unit_tmp=$(mktmp_for "$UNIT_FILE")
	unit >"$unit_tmp"

	if [[ $state == absent ]]; then
		"$PODMAN" network create --disable-dns --subnet "$SUBNET" --interface-name "$IFACE" "$NETWORK" >/dev/null
		say "created: podman network $NETWORK ($SUBNET, interface $IFACE, DNS disabled)"
		state=$(network_state)
		[[ $state == ok ]] || die "network $NETWORK was created but ${state}"
	else
		say "unchanged: podman network $NETWORK"
	fi
	install_file "$rules_tmp" "$RULES_FILE" 0644
	install_file "$unit_tmp" "$UNIT_FILE" 0644
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

# verify: the kernel has every rule that was meant to be loaded, both chains are on their
# hooks at the priority, and the unit is enabled.
verify() {
	local listed want chain
	listed=$("$NFT" list table inet "$TABLE" | normalize) || die "table inet $TABLE is not loaded"
	while IFS= read -r want; do
		grep -Fxq -- "$want" <<<"$listed" || die "rule missing from the loaded table: $want"
	done < <(ruleset | grep -E '^[[:space:]]+iifname ' | normalize)
	for chain in forward input; do
		grep -Eq "^type filter hook ${chain} priority (filter - 10|-10);" <<<"$listed" ||
			die "chain ${chain} is not a filter chain on its hook at priority ${PRIORITY}"
	done
	systemctl is-enabled --quiet "$UNIT_NAME" || die "$UNIT_NAME is not enabled"
	say "verified: every rule is loaded, both hooks are at priority ${PRIORITY} and $UNIT_NAME is enabled"
	if [[ $(cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || true) != 1 ]]; then
		say "WARNING: br_netfilter is not active (net.bridge.bridge-nf-call-iptables is not 1): the agent refuses jobs until it is"
	fi
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
