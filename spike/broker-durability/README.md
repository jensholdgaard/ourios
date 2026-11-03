# Broker-durability spike (throwaway)

Checks the broker assumptions behind RFC 0066 and RFC 0067 against real
Apache Kafka (KRaft, one controller plus three brokers) and Redpanda (three
nodes), run as host processes on a GitHub-hosted Ubuntu runner by
`.github/workflows/spike-broker-durability.yml`. Each broker's data
directory is an ext4 file system (no journal) on a loop file behind a
`dm-delay` device, so a follower's writes and flushes can be slowed live
with `dmsetup reload`.

| Script | Question |
|---|---|
| `s1-kafka-flush-messages.sh` | Does `flush.messages=1` make the ack wait for the followers' fsync (RFC0066.23)? |
| `s2-redpanda-write-caching.sh` | Does `write.caching=false` wait for a majority fsync, and `true` not? |
| `s3-kafka-elr.sh` (`ELR=0/1`) | Can a replica that lost unflushed data lead, with and without ELR? |
| `s4-redpanda-power-loss.sh` (`WC=true/false`) | Does one follower's power loss plus a cut-off leader lose acked data? |
| `s5-kafka-describe.sh`, `s5-redpanda-describe.sh` | What does DescribeConfigs report, and from which source? |

Power loss is modelled as SIGKILL plus restoring the node's data directory
from a copy taken just before the produce. `k.py` is the client: sequential
`acks=all` produces with per-record latency, a record count, metadata, and a
raw DescribeConfigs v1 request addressed to one broker. Every job prints an
`S<n>` verdict line and uploads its logs.
