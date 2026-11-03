#!/usr/bin/env bash
# S5 Kafka (RFC 0066 §3.1): what DescribeConfigs reports, per answering broker,
# when only broker 1 carries log.flush.interval.messages=1 and
# min.insync.replicas=2 as static config and the topic has no overrides; then
# with topic overrides. Also the finalized ELR feature level.
source "$(dirname "$0")/lib.sh"
trap dump_logs_on_exit EXIT
T=s5
KEYS='flush.messages min.insync.replicas unclean.leader.election.enable'

setup_python
install_kafka
for i in 1 2 3; do mkdir -p "$W/mnt/b$i"; done
S5_STATIC_ONE_BROKER=1 kafka_configure 1
kafka_cluster_up
echo "=== finalized features ==="
kfeatures

kt --create --topic "$T" --replica-assignment 1:2:3
wait_isr "$T" 3
mkdir -p "$W/out"

table() { # table PHASE
  local i b
  echo "=== $1: DescribeConfigs v1 (include_synonyms) sent to each broker ==="
  for i in 1 2 3; do
    b=127.0.0.1:$(kport "$i")
    "$PY" "$HERE/k.py" describe "$b" "$T" > "$W/out/$1-b$i.json"
    for k in $KEYS; do
      sed 's/^DESCRIBE //' "$W/out/$1-b$i.json" | jq -r --arg k "$k" --arg b "broker $i" \
        '"S5K \($b) \($k) value=\(.configs[$k].value) source=\(.configs[$k].source) synonyms=\(.configs[$k].synonyms|tostring)"'
    done
  done
  for i in 1 2 3; do
    echo "--- kafka-configs.sh --describe --all, bootstrap broker $i only"
    KB=127.0.0.1:$(kport "$i") kc --describe --all --entity-type topics --entity-name "$T" \
      | grep -E 'flush.messages=|min.insync.replicas=|unclean.leader.election.enable=' || true
  done
}

table no-overrides
kc --alter --entity-type topics --entity-name "$T" \
  --add-config flush.messages=1,min.insync.replicas=2,unclean.leader.election.enable=false
sleep 3
table overrides
echo "S5-kafka DONE"
