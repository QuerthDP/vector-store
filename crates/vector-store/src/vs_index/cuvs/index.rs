/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

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
use tracing::warn;

#[derive(Debug)]
struct Rows {
    dimensions: usize,
    values: Vec<f32>,
    ids: Vec<PrimaryId>,
    positions: HashMap<PrimaryId, usize>,
}

impl Rows {
    fn new(dimensions: Dimensions) -> Self {
        Self {
            dimensions: dimensions.0.get(),
            values: Vec::new(),
            ids: Vec::new(),
            positions: HashMap::new(),
        }
    }

    fn upsert(&mut self, primary_id: PrimaryId, embedding: &Vector) {
        let values = embedding.as_slice();
        assert_eq!(values.len(), self.dimensions);
        if let Some(&row) = self.positions.get(&primary_id) {
            let start = row * self.dimensions;
            self.values[start..start + self.dimensions].copy_from_slice(values);
            return;
        }
        self.positions.insert(primary_id, self.ids.len());
        self.ids.push(primary_id);
        self.values.extend_from_slice(values);
    }

    fn remove(&mut self, primary_id: PrimaryId) -> bool {
        let Some(row) = self.positions.remove(&primary_id) else {
            return false;
        };
        let last = self.ids.len() - 1;
        let last_start = last * self.dimensions;
        if row != last {
            let start = row * self.dimensions;
            self.values
                .copy_within(last_start..last_start + self.dimensions, start);
            self.ids[row] = self.ids[last];
            self.positions.insert(self.ids[row], row);
        }
        self.ids.pop();
        self.values.truncate(last_start);
        true
    }
}

/// A CAGRA index and the device memory it reads, which `Index<'d>` only borrows.
#[derive(Debug)]
struct BuiltIndex {
    // DO NOT REORDER: `_index` borrows `_dataset` and must drop first.
    _index: Index<'static>,
    _dataset: DeviceMatrix<f32>,
    /// The id of each graph row at build time.
    ids: Vec<PrimaryId>,
}

impl BuiltIndex {
    fn build(
        resources: &Resources,
        index_params: &IndexParams,
        rows: &Rows,
    ) -> anyhow::Result<Self> {
        let row_count = rows.ids.len();
        let dataset = DeviceMatrix::from_host(resources, &rows.values, row_count, rows.dimensions)?;

        let index = Index::build(resources, index_params, &dataset)
            .map_err(|err| anyhow!("failed to build cuVS CAGRA index: {err}"))?;

        // The build returns before its kernels finish, so wait for them here.
        resources
            .sync_stream()
            .map_err(|err| anyhow!("failed to build cuVS CAGRA index: {err}"))?;

        // SAFETY: the index keeps a pointer to the device rows of `dataset`
        // rather than a copy. Moving `dataset` into the struct below doesn't
        // move those rows, and `_index` is dropped before `_dataset`.
        let index: Index<'static> = unsafe { std::mem::transmute(index) };

        Ok(Self {
            _index: index,
            _dataset: dataset,
            ids: rows.ids.clone(),
        })
    }
}

#[derive(Debug)]
pub(super) struct CuvsIndex {
    params: CagraParams,
    rows: Rows,
    built: Option<BuiltIndex>,
    // DO NOT REORDER: `built` was allocated with these and must drop first.
    resources: Resources,
    /// Guards for staged writes, released by the next build, so the index is
    /// not reported as caught up before one has tried to include them.
    pending: Vec<AsyncInProgress>,
    /// Whether the staged rows changed since the last successful build.
    stale: bool,
}

impl CuvsIndex {
    pub(super) fn new(params: CagraParams) -> anyhow::Result<Self> {
        let resources =
            Resources::new().map_err(|err| anyhow!("failed to create cuVS resources: {err}"))?;
        Ok(Self {
            params,
            rows: Rows::new(params.dimensions),
            built: None,
            resources,
            pending: Vec::new(),
            stale: false,
        })
    }

    pub(super) fn add(
        &mut self,
        primary_id: PrimaryId,
        embedding: &Vector,
        in_progress: AsyncInProgress,
    ) {
        // A row whose norm overflows, which NaN and infinity also cause, fails
        // the build, and every rebuild would include it until it is removed.
        let squared_norm: f32 = embedding.as_slice().iter().map(|value| value * value).sum();
        let skipped = if embedding.len() != self.rows.dimensions {
            Some(format!(
                "it has {} dimensions, not {}",
                embedding.len(),
                self.rows.dimensions
            ))
        } else if !squared_norm.is_finite() {
            Some("its squared norm is not finite".to_owned())
        } else {
            None
        };
        if let Some(reason) = skipped {
            warn!("cuVS skips vector {primary_id:?}: {reason}");
            self.stale |= self.rows.remove(primary_id);
            // Keep the guard until the next build drops an update's old row.
            if self.stale {
                self.pending.push(in_progress);
            }
            return;
        }

        self.rows.upsert(primary_id, embedding);
        self.pending.push(in_progress);
        self.stale = true;
    }

    pub(super) fn remove(&mut self, primary_id: PrimaryId, in_progress: AsyncInProgress) {
        if self.rows.remove(primary_id) {
            self.pending.push(in_progress);
            self.stale = true;
        }
    }

    /// Vectors in the last built graph, deliberately not the staged row count.
    pub(super) fn count(&self) -> usize {
        self.built.as_ref().map_or(0, |built| built.ids.len())
    }

    /// Rebuilds from the staged rows if they changed, releasing their guards
    /// once it returns, even on failure, since a failure that repeats would
    /// otherwise stall the pipeline. A failure keeps the previous graph and
    /// leaves the index stale, so the next tick retries.
    pub(super) fn build(&mut self) -> anyhow::Result<()> {
        if !self.stale {
            return Ok(());
        }
        let _released = std::mem::take(&mut self.pending);
        let Some(params) = self.params.fit(self.rows.ids.len()) else {
            // CAGRA rejects fewer than two rows. Dropping the graph is what such
            // a set means.
            self.built = None;
            self.stale = false;
            return Ok(());
        };

        let index_params = params.to_index_params()?;
        self.built = Some(BuiltIndex::build(
            &self.resources,
            &index_params,
            &self.rows,
        )?);
        self.stale = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connectivity;
    use crate::ExpansionAdd;
    use cuvs::distance::DistanceType;
    use rstest::rstest;
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

        assert_eq!(rows.ids.len(), 2);
        assert_eq!(rows.values, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rows.ids, vec![1.into(), 2.into()]);
    }

    #[test]
    fn upsert_replaces_existing_row_in_place() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));
        rows.upsert(2.into(), &vector(&[3.0, 4.0]));
        rows.upsert(1.into(), &vector(&[9.0, 9.0]));

        assert_eq!(rows.ids.len(), 2, "replacing must not append a row");
        assert_eq!(rows.values, vec![9.0, 9.0, 3.0, 4.0]);
    }

    #[test]
    fn remove_swaps_the_last_row_into_the_hole() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));
        rows.upsert(2.into(), &vector(&[3.0, 4.0]));
        rows.upsert(3.into(), &vector(&[5.0, 6.0]));

        assert!(rows.remove(1.into()));

        assert_eq!(rows.ids.len(), 2);
        assert_eq!(rows.values, vec![5.0, 6.0, 3.0, 4.0]);
        assert_eq!(rows.ids, vec![3.into(), 2.into()]);

        rows.upsert(3.into(), &vector(&[7.0, 7.0]));

        assert_eq!(rows.ids.len(), 2, "the swapped row lost its position");
        assert_eq!(rows.values, vec![7.0, 7.0, 3.0, 4.0]);
    }

    #[test]
    fn remove_empties_the_row_set() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));

        assert!(rows.remove(1.into()));

        assert_eq!(rows.ids.len(), 0);
        assert!(rows.values.is_empty());
    }

    #[test]
    fn remove_reports_an_id_it_never_held() {
        let mut rows = Rows::new(dimensions(2));
        rows.upsert(1.into(), &vector(&[1.0, 2.0]));

        assert!(!rows.remove(9.into()));
        assert_eq!(rows.ids.len(), 1);
    }

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
        assert_eq!(index.pending.len(), 256);

        index.build().unwrap();

        assert_eq!(index.count(), 256);
        assert_eq!(index.pending.len(), 0);
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
        assert_eq!(index.pending.len(), 0, "the guards must not be held");
    }

    #[rstest]
    fn a_set_too_small_for_the_degrees_still_gets_a_graph(
        #[values(2, 3, 17, 128, 129)] rows: usize,
    ) {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(rows, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }

        index.build().unwrap();

        assert_eq!(index.count(), rows);
        assert_eq!(index.pending.len(), 0);
    }

    #[test]
    fn a_single_row_gets_no_graph() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.build().unwrap();

        for row in 1..256u64 {
            index.remove(row.into(), AsyncInProgress::None);
        }
        index.build().unwrap();

        assert_eq!(index.count(), 0, "a single row has no graph to count");
        assert_eq!(index.pending.len(), 0, "the guards must not be held");
    }

    #[rstest]
    fn an_unbuildable_vector_is_skipped(
        #[values(vec![f32::NAN; 4], vec![f32::INFINITY; 4], vec![1e20; 4])] values: Vec<f32>,
    ) {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.add(256.into(), &vector(&values), AsyncInProgress::None);

        index.build().unwrap();

        assert_eq!(index.count(), 256);
    }

    #[test]
    fn a_vector_of_the_wrong_dimensions_is_skipped() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        index.add(1.into(), &vector(&[1.0, 2.0, 3.0]), AsyncInProgress::None);

        assert!(index.rows.ids.is_empty());
        assert!(!index.stale);
    }

    #[test]
    fn an_unbuildable_update_removes_the_row() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.add(0.into(), &vector(&[f32::NAN; 4]), AsyncInProgress::None);

        index.build().unwrap();

        assert_eq!(index.count(), 255);
    }

    #[test]
    fn a_failed_build_releases_the_guards_and_retries() {
        let mut index = CuvsIndex::new(params(4)).unwrap();
        for (row, embedding) in many_vectors(256, 4).iter().enumerate() {
            index.add((row as u64).into(), embedding, AsyncInProgress::None);
        }
        index.build().unwrap();

        // Bypasses the check in `add` to stage a row cuVS cannot build.
        index.rows.upsert(256.into(), &vector(&[f32::NAN; 4]));
        index.pending.push(AsyncInProgress::None);
        index.stale = true;

        assert!(index.build().is_err());
        assert_eq!(index.count(), 256, "the previous graph must be kept");
        assert_eq!(index.pending.len(), 0, "the guards must not be held");
        assert!(index.stale, "the next tick must retry");
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
