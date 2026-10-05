/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

use crate::Dimensions;
use crate::Distance;
use crate::Quantization;
use crate::SpaceType;
use crate::vs_index::VsIndexConfiguration;
use anyhow::anyhow;
use anyhow::bail;
use cuvs::distance::DistanceType;
use cuvs::neighbors::cagra::IndexParams;
use cuvs::neighbors::cagra::SearchParams;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CagraParams {
    pub(super) dimensions: Dimensions,
    pub(super) metric: DistanceType,
    pub(super) graph_degree: usize,
    pub(super) intermediate_graph_degree: usize,
    pub(super) expansion_search: usize,
}

impl TryFrom<&VsIndexConfiguration> for CagraParams {
    type Error = anyhow::Error;

    fn try_from(config: &VsIndexConfiguration) -> anyhow::Result<Self> {
        if config.quantization != Quantization::F32 {
            bail!(
                "cuVS index does not support quantization {:?}",
                config.quantization
            );
        }

        let metric = distance_type(config.space_type)?;
        let graph_degree = *config.connectivity.as_ref();
        let intermediate_graph_degree = *config.expansion_add.as_ref();
        let expansion_search = *config.expansion_search.as_ref();

        if graph_degree == 0 {
            bail!("cuVS index requires `maximum_node_connections` to be greater than 0");
        }

        if intermediate_graph_degree < graph_degree {
            bail!(
                "cuVS index requires `construction_beam_width` >= `maximum_node_connections`, \
                because the intermediate graph must be at least as large as the final graph."
            );
        }

        Ok(Self {
            dimensions: config.dimensions,
            metric,
            graph_degree,
            intermediate_graph_degree,
            expansion_search,
        })
    }
}

impl CagraParams {
    /// CAGRA needs more rows than the intermediate degree, so shrink both
    /// degrees for a smaller set. A single row cannot form a graph.
    pub(super) fn fit(self, rows: usize) -> Option<Self> {
        let intermediate_graph_degree = self.intermediate_graph_degree.min(rows.checked_sub(1)?);
        if intermediate_graph_degree == 0 {
            return None;
        }
        Some(Self {
            graph_degree: self.graph_degree.min(intermediate_graph_degree),
            intermediate_graph_degree,
            ..self
        })
    }

    pub(super) fn to_index_params(self) -> anyhow::Result<IndexParams> {
        IndexParams::builder()
            .metric(self.metric)
            .graph_degree(self.graph_degree)
            .intermediate_graph_degree(self.intermediate_graph_degree)
            .build()
            .map_err(|err| anyhow!("failed to build cuVS CAGRA index params: {err}"))
    }

    /// CAGRA keeps `itopk_size` candidates, so it must hold all `limit` results.
    pub(super) fn to_search_params(self, limit: usize) -> anyhow::Result<SearchParams> {
        SearchParams::builder()
            .itopk_size(self.expansion_search.max(limit))
            .build()
            .map_err(|err| anyhow!("failed to build cuVS CAGRA search params: {err}"))
    }

    pub(super) fn distance(self, value: f32) -> anyhow::Result<Distance> {
        match self.metric {
            DistanceType::L2Expanded => Distance::new_euclidean(value),
            DistanceType::CosineExpanded => Distance::new_cosine(value),
            DistanceType::InnerProduct => Distance::new_dot_product(value),
            metric => bail!("cuVS index does not support the {metric:?} metric"),
        }
    }
}

fn distance_type(space_type: SpaceType) -> anyhow::Result<DistanceType> {
    Ok(match space_type {
        SpaceType::Euclidean => DistanceType::L2Expanded,
        SpaceType::Cosine => DistanceType::CosineExpanded,
        SpaceType::DotProduct => DistanceType::InnerProduct,
        SpaceType::Hamming => bail!("cuVS index does not support the Hamming similarity metric"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connectivity;
    use crate::ExpansionAdd;
    use crate::ExpansionSearch;
    use crate::IndexKey;
    use std::num::NonZeroUsize;

    fn configuration() -> VsIndexConfiguration {
        VsIndexConfiguration {
            key: IndexKey::new(&"vector".into(), &"store".into()),
            dimensions: Dimensions::from(NonZeroUsize::new(3).unwrap()),
            connectivity: Connectivity::default(),
            expansion_add: ExpansionAdd::default(),
            expansion_search: ExpansionSearch::default(),
            space_type: SpaceType::default(),
            quantization: Quantization::default(),
        }
    }

    #[test]
    fn defaults_map_to_valid_cagra_params() {
        let params = CagraParams::try_from(&configuration()).unwrap();

        assert_eq!(
            params.dimensions,
            Dimensions::from(NonZeroUsize::new(3).unwrap())
        );
        // The service defaults to cosine.
        assert_eq!(params.metric, DistanceType::CosineExpanded);
        assert_eq!(params.graph_degree, *Connectivity::default().as_ref());
        assert_eq!(
            params.intermediate_graph_degree,
            *ExpansionAdd::default().as_ref()
        );
        assert_eq!(
            params.expansion_search,
            *ExpansionSearch::default().as_ref()
        );
        assert!(params.intermediate_graph_degree >= params.graph_degree);
    }

    #[test]
    fn space_types_map_to_cagra_metrics() {
        for (space_type, expected) in [
            (SpaceType::Euclidean, DistanceType::L2Expanded),
            (SpaceType::Cosine, DistanceType::CosineExpanded),
            (SpaceType::DotProduct, DistanceType::InnerProduct),
        ] {
            let config = VsIndexConfiguration {
                space_type,
                ..configuration()
            };
            assert_eq!(CagraParams::try_from(&config).unwrap().metric, expected);
        }
    }

    #[test]
    fn hamming_space_type_is_rejected() {
        let config = VsIndexConfiguration {
            space_type: SpaceType::Hamming,
            ..configuration()
        };
        let err = CagraParams::try_from(&config).unwrap_err().to_string();
        assert!(err.contains("Hamming"), "got: {err}");
    }

    #[test]
    fn non_f32_quantization_is_rejected() {
        for quantization in [
            Quantization::F16,
            Quantization::BF16,
            Quantization::I8,
            Quantization::B1,
        ] {
            let config = VsIndexConfiguration {
                quantization,
                ..configuration()
            };
            let err = CagraParams::try_from(&config).unwrap_err().to_string();
            assert!(err.contains("quantization"), "got: {err}");
        }
    }

    #[test]
    fn zero_connectivity_is_rejected() {
        let config = VsIndexConfiguration {
            connectivity: Connectivity::from(0),
            ..configuration()
        };
        let err = CagraParams::try_from(&config).unwrap_err().to_string();
        assert!(err.contains("maximum_node_connections"), "got: {err}");
    }

    #[test]
    fn fit_shrinks_the_degrees_below_the_row_count() {
        let params = CagraParams::try_from(&configuration()).unwrap();
        // 128 by default, so 129 rows is the smallest set built as configured.
        let degree = params.intermediate_graph_degree;

        assert_eq!(params.fit(0), None);
        assert_eq!(params.fit(1), None);
        assert_eq!(params.fit(degree + 1), Some(params));

        let fitted = params.fit(8).unwrap();
        assert_eq!(fitted.intermediate_graph_degree, 7);
        assert_eq!(fitted.graph_degree, 7);
    }

    #[test]
    fn connectivity_above_expansion_add_is_rejected() {
        let config = VsIndexConfiguration {
            connectivity: Connectivity::from(256),
            expansion_add: ExpansionAdd::from(64),
            ..configuration()
        };
        let err = CagraParams::try_from(&config).unwrap_err().to_string();
        assert!(err.contains("construction_beam_width"), "got: {err}");
        assert!(err.contains("maximum_node_connections"), "got: {err}");
    }
}
