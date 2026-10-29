#!/bin/sh
trap cleanup EXIT
cleanup() {
  rm -f -- /etc/jobscheduler/ts-authkey
  rm -rf -- /var/lib/cloud/instance /var/lib/cloud/instances/* /var/lib/cloud/data/* /var/log/cloud-init*.log
}
set -eu
umask 077
# image preparation
apt-get install -y curl
# identity
mkdir -p /etc/jobscheduler
printf '%s' '***' > /etc/jobscheduler/token
chmod 0600 -- /etc/jobscheduler/token
printf '%s\n' 'gpu:<HOST>' > /etc/jobscheduler/instance
# network
curl --fail --silent --show-error --location --proto =https --output /usr/local/lib/tailscale.tgz --url https://pkgs.example.org/tailscale_static_amd64.tgz
printf '%s  %s\n' 5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a /usr/local/lib/tailscale.tgz | sha256sum --check --status -
tar -xzf /usr/local/lib/tailscale.tgz --strip-components=1 -C /usr/local/bin
printf '%s' '***' > /etc/jobscheduler/ts-authkey
chmod 0600 -- /etc/jobscheduler/ts-authkey
systemd-run --unit=tailscaled /usr/local/bin/tailscaled --state=/var/lib/tailscale/tailscaled.state
/usr/local/bin/tailscale up --auth-key=file:/etc/jobscheduler/ts-authkey --hostname=<HOST> --accept-dns=false
rm -f -- /etc/jobscheduler/ts-authkey
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
Wants=network-online.target
RequiresMountsFor=/var/lib/js-agent/scratch

[Service]
ExecStart=/usr/local/bin/js-agent --server "https://sched.example.org:7301" --token-file /etc/jobscheduler/token --instance-file /etc/jobscheduler/instance --pool "gpu" --host-id "<HOST>" --container-bin /usr/bin/docker --container-pull missing --gpu-toolkit-hook /usr/local/lib/js-agent/gpu-toolkit-hook --state-root /var/lib/js-agent/state --scratch-root /var/lib/js-agent/scratch
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
chmod 0644 /etc/systemd/system/js-agent.service
systemctl daemon-reload
systemctl enable --now js-agent.service
