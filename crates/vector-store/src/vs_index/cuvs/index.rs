/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

//! CAGRA has no incremental insert in the Rust bindings, so the host-side row
//! set is the source of truth and every change rebuilds the whole graph.

use crate::AsyncInProgress;
use crate::Dimensions;
use crate::Vector;
use crate::table::PrimaryId;
use crate::vs_index::cuvs::params::CagraParams;
use anyhow::anyhow;
use cuvs::Resources;
use cuvs::dlpack::AsDlTensor;
use cuvs::dlpack::DLDevice;
use cuvs::dlpack::DLDeviceType;
use cuvs::dlpack::DLPackError;
use cuvs::dlpack::DLTensorView;
use cuvs::dlpack::DType;
use cuvs::neighbors::cagra::Index;
use cuvs::neighbors::cagra::IndexParams;
use std::collections::HashMap;
use std::ffi::c_void;
use tracing::debug;

/// The row set a build snapshots, flat and row-major so it uploads in one copy.
#[derive(Debug)]
struct Rows {
    dimensions: usize,
    values: Vec<f32>,
    ids: Vec<PrimaryId>,
    /// Primary id -> row index.
    positions: HashMap<PrimaryId, usize>,
}

impl Rows {
    fn new(dimensions: Dimensions) -> Self {
        Self {
            dimensions: usize::from(*dimensions.as_ref()),
            values: Vec::new(),
            ids: Vec::new(),
            positions: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn upsert(&mut self, primary_id: PrimaryId, embedding: &Vector) {
        let values = embedding.as_slice();
        if let Some(&row) = self.positions.get(&primary_id) {
            let start = row * self.dimensions;
            self.values[start..start + self.dimensions].copy_from_slice(values);
            return;
        }
        self.positions.insert(primary_id, self.ids.len());
        self.ids.push(primary_id);
        self.values.extend_from_slice(values);
    }

    /// Swap-removes, so row order is not preserved. Nothing depends on it: a
    /// build reads the whole set and search maps back through `ids`.
    fn remove(&mut self, primary_id: PrimaryId) -> bool {
        let Some(row) = self.positions.remove(&primary_id) else {
            return false;
        };
        let last = self.ids.len() - 1;
        if row != last {
            let (start, last_start) = (row * self.dimensions, last * self.dimensions);
            self.values
                .copy_within(last_start..last_start + self.dimensions, start);
            self.ids[row] = self.ids[last];
            self.positions.insert(self.ids[row], row);
        }
        self.ids.pop();
        self.values.truncate(last * self.dimensions);
        true
    }
}

/// A contiguous row-major matrix in host memory. The `cuvs` crate ships no
/// public tensor type, so the backend brings its own.
struct HostMatrix {
    values: Vec<f32>,
    shape: [i64; 2],
}

impl HostMatrix {
    fn new(values: Vec<f32>, rows: usize, columns: usize) -> Self {
        Self {
            values,
            shape: [rows as i64, columns as i64],
        }
    }

    fn allocated_bytes(&self) -> usize {
        self.values.len() * size_of::<f32>()
    }
}

/// Hand-written: a dataset runs to millions of values, which no log line wants.
impl std::fmt::Debug for HostMatrix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMatrix")
            .field("rows", &self.shape[0])
            .field("columns", &self.shape[1])
            .field("bytes", &self.allocated_bytes())
            .finish()
    }
}

impl AsDlTensor for HostMatrix {
    fn as_dl_tensor(&self) -> Result<DLTensorView<'_>, DLPackError> {
        // SAFETY: `values` holds exactly the elements `shape` describes, and the
        // view borrows `self`. `None` strides declare the layout it has.
        unsafe {
            DLTensorView::from_raw_parts(
                self.values.as_ptr().cast_mut() as *mut c_void,
                DLDevice {
                    device_type: DLDeviceType::kDLCPU,
                    device_id: 0,
                },
                &self.shape,
                None,
                f32::dl_dtype(),
            )
        }
    }
}

/// A CAGRA index and the matrix it reads, which `Index<'d>` only borrows.
#[derive(Debug)]
struct BuiltIndex {
    // Do not reorder: `_index` borrows `dataset` and must drop first, which
    // declaration order is what guarantees. The `cuvs` crate relies on the same
    // ordering in its own `DeserializedIndex`.
    _index: Index<'static>,
    dataset: Box<HostMatrix>,
    rows: usize,
}

impl BuiltIndex {
    fn build(
        resources: &Resources,
        index_params: &IndexParams,
        rows: &Rows,
    ) -> anyhow::Result<Self> {
        let row_count = rows.len();
        // A snapshot, because staging keeps growing while the index reads this
        // buffer. Moving it into VRAM is the next commit.
        let dataset = Box::new(HostMatrix::new(
            rows.values.clone(),
            row_count,
            rows.dimensions,
        ));

        let index = Index::build(resources, index_params, dataset.as_ref())
            .map_err(|err| anyhow!("failed to build cuVS CAGRA index: {err}"))?;

        // SAFETY: the `Box` gives `dataset` a stable address, so moving it into
        // the struct below leaves the borrow valid. The extended lifetime never
        // escapes that struct, whose field order drops `_index` first.
        let index: Index<'static> = unsafe { std::mem::transmute(index) };

        Ok(Self {
            _index: index,
            dataset,
            rows: row_count,
        })
    }
}

/// All cuVS state for one vector index, owned by the dedicated cuVS thread.
#[derive(Debug)]
pub(super) struct CuvsIndex {
    resources: Resources,
    index_params: IndexParams,
    rows: Rows,
    built: Option<BuiltIndex>,
    /// Guards for staged writes, released only by a successful build, so the
    /// index is not reported as caught up before they are queryable. The full-text
    /// backend holds them across a commit for the same reason.
    pending: Vec<AsyncInProgress>,
}

impl CuvsIndex {
    pub(super) fn new(params: CagraParams) -> anyhow::Result<Self> {
        let resources =
            Resources::new().map_err(|err| anyhow!("failed to create cuVS resources: {err}"))?;
        let index_params = params.to_index_params()?;
        Ok(Self {
            resources,
            index_params,
            rows: Rows::new(params.dimensions),
            built: None,
            pending: Vec::new(),
        })
    }

    pub(super) fn add(
        &mut self,
        primary_id: PrimaryId,
        embedding: &Vector,
        in_progress: AsyncInProgress,
    ) {
        self.rows.upsert(primary_id, embedding);
        self.pending.push(in_progress);
    }

    /// An update arrives as a removal of the old epoch's id followed by an add
    /// of the new one, so dropping removals would leak a row per update.
    pub(super) fn remove(&mut self, primary_id: PrimaryId, in_progress: AsyncInProgress) {
        if self.rows.remove(primary_id) {
            self.pending.push(in_progress);
        }
    }

    pub(super) fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Vectors in the last built graph, deliberately not the staged row count.
    pub(super) fn count(&self) -> usize {
        self.built.as_ref().map_or(0, |built| built.rows)
    }

    /// Rebuilds from the staged rows, releasing their guards. A failure keeps
    /// the previous graph and the guards, leaving the next trigger to retry.
    pub(super) fn build(&mut self) -> anyhow::Result<()> {
        if self.rows.len() == 0 {
            // CAGRA rejects an empty dataset, and a retry loop would hold the
            // guards forever. Dropping the graph is what an empty set means.
            self.built = None;
            self.pending.clear();
            return Ok(());
        }

        let built = BuiltIndex::build(&self.resources, &self.index_params, &self.rows)?;
        debug!(
            rows = built.rows,
            bytes = built.dataset.allocated_bytes(),
            "built cuVS CAGRA index"
        );
        self.built = Some(built);
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connectivity;
    use crate::ExpansionAdd;
    use cuvs::distance::DistanceType;
    use std::num::NonZeroUsize;

    fn dimensions(value: usize) -> Dimensions {
        Dimensions::from(NonZeroUsize::new(value).unwrap())
    }

    fn params(dims: usize) -> CagraParams {
        CagraParams {
            dimensions: dimensions(dims),
            metric: DistanceType::L2Expanded,
            graph_degree: *Connectivity::default().as_ref(),
            intermediate_graph_degree: *ExpansionAdd::default().as_ref(),
        }
    }

    fn vector(values: &[f32]) -> Vector {
        Vector::from(values.to_vec())
    }

    #[test]
    fn upsert_appends_new_rows() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));
        rows.upsert(2.into(), &vector(&[3.0, 4.0]));

        assert_eq!(rows.len(), 2);
        assert_eq!(rows.values, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rows.ids, vec![1.into(), 2.into()]);
    }

    #[test]
    fn upsert_replaces_existing_row_in_place() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));
        rows.upsert(2.into(), &vector(&[3.0, 4.0]));
        rows.upsert(1.into(), &vector(&[9.0, 9.0]));

        assert_eq!(rows.len(), 2, "replacing must not append a row");
        assert_eq!(rows.values, vec![9.0, 9.0, 3.0, 4.0]);
    }

    #[test]
    fn remove_swaps_the_last_row_into_the_hole() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));
        rows.upsert(2.into(), &vector(&[3.0, 4.0]));
        rows.upsert(3.into(), &vector(&[5.0, 6.0]));

        assert!(rows.remove(1.into()));

        assert_eq!(rows.len(), 2);
        assert_eq!(rows.values, vec![5.0, 6.0, 3.0, 4.0]);
        assert_eq!(rows.ids, vec![3.into(), 2.into()]);

        // The moved row has to stay findable, or a later write appends beside it.
        rows.upsert(3.into(), &vector(&[7.0, 7.0]));

        assert_eq!(rows.len(), 2, "the swapped row lost its position");
        assert_eq!(rows.values, vec![7.0, 7.0, 3.0, 4.0]);
    }

    #[test]
    fn remove_empties_the_row_set() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));

        assert!(rows.remove(1.into()));

        assert_eq!(rows.len(), 0);
        assert!(rows.values.is_empty());
    }

    #[test]
    fn remove_reports_an_id_it_never_held() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));

        assert!(!rows.remove(9.into()));
        assert_eq!(rows.len(), 1);
    }

    /// CAGRA needs a non-trivial dataset before it will build a graph.
    fn many_vectors(count: usize, dims: usize) -> Vec<Vector> {
        (0..count)
            .map(|row| {
                vector(
                    &(0..dims)
                        .map(|col| (row * dims + col) as f32 * 0.001)
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    #[test]
    fn count_is_zero_until_a_build_succeeds() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }

        assert_eq!(index.count(), 0, "staged rows must not be counted");
        assert_eq!(index.pending(), 256);

        index.build().unwrap();

        assert_eq!(index.count(), 256);
        assert_eq!(index.pending(), 0);
    }

    #[test]
    fn emptying_the_row_set_drops_the_graph() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.build().unwrap();

        for row in 0..256u64 {
            index.remove(row.into(), AsyncInProgress::None);
        }
        index.build().unwrap();

        assert_eq!(index.count(), 0, "the emptied graph must not be reported");
        assert_eq!(
            index.pending(),
            0,
            "the guards must not be held for a retry"
        );
    }

    #[test]
    fn build_with_unaligned_dimensions_succeeds() {
        // Not a multiple of 4, so cuVS sees a standard rather than a padded
        // layout. CAGRA builds from either.
        let mut index = CuvsIndex::new(params(3)).unwrap();
        for (row, embedding) in many_vectors(256, 3).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }

        index.build().unwrap();
        assert_eq!(index.count(), 256);
    }
}
