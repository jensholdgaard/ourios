# shellcheck shell=bash
# Shared helpers for the broker-durability spike. Sourced by the s*.sh scripts.
set -euo pipefail

W=${W:-/tmp/spike}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
KAFKA_VERSION=${KAFKA_VERSION:-4.1.2}
KAFKA_HOME=$W/kafka_2.13-$KAFKA_VERSION
PY=$W/venv/bin/python
export KAFKA_HEAP_OPTS="-Xmx512m -Xms256m"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*" >&2; }

# --- devices ----------------------------------------------------------------

# mkdev NAME: a 4 GiB loop file behind a dm-delay device (no delay yet),
# ext4 without a journal (so the only device I/O an append causes is the
# writeback or fsync of its own pages), mounted at $W/mnt/NAME.
mkdev() {
  local name=$1 img=$W/img/$1.img loop sectors
  mkdir -p "$W/img" "$W/mnt/$name"
  truncate -s 4G "$img"
  loop=$(sudo losetup --find --show "$img")
  sectors=$(sudo blockdev --getsz "$loop")
  echo "$loop" > "$W/img/$name.loop"
  echo "$sectors" > "$W/img/$name.sectors"
  echo "0 $sectors delay $loop 0 0 $loop 0 0 $loop 0 0" | sudo dmsetup create "$name"
  sudo mkfs.ext4 -q -F -O ^has_journal "/dev/mapper/$name"
  sudo mount -o noatime "/dev/mapper/$name" "$W/mnt/$name"
  sudo chown "$(id -u):$(id -g)" "$W/mnt/$name"
  log "device $name: $loop -> /dev/mapper/$name ($sectors sectors) at $W/mnt/$name"
}

# setdelay NAME WRITE_MS FLUSH_MS: swap the live table (reads never delayed).
setdelay() {
  local name=$1 w=$2 f=$3 loop sectors
  loop=$(cat "$W/img/$name.loop")
  sectors=$(cat "$W/img/$name.sectors")
  sudo dmsetup suspend "$name"
  echo "0 $sectors delay $loop 0 0 $loop 0 $w $loop 0 $f" | sudo dmsetup reload "$name"
  sudo dmsetup resume "$name"
  log "device $name: delay write=${w}ms flush=${f}ms -> $(sudo dmsetup table "$name")"
}

# Prove the table does what we think before trusting any broker number.
probe_delay() {
  local name=$1 t0 t1
  t0=$(date +%s%N)
  dd if=/dev/zero of="$W/mnt/$name/probe" bs=4k count=1 conv=fsync status=none
  t1=$(date +%s%N)
  rm -f "$W/mnt/$name/probe"
  echo "PROBE $name dd+fsync 4k: $(( (t1 - t0) / 1000000 )) ms"
}

# --- python -----------------------------------------------------------------

setup_python() {
  python3 -m venv "$W/venv"
  "$W/venv/bin/pip" install -q confluent-kafka
  "$PY" -c 'import confluent_kafka as c; print("confluent-kafka", c.version(), "librdkafka", c.libversion())'
}

# --- kafka ------------------------------------------------------------------

install_kafka() {
  mkdir -p "$W"
  if [ ! -d "$KAFKA_HOME" ]; then
    curl -fsSL "https://archive.apache.org/dist/kafka/$KAFKA_VERSION/kafka_2.13-$KAFKA_VERSION.tgz" | tar -xz -C "$W"
  fi
  java -version 2>&1 | head -1
  "$KAFKA_HOME/bin/kafka-topics.sh" --version
}

kport() { echo $((19090 + $1)); }
KBOOT=127.0.0.1:19091,127.0.0.1:19092,127.0.0.1:19093

# kafka_configure ELR_VERSION [EXTRA_FOR_BROKER_1...]: one controller (node 100,
# undelayed disk) plus brokers 1..3 whose log dirs live on $W/mnt/b{1,2,3}.
kafka_configure() {
  local elr=$1; shift
  local cid i cfg
  mkdir -p "$W/kcfg" "$W/klogs" "$W/ctrl"
  cat > "$W/kcfg/ctrl.properties" <<EOF
process.roles=controller
node.id=100
controller.quorum.voters=100@127.0.0.1:19190
listeners=CONTROLLER://127.0.0.1:19190
controller.listener.names=CONTROLLER
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
log.dirs=$W/ctrl/data
min.insync.replicas=2
unclean.leader.election.enable=false
EOF
  for i in 1 2 3; do
    cfg=$W/kcfg/b$i.properties
    cat > "$cfg" <<EOF
process.roles=broker
node.id=$i
controller.quorum.voters=100@127.0.0.1:19190
controller.listener.names=CONTROLLER
listeners=PLAINTEXT://127.0.0.1:$(kport "$i")
advertised.listeners=PLAINTEXT://127.0.0.1:$(kport "$i")
inter.broker.listener.name=PLAINTEXT
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
log.dirs=$W/mnt/b$i/data
auto.create.topics.enable=false
offsets.topic.replication.factor=3
transaction.state.log.replication.factor=3
transaction.state.log.min.isr=2
group.initial.rebalance.delay.ms=0
auto.leader.rebalance.enable=false
unclean.leader.election.enable=false
EOF
  done
  if [ "${S5_STATIC_ONE_BROKER:-0}" = 1 ]; then
    printf 'log.flush.interval.messages=1\nmin.insync.replicas=2\n' >> "$W/kcfg/b1.properties"
  else
    for i in 1 2 3; do echo "min.insync.replicas=2" >> "$W/kcfg/b$i.properties"; done
  fi
  cid=$("$KAFKA_HOME/bin/kafka-storage.sh" random-uuid)
  "$KAFKA_HOME/bin/kafka-storage.sh" format -t "$cid" -c "$W/kcfg/ctrl.properties" \
    --feature "eligible.leader.replicas.version=$elr"
  for i in 1 2 3; do
    "$KAFKA_HOME/bin/kafka-storage.sh" format -t "$cid" -c "$W/kcfg/b$i.properties"
  done
}

kafka_start_ctrl() {
  nohup "$KAFKA_HOME/bin/kafka-server-start.sh" "$W/kcfg/ctrl.properties" > "$W/klogs/ctrl.out" 2>&1 &
  echo $! > "$W/klogs/ctrl.pid"
}

kafka_start() {
  local i=$1
  LOG_DIR=$W/klogs/b$i nohup "$KAFKA_HOME/bin/kafka-server-start.sh" "$W/kcfg/b$i.properties" \
    > "$W/klogs/b$i.out" 2>&1 &
  echo $! > "$W/klogs/b$i.pid"
  log "kafka broker $i started pid $(cat "$W/klogs/b$i.pid")"
}

kafka_wait_up() {
  local i=$1 n
  for n in $(seq 1 90); do
    if "$KAFKA_HOME/bin/kafka-broker-api-versions.sh" --bootstrap-server "127.0.0.1:$(kport "$i")" >/dev/null 2>&1; then
      log "kafka broker $i answers"; return 0
    fi
    sleep 1
  done
  log "kafka broker $i never answered"; tail -50 "$W/klogs/b$i.out"; return 1
}

# The java pid of broker i (the start script execs java, so $! is it).
kpid() { cat "$W/klogs/b$1.pid"; }

kafka_term() {
  local i=$1 pid
  pid=$(kpid "$i")
  kill -TERM "$pid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.5; done
  log "kafka broker $i stopped cleanly (SIGTERM)"
}

kafka_kill() {
  local i=$1 pid
  pid=$(kpid "$i")
  kill -KILL "$pid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
  log "kafka broker $i SIGKILLed"
}

kafka_cluster_up() {
  kafka_start_ctrl
  sleep 3
  for i in 1 2 3; do kafka_start "$i"; done
  for i in 1 2 3; do kafka_wait_up "$i"; done
}

kt() { "$KAFKA_HOME/bin/kafka-topics.sh" --bootstrap-server "${KB:-$KBOOT}" "$@"; }
kc() { "$KAFKA_HOME/bin/kafka-configs.sh" --bootstrap-server "${KB:-$KBOOT}" "$@"; }
kfeatures() { "$KAFKA_HOME/bin/kafka-features.sh" --bootstrap-server "${KB:-$KBOOT}" describe; }

# wait_isr TOPIC N: until partition 0 shows N in-sync replicas.
wait_isr() {
  local topic=$1 want=$2 n
  for n in $(seq 1 60); do
    if "$PY" "$HERE/k.py" meta "$KBOOT" "$topic" --isr-count "$want" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  log "ISR of $topic never reached $want"; "$PY" "$HERE/k.py" meta "$KBOOT" "$topic" || true; return 1
}

# --- redpanda ---------------------------------------------------------------

install_redpanda() {
  curl -1sLf 'https://dl.redpanda.com/nzc4ZYQK3WRGd9sy/redpanda/cfg/setup/bash.deb.sh' | sudo -E bash >/dev/null
  sudo apt-get install -y -q redpanda >/dev/null
  rpk version
  sudo sysctl -w fs.aio-max-nr=1048576
}

RBOOT=127.0.0.1:19091,127.0.0.1:19092,127.0.0.1:19093
RADMIN=127.0.0.1:9641,127.0.0.1:9642,127.0.0.1:9643
rpkx() { rpk -X "brokers=${RB:-$RBOOT}" -X "admin.hosts=$RADMIN" "$@"; }

redpanda_configure() {
  local i
  mkdir -p "$W/rcfg" "$W/rlogs"
  for i in 1 2 3; do
    mkdir -p "$W/mnt/b$i/data" "$W/rcfg/b$i"
    cat > "$W/rcfg/b$i/redpanda.yaml" <<EOF
redpanda:
  data_directory: $W/mnt/b$i/data
  node_id: $i
  empty_seed_starts_cluster: false
  seed_servers:
    - host: {address: 127.0.0.1, port: 33141}
    - host: {address: 127.0.0.1, port: 33142}
    - host: {address: 127.0.0.1, port: 33143}
  rpc_server: {address: 127.0.0.1, port: 3314$i}
  advertised_rpc_api: {address: 127.0.0.1, port: 3314$i}
  kafka_api:
    - {address: 127.0.0.1, port: 1909$i}
  advertised_kafka_api:
    - {address: 127.0.0.1, port: 1909$i}
  admin:
    - {address: 127.0.0.1, port: 964$i}
  developer_mode: false
rpk:
  overprovisioned: true
  tune_aio_events: false
EOF
  done
}

redpanda_start() {
  local i=$1
  nohup rpk redpanda start --config "$W/rcfg/b$i/redpanda.yaml" --check=false \
    --smp 1 --memory 1G --reserve-memory 0M --overprovisioned \
    > "$W/rlogs/b$i.out" 2>&1 &
  echo $! > "$W/rlogs/b$i.rpkpid"
  log "redpanda node $i starting"
}

# The redpanda process pid of node i (rpk execs it, but be robust).
rpid() {
  local p
  p=$(pgrep -f -- "redpanda-cfg $W/rcfg/b$1/redpanda.yaml" | head -1 || true)
  echo "${p:-$(cat "$W/rlogs/b$1.rpkpid")}"
}

redpanda_wait_up() {
  local i=$1 n
  for n in $(seq 1 90); do
    if curl -fs "http://127.0.0.1:964$i/v1/status/ready" 2>/dev/null | grep -q ready; then
      log "redpanda node $i ready (pid $(rpid "$i"))"; return 0
    fi
    sleep 1
  done
  log "redpanda node $i never ready"; tail -80 "$W/rlogs/b$i.out"; return 1
}

redpanda_cluster_up() {
  local i
  for i in 1 2 3; do redpanda_start "$i"; done
  for i in 1 2 3; do redpanda_wait_up "$i"; done
  rpkx cluster health --watch --exit-when-healthy || true
  rpkx cluster config set enable_leader_balancer false
}

dump_logs_on_exit() {
  local rc=$?
  mkdir -p "$W/out"
  cp -r "$W"/klogs "$W"/rlogs "$W/out/" 2>/dev/null || true
  sudo dmsetup table 2>/dev/null > "$W/out/dmsetup-table.txt" || true
  exit $rc
}
