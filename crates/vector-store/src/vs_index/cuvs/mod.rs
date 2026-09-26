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
use anyhow::bail;
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
        let is_global = table
            .read()
            .unwrap()
            .index_id(&index.key)
            .is_some_and(|id| id.is_global());
        if !is_global {
            bail!("cuVS does not support local indexes yet: {}", index.key);
        }
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
    cuvs::Resources::new().map_err(|err| {
        anyhow!(
            "failed to initialize cuVS/CUDA resources: {err}. \
             Check that the machine has a GPU with a working NVIDIA driver, \
             or unset VECTOR_STORE_USE_GPU to fall back to the default USearch backend."
        )
    })?;
    Ok(CuvsIndexFactory)
}

enum Request {
    Message(Message),
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
    let (tx_gpu, mut rx_gpu) = mpsc::channel(channel_size);
    let flush_queued = Arc::new(AtomicBool::new(false));

    let thread_key = index_key.clone();
    let thread_flush = Arc::clone(&flush_queued);
    thread::Builder::new()
        .name(format!("cuvs-{index_key}"))
        .spawn(move || {
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
            warn!("not implemented yet");
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
    use crate::NonemptyArc;
    use crate::Quantization;
    use crate::SpaceType;
    use crate::Vector;
    use crate::table::IndexId;
    use crate::table::IndexIdGenerator;
    use crate::table::MockTableSearch;
    use crate::table::PartitionId;
    use crate::table::PrimaryId;
    use crate::vs_index::VsIndexModifyExt;
    use crate::vs_index::VsIndexSearchExt;
    use rstest::rstest;
    use scylla::cluster::metadata::NativeType;
    use std::collections::HashMap;
    use std::num::NonZeroUsize;
    use std::ops::Range;

    const TEST_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

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

    fn table_with(index_id: IndexId) -> Arc<RwLock<MockTableSearch>> {
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

    /// Adds `rows` vectors and waits for a successful build that includes them.
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

    /// Removes `rows` and waits for a successful build without them.
    async fn remove_rows(harness: &Harness, rows: Range<usize>) {
        let (tx, mut rx) = mpsc::channel(1);
        for row in rows {
            harness
                .modify
                .remove_vector(
                    harness.partition_id,
                    PrimaryId::from(row as u64),
                    AsyncInProgress::Fullscan(tx.clone()),
                )
                .await
                .unwrap();
        }
        drop(tx);
        while rx.recv().await.is_some() {}
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn count_reflects_the_built_index() {
        let harness = harness();
        add_rows(&harness, 0..TEST_ROWS).await;

        assert_eq!(harness.search.count(index_key()).await.unwrap(), TEST_ROWS);
    }

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

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn removing_a_row_lowers_the_count() {
        let harness = harness();
        add_rows(&harness, 0..TEST_ROWS).await;

        remove_rows(&harness, 0..1).await;

        assert_eq!(
            harness.search.count(index_key()).await.unwrap(),
            TEST_ROWS - 1
        );
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

    #[tokio::test]
    async fn create_index_rejects_a_local_index() {
        let table = Arc::new(RwLock::new(
            Table::new(
                index_key(),
                NonemptyArc::new(["pk"]).unwrap(),
                NonZeroUsize::new(1).unwrap(),
                Some(NonemptyArc::new(["pk"]).unwrap()),
                NonZeroUsize::new(1).unwrap(),
                Arc::new([]),
                Arc::new(HashMap::from([("pk".into(), NativeType::Int)])),
            )
            .unwrap(),
        ));

        let err = CuvsIndexFactory
            .create_index(configuration(), table)
            .unwrap_err();
        assert!(err.to_string().contains("local indexes"));
    }
}
