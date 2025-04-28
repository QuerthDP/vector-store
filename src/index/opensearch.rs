/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

// TODO: Please remove if necessary implementation is provided.
#![allow(dead_code)]

use crate::Connectivity;
use crate::Dimensions;
use crate::Embedding;
use crate::ExpansionAdd;
use crate::ExpansionSearch;
use crate::IndexFactory;
use crate::IndexId;
use crate::PrimaryKey;
use crate::index::actor::Index;
use bimap::BiMap;
use opensearch::IndexParts;
use opensearch::OpenSearch;
use opensearch::indices::IndicesCreateParts;
use serde_json::json;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::Instrument;
use tracing::debug;
use tracing::debug_span;
use tracing::error;
use tracing::info;

pub struct OpenSearchIndexFactory {
    client: Arc<OpenSearch>,
}

impl OpenSearchIndexFactory {
    fn create_opensearch_client(addr: &str) -> Result<OpenSearch, anyhow::Error> {
        use opensearch::http::Url;
        use opensearch::http::transport::SingleNodeConnectionPool;
        use opensearch::http::transport::TransportBuilder;

        let address = Url::parse(addr)?;
        let conn_pool = SingleNodeConnectionPool::new(address);
        let transport = TransportBuilder::new(conn_pool).disable_proxy().build()?;
        let client = OpenSearch::new(transport);
        Ok(client)
    }
}

impl IndexFactory for OpenSearchIndexFactory {
    fn create_index(
        &self,
        id: IndexId,
        dimensions: Dimensions,
        connectivity: Connectivity,
        expansion_add: ExpansionAdd,
        expansion_search: ExpansionSearch,
    ) -> anyhow::Result<mpsc::Sender<Index>> {
        new(
            id,
            dimensions,
            connectivity,
            expansion_add,
            expansion_search,
            self.client.clone(),
        )
    }
}

pub fn new_opensearch(addr: &str) -> Result<OpenSearchIndexFactory, anyhow::Error> {
    Ok(OpenSearchIndexFactory {
        client: Arc::new(OpenSearchIndexFactory::create_opensearch_client(addr)?),
    })
}

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    derive_more::From,
    derive_more::AsRef,
    derive_more::Display,
)]
/// Key for index embeddings
struct Key(u64);

pub fn new(
    id: IndexId,
    dimensions: Dimensions,
    connectivity: Connectivity,
    expansion_add: ExpansionAdd,
    expansion_search: ExpansionSearch,
    client: Arc<OpenSearch>,
) -> anyhow::Result<mpsc::Sender<Index>> {
    info!("Creating new index with id: {id}");
    // TODO: The value of channel size was taken from initial benchmarks. Needs more testing
    const CHANNEL_SIZE: usize = 100000;
    let (tx, mut rx) = mpsc::channel(CHANNEL_SIZE);

    tokio::spawn(
        {
            let id = id.clone();
            async move {
                let response = client
                    .indices()
                    .create(IndicesCreateParts::Index(&id.0))
                    .body(json!({
                        "settings": {
                            "index.knn": true
                        },
                        "mappings": {
                            "properties": {
                                "vector": {
                                    "type": "knn_vector",
                                    "dimension": dimensions.0.get(),
                                    "method": {
                                        "name": "hnsw",
                                        "parameters": {
                                            "ef_search": if expansion_search.0 > 0 {
                                                expansion_search.0
                                            } else {
                                                100
                                            },
                                            "ef_construction": if expansion_add.0 > 0 {
                                                expansion_add.0
                                            } else {
                                                100
                                            },
                                            "m": if connectivity.0 > 0 {
                                                connectivity.0
                                            } else {
                                                16
                                            },
                                        }
                                    }
                                },
                            }
                        }
                    }))
                    .send()
                    .await
                    .map_or_else(
                        |err| Err(err),
                        opensearch::http::response::Response::error_for_status_code,
                    )
                    .map_err(|err| {
                        error!("engine::new: unable to create index with id {id}: {err}");
                    });

                if response.is_err() {
                    return;
                }

                debug!("starting");

                // bimap between PrimaryKey and Key for an usearch index
                let keys = Arc::new(RwLock::new(BiMap::new()));

                // Incremental key for a usearch index
                let opensearch_key = Arc::new(AtomicU64::new(0));

                // This semaphore decides how many tasks are queued for an usearch process. It is
                // calculated as a number of threads multiply 2, to be sure that there is always a new
                // task waiting in the queue.
                let semaphore = Arc::new(Semaphore::new(rayon::current_num_threads() * 2));

                let id = Arc::new(id);

                while let Some(msg) = rx.recv().await {
                    let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
                    tokio::spawn({
                        let id = Arc::clone(&id);
                        let keys = Arc::clone(&keys);
                        let opensearch_key = Arc::clone(&opensearch_key);
                        let client = Arc::clone(&client);
                        async move {
                            process(msg, dimensions, id, keys, opensearch_key, client).await;
                            drop(permit);
                        }
                    });
                }

                debug!("finished");
            }
        }
        .instrument(debug_span!("opensearch", "{id}")),
    );

    Ok(tx)
}

async fn process(
    msg: Index,
    _dimensions: Dimensions,
    id: Arc<IndexId>,
    keys: Arc<RwLock<BiMap<PrimaryKey, Key>>>,
    opensearch_key: Arc<AtomicU64>,
    client: Arc<OpenSearch>,
) {
    // TODO: Implement the logic for processing the messages
    match msg {
        Index::AddOrReplace {
            primary_key,
            embedding,
        } => {
            add(id, keys, opensearch_key, primary_key, embedding, client).await;
        }
        Index::Remove { primary_key } => {
            let _ = primary_key;
        }
        Index::Ann {
            embedding,
            limit,
            tx,
        } => {
            let _ = embedding;
            let _ = limit;
            let _ = tx;
        }
        Index::Count { tx } => {
            let _ = tx;
        }
    }
}

async fn add(
    id: Arc<IndexId>,
    keys: Arc<RwLock<BiMap<PrimaryKey, Key>>>,
    opensearch_key: Arc<AtomicU64>,
    primary_key: PrimaryKey,
    embeddings: Embedding,
    client: Arc<OpenSearch>,
) {
    let key = opensearch_key.fetch_add(1, Ordering::Relaxed).into();

    if keys
        .write()
        .unwrap()
        .insert_no_overwrite(primary_key.clone(), key)
        .is_err()
    {
        debug!("add: primary_key already exists: {primary_key:?}");
        return;
    }

    let (tx, rx) = oneshot::channel();

    tokio::spawn({
        async move {
            let response = client
                .index(IndexParts::IndexId(&id.0, &key.0.to_string()))
                .body(json!({
                    "vector": embeddings.0,
                }))
                .send()
                .await
                .map_or_else(
                    |err| Err(err),
                    opensearch::http::response::Response::error_for_status_code,
                )
                .map_err(|err| {
                    error!(
                        "add: unable to add embeddings for key {key} to index with id {id}: {err}"
                    );
                });

            if response.is_err() {
                _ = tx.send(false);
                return;
            }

            _ = tx.send(true);
        }
    });

    if let Ok(false) = rx.await {
        keys.write().unwrap().remove_by_right(&key);
    }
}
