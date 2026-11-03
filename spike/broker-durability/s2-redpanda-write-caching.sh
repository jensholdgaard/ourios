#!/usr/bin/env bash
# S2: does Redpanda's write.caching=false make the acks=all ack wait for a
# majority fsync, and write.caching=true not? Both followers are delayed, so
# every majority includes a delayed disk.
source "$(dirname "$0")/lib.sh"
trap dump_logs_on_exit EXIT
DELAY=${DELAY:-2000}
N=${N:-20}
T=s2

setup_python
install_redpanda
for i in 1 2 3; do mkdev "b$i"; done
redpanda_configure
redpanda_cluster_up
rpkx cluster config get write_caching_default
rpkx topic create "$T" -p 1 -r 3 -c write.caching=false
sleep 5
rpkx topic describe "$T" -a
leader=$("$PY" "$HERE/k.py" meta "$RBOOT" "$T" | sed 's/^META //' | jq -r .leader)
log "leader of $T is node $leader"

mkdir -p "$W/out"
run() { # run LABEL
  "$PY" "$HERE/k.py" meta "$RBOOT" "$T"
  "$PY" "$HERE/k.py" produce "$RBOOT" "$T" "$N" --label "$1" | tee "$W/out/$1.txt"
  "$PY" "$HERE/k.py" meta "$RBOOT" "$T"
}

run nodelay-wcfalse
for i in 1 2 3; do
  [ "$i" = "$leader" ] && continue
  setdelay "b$i" "$DELAY" "$DELAY"; probe_delay "b$i"
done
probe_delay "b$leader"
run wf-wcfalse
rpkx topic alter-config "$T" --set write.caching=true
rpkx topic describe "$T" -c | grep -i 'write.caching\|flush'
run wf-wctrue
rpkx topic alter-config "$T" --set write.caching=false
run wf-wcfalse-again
for i in 1 2 3; do setdelay "b$i" 0 0; done

r() { grep '^RESULT ' "$W/out/$1.txt" | sed 's/^RESULT //' | jq -r ".$2"; }
echo
echo "=== S2 summary (delay ${DELAY} ms on the two followers; leader $leader undelayed) ==="
for a in nodelay-wcfalse wf-wcfalse wf-wctrue wf-wcfalse-again; do
  printf '%-18s acked=%s min=%s median=%s max=%s ms  lat=%s\n' "$a" "$(r "$a" acked)" \
    "$(r "$a" min_ms)" "$(r "$a" median_ms)" "$(r "$a" max_ms)" "$(r "$a" 'lat_ms|tostring')"
done
fmin=$(r wf-wcfalse min_ms); tmed=$(r wf-wctrue median_ms); facked=$(r wf-wcfalse acked)
ver=$(rpk version | head -1)
if [ "$facked" = "$N" ] && awk -v a="$fmin" -v b="$tmed" -v d="$DELAY" 'BEGIN{exit !(a>=d && b<d/4)}'; then
  echo "S2 PASS: write.caching=false min ack ${fmin} ms >= ${DELAY} ms; true median ${tmed} ms < $((DELAY / 4)) ms ($ver)"
else
  echo "S2 FAIL: write.caching=false min ack ${fmin} ms (acked $facked/$N); true median ${tmed} ms; delay ${DELAY} ms ($ver)"
  exit 1
fi
