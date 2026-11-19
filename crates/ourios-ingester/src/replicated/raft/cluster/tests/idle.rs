//! A named flusher whose group goes quiet (#1301): what it keeps reaches
//! the store without another entry applying.

use std::time::Duration;

use ourios_parquet::Store;

use super::publication::{form, mining_group_over, published};

/// The group's last entry applies while its flusher cannot read the log
/// id, since nothing has recorded `formed.json` yet. Once it is recorded,
/// the flusher builds its publication and publishes everything it keeps,
/// though no further entry applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_named_flusher_publishes_once_its_log_id_reads() {
    let unformed = Store::in_memory();
    let (cluster, stores, last, _, _) =
        mining_group_over([unformed.clone(), Store::in_memory()]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        published(&stores[0]).is_empty(),
        "nothing publishes without a log id"
    );

    form(&unformed);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = published(&stores[0]);
        let through = rows
            .iter()
            .filter_map(|row| row.ingest_position)
            .map(|position| position.seq)
            .max();
        if through == Some(last) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the idle flusher never published: through {through:?}, not {last}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cluster.shutdown().await;
}
