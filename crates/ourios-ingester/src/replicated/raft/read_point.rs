//! A tail replica's read point (RFC 0065 §6.7, "Read your acks"): the log
//! index its tail is served at. It asks its group's leader for a
//! quorum-confirmed read point (`ReadIndex`, not a lease), then waits until
//! it has applied through it and mined every entry it was handed by then. Without a confirmed point in time
//! it has none, and its tail is unavailable rather than possibly stale.

use std::fmt;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use tokio::time::Instant;

use super::host::{GroupHost, GroupRaft, Tailed};
use super::transport::Peers;
use super::transport::read_index::Outcome;

/// Why a tail replica has no read point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable(pub String);

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no read point: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// `group`'s read point on the tail replica `host`, asking its leader
/// through `peers`, within `patience`: the index it has applied and mined
/// through.
///
/// # Errors
///
/// [`Unavailable`] when `host` does not mine `group`, no leader confirms a
/// point, or the replica has not applied and mined through it in time.
pub async fn read_point(
    host: &GroupHost,
    peers: &Peers,
    group: u32,
    patience: Duration,
) -> Result<u64, Unavailable> {
    let tailed = host
        .tailed(group)
        .ok_or_else(|| Unavailable("not mined here".to_owned()))?;
    read_point_of(&tailed, peers, group, patience).await
}

/// As [`read_point`], on the group `tailed` holds.
///
/// # Errors
///
/// As [`read_point`].
pub async fn read_point_of(
    tailed: &Tailed,
    peers: &Peers,
    group: u32,
    patience: Duration,
) -> Result<u64, Unavailable> {
    let deadline = Instant::now() + patience;
    let index = confirmed(&tailed.raft, peers, group, deadline).await?;
    caught_up(tailed, index, deadline).await?;
    Ok(index)
}

/// Wait until the replica `tailed` holds has applied through `index` and
/// mined every entry it was handed by then, before `deadline`.
///
/// # Errors
///
/// [`Unavailable`] when it has not by `deadline`, or its miner stopped.
pub async fn caught_up(tailed: &Tailed, index: u64, deadline: Instant) -> Result<(), Unavailable> {
    let left = deadline.saturating_duration_since(Instant::now());
    tailed
        .raft
        .wait(Some(left))
        .applied_index_at_least(Some(index), "applied through the read point")
        .await
        .map_err(|e| Unavailable(format!("not applied through {index}: {e}")))?;
    // Applied through the point, the miner has been handed every entry
    // at or below it that it mines; entries it never sees (blank,
    // membership) need no mining, so the point itself may lie past them.
    let handed = tailed.sm.handed_through();
    let mut mined = tailed.mined.clone();
    tokio::time::timeout_at(deadline, mined.wait_for(|through| *through >= handed))
        .await
        .map_err(|_| Unavailable(format!("not mined through {index} in time")))?
        .map_err(|_| Unavailable("the miner stopped".to_owned()))?;
    Ok(())
}

/// The read point the leader confirms, asking the leader this replica
/// knows of and, once, the one a refusal names instead.
async fn confirmed(
    raft: &GroupRaft,
    peers: &Peers,
    group: u32,
    deadline: Instant,
) -> Result<u64, Unavailable> {
    let mut leader = known_leader(raft);
    for _ in 0..2 {
        let Some(name) = leader.take() else {
            return Err(Unavailable("no leader known".to_owned()));
        };
        let left = deadline.saturating_duration_since(Instant::now());
        match peers.read_index(&name, group, left).await {
            Outcome::Read(point) => return Ok(point.index),
            Outcome::NotLeader { leader: hint } if hint.as_deref() != Some(name.as_str()) => {
                leader = hint;
            }
            other => return Err(Unavailable(format!("{name}: {other:?}"))),
        }
    }
    Err(Unavailable("the leader moved while asking".to_owned()))
}

/// The `node_id` of the leader `raft` knows of.
fn known_leader(raft: &GroupRaft) -> Option<String> {
    let metrics = raft.metrics().borrow_watched().clone();
    let leader = metrics.current_leader?;
    metrics
        .membership_config
        .membership()
        .get_node(&leader)
        .cloned()
}
