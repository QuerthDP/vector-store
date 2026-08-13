/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

pub(crate) mod actor;
pub(crate) mod factory;
pub(crate) mod tantivy;

pub(crate) use actor::FtsIndex;
pub(crate) use actor::FtsIndexExt;
pub(crate) use actor::HighlightOptions;
pub(crate) use factory::FtsIndexFactory;
pub(crate) use tantivy::TantivyIndexFactory;
