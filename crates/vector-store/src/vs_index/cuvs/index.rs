/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

//! The GPU-resident CAGRA index and the host-side rows it is built from.
//!
//! Everything here runs on the backend's dedicated cuVS thread. `Resources` is
//! the only cuVS type that would cross a thread boundary safely; `Index` and the
//! device buffers are not `Send`, so they are created and dropped here and
//! nowhere else.
//!
//! CAGRA has no incremental insert -- `cuvsCagraExtend` exists in the C API but
//! is not exposed by the Rust bindings -- so every change requires rebuilding
//! the whole graph. That makes the host-side row set, not the GPU index, the
//! source of truth: adds and removes edit it freely, and a build snapshots it.

use crate::AsyncInProgress;
use crate::Dimensions;
use crate::Vector;
use crate::table::PrimaryId;
use crate::vs_index::cuvs::device::DeviceMatrix;
use crate::vs_index::cuvs::params::CagraParams;
use anyhow::anyhow;
use cuvs::Resources;
use cuvs::neighbors::cagra::Index;
use cuvs::neighbors::cagra::IndexParams;
use std::collections::HashMap;
use tracing::debug;

/// The host-side row set a build snapshots.
///
/// Rows are stored flat and row-major so they can be uploaded in one copy.
#[derive(Debug)]
struct Rows {
    dimensions: usize,
    /// `rows * dimensions` values.
    values: Vec<f32>,
    /// Row index -> primary id.
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

    /// Adds a vector, replacing any row already held for `primary_id`.
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

    /// Removes the row for `primary_id`, if present.
    ///
    /// Uses swap-remove, so row order is not stable across removals. That is
    /// fine: rows only need to line up with `ids` within a single build, and the
    /// graph is rebuilt from scratch afterwards.
    fn remove(&mut self, primary_id: PrimaryId) -> bool {
        let Some(row) = self.positions.remove(&primary_id) else {
            return false;
        };
        let last = self.ids.len() - 1;
        if row != last {
            let (dst, src) = (row * self.dimensions, last * self.dimensions);
            self.values.copy_within(src..src + self.dimensions, dst);
            self.ids[row] = self.ids[last];
            self.positions.insert(self.ids[row], row);
        }
        self.values.truncate(last * self.dimensions);
        self.ids.truncate(last);
        true
    }

    fn clear(&mut self) {
        self.values.clear();
        self.ids.clear();
        self.positions.clear();
    }
}

/// A CAGRA index and the device memory it reads.
///
/// `cuvs::neighbors::cagra::Index<'d>` keeps only a non-owning view of its
/// dataset, so the two have to be owned together.
#[derive(Debug)]
struct BuiltIndex {
    // SAFETY-CRITICAL FIELD ORDER: `index` borrows `dataset` and must be
    // destroyed first. Rust drops fields in declaration order, and the `cuvs`
    // crate makes the same guarantee the same way in its own `DeserializedIndex`
    // ("Field order is significant: the native index is destroyed before its
    // dataset owner"). Do not reorder these two fields.
    #[allow(
        dead_code,
        reason = "held to keep the graph resident in VRAM; read once search lands"
    )]
    index: Index<'static>,
    #[allow(
        dead_code,
        reason = "owns the device memory `index` borrows; see the field-order note"
    )]
    dataset: Box<DeviceMatrix<f32>>,
    /// Number of vectors in the graph.
    rows: usize,
}

impl BuiltIndex {
    fn build(
        resources: &Resources,
        index_params: &IndexParams,
        rows: &Rows,
    ) -> anyhow::Result<Self> {
        let row_count = rows.len();
        let dataset = Box::new(DeviceMatrix::from_host(
            resources,
            &rows.values,
            row_count,
            rows.dimensions,
        )?);

        let index = Index::build(resources, index_params, dataset.as_ref())
            .map_err(|err| anyhow!("failed to build cuVS CAGRA index: {err}"))?;

        // SAFETY: `index` borrows the `DeviceMatrix` behind `dataset`. Boxing it
        // gives that allocation a stable address, so moving the `Box` into the
        // struct below does not move the pointee and the borrow stays valid.
        // The extended lifetime is confined to this struct, whose field order
        // guarantees `index` is dropped before `dataset`.
        let index: Index<'static> = unsafe { std::mem::transmute(index) };

        Ok(Self {
            index,
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
    dimensions: Dimensions,
    rows: Rows,
    built: Option<BuiltIndex>,
    /// In-progress guards for writes that are staged but not yet in a built
    /// graph. Held here so the index is not reported as caught up until the
    /// build that makes those writes visible has succeeded -- the same reason
    /// the full-text backend holds them across a commit. The CPU vector
    /// backends drop theirs immediately, because their writes are visible at
    /// once.
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
            dimensions: params.dimensions,
            rows: Rows::new(params.dimensions),
            built: None,
            pending: Vec::new(),
        })
    }

    pub(super) fn dimensions(&self) -> Dimensions {
        self.dimensions
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

    pub(super) fn remove(&mut self, primary_id: PrimaryId, in_progress: AsyncInProgress) {
        if self.rows.remove(primary_id) {
            self.pending.push(in_progress);
        }
    }

    /// Drops every row. Global indexes have a single partition, so removing it
    /// empties the index.
    pub(super) fn clear(&mut self) {
        if self.rows.len() == 0 {
            return;
        }
        self.rows.clear();
        // No guard to hold: `RemovePartition` carries none.
        self.pending.push(AsyncInProgress::None);
    }

    /// Writes staged since the last successful build.
    pub(super) fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Vectors in the last successfully built graph.
    ///
    /// Deliberately not the staged row count: a vector is only counted once it
    /// is actually in the index, which is what makes `count` a usable signal
    /// that a build succeeded.
    pub(super) fn count(&self) -> usize {
        self.built.as_ref().map_or(0, |built| built.rows)
    }

    /// Rebuilds the graph from the staged rows.
    ///
    /// On success the staged writes are now in the index, so their in-progress
    /// guards are released. On failure the previously built graph is kept and
    /// the guards are held, so the next trigger retries and the index keeps
    /// reporting that it is behind.
    pub(super) fn build(&mut self) -> anyhow::Result<()> {
        if self.rows.len() == 0 {
            // CAGRA cannot build an empty graph; an emptied index simply has
            // nothing resident.
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
            // Small graph degrees keep the test datasets tiny.
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
    fn remove_swaps_the_last_row_into_the_gap() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 1.0]));
        rows.upsert(2.into(), &vector(&[2.0, 2.0]));
        rows.upsert(3.into(), &vector(&[3.0, 3.0]));

        assert!(rows.remove(1.into()));

        assert_eq!(rows.len(), 2);
        assert_eq!(rows.values, vec![3.0, 3.0, 2.0, 2.0]);
        assert_eq!(rows.ids, vec![3.into(), 2.into()]);
        // The moved row must still be reachable by its id.
        assert_eq!(rows.positions[&PrimaryId::from(3)], 0);
        assert_eq!(rows.positions[&PrimaryId::from(2)], 1);
    }

    #[test]
    fn remove_of_the_last_row_leaves_the_rest_intact() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 1.0]));
        rows.upsert(2.into(), &vector(&[2.0, 2.0]));

        assert!(rows.remove(2.into()));

        assert_eq!(rows.values, vec![1.0, 1.0]);
        assert_eq!(rows.ids, vec![1.into()]);
    }

    #[test]
    fn remove_of_an_unknown_id_is_a_no_op() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 1.0]));

        assert!(!rows.remove(7.into()));
        assert_eq!(rows.len(), 1);
    }

    /// CAGRA needs a non-trivial dataset to build a graph, so these tests use a
    /// few hundred rows rather than a handful.
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
    fn rebuild_reflects_removals() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.build().unwrap();
        assert_eq!(index.count(), 256);

        for row in 0..56u64 {
            index.remove(row.into(), AsyncInProgress::None);
        }
        // The built graph is untouched until the next build.
        assert_eq!(index.count(), 256);

        index.build().unwrap();
        assert_eq!(index.count(), 200);
    }

    #[test]
    fn build_with_unaligned_dimensions_succeeds() {
        // 3 dimensions is not a multiple of 4, so cuVS sees a standard rather
        // than a padded dataset layout. CAGRA accepts both for building.
        let mut index = CuvsIndex::new(params(3)).unwrap();
        for (row, embedding) in many_vectors(256, 3).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }

        index.build().unwrap();
        assert_eq!(index.count(), 256);
    }

    #[test]
    fn clearing_all_rows_empties_the_index() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.build().unwrap();
        assert_eq!(index.count(), 256);

        index.clear();
        index.build().unwrap();

        assert_eq!(index.count(), 0);
    }
}
