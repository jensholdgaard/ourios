//! A receiver restarted through startup recovery, and what it mines.

use std::collections::BTreeSet;

use ourios_core::audit::SharedAuditSink;
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::recovery::RecoveryReport;
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;

use super::structured_logs;
use crate::ingest_support::{request, resource_logs};

/// A restarted receiver.
pub struct Restarted {
    pub report: RecoveryReport,
    pub miner: MinerCluster,
    pub records: SharedRecordSink,
    /// What recovery and the miner published to the audit sink.
    pub audit: SharedAuditSink,
    pub(super) _ids: TemplateIds,
}

impl Restarted {
    /// Mine one string line for `tenant`, returning its template id.
    pub fn mine(&mut self, tenant: &str, body: &str) -> u64 {
        self.mine_logs(tenant, resource_logs(tenant, &[body]))
    }

    /// Mine one structured record for `tenant`, keyed by `event`.
    pub fn mine_structured(&mut self, tenant: &str, event: &str) -> u64 {
        self.mine_logs(tenant, structured_logs(tenant, event))
    }

    /// Mine `logs` for `tenant`, returning its last record's template id.
    fn mine_logs(
        &mut self,
        tenant: &str,
        logs: opentelemetry_proto::tonic::logs::v1::ResourceLogs,
    ) -> u64 {
        let tenant = TenantId::new(tenant);
        assign(request(vec![logs]), &tenant)
            .iter()
            .map(|record| self.miner.ingest(record))
            .last()
            .expect("one record")
    }

    /// Every id a record emitted since the restart carries.
    pub fn emitted(&self) -> BTreeSet<u64> {
        self.records
            .drain()
            .iter()
            .map(|r| r.template_id)
            .filter(|id| *id != 0)
            .collect()
    }
}
