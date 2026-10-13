//! RFC 0060 — background template-map refresh, its configuration and
//! the served role's task.
//! See `docs/rfcs/0060-background-template-map-refresh.md` §5.
//!
//! The served-role scenario watches the one effect an acquisition has on
//! the store, the tenant's published `template_map` artifact: with the
//! task enabled the start pass publishes it without any query, and with
//! the task disabled nothing does.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, ParamType, TemplateChange};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, PartitionKey, Store, StoreConfig, Writer};
use ourios_querier::TEMPLATE_MAP_FILENAME;
use ourios_server::config::file::parse;
use ourios_server::config::resolve::server_config_from_file;
use ourios_server::querier::{QuerierConfig, QuerierHandle, serve};
use ourios_serving::AuthResolver;

const TENANT: &str = "acme";
const KEY: &str = "querier.template_map.refresh.interval";

fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(0)
}

fn adopted(ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: UNIX_EPOCH + Duration::from_nanos(ts_ns),
        payload: AuditPayload::Template {
            template_id: 1,
            triggering_line_hash: ourios_core::audit::hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Adopted {
                template_version: 1,
                new_template: "user <*>".to_string(),
            },
        },
    }
}

fn mined(ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(TENANT),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// A store whose one tenant has an audit file and a data row, and no
/// published template map.
fn store() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let ts = now_ns();
    let mut sink = ParquetAuditSink::new(Store::local(dir.path()).expect("store"));
    sink.emit(adopted(ts));
    assert_eq!(sink.write_failures(), 0, "the audit fixture persists");
    let record = mined(ts);
    let part = PartitionKey::derive(&record).expect("partition");
    let mut writer = Writer::open(dir.path(), part).expect("writer");
    writer.append_records(&[record]).expect("append");
    writer.close().expect("close");
    dir
}

fn artifact(root: &Path) -> std::path::PathBuf {
    root.join("audit")
        .join(format!("tenant_id={TENANT}"))
        .join(TEMPLATE_MAP_FILENAME)
}

async fn served(root: &Path, template_map_refresh: Option<Duration>) -> QuerierHandle {
    serve(QuerierConfig {
        http_addr: "127.0.0.1:0".parse().expect("addr"),
        http_tls: None,
        store: StoreConfig::Local(root.to_path_buf()),
        mcp_enabled: false,
        auth: AuthResolver::static_only(None),
        default_window_nanos: 3600 * 1_000_000_000,
        promoted: ourios_parquet::PromotedAttributes::default(),
        template_map_refresh,
    })
    .await
    .expect("querier serves")
}

/// Scenario RFC0060.5 — Disabled means today's behaviour.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
///
/// The enabled run is the control: the same store and the same wait are
/// enough for the start pass to publish, so the disabled run's missing
/// artifact means no acquisition ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_5_disabled_means_todays_behaviour() {
    let enabled = store();
    let started = Instant::now();
    let handle = served(enabled.path(), Some(Duration::from_secs(10))).await;
    while !artifact(enabled.path()).exists() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the enabled start pass never published"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let published_after = started.elapsed();
    handle.shutdown().await.expect("enabled shutdown");

    let disabled = store();
    let handle = served(disabled.path(), None).await;
    tokio::time::sleep((published_after * 3).max(Duration::from_secs(1))).await;
    assert!(
        !artifact(disabled.path()).exists(),
        "with refresh disabled nothing acquires the map before a query"
    );
    handle.shutdown().await.expect("disabled shutdown");
}

fn resolve(refresh: &str) -> Result<Option<Duration>, String> {
    let yaml = format!(
        "storage:\n  local:\n    bucket_root: /tmp/ourios\n\
         querier:\n  enabled: true\n  template_map:\n    refresh:\n{refresh}"
    );
    let file = parse(&yaml, &|_| None).map_err(|e| e.to_string())?;
    let config = server_config_from_file(&file)?;
    Ok(config
        .querier
        .expect("querier enabled")
        .template_map_refresh)
}

/// Scenario RFC0060.6 — Configuration is validated.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
fn rfc0060_6_configuration_is_validated() {
    for bad in [
        "1s", "9s", "9999ms", "soon", "5", "5 m", "-5m", "1.5m", "5d",
    ] {
        let err = resolve(&format!("      interval: \"{bad}\"\n"))
            .expect_err(&format!("{bad:?} is rejected when enabled"));
        assert!(err.contains(KEY), "the error names the key: {err}");
        let err = resolve(&format!("      enabled: true\n      interval: \"{bad}\"\n"))
            .expect_err(&format!("{bad:?} is rejected when explicitly enabled"));
        assert!(err.contains(KEY), "the error names the key: {err}");
        assert_eq!(
            resolve(&format!(
                "      enabled: false\n      interval: \"{bad}\"\n"
            )),
            Ok(None),
            "{bad:?} resolves when disabled"
        );
    }
    assert_eq!(
        resolve("      enabled: true\n"),
        Ok(Some(Duration::from_secs(300))),
        "the default interval is 5m"
    );
    assert_eq!(
        resolve("      interval: 10s\n"),
        Ok(Some(Duration::from_secs(10))),
        "the floor itself is accepted"
    );
    assert_eq!(
        resolve("      interval: 2h\n"),
        Ok(Some(Duration::from_secs(7200)))
    );
}
