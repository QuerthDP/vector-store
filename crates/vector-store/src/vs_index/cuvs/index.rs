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
}
