/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

mod index;
mod params;

use crate::Config;
use crate::IndexKey;
use crate::VsIndexFactory;
use crate::perf;
use crate::table::Table;
use crate::table::TableSearch;
use crate::vs_index;
use crate::vs_index::Message;
use crate::vs_index::VsIndexModify;
use crate::vs_index::VsIndexSearch;
use crate::vs_index::factory::VsIndexConfiguration;
use anyhow::anyhow;
use index::CuvsIndex;
use params::CagraParams;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::Instrument;
use tracing::debug;
use tracing::debug_span;
use tracing::error;
use tracing::warn;

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

pub struct CuvsIndexFactory;

impl VsIndexFactory for CuvsIndexFactory {
    fn create_index(
        &self,
        index: VsIndexConfiguration,
        table: Arc<RwLock<Table>>,
    ) -> anyhow::Result<(mpsc::Sender<VsIndexModify>, mpsc::Sender<VsIndexSearch>)> {
        // On this thread, so a bad option fails `CREATE INDEX` rather than
        // surfacing much later.
        let params = CagraParams::try_from(&index)?;
        new(index.key, params, table, FLUSH_INTERVAL)
    }

    fn index_engine_version(&self) -> String {
        match cuvs::version::version() {
            Ok((major, minor, patch)) => format!("cuvs-{major}.{minor}.{patch}"),
            Err(err) => format!("cuvs-unknown ({err})"),
        }
    }
}

pub fn new_cuvs(_config_rx: watch::Receiver<Arc<Config>>) -> anyhow::Result<CuvsIndexFactory> {
    // Fail at startup rather than at the first index creation.
    cuvs::Resources::new()
        .map_err(|err| anyhow!("failed to initialize cuVS/CUDA resources: {err}"))?;
    Ok(CuvsIndexFactory)
}

enum Request {
    Message(Message),
    /// Timer tick: rebuild if anything is staged.
    Flush,
}

fn new(
    index_key: IndexKey,
    params: CagraParams,
    table: Arc<RwLock<impl TableSearch + Send + Sync + 'static>>,
    flush_interval: Duration,
) -> anyhow::Result<(mpsc::Sender<VsIndexModify>, mpsc::Sender<VsIndexSearch>)> {
    let channel_size = perf::channel_size().into();
    let (tx_modify, mut rx_modify) = mpsc::channel(channel_size);
    let (tx_search, mut rx_search) = mpsc::channel(channel_size);
    // Bounded, so the actor applies backpressure while a build is in flight.
    let (tx_gpu, mut rx_gpu) = mpsc::channel(channel_size);
    // Set while a tick is outstanding, so a rebuild in progress queues none.
    let flush_queued = Arc::new(AtomicBool::new(false));

    let thread_key = index_key.clone();
    let thread_flush = Arc::clone(&flush_queued);
    thread::Builder::new()
        .name(format!("cuvs-{index_key}"))
        .spawn(move || {
            // All cuVS state is created here and never leaves this thread.
            let mut index = match CuvsIndex::new(params) {
                Ok(index) => index,
                Err(err) => {
                    error!("unable to create cuVS index for {thread_key}: {err}");
                    // Draining keeps senders from blocking, so every search gets
                    // an error rather than hanging.
                    while let Some(request) = rx_gpu.blocking_recv() {
                        if let Request::Message(msg) = request {
                            reject(msg, anyhow!("cuVS index is unavailable: {err}"));
                        }
                    }
                    return;
                }
            };

            debug!("cuVS thread starting for {thread_key}");
            while let Some(request) = rx_gpu.blocking_recv() {
                handle(
                    &mut index,
                    &thread_flush,
                    table.as_ref(),
                    &thread_key,
                    request,
                );
            }
            debug!("cuVS thread finished for {thread_key}");
        })
        .map_err(|err| anyhow!("unable to spawn cuVS thread for {index_key}: {err}"))?;

    let span_key = index_key.clone();
    tokio::spawn(perf::hotpath_async(
        async move {
            debug!("starting");

            let mut interval = tokio::time::interval(flush_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Prefer real work over firing the rebuild timer.
                    biased;

                    msg = vs_index::recv(&mut rx_search, &mut rx_modify) => {
                        let Some(msg) = msg else {
                            break;
                        };
                        if tx_gpu.send(Request::Message(msg)).await.is_err() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        if !flush_queued.swap(true, Ordering::Relaxed)
                            && tx_gpu.send(Request::Flush).await.is_err()
                        {
                            break;
                        }
                    }
                }
            }

            debug!("finished");
        }
        .instrument(debug_span!("cuvs", "{span_key}")),
    ));

    Ok((tx_modify, tx_search))
}

fn reject(msg: Message, err: anyhow::Error) {
    match msg {
        Message::Search(VsIndexSearch::Ann { tx, .. } | VsIndexSearch::FilteredAnn { tx, .. }) => {
            _ = tx.send(Err(err));
        }
        Message::Search(VsIndexSearch::Count { tx, .. }) => {
            _ = tx.send(Err(err));
        }
        Message::Modify(_) => {}
    }
}

fn handle(
    index: &mut CuvsIndex,
    flush_queued: &AtomicBool,
    table: &RwLock<impl TableSearch>,
    index_key: &IndexKey,
    request: Request,
) {
    match request {
        Request::Flush => {
            build_if_pending(index, index_key);
            // Cleared after the build, not on receipt, so no tick queues while one runs.
            flush_queued.store(false, Ordering::Relaxed);
        }
        Request::Message(Message::Modify(VsIndexModify::AddVector {
            primary_id,
            embedding,
            in_progress,
            ..
        })) => {
            index.add(primary_id, &embedding, in_progress);
        }
        Request::Message(Message::Modify(VsIndexModify::RemoveVector {
            primary_id,
            in_progress,
            ..
        })) => {
            index.remove(primary_id, in_progress);
        }
        Request::Message(Message::Modify(VsIndexModify::RemovePartition { .. })) => {
            // Only reached once a partition is already empty, so for the
            // global-only indexes this backend builds there is nothing to drop.
            warn!("removing a partition is not implemented yet");
        }
        Request::Message(Message::Search(VsIndexSearch::Count { index_key, tx })) => {
            let result = match table.read().unwrap().index_id(&index_key) {
                Some(_) => Ok(index.count()),
                None => Err(anyhow!("index id not found for index key {index_key}")),
            };
            _ = tx.send(result);
        }
        Request::Message(Message::Search(
            VsIndexSearch::Ann { tx, .. } | VsIndexSearch::FilteredAnn { tx, .. },
        )) => {
            _ = tx.send(Err(anyhow!("cuVS index search is not implemented yet")));
        }
    }
}

fn build_if_pending(index: &mut CuvsIndex, index_key: &IndexKey) {
    if index.pending() == 0 {
        return;
    }
    if let Err(err) = index.build() {
        error!("Unable to build cuVS index {index_key}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncInProgress;
    use crate::Connectivity;
    use crate::Dimensions;
    use crate::ExpansionAdd;
    use crate::ExpansionSearch;
    use crate::Quantization;
    use crate::SpaceType;
    use crate::Vector;
    use crate::table::IndexIdGenerator;
    use crate::table::MockTableSearch;
    use crate::table::PartitionId;
    use crate::table::PrimaryId;
    use crate::vs_index::VsIndexModifyExt;
    use crate::vs_index::VsIndexSearchExt;
    use rstest::rstest;
    use std::num::NonZeroUsize;
    use std::ops::Range;

    const TEST_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

    /// CAGRA needs a non-trivial dataset before it will build a graph.
    const TEST_ROWS: usize = 256;
    const TEST_DIMENSIONS: usize = 4;

    fn index_key() -> IndexKey {
        IndexKey::new(&"vector".into(), &"store".into())
    }

    fn configuration() -> VsIndexConfiguration {
        VsIndexConfiguration {
            key: index_key(),
            dimensions: Dimensions::from(NonZeroUsize::new(TEST_DIMENSIONS).unwrap()),
            connectivity: Connectivity::default(),
            expansion_add: ExpansionAdd::default(),
            expansion_search: ExpansionSearch::default(),
            space_type: SpaceType::default(),
            quantization: Quantization::default(),
        }
    }

    fn table_with(index_id: crate::table::IndexId) -> Arc<RwLock<MockTableSearch>> {
        let mut mock = MockTableSearch::new();
        mock.expect_index_id().returning(move |_| Some(index_id));
        Arc::new(RwLock::new(mock))
    }

    struct Harness {
        modify: mpsc::Sender<VsIndexModify>,
        search: mpsc::Sender<VsIndexSearch>,
        partition_id: PartitionId,
    }

    fn harness() -> Harness {
        let index_id = IndexIdGenerator::new().next(true).unwrap();
        let partition_id = PartitionId::global(index_id);
        let (modify, search) = new(
            index_key(),
            CagraParams::try_from(&configuration()).unwrap(),
            table_with(index_id),
            TEST_FLUSH_INTERVAL,
        )
        .unwrap();
        Harness {
            modify,
            search,
            partition_id,
        }
    }

    fn embedding(row: usize) -> Vector {
        Vector::from(
            (0..TEST_DIMENSIONS)
                .map(|col| (row * TEST_DIMENSIONS + col) as f32 * 0.001)
                .collect::<Vec<_>>(),
        )
    }

    /// Adds `rows` vectors and waits for every guard to drop, which happens only
    /// once a build includes them. One sender is cloned across the batch, as a
    /// full scan does for a range; waiting per vector would rebuild each time.
    async fn add_rows(harness: &Harness, rows: Range<usize>) {
        let (tx, mut rx) = mpsc::channel(1);
        for row in rows {
            harness
                .modify
                .add_vector(
                    harness.partition_id,
                    PrimaryId::from(row as u64),
                    embedding(row),
                    AsyncInProgress::Fullscan(tx.clone()),
                )
                .await
                .unwrap();
        }
        drop(tx);
        while rx.recv().await.is_some() {}
    }

    /// An update reaches a backend as a removal of the superseded id followed by
    /// an add under a fresh one, since the engine bumps the row's epoch.
    async fn update_row(harness: &Harness, superseded: usize, fresh: usize) {
        let (tx, mut rx) = mpsc::channel(1);
        harness
            .modify
            .remove_vector(
                harness.partition_id,
                PrimaryId::from(superseded as u64),
                AsyncInProgress::Fullscan(tx.clone()),
            )
            .await
            .unwrap();
        harness
            .modify
            .add_vector(
                harness.partition_id,
                PrimaryId::from(fresh as u64),
                embedding(fresh),
                AsyncInProgress::Fullscan(tx.clone()),
            )
            .await
            .unwrap();
        drop(tx);
        while rx.recv().await.is_some() {}
    }

    async fn remove_row(harness: &Harness, row: usize) {
        let (tx, mut rx) = mpsc::channel(1);
        harness
            .modify
            .remove_vector(
                harness.partition_id,
                PrimaryId::from(row as u64),
                AsyncInProgress::Fullscan(tx.clone()),
            )
            .await
            .unwrap();
        drop(tx);
        while rx.recv().await.is_some() {}
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn count_reflects_the_built_index() {
        let harness = harness();
        add_rows(&harness, 0..TEST_ROWS).await;

        // No polling: the guards `add_rows` waited on are released only by a
        // successful build, so the count must already be correct.
        assert_eq!(
            harness.search.count(index_key()).await.unwrap(),
            TEST_ROWS,
            "in-progress guards must be held until the build that includes the writes"
        );
    }

    /// Writes after a build need a second one, which a stuck flush gate would
    /// never deliver.
    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn a_later_write_reaches_a_later_build() {
        let harness = harness();
        add_rows(&harness, 0..TEST_ROWS).await;
        add_rows(&harness, TEST_ROWS..TEST_ROWS + 8).await;

        assert_eq!(
            harness.search.count(index_key()).await.unwrap(),
            TEST_ROWS + 8
        );
    }

    /// Dropping removals used to leak a row per update, so a table whose rows
    /// were each written twice reported double the vectors it held.
    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn updating_every_row_leaves_the_count_alone() {
        let harness = harness();
        add_rows(&harness, 0..TEST_ROWS).await;

        for row in 0..TEST_ROWS {
            update_row(&harness, row, row + TEST_ROWS).await;
        }

        assert_eq!(harness.search.count(index_key()).await.unwrap(), TEST_ROWS);
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn removing_a_row_lowers_the_count() {
        let harness = harness();
        add_rows(&harness, 0..TEST_ROWS).await;

        remove_row(&harness, 0).await;

        assert_eq!(
            harness.search.count(index_key()).await.unwrap(),
            TEST_ROWS - 1
        );
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn count_is_zero_before_the_first_build() {
        let harness = harness();

        assert_eq!(harness.search.count(index_key()).await.unwrap(), 0);
    }

    #[test]
    fn index_engine_version_reports_cuvs_library_version() {
        let factory = CuvsIndexFactory;
        let (major, minor, patch) = cuvs::version::version().unwrap();
        assert_eq!(
            factory.index_engine_version(),
            format!("cuvs-{major}.{minor}.{patch}")
        );
    }
}
