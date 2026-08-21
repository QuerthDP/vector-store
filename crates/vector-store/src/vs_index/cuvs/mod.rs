/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

//! GPU-accelerated vector index backed by NVIDIA cuVS (CAGRA).
//!
//! # Threading
//!
//! cuVS index and dataset handles are raw pointers, so they are neither `Send`
//! nor `Sync`, and every cuVS call blocks the calling thread. Both facts rule
//! out the shared [`crate::worker`] pool, which requires `Send` closures and
//! runs them inline on a runtime worker.
//!
//! So each index owns a dedicated thread. The async actor keeps the usual
//! search-over-modify priority and forwards messages to that thread over a
//! bounded channel; the thread creates all cuVS state inside its own closure, so
//! nothing that is not `Send` ever crosses a thread boundary and no `unsafe` is
//! needed to make it legal. Replies go straight back on the `oneshot::Sender`
//! carried in each search message.
//!
//! Upstream permits sharing an index across threads when each has its own
//! `raft::resources`, so this can grow into a pool of GPU threads later; a
//! single one is enough while only builds are implemented, since they serialize
//! on one GPU regardless.
//!
//! # Rebuilds
//!
//! CAGRA is batch-built and the Rust bindings expose no incremental insert, so
//! writes accumulate host-side and the graph is rebuilt from them on a timer.
//! Until a rebuild lands, the writes are not in the index -- so the in-progress
//! guards that report indexing lag are held until it does.

mod device;
mod index;
mod params;

use crate::Config;
use crate::IndexKey;
use crate::VsIndexFactory;
use crate::memory::Allocate;
use crate::memory::Memory;
use crate::memory::MemoryExt;
use crate::perf;
use crate::table::PartitionId;
use crate::table::Table;
use crate::table::TableSearch;
use crate::vs_index;
use crate::vs_index::Message;
use crate::vs_index::VsIndexModify;
use crate::vs_index::VsIndexSearch;
use crate::vs_index::factory::VsIndexConfiguration;
use crate::vs_index::validator;
use anyhow::anyhow;
use index::CuvsIndex;
use params::CagraParams;
use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::Instrument;
use tracing::debug;
use tracing::debug_span;
use tracing::error;
use tracing::warn;

/// Rebuild this often while changes are staged.
const BUILD_INTERVAL: Duration = Duration::from_secs(3);

pub struct CuvsIndexFactory {
    memory: mpsc::Sender<Memory>,
}

impl VsIndexFactory for CuvsIndexFactory {
    fn create_index(
        &self,
        index: VsIndexConfiguration,
        table: Arc<RwLock<Table>>,
    ) -> anyhow::Result<(mpsc::Sender<VsIndexModify>, mpsc::Sender<VsIndexSearch>)> {
        // Validated here rather than on the cuVS thread so a bad index option is
        // reported when the index is created, not silently much later.
        let params = CagraParams::try_from(&index)?;
        new(
            index.key,
            params,
            table,
            self.memory.clone(),
            BUILD_INTERVAL,
        )
    }

    fn index_engine_version(&self) -> String {
        match cuvs::version::version() {
            Ok((major, minor, patch)) => format!("cuvs-{major}.{minor}.{patch}"),
            Err(err) => format!("cuvs-unknown ({err})"),
        }
    }
}

pub fn new_cuvs(
    _config_rx: watch::Receiver<Arc<Config>>,
    memory: mpsc::Sender<Memory>,
) -> anyhow::Result<CuvsIndexFactory> {
    // Fail at startup rather than at the first index creation if there is no
    // usable GPU.
    cuvs::Resources::new()
        .map_err(|err| anyhow!("failed to initialize cuVS/CUDA resources: {err}"))?;
    Ok(CuvsIndexFactory { memory })
}

/// What the async actor sends to the cuVS thread.
enum Request {
    Message(Message),
    /// Timer tick: rebuild if anything is staged.
    Flush,
}

fn new(
    index_key: IndexKey,
    params: CagraParams,
    table: Arc<RwLock<impl TableSearch + Send + Sync + 'static>>,
    memory: mpsc::Sender<Memory>,
    build_interval: Duration,
) -> anyhow::Result<(mpsc::Sender<VsIndexModify>, mpsc::Sender<VsIndexSearch>)> {
    let channel_size = perf::channel_size().into();
    let (tx_modify, mut rx_modify) = mpsc::channel(channel_size);
    let (tx_search, mut rx_search) = mpsc::channel(channel_size);
    // Bounded so the actor applies backpressure while a build is in flight.
    let (tx_gpu, mut rx_gpu) = mpsc::channel(channel_size);

    let thread_key = index_key.clone();
    thread::Builder::new()
        .name(format!("cuvs-{index_key}"))
        .spawn(move || {
            // All cuVS state is created here and never leaves this thread.
            let mut index = match CuvsIndex::new(params) {
                Ok(index) => index,
                Err(err) => {
                    error!("unable to create cuVS index for {thread_key}: {err}");
                    // Draining keeps senders from blocking; every search gets an
                    // error rather than hanging.
                    while let Some(request) = rx_gpu.blocking_recv() {
                        if let Request::Message(msg) = request {
                            reject(msg, || anyhow!("cuVS index is unavailable: {err}"));
                        }
                    }
                    return;
                }
            };

            debug!("cuVS thread starting for {thread_key}");
            while let Some(request) = rx_gpu.blocking_recv() {
                handle(&mut index, table.as_ref(), &thread_key, request);
            }
            debug!("cuVS thread finished for {thread_key}");
        })
        .map_err(|err| anyhow!("unable to spawn cuVS thread for {index_key}: {err}"))?;

    let span_key = index_key.clone();
    tokio::spawn(perf::hotpath_async(
        async move {
            debug!("starting");

            let mut allocate_prev = Allocate::Can;
            let allocate_rx = memory.subscribe_allocate().await;

            let mut interval = tokio::time::interval(build_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Prefer forwarding real work over firing the rebuild timer.
                    biased;

                    msg = vs_index::recv(&mut rx_search, &mut rx_modify) => {
                        let Some(msg) = msg else {
                            break;
                        };
                        if !check_memory_allocation(
                            &msg,
                            &allocate_rx,
                            &mut allocate_prev,
                            &index_key,
                        ) {
                            continue;
                        }
                        if tx_gpu.send(Request::Message(msg)).await.is_err() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        if tx_gpu.send(Request::Flush).await.is_err() {
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

/// Answers a message with an error, for when the index cannot serve it at all.
fn reject(msg: Message, err: impl Fn() -> anyhow::Error) {
    match msg {
        Message::Search(VsIndexSearch::Ann { tx, .. } | VsIndexSearch::FilteredAnn { tx, .. }) => {
            _ = tx.send(Err(err()));
        }
        Message::Search(VsIndexSearch::Count { tx, .. }) => {
            _ = tx.send(Err(err()));
        }
        Message::Modify(_) => {}
    }
}

/// Drops writes while the host is out of memory.
///
/// The staged rows a rebuild reads from live in host RAM, so the existing
/// host-memory gate is the right one; a VRAM budget is a separate concern.
/// Logged only on the transition, to avoid a flood.
fn check_memory_allocation(
    msg: &Message,
    rx_allocate: &watch::Receiver<Allocate>,
    allocate_prev: &mut Allocate,
    index_key: &IndexKey,
) -> bool {
    if !matches!(msg, Message::Modify(VsIndexModify::AddVector { .. })) {
        return true;
    }
    let allocate = *rx_allocate.borrow();
    if allocate == Allocate::Cannot {
        if *allocate_prev == Allocate::Can {
            error!("Unable to add vector for index {index_key}: not enough memory");
        }
        *allocate_prev = allocate;
        return false;
    }
    *allocate_prev = allocate;
    true
}

fn handle(
    index: &mut CuvsIndex,
    table: &RwLock<impl TableSearch>,
    index_key: &IndexKey,
    request: Request,
) {
    match request {
        Request::Flush => {
            build_if_pending(index, index_key);
        }
        Request::Message(Message::Modify(VsIndexModify::AddVector {
            partition_id,
            primary_id,
            embedding,
            in_progress,
        })) => {
            if !is_global(partition_id, index_key) {
                return;
            }
            // Unlike the CPU backends, which only validate on the query path, a
            // wrong-length vector here would corrupt the row-major matrix every
            // row is copied into.
            if let Err(err) = validator::embedding_dimensions(&embedding, index.dimensions()) {
                warn!("Unable to add vector to index {index_key}: {err}");
                return;
            }
            index.add(primary_id, &embedding, in_progress);
        }
        Request::Message(Message::Modify(VsIndexModify::RemoveVector {
            partition_id,
            primary_id,
            in_progress,
        })) => {
            if !is_global(partition_id, index_key) {
                return;
            }
            index.remove(primary_id, in_progress);
        }
        Request::Message(Message::Modify(VsIndexModify::RemovePartition { partition_id })) => {
            if !is_global(partition_id, index_key) {
                return;
            }
            index.clear();
            build_if_pending(index, index_key);
        }
        Request::Message(Message::Search(VsIndexSearch::Count { index_key, tx })) => {
            let result = match table.read().unwrap().index_id(&index_key) {
                Some(_) => Ok(index.count()),
                None => Err(anyhow!("index id not found for index key {index_key}")),
            };
            _ = tx.send(result);
        }
        Request::Message(Message::Search(VsIndexSearch::Ann { tx, .. })) => {
            _ = tx.send(Err(anyhow!("cuVS index search is not implemented yet")));
        }
        Request::Message(Message::Search(VsIndexSearch::FilteredAnn { tx, .. })) => {
            _ = tx.send(Err(anyhow!(
                "cuVS index does not support filtered search: the GPU backend serves unfiltered \
                 ANN queries only"
            )));
        }
    }
}

fn build_if_pending(index: &mut CuvsIndex, index_key: &IndexKey) {
    if index.pending() == 0 {
        return;
    }
    if let Err(err) = index.build() {
        // The previously built graph is kept and the staged changes are not
        // cleared, so the next trigger retries.
        error!("Unable to build cuVS index {index_key}: {err}");
    }
}

/// The GPU backend serves global indexes only.
///
/// A local index would create one small graph per partition, which wastes VRAM
/// on per-index overhead and gives the GPU batches too small to be worth the
/// transfer. `VsIndexConfiguration` carries no partitioning field, so this is
/// checked per message rather than at index creation.
fn is_global(partition_id: PartitionId, index_key: &IndexKey) -> bool {
    if partition_id.index_id().is_global() {
        return true;
    }
    warn!(
        "Ignoring modification for non-global index {index_key}: the cuVS backend supports \
         global indexes only"
    );
    false
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
    use crate::memory::Memory;
    use crate::table::IndexIdGenerator;
    use crate::table::MockTableSearch;
    use crate::table::PrimaryId;
    use crate::vs_index::VsIndexModifyExt;
    use crate::vs_index::VsIndexSearchExt;
    use rstest::rstest;
    use std::num::NonZeroUsize;

    const TEST_BUILD_INTERVAL: Duration = Duration::from_millis(50);

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

    fn memory_actor(allocate: Allocate) -> mpsc::Sender<Memory> {
        let (tx, mut rx) = mpsc::channel::<Memory>(1);
        tokio::spawn(async move {
            let (watch_tx, _) = watch::channel(allocate);
            while let Some(Memory::SubscribeAllocate { tx }) = rx.recv().await {
                let _ = tx.send(watch_tx.subscribe());
            }
        });
        tx
    }

    struct Harness {
        modify: mpsc::Sender<VsIndexModify>,
        search: mpsc::Sender<VsIndexSearch>,
        partition_id: PartitionId,
    }

    fn harness(global: bool, allocate: Allocate) -> Harness {
        let index_id = IndexIdGenerator::new().next(global).unwrap();
        let partition_id = PartitionId::global(index_id);
        let (modify, search) = new(
            index_key(),
            CagraParams::try_from(&configuration()).unwrap(),
            table_with(index_id),
            memory_actor(allocate),
            TEST_BUILD_INTERVAL,
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

    /// Adds `rows` vectors and waits for every in-progress guard to drop, which
    /// the backend does only once the writes are in a built index.
    ///
    /// One guard sender is cloned across the whole batch and the receiver is
    /// drained until the channel closes, mirroring how a full scan tracks a
    /// range (see `db_index.rs`). Waiting per vector instead would serialize the
    /// batch on one rebuild each.
    async fn add_rows(harness: &Harness, rows: usize) {
        let (tx, mut rx) = mpsc::channel(1);
        for row in 0..rows {
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

    async fn remove_rows(harness: &Harness, rows: std::ops::Range<u64>) {
        let (tx, mut rx) = mpsc::channel(1);
        for row in rows {
            harness
                .modify
                .remove_vector(
                    harness.partition_id,
                    PrimaryId::from(row),
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
        let harness = harness(true, Allocate::Can);
        add_rows(&harness, TEST_ROWS).await;

        // No polling: the in-progress guards `add_rows` waited on are released
        // only by a successful build, so the count must already be correct. A
        // backend that dropped them on staging would fail here.
        assert_eq!(
            harness.search.count(index_key()).await.unwrap(),
            TEST_ROWS,
            "in-progress guards must be held until the build that includes the writes"
        );
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn removed_vectors_leave_the_index_on_rebuild() {
        let harness = harness(true, Allocate::Can);
        add_rows(&harness, TEST_ROWS).await;
        assert_eq!(harness.search.count(index_key()).await.unwrap(), TEST_ROWS);

        remove_rows(&harness, 0..56).await;

        assert_eq!(
            harness.search.count(index_key()).await.unwrap(),
            TEST_ROWS - 56
        );
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn count_is_zero_before_the_first_build() {
        let harness = harness(true, Allocate::Can);

        assert_eq!(harness.search.count(index_key()).await.unwrap(), 0);
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn vectors_are_dropped_when_memory_is_exhausted() {
        let harness = harness(true, Allocate::Cannot);
        for row in 0..TEST_ROWS {
            harness
                .modify
                .add_vector(
                    harness.partition_id,
                    PrimaryId::from(row as u64),
                    embedding(row),
                    AsyncInProgress::None,
                )
                .await
                .unwrap();
        }

        // Give the interval trigger a chance to fire; nothing should be indexed.
        tokio::time::sleep(TEST_BUILD_INTERVAL * 3).await;
        assert_eq!(harness.search.count(index_key()).await.unwrap(), 0);
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn non_global_indexes_are_not_served() {
        let harness = harness(false, Allocate::Can);
        for row in 0..TEST_ROWS {
            harness
                .modify
                .add_vector(
                    harness.partition_id,
                    PrimaryId::from(row as u64),
                    embedding(row),
                    AsyncInProgress::None,
                )
                .await
                .unwrap();
        }

        tokio::time::sleep(TEST_BUILD_INTERVAL * 3).await;
        assert_eq!(harness.search.count(index_key()).await.unwrap(), 0);
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn wrong_dimension_vectors_are_rejected() {
        let harness = harness(true, Allocate::Can);
        harness
            .modify
            .add_vector(
                harness.partition_id,
                PrimaryId::from(0),
                Vector::from(vec![1.0, 2.0]),
                AsyncInProgress::None,
            )
            .await
            .unwrap();

        tokio::time::sleep(TEST_BUILD_INTERVAL * 3).await;
        assert_eq!(harness.search.count(index_key()).await.unwrap(), 0);
    }

    #[rstest]
    #[timeout(Duration::from_secs(60))]
    #[tokio::test]
    async fn filtered_search_reports_that_it_is_unsupported() {
        let harness = harness(true, Allocate::Can);

        let err = harness
            .search
            .filtered_ann(
                index_key(),
                embedding(0),
                crate::Filter {
                    restrictions: Vec::new(),
                    allow_filtering: false,
                },
                NonZeroUsize::new(1).unwrap().into(),
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("filtered search"), "got: {err}");
    }

    #[test]
    fn index_engine_version_reports_cuvs_library_version() {
        let factory = CuvsIndexFactory {
            memory: mpsc::channel(1).0,
        };
        let (major, minor, patch) = cuvs::version::version().unwrap();
        assert_eq!(
            factory.index_engine_version(),
            format!("cuvs-{major}.{minor}.{patch}")
        );
    }
}
