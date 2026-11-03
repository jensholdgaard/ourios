#!/usr/bin/env bash
# S5 Redpanda (RFC 0066 §3.1): which keys a topic DescribeConfigs returns, with
# write_caching_default unset and then set cluster-wide.
source "$(dirname "$0")/lib.sh"
trap dump_logs_on_exit EXIT
T=s5
KEYS='write.caching min.insync.replicas unclean.leader.election.enable flush.messages replication.factor flush.ms flush.bytes cleanup.policy retention.ms retention.bytes max.message.bytes'

setup_python
install_redpanda
for i in 1 2 3; do mkdir -p "$W/mnt/b$i"; done
redpanda_configure
redpanda_cluster_up
rpkx topic create "$T" -p 1 -r 3
sleep 3
mkdir -p "$W/out"

table() {
  local i
  echo "=== $1 ==="
  rpkx cluster config get write_caching_default || true
  for i in 1 2 3; do
    "$PY" "$HERE/k.py" describe "127.0.0.1:1909$i" "$T" > "$W/out/$1-b$i.json"
  done
  sed 's/^DESCRIBE //' "$W/out/$1-b1.json" | jq -r '"all keys returned: \(.configs|keys|join(","))"'
  for k in $KEYS; do
    for i in 1 2 3; do
      sed 's/^DESCRIBE //' "$W/out/$1-b$i.json" | jq -r --arg k "$k" --arg b "node $i" \
        'if .configs|has($k) then "S5R \($b) \($k) PRESENT value=\(.configs[$k].value) source=\(.configs[$k].source)" else "S5R \($b) \($k) ABSENT" end'
    done
  done
  rpkx topic describe "$T" -c || true
}

table "write_caching_default unset"
rpkx cluster config set write_caching_default true
sleep 3
table "write_caching_default=true"
rpkx topic alter-config "$T" --set write.caching=false
sleep 2
table "write_caching_default=true, topic write.caching=false"
echo "S5-redpanda DONE"
