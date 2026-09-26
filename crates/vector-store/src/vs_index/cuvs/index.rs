/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

use crate::Dimensions;
use crate::Vector;
use crate::table::PrimaryId;
use std::collections::HashMap;

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

#[derive(Debug)]
pub(super) struct CuvsIndex {
    rows: Rows,
}

impl CuvsIndex {
    pub(super) fn new(dimensions: Dimensions) -> Self {
        Self {
            rows: Rows::new(dimensions),
        }
    }

    pub(super) fn add(&mut self, primary_id: PrimaryId, embedding: &Vector) {
        self.rows.upsert(primary_id, embedding);
    }

    pub(super) fn remove(&mut self, primary_id: PrimaryId) {
        self.rows.remove(primary_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn dimensions(value: usize) -> Dimensions {
        Dimensions::from(NonZeroUsize::new(value).unwrap())
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
}
