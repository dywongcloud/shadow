#!/usr/bin/env bash
# Region-affinity ingress (iad/sin/sfo/lax) — deployment URLs become
# `<dep>.<code>.ngrok.pizza`, entering DIRECTLY on a node in that region.
# Two changes per node:
#   1. ngrok.yml: a per-region POOLED endpoint `*.<code>.ngrok.pizza` -> :8787
#   2. hive-node: HIVE_DEPLOY_SUFFIXES so the edge gate accepts `<dep>.<code>.ngrok.pizza`
#      (the edge already extracts the alias as the first host label — no code change).
#
# PREREQUISITE: the 4 WILDCARD domains must be reserved in ngrok
# (dashboard.ngrok.com/domains/new), exactly:
#   *.iad.ngrok.pizza  *.sin.ngrok.pizza  *.sfo.ngrok.pizza  *.lax.ngrok.pizza
# NOT the apex `iad.ngrok.pizza` — ngrok needs the wildcard (ERR_NGROK_318).
# SAFE: backs up configs and AUTO-REVERTS any node whose ngrok doesn't come back up.
#
# Usage: KEY=~/.ssh/billing.pem ./ngrok-region-rollout.sh           # apply
#        KEY=~/.ssh/billing.pem ./ngrok-region-rollout.sh --revert  # undo
set -euo pipefail
KEY="${KEY:-$HOME/.ssh/billing.pem}"
REVERT="${1:-}"
# Every node accepts EVERY region suffix (mesh can forward cross-region) + the legacy
# zone + localhost. Keep `deployment.shadow.ngrok.pizza` so existing URLs keep working.
SUFFIXES="deployment.shadow.ngrok.pizza,iad.ngrok.pizza,sin.ngrok.pizza,sfo.ngrok.pizza,lax.ngrok.pizza,localhost"

NODES=(
  "43.166.206.175 iad"   # fc-virginia
  "170.106.40.67  iad"   # fc-virginia-2
  "43.152.247.70  sin"   # fc-bangkok
  "170.106.158.151 sfo"  # fc-sanjose
)

for entry in "${NODES[@]}"; do
  ip="${entry%% *}"; code="${entry##* }"
  echo "== $ip (code=$code) =="
  if [ "$REVERT" = "--revert" ]; then
    ssh -i "$KEY" -o ConnectTimeout=20 "root@$ip" '
      [ -f /etc/ngrok/ngrok.yml.preregion ] && mv /etc/ngrok/ngrok.yml.preregion /etc/ngrok/ngrok.yml
      rm -f /etc/systemd/system/hive-node.service.d/deploy-suffixes.conf
      systemctl daemon-reload; systemctl restart ngrok* hive-node 2>/dev/null; sleep 7
      echo "  reverted; ngrok=$(systemctl is-active ngrok 2>/dev/null) hive=$(systemctl is-active hive-node 2>/dev/null)"'
    continue
  fi
  ssh -i "$KEY" -o ConnectTimeout=40 "root@$ip" "CODE=$code SUFFIXES=$SUFFIXES"' bash -s' <<"EOS"
    set -e
    # --- 1) edge gate: accept <dep>.<code>.ngrok.pizza ---
    mkdir -p /etc/systemd/system/hive-node.service.d
    printf '[Service]\nEnvironment=HIVE_DEPLOY_SUFFIXES=%s\n' "$SUFFIXES" \
      > /etc/systemd/system/hive-node.service.d/deploy-suffixes.conf
    systemctl daemon-reload && systemctl restart hive-node && sleep 8
    [ "$(systemctl is-active hive-node)" = active ] || { echo "  !! hive-node not active"; exit 1; }
    # --- 2) ngrok per-region endpoint ---
    cfg=/etc/ngrok/ngrok.yml; cp -n "$cfg" "$cfg.preregion"
    if ! grep -q "\*.${CODE}.ngrok.pizza" "$cfg"; then
      cat >> "$cfg" <<EOF
  - name: deployments-${CODE}
    url: https://*.${CODE}.ngrok.pizza
    pooling_enabled: true
    upstream:
      url: 8787
EOF
      echo "  added *.${CODE}.ngrok.pizza endpoint"
    fi
    systemctl restart ngrok* 2>/dev/null; sleep 7
    if [ "$(systemctl is-active ngrok 2>/dev/null)" != active ]; then
      echo "  !! ngrok NOT active (wildcard not reserved?) — AUTO-REVERTING ngrok"
      cp "$cfg.preregion" "$cfg"; systemctl restart ngrok* 2>/dev/null; sleep 6
      echo "  ngrok reverted=$(systemctl is-active ngrok)"; exit 1
    fi
    echo "  OK: hive-node + ngrok active; region endpoint live"
EOS
done
echo "Done. Verify: curl -sI https://<dep>.<code>.ngrok.pizza/"
