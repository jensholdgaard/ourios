//! A checkpoint's bundle (RFC 0065 §6.5): the miner state a flusher
//! writes for a cut and a restoring replica reads back.
//!
//! Under [`prefix`], `tenants/<tenant>.snap` holds each tenant's miner
//! snapshot as `ourios_miner::snapshot::snapshot` writes it, marked with
//! the cut, and `bundle.bin` names them, every integer little-endian:
//!
//! ```text
//! format: u8 = 1 | log: u32 | seq: u64 | term: u64 |
//! next: u64 | current after: u64 | current through: u64 |
//! blocks: u32 | blocks × (after: u64 | through: u64) |
//! tenants: u32 | tenants × (len: u16 | tenant: len bytes | config kind: u8 | config body |
//!                           snapshot len: u64 | snapshot crc32c: u32) |
//! crc32c: u32
//! ```
//!
//! A replica restores only a bundle that decodes, [`Bundle::check`]s
//! against its snapshot record's cut, and whose every `.snap` passes
//! [`Bundle::snapshot`]; anything else fails the restore closed.

pub mod pointer;

use std::fmt;

use ourios_core::tenant::{MAX_TENANT_BYTES, TenantId};
use ourios_miner::snapshot::{LogMark, SnapshotState, WalHighWater, load_snapshot};
use ourios_parquet::percent_encode_tenant;
use ourios_wal::MinerConfigV1;

use super::snapshot_record::{GroupMinerConfig, IdBlock};

const FORMAT: u8 = 1;
const MINER_CONFIG_V1: u8 = 0x04;
const CRC_LEN: usize = 4;

/// The bundle's prefix in the bucket: the log id in decimal, the cut's
/// index as 20 zero-padded digits, so key order is cut order.
#[must_use]
pub fn prefix(log: u32, seq: u64) -> String {
    format!("wal/replicated/logs/{log}/checkpoints/{seq:020}/")
}

/// The key of the bundle's `bundle.bin`.
#[must_use]
pub fn bundle_key(log: u32, seq: u64) -> String {
    format!("{}bundle.bin", prefix(log, seq))
}

/// The key of `tenant`'s `.snap`, its id percent-encoded as the local
/// snapshot files name it.
#[must_use]
pub fn snapshot_key(log: u32, seq: u64, tenant: &TenantId) -> String {
    format!(
        "{}tenants/{}.snap",
        prefix(log, seq),
        percent_encode_tenant(tenant.as_str())
    )
}

/// A cut: the group's log id and the cut entry's index and term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutId {
    pub log: u32,
    pub seq: u64,
    pub term: u64,
}

/// Why a bundle is refused.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BundleError {
    /// The bytes end before the layout does.
    Truncated,
    /// Bytes follow the last tenant.
    Trailing,
    /// The trailing CRC does not cover the bytes before it.
    Crc,
    /// A format this release does not read.
    Format(u8),
    /// More blocks or tenants, or a longer tenant id, than the layout
    /// counts.
    TooLarge,
    /// A block that is not `after < through <= 2^63 - 1`.
    Block { after: u64, through: u64 },
    /// `next` outside the current block, or not `1` before one started.
    Next { next: u64, current: Option<IdBlock> },
    /// A pending block that starts below the block before it.
    Pending { after: u64, floor: u64 },
    /// A tenant id that is empty, too long or not UTF-8.
    Tenant,
    /// Tenants not sorted bytewise, or one repeated.
    TenantOrder,
    /// A configuration kind this release does not read.
    ConfigKind(u8),
    /// A configuration body its kind's checks refuse.
    Config(String),
    /// The bundle is for another log id, cut index or term.
    Cut { found: CutId, expected: CutId },
    /// The allocator's latest block is not the record's.
    LatestBlock {
        found: Option<IdBlock>,
        expected: Option<IdBlock>,
    },
    /// A tenant's configuration is not the one in force at the cut.
    TenantConfig(TenantId),
    /// A `.snap` whose length is not the one the bundle names.
    SnapshotLength {
        tenant: TenantId,
        found: u64,
        expected: u64,
    },
    /// A `.snap` whose CRC is not the one the bundle names.
    SnapshotCrc(TenantId),
    /// A `.snap` that `load_snapshot` refuses.
    Snapshot { tenant: TenantId, detail: String },
    /// A `.snap` not marked with this bundle's log id and cut.
    SnapshotMark(TenantId),
    /// A `.snap` holding a template id the allocator could issue again.
    SnapshotId {
        tenant: TenantId,
        id: u64,
        next: u64,
    },
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated"),
            Self::Trailing => f.write_str("bytes after the last tenant"),
            Self::Crc => f.write_str("CRC mismatch"),
            Self::Format(format) => write!(f, "format {format}, not {FORMAT}"),
            Self::TooLarge => f.write_str("more than the layout counts"),
            Self::Block { after, through } => write!(f, "id block ({after}, {through}]"),
            Self::Next { next, current } => write!(f, "next {next} against block {current:?}"),
            Self::Pending { after, floor } => {
                write!(f, "pending block from {after} starts below {floor}")
            }
            Self::Tenant => f.write_str("a tenant id empty, too long or not UTF-8"),
            Self::TenantOrder => f.write_str("tenants not sorted or repeated"),
            Self::ConfigKind(kind) => write!(f, "configuration kind {kind:#04x}"),
            Self::Config(detail) => write!(f, "configuration refused: {detail}"),
            Self::Cut { found, expected } => {
                write!(f, "cut {found:?}, not {expected:?}")
            }
            Self::LatestBlock { found, expected } => {
                write!(f, "latest id block {found:?}, not {expected:?}")
            }
            Self::TenantConfig(tenant) => {
                write!(f, "tenant {tenant}'s configuration is not the one in force")
            }
            Self::SnapshotLength {
                tenant,
                found,
                expected,
            } => write!(
                f,
                "tenant {tenant}'s snapshot is {found} bytes, not {expected}"
            ),
            Self::SnapshotCrc(tenant) => write!(f, "tenant {tenant}'s snapshot CRC mismatch"),
            Self::Snapshot { tenant, detail } => {
                write!(f, "tenant {tenant}'s snapshot refused: {detail}")
            }
            Self::SnapshotMark(tenant) => {
                write!(f, "tenant {tenant}'s snapshot is not marked with the cut")
            }
            Self::SnapshotId { tenant, id, next } => {
                write!(
                    f,
                    "tenant {tenant}'s snapshot holds id {id}, at or above next {next}"
                )
            }
        }
    }
}

impl std::error::Error for BundleError {}

/// The group's template-id allocator at the cut: the next id, the block
/// that id comes from, and the applied blocks not started yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocator {
    next: u64,
    current: Option<IdBlock>,
    pending: Vec<IdBlock>,
}

impl Allocator {
    /// # Errors
    ///
    /// [`BundleError::Next`] unless `next` is `1` with no current block,
    /// or `after < next <= through + 1` of the current one, so `0` and no
    /// id at or below `after` is ever issued; [`BundleError::Pending`]
    /// unless each pending block starts at or above the block before it.
    pub fn new(
        next: u64,
        current: Option<IdBlock>,
        pending: Vec<IdBlock>,
    ) -> Result<Self, BundleError> {
        let fits = match current {
            None => next == 1,
            Some(block) => block.after() < next && next <= block.through() + 1,
        };
        if !fits {
            return Err(BundleError::Next { next, current });
        }
        let mut floor = current.map_or(0, IdBlock::through);
        for block in &pending {
            if block.after() < floor {
                return Err(BundleError::Pending {
                    after: block.after(),
                    floor,
                });
            }
            floor = block.through();
        }
        Ok(Self {
            next,
            current,
            pending,
        })
    }

    #[must_use]
    pub fn next(&self) -> u64 {
        self.next
    }

    #[must_use]
    pub fn current(&self) -> Option<IdBlock> {
        self.current
    }

    #[must_use]
    pub fn pending(&self) -> &[IdBlock] {
        &self.pending
    }

    /// The latest applied block: the last pending one, else the current.
    #[must_use]
    pub fn latest(&self) -> Option<IdBlock> {
        self.pending.last().copied().or(self.current)
    }
}

/// One tenant of a bundle: its configuration in force and its `.snap`
/// object's length and CRC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantEntry {
    pub tenant: TenantId,
    pub config: GroupMinerConfig,
    pub snapshot_len: u64,
    pub snapshot_crc: u32,
}

impl TenantEntry {
    /// The entry for `snapshot`, the `.snap` bytes written for `tenant`.
    #[must_use]
    pub fn of(tenant: TenantId, config: GroupMinerConfig, snapshot: &[u8]) -> Self {
        Self {
            tenant,
            config,
            snapshot_len: snapshot.len() as u64,
            snapshot_crc: crc32c::crc32c(snapshot),
        }
    }
}

/// What a replica's snapshot record holds at the cut it restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub id: CutId,
    pub latest_block: Option<IdBlock>,
    pub config: Option<GroupMinerConfig>,
}

/// A decoded `bundle.bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    cut: CutId,
    ids: Allocator,
    tenants: Vec<TenantEntry>,
}

impl Bundle {
    /// # Errors
    ///
    /// [`BundleError::Tenant`] for a tenant id empty or over
    /// [`MAX_TENANT_BYTES`]; [`BundleError::TenantOrder`] unless the
    /// tenants are sorted bytewise with none repeated.
    pub fn new(cut: CutId, ids: Allocator, tenants: Vec<TenantEntry>) -> Result<Self, BundleError> {
        if tenants
            .iter()
            .any(|entry| !(1..=MAX_TENANT_BYTES).contains(&entry.tenant.as_str().len()))
        {
            return Err(BundleError::Tenant);
        }
        if tenants
            .windows(2)
            .any(|pair| pair[0].tenant.as_str().as_bytes() >= pair[1].tenant.as_str().as_bytes())
        {
            return Err(BundleError::TenantOrder);
        }
        Ok(Self { cut, ids, tenants })
    }

    #[must_use]
    pub fn cut(&self) -> CutId {
        self.cut
    }

    #[must_use]
    pub fn ids(&self) -> &Allocator {
        &self.ids
    }

    #[must_use]
    pub fn tenants(&self) -> &[TenantEntry] {
        &self.tenants
    }

    /// `bundle.bin`'s bytes.
    ///
    /// # Errors
    ///
    /// [`BundleError::TooLarge`] when the blocks or tenants outnumber a
    /// `u32`.
    pub fn encode(&self) -> Result<Vec<u8>, BundleError> {
        let mut out = vec![FORMAT];
        out.extend_from_slice(&self.cut.log.to_le_bytes());
        for word in [self.cut.seq, self.cut.term, self.ids.next] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        put_block(&mut out, self.ids.current);
        out.extend_from_slice(&count(self.ids.pending.len())?.to_le_bytes());
        for block in &self.ids.pending {
            put_block(&mut out, Some(*block));
        }
        out.extend_from_slice(&count(self.tenants.len())?.to_le_bytes());
        for entry in &self.tenants {
            let tenant = entry.tenant.as_str().as_bytes();
            let len = u16::try_from(tenant.len()).map_err(|_| BundleError::TooLarge)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(tenant);
            out.push(MINER_CONFIG_V1);
            out.extend_from_slice(&entry.config.body().to_bytes());
            out.extend_from_slice(&entry.snapshot_len.to_le_bytes());
            out.extend_from_slice(&entry.snapshot_crc.to_le_bytes());
        }
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// The bundle `bytes` hold, if they are one this release reads.
    ///
    /// # Errors
    ///
    /// The [`BundleError`] naming the first rule `bytes` break.
    pub fn decode(bytes: &[u8]) -> Result<Self, BundleError> {
        let Some(body_len) = bytes.len().checked_sub(CRC_LEN) else {
            return Err(BundleError::Truncated);
        };
        let (body, crc) = bytes.split_at(body_len);
        if u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32c::crc32c(body) {
            return Err(BundleError::Crc);
        }
        let mut reader = Reader(body);
        match reader.u8()? {
            FORMAT => {}
            other => return Err(BundleError::Format(other)),
        }
        let log = reader.u32()?;
        let (seq, term, next) = (reader.u64()?, reader.u64()?, reader.u64()?);
        let current = reader.block()?;
        let pending = (0..reader.u32()?)
            .map(|_| {
                reader.block()?.ok_or(BundleError::Block {
                    after: 0,
                    through: 0,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ids = Allocator::new(next, current, pending)?;
        let tenants = (0..reader.u32()?)
            .map(|_| reader.tenant())
            .collect::<Result<Vec<_>, _>>()?;
        if !reader.0.is_empty() {
            return Err(BundleError::Trailing);
        }
        Self::new(CutId { log, seq, term }, ids, tenants)
    }

    /// Whether this is the bundle of `cut`: its log id, index and term,
    /// the record's latest id block, and the configuration in force for
    /// every tenant.
    ///
    /// # Errors
    ///
    /// [`BundleError::Cut`], [`BundleError::LatestBlock`] or
    /// [`BundleError::TenantConfig`] for the first that differs.
    pub fn check(&self, cut: &Cut) -> Result<(), BundleError> {
        if self.cut != cut.id {
            return Err(BundleError::Cut {
                found: self.cut,
                expected: cut.id,
            });
        }
        if self.ids.latest() != cut.latest_block {
            return Err(BundleError::LatestBlock {
                found: self.ids.latest(),
                expected: cut.latest_block,
            });
        }
        match self
            .tenants
            .iter()
            .find(|entry| Some(entry.config) != cut.config)
        {
            Some(entry) => Err(BundleError::TenantConfig(entry.tenant.clone())),
            None => Ok(()),
        }
    }

    /// The state `bytes`, `entry`'s `.snap`, restores, if they are the
    /// object the bundle names, marked with its cut, holding no id the
    /// allocator could issue again.
    ///
    /// # Errors
    ///
    /// The [`BundleError`] naming the first rule `bytes` break.
    pub fn snapshot(
        &self,
        entry: &TenantEntry,
        bytes: &[u8],
    ) -> Result<SnapshotState, BundleError> {
        let tenant = || entry.tenant.clone();
        let found = bytes.len() as u64;
        if found != entry.snapshot_len {
            return Err(BundleError::SnapshotLength {
                tenant: tenant(),
                found,
                expected: entry.snapshot_len,
            });
        }
        if crc32c::crc32c(bytes) != entry.snapshot_crc {
            return Err(BundleError::SnapshotCrc(tenant()));
        }
        let state = load_snapshot(bytes).map_err(|e| BundleError::Snapshot {
            tenant: tenant(),
            detail: e.to_string(),
        })?;
        let cut = LogMark {
            log: self.cut.log,
            seq: self.cut.seq,
        };
        if state.wal_high_water != Some(WalHighWater::Replicated(cut)) {
            return Err(BundleError::SnapshotMark(tenant()));
        }
        match state.highest_template_id() {
            Some(id) if id >= self.ids.next => Err(BundleError::SnapshotId {
                tenant: tenant(),
                id,
                next: self.ids.next,
            }),
            _ => Ok(state),
        }
    }
}

fn count(len: usize) -> Result<u32, BundleError> {
    u32::try_from(len).map_err(|_| BundleError::TooLarge)
}

fn put_block(out: &mut Vec<u8>, block: Option<IdBlock>) {
    let (after, through) = block.map_or((0, 0), |block| (block.after(), block.through()));
    out.extend_from_slice(&after.to_le_bytes());
    out.extend_from_slice(&through.to_le_bytes());
}

/// The bytes of a bundle not read yet.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], BundleError> {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(BundleError::Truncated)?;
        self.0 = rest;
        Ok(*head)
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], BundleError> {
        let (head, rest) = self.0.split_at_checked(len).ok_or(BundleError::Truncated)?;
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, BundleError> {
        Ok(self.take::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, BundleError> {
        self.take().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, BundleError> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, BundleError> {
        self.take().map(u64::from_le_bytes)
    }

    /// A block, `None` for `(0, 0]`.
    fn block(&mut self) -> Result<Option<IdBlock>, BundleError> {
        match (self.u64()?, self.u64()?) {
            (0, 0) => Ok(None),
            (after, through) => IdBlock::new(after, through)
                .map(Some)
                .ok_or(BundleError::Block { after, through }),
        }
    }

    fn tenant(&mut self) -> Result<TenantEntry, BundleError> {
        let len = usize::from(self.u16()?);
        let tenant = std::str::from_utf8(self.bytes(len)?).map_err(|_| BundleError::Tenant)?;
        let config = match self.u8()? {
            MINER_CONFIG_V1 => {
                let body = MinerConfigV1::from_bytes(self.take()?)
                    .map_err(|e| BundleError::Config(e.to_string()))?;
                GroupMinerConfig::new(body).map_err(|e| BundleError::Config(e.to_string()))?
            }
            kind => return Err(BundleError::ConfigKind(kind)),
        };
        Ok(TenantEntry {
            tenant: TenantId::new(tenant),
            config,
            snapshot_len: self.u64()?,
            snapshot_crc: self.u32()?,
        })
    }
}

#[cfg(test)]
mod tests;
