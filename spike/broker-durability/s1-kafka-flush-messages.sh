#!/usr/bin/env bash
# S1 (RFC0066.23): does Kafka's flush.messages=1 make the ack wait for the
# followers' fsync? Followers 2 and 3 sit on dm-delay devices; leader 1 does not.
source "$(dirname "$0")/lib.sh"
trap dump_logs_on_exit EXIT
DELAY=${DELAY:-2000}
N=${N:-20}

setup_python
install_kafka
for i in 1 2 3; do mkdev "b$i"; done
kafka_configure 1
kafka_cluster_up
kfeatures

for t in s1-flush s1-noflush; do
  extra=()
  [ "$t" = s1-flush ] && extra=(--config flush.messages=1)
  kt --create --topic "$t" --replica-assignment 1:2:3 \
    --config min.insync.replicas=2 --config unclean.leader.election.enable=false "${extra[@]}"
  wait_isr "$t" 3
done
kt --describe --topic s1-flush,s1-noflush
kc --describe --entity-type topics --entity-name s1-flush
kc --describe --entity-type topics --entity-name s1-noflush

mkdir -p "$W/out"
run() { # run LABEL TOPIC
  "$PY" "$HERE/k.py" meta "$KBOOT" "$2"
  "$PY" "$HERE/k.py" produce "$KBOOT" "$2" "$N" --label "$1" | tee "$W/out/$1.txt"
  "$PY" "$HERE/k.py" meta "$KBOOT" "$2"
}

run nodelay-flush s1-flush

for i in 2 3; do setdelay "b$i" "$DELAY" "$DELAY"; probe_delay "b$i"; done
probe_delay b1
run wf-noflush s1-noflush
run wf-flush s1-flush
run wf-noflush-again s1-noflush

for i in 2 3; do setdelay "b$i" "$DELAY" 0; probe_delay "b$i"; done
run w-flush s1-flush
for i in 2 3; do setdelay "b$i" 0 "$DELAY"; probe_delay "b$i"; done
run f-flush s1-flush
for i in 2 3; do setdelay "b$i" 0 0; done

r() { grep '^RESULT ' "$W/out/$1.txt" | sed 's/^RESULT //' | jq -r ".$2"; }
echo
echo "=== S1 summary (delay ${DELAY} ms on followers 2,3; leader 1 undelayed) ==="
for a in nodelay-flush wf-noflush wf-flush wf-noflush-again w-flush f-flush; do
  printf '%-18s acked=%s min=%s median=%s max=%s ms  lat=%s\n' "$a" "$(r "$a" acked)" \
    "$(r "$a" min_ms)" "$(r "$a" median_ms)" "$(r "$a" max_ms)" "$(r "$a" 'lat_ms|tostring')"
done
fmin=$(r wf-flush min_ms); nmed=$(r wf-noflush median_ms); facked=$(r wf-flush acked)
if [ "$facked" = "$N" ] && awk -v a="$fmin" -v b="$nmed" -v d="$DELAY" 'BEGIN{exit !(a>=d && b<d/4)}'; then
  echo "S1 PASS: flush.messages=1 min ack ${fmin} ms >= ${DELAY} ms; unset median ${nmed} ms < $((DELAY / 4)) ms (Kafka $KAFKA_VERSION)"
else
  echo "S1 FAIL: flush.messages=1 min ack ${fmin} ms (acked $facked/$N); unset median ${nmed} ms; delay ${DELAY} ms (Kafka $KAFKA_VERSION)"
  exit 1
fi
