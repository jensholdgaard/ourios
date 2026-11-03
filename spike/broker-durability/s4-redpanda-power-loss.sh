#!/usr/bin/env bash
# S4 (RFC 0067 §3.1): with write.caching=true, do one follower's power loss
# plus a cut-off leader lose acked records on Redpanda? WC=true or WC=false.
#
#   1. F2 is SIGSTOPped (a lagging follower), so the majority behind every
#      ack is the leader L plus F1.
#   2. N records are produced with acks=all.
#   3. F1 loses power: SIGKILL, and with WC=true its data directory is
#      restored from a copy taken just before the produce (nothing it took
#      in was fsynced: flush.ms / flush.bytes are raised so no periodic flush
#      runs in the window). With WC=false the ack already waited for F1's
#      fsync (S2), so the power loss removes nothing past the ack and the
#      model is the SIGKILL alone.
#   4. L is SIGSTOPped (cut off), F2 resumes, F1 restarts: F1 and F2 are a
#      majority and may elect a leader from themselves.
source "$(dirname "$0")/lib.sh"
trap dump_logs_on_exit EXIT
WC=${WC:?set WC=true or WC=false}
N=${N:-50}
T=s4

setup_python
install_redpanda
for i in 1 2 3; do mkdev "b$i"; done
redpanda_configure
redpanda_cluster_up
extra=()
[ "$WC" = true ] && extra=(-c flush.ms=3600000 -c flush.bytes=1073741824)
rpkx topic create "$T" -p 1 -r 3 -c "write.caching=$WC" "${extra[@]}"
sleep 5
rpkx topic describe "$T" -a

meta() { "$PY" "$HERE/k.py" meta "$1" "$T" 2>/dev/null | sed 's/^META //'; }
L=$(meta "$RBOOT" | jq -r .leader)
mapfile -t FS < <(for i in 1 2 3; do [ "$i" != "$L" ] && echo "$i"; done)
F1=${FS[0]}; F2=${FS[1]}
log "leader L=$L, F1=$F1 (will lose power), F2=$F2 (lagging)"
LP=$(rpid "$L"); F1P=$(rpid "$F1"); F2P=$(rpid "$F2")
bootof() { local o="" i; for i in "$@"; do o="$o${o:+,}127.0.0.1:1909$i"; done; echo "$o"; }

kill -STOP "$F2P"; log "F2 (node $F2) SIGSTOPped"
sleep 3
mkdir -p "$W/snap" "$W/out"
cp -a --sparse=always "$W/mnt/b$F1/data" "$W/snap/f1"
log "F1 data dir copied (pre-produce)"

"$PY" "$HERE/k.py" produce "$RBOOT" "$T" "$N" --label acked | tee "$W/out/produce.txt"
acked=$(grep '^RESULT ' "$W/out/produce.txt" | sed 's/^RESULT //' | jq -r .acked)
last=$(grep '^RESULT ' "$W/out/produce.txt" | sed 's/^RESULT //' | jq -r '.offsets[-1]')
want=$((last + 1))
log "acked $acked records (+warmup), last offset $last"
"$PY" "$HERE/k.py" count "$(bootof "$L")" "$T" | tee "$W/out/count-before.txt" || true

kill -KILL "$F1P"; while kill -0 "$F1P" 2>/dev/null; do sleep 0.2; done
log "F1 (node $F1) SIGKILLed"
if [ "$WC" = true ]; then
  find "$W/mnt/b$F1/data/kafka/$T" -type f -printf '%p %s\n' | sort > "$W/out/f1-files-at-kill.txt"
  rm -rf "$W/mnt/b$F1/data"
  cp -a --sparse=always "$W/snap/f1" "$W/mnt/b$F1/data"
  find "$W/mnt/b$F1/data/kafka/$T" -type f -printf '%p %s\n' | sort > "$W/out/f1-files-restored.txt"
  log "F1 data dir restored to the pre-produce copy"
  diff "$W/out/f1-files-at-kill.txt" "$W/out/f1-files-restored.txt" || true
fi

kill -STOP "$LP"; log "L (node $L) SIGSTOPped (cut off)"
kill -CONT "$F2P"; log "F2 (node $F2) resumed"
redpanda_start "$F1"
redpanda_wait_up "$F1" || true

B2=$(bootof "$F1" "$F2")
newleader=none
for n in $(seq 1 60); do
  l=$(meta "$B2" | jq -r '.leader // "none"' || echo none)
  if [ "$l" = "$F1" ] || [ "$l" = "$F2" ]; then newleader=$l; break; fi
  sleep 2
done
log "leader among F1/F2 after $((n * 2)) s: $newleader"
RB=$B2 rpkx topic describe "$T" -p || true
"$PY" "$HERE/k.py" count "$B2" "$T" | tee "$W/out/count-split.txt" || true
read_split=$(grep '^COUNT ' "$W/out/count-split.txt" | sed 's/^COUNT //' | jq -r '.read // 0')

kill -CONT "$LP"; log "L (node $L) resumed"
sleep 20
rpkx topic describe "$T" -p || true
"$PY" "$HERE/k.py" count "$RBOOT" "$T" | tee "$W/out/count-final.txt" || true
read_final=$(grep '^COUNT ' "$W/out/count-final.txt" | sed 's/^COUNT //' | jq -r '.read // 0')
grep -h -i 'truncat\|election\|vote' "$W/rlogs/b$F1.out" | tail -30 || true

ver=$(rpk version | head -1)
echo
echo "=== S4 summary (write.caching=$WC, $ver) ==="
echo "acked: $acked (+1 warmup), offsets 0..$last; L=$L F1=$F1 F2=$F2"
echo "leader elected by F1+F2 while L cut off: $newleader"
echo "readable from F1+F2 majority: $read_split of $want; after L resumed: $read_final of $want"
if [ "$read_final" -lt "$want" ] || [ "$read_split" -lt "$want" ]; then
  verdict="ACKED LOSS (split: $((want - read_split)) missing, final: $((want - read_final)) missing)"
else
  verdict="no loss"
fi
echo "S4 RESULT write.caching=$WC: $verdict; new leader=$newleader"
