#!/bin/sh
trap cleanup EXIT
cleanup() {
  rm -f --
  rm -rf -- /var/lib/cloud/instance /var/lib/cloud/instances/* /var/lib/cloud/data/* /var/log/cloud-init*.log
}
set -eu
umask 077
# image preparation
nvidia-smi
test -x /usr/bin/docker
install -d -m 0700 -- /var/lib/js-agent/scratch
fallocate -l 200G -- /var/lib/js-agent/scratch.img
chmod 0600 -- /var/lib/js-agent/scratch.img
mkfs.ext4 -q -F -m 0 -L js-scratch /var/lib/js-agent/scratch.img
sed -i -e '$a /var/lib/js-agent/scratch.img /var/lib/js-agent/scratch ext4 loop,nodev,nosuid,noatime,nofail 0 0' /etc/fstab
mount -o loop,nodev,nosuid,noatime /var/lib/js-agent/scratch.img /var/lib/js-agent/scratch
chmod 0700 -- /var/lib/js-agent/scratch
# metadata block
test -x /usr/sbin/nft
mkdir -p /etc/jobscheduler
cat > /etc/jobscheduler/metadata-block.nft <<'RULES'
table inet js_metadata
delete table inet js_metadata
table inet js_metadata {
  chain forward {
    type filter hook forward priority -10; policy accept;
    ip daddr 169.254.169.254 drop
  }
  chain output {
    type filter hook output priority -10; policy accept;
    meta skuid != 0 ip daddr 169.254.169.254 drop
  }
}
RULES
chmod 0644 /etc/jobscheduler/metadata-block.nft
cat > /etc/systemd/system/js-metadata-block.service <<'BLOCK'
[Unit]
Description=jobscheduler metadata address block
DefaultDependencies=no
After=nftables.service
Before=network-pre.target js-agent.service
Wants=network-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/sbin/nft --file /etc/jobscheduler/metadata-block.nft

[Install]
WantedBy=multi-user.target
BLOCK
chmod 0644 /etc/systemd/system/js-metadata-block.service
systemctl daemon-reload
systemctl enable --now js-metadata-block.service
test "$(/usr/sbin/nft list table inet js_metadata | grep -c drop)" = 2
# container user namespace
for f in /etc/subuid /etc/subgid; do test -e "$f" || install -m 0644 /dev/null "$f"; awk -F: -v u=js-jobs -v lo=2000000000 -v hi=2000065536 '/^#/ { next } $1 == u || ($2 + 0 < hi + 0 && $2 + $3 > lo + 0) { bad = 1 } END { exit bad }' "$f" || { echo "bootstrap: $f is unreadable, holds an entry for js-jobs, or holds one overlapping 2000000000:65536" >&2; exit 1; }; done
mkdir -p /etc/docker
python3 -I - <<'PY'
import json, os, sys
path = "/etc/docker/daemon.json"
user = "js-jobs"
try:
    with open(path) as f:
        text = f.read()
except FileNotFoundError:
    text = ""
conf = json.loads(text) if text.strip() else {}
if not isinstance(conf, dict):
    sys.exit("bootstrap: " + path + " is not a JSON object")
if conf.get("userns-remap", user) != user:
    sys.exit("bootstrap: " + path + " already sets userns-remap to " + str(conf["userns-remap"]))
conf["userns-remap"] = user
tmp = path + ".js-tmp"
with open(tmp, "w") as f:
    json.dump(conf, f, indent=2)
    f.write("\n")
os.chmod(tmp, 0o644)
os.rename(tmp, path)
PY
useradd --system --no-create-home --shell /usr/sbin/nologin js-jobs
printf '%s\n' js-jobs:2000000000:65536 >> /etc/subuid
printf '%s\n' js-jobs:2000000000:65536 >> /etc/subgid
systemctl restart docker.service
# identity
mkdir -p /etc/jobscheduler
printf '%s' '***' > /etc/jobscheduler/token
chmod 0600 -- /etc/jobscheduler/token
printf '%s\n' 'gpu:<HOST>' > /etc/jobscheduler/instance
# network
# agent
install -d -m 0700 -- /var/lib/js-agent/state /var/lib/js-agent/scratch
curl --fail --silent --show-error --location --proto =https --output /usr/local/bin/js-agent --url https://sched.example.org:7301/v1/bootstrap/agent/x86_64
printf '%s  %s\n' abababababababababababababababababababababababababababababababab /usr/local/bin/js-agent | sha256sum --check --status -
chmod 0755 /usr/local/bin/js-agent
# gpu toolkit hook
install -d -m 0755 -- /usr/local/lib/js-agent
cat > /usr/local/lib/js-agent/gpu-toolkit-hook <<'HOOK'
#!/bin/sh
exec nvidia-container-cli info
HOOK
chmod 0700 /usr/local/lib/js-agent/gpu-toolkit-hook
cat > /etc/systemd/system/js-agent.service <<'UNIT'
[Unit]
Description=jobscheduler agent
After=network-online.target
Requires=js-metadata-block.service
After=js-metadata-block.service
Wants=network-online.target
RequiresMountsFor=/var/lib/js-agent/scratch

[Service]
ExecStart=/usr/local/bin/js-agent --server "https://sched.example.org:7301" --token-file /etc/jobscheduler/token --instance-file /etc/jobscheduler/instance --pool "gpu" --host-id "<HOST>" --runtime docker --container-bin /usr/bin/docker --container-subuid js-jobs --container-pull missing --gpu-toolkit-hook /usr/local/lib/js-agent/gpu-toolkit-hook --state-root /var/lib/js-agent/state --scratch-root /var/lib/js-agent/scratch
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
chmod 0644 /etc/systemd/system/js-agent.service
systemctl daemon-reload
systemctl enable --now js-agent.service
