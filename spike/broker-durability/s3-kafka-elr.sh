#!/usr/bin/env bash
# S3 (RFC 0067 §3.1, "One machine loses power"): with flush.messages unset,
# can a replica that lost its unflushed data become leader and lose acked
# records? Run with ELR=1 and ELR=0; unclean leader election off both times.
#
# Page-cache loss is modelled by SIGKILL plus restoring the broker's whole log
# directory from a copy taken just before the produce: what had not been
# fsynced since then is gone, and no clean-shutdown marker exists.
source "$(dirname "$0")/lib.sh"
trap dump_logs_on_exit EXIT
ELR=${ELR:?set ELR=0 or 1}
N=${N:-50}
T=s3

setup_python
install_kafka
for i in 1 2 3; do mkdev "b$i"; done
kafka_configure "$ELR"
kafka_cluster_up
kfeatures

kt --create --topic "$T" --replica-assignment 1:2:3 \
  --config min.insync.replicas=2 --config unclean.leader.election.enable=false
wait_isr "$T" 3
getleader() { { "$PY" "$HERE/k.py" meta "$1" "$T" 2>/dev/null || true; } | sed 's/^META //' | jq -r '.leader // "none"' | head -1; }
state() { echo "--- state: $1"; KB=$2 kt --describe --topic "$T" || echo "(describe failed)"; }
state "start" "$KBOOT"

sleep 6  # let the replication-offset checkpoint settle before the copy
mkdir -p "$W/snap" "$W/out"
cp -a --sparse=always "$W/mnt/b1/data" "$W/snap/b1"
log "broker 1 log dir copied (pre-produce): $(ls -l "$W/snap/b1/$T-0" | tr '\n' ' ')"

"$PY" "$HERE/k.py" produce "$KBOOT" "$T" "$N" --label acked | tee "$W/out/produce.txt"
acked=$(grep '^RESULT ' "$W/out/produce.txt" | sed 's/^RESULT //' | jq -r '.acked')
last=$(grep '^RESULT ' "$W/out/produce.txt" | sed 's/^RESULT //' | jq -r '.offsets[-1]')
want=$((last + 1))
"$PY" "$HERE/k.py" count "$KBOOT" "$T" | tee "$W/out/count-before.txt"
state "after produce ($acked acked, last offset $last)" "$KBOOT"

kafka_term 2
sleep 3
state "broker 2 stopped cleanly" "127.0.0.1:19091"
kafka_term 3
sleep 3
state "broker 3 stopped cleanly" "127.0.0.1:19091"

kafka_kill 1
rm -rf "$W/mnt/b1/data"
cp -a --sparse=always "$W/snap/b1" "$W/mnt/b1/data"
log "broker 1 page cache lost: log dir restored to the pre-produce copy: $(ls -l "$W/mnt/b1/data/$T-0" | tr '\n' ' ')"
ls -la "$W/mnt/b1/data"
sleep 15  # controller fences broker 1 (broker.session.timeout.ms 9 s)

kafka_start 1
kafka_wait_up 1
sleep 20
state "truncated broker 1 restarted, 2 and 3 down" "127.0.0.1:19091"
leader1=$(getleader 127.0.0.1:19091)
"$PY" "$HERE/k.py" count 127.0.0.1:19091 "$T" | tee "$W/out/count-after-1.txt" || true

kafka_start 3
kafka_wait_up 3
sleep 20
state "broker 3 (last to leave the ISR, clean shutdown) restarted" "127.0.0.1:19091,127.0.0.1:19093"
leader3=$(getleader 127.0.0.1:19091,127.0.0.1:19093)

kafka_start 2
kafka_wait_up 2
sleep 20
state "all three up" "$KBOOT"
"$PY" "$HERE/k.py" count "$KBOOT" "$T" | tee "$W/out/count-final.txt" || true
read_final=$(grep '^COUNT ' "$W/out/count-final.txt" | sed 's/^COUNT //' | jq -r '.read // 0')
grep -h -i 'elr\|unclean\|leader election\|truncat' "$W/klogs/ctrl.out" | tail -40 || true
grep -h -i 'truncat' "$W"/klogs/b*/server.log 2>/dev/null | tail -20 || true

echo
echo "=== S3 summary (ELR=$ELR, unclean.leader.election.enable=false, Kafka $KAFKA_VERSION) ==="
echo "acked records: $acked (+1 warmup), offsets 0..$last"
echo "leader with only truncated broker 1 up: $leader1"
echo "leader after clean broker 3 returned: $leader3"
echo "records readable at the end: $read_final of $want"
if [ "$read_final" -lt "$want" ]; then verdict="ACKED LOSS ($((want - read_final)) records)"; else verdict="no loss"; fi
echo "S3 RESULT ELR=$ELR: $verdict; leader-while-only-truncated-up=$leader1; leader-after-broker-3=$leader3"
if [ "$ELR" = 1 ]; then
  [ "$read_final" -ge "$want" ] && [ "$leader1" != 1 ] && echo "S3[ELR=1] PASS: truncated replica not elected, no acked loss" || { echo "S3[ELR=1] FAIL"; exit 1; }
else
  [ "$read_final" -lt "$want" ] && echo "S3[ELR=0] PASS (expected loss reproduced)" || echo "S3[ELR=0] no loss reproduced: see the state log"
fi
