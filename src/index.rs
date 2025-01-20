/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: Apache-2.0
 */

use {
    crate::{
        actor::{ActorHandle, MessageStop},
        modify_indexes::{ModifyIndexes, ModifyIndexesExt},
        Connectivity, Dimensions, Distance, Embeddings, ExpansionAdd, IndexId, IndexItemsCount,
        Key, Limit,
    },
    anyhow::anyhow,
    tokio::sync::{mpsc, oneshot},
    tracing::warn,
    usearch::IndexOptions,
};

pub(crate) enum Index {
    Add {
        key: Key,
        embeddings: Embeddings,
    },
    Ann {
        embeddings: Embeddings,
        limit: Limit,
        tx: oneshot::Sender<anyhow::Result<(Vec<Key>, Vec<Distance>)>>,
    },
    Stop,
}

impl MessageStop for Index {
    fn message_stop() -> Self {
        Index::Stop
    }
}

pub(crate) trait IndexExt {
    async fn add(&self, key: Key, embeddings: Embeddings);
    async fn ann(
        &self,
        embeddings: Embeddings,
        limit: Limit,
    ) -> anyhow::Result<(Vec<Key>, Vec<Distance>)>;
}

impl IndexExt for mpsc::Sender<Index> {
    async fn add(&self, key: Key, embeddings: Embeddings) {
        self.send(Index::Add { key, embeddings })
            .await
            .unwrap_or_else(|err| warn!("IndexExt::add: unable to send request: {err}"));
    }

    async fn ann(
        &self,
        embeddings: Embeddings,
        limit: Limit,
    ) -> anyhow::Result<(Vec<Key>, Vec<Distance>)> {
        let (tx, rx) = oneshot::channel();
        self.send(Index::Ann {
            embeddings,
            limit,
            tx,
        })
        .await?;
        rx.await?
    }
}

pub(crate) fn new(
    id: IndexId,
    modify_actor: mpsc::Sender<ModifyIndexes>,
    dimensions: Dimensions,
    connectivity: Connectivity,
    expansion_add: ExpansionAdd,
) -> anyhow::Result<(mpsc::Sender<Index>, ActorHandle)> {
    let options = IndexOptions {
        dimensions: dimensions.0,
        connectivity: connectivity.0,
        expansion_add: expansion_add.0,
        ..Default::default()
    };
    let idx = usearch::Index::new(&options)?;
    idx.reserve(1000)?;
    let (tx, mut rx) = mpsc::channel(10);
    let task = tokio::spawn(async move {
        let mut items_count = IndexItemsCount(0);
        modify_actor.update_items_count(id, items_count).await;
        while let Some(msg) = rx.recv().await {
            match msg {
                Index::Add { key, embeddings } => {
                    idx.add(key.0, &embeddings.0).unwrap_or_else(|err| {
                        warn!(
                            "index::new: Index::add: unable to add embeddings for key {key}: {err}"
                        )
                    });
                    items_count.0 += 1;
                    modify_actor.update_items_count(id, items_count).await;
                }
                Index::Ann {
                    embeddings,
                    limit,
                    tx,
                } => {
                    tx.send(if embeddings.0.len() != dimensions.0 {
                        Err(anyhow!(
                            "index ann query: wrong embeddings dimensions: {} != {dimensions}",
                            embeddings.0.len()
                        ))
                    } else if limit.0 < 1 {
                        Err(anyhow!("index ann query: wrong limit value {limit}"))
                    } else if let Ok(results) = idx.search(&embeddings.0, limit.0) {
                        Ok((
                            results.keys.into_iter().map(|key| key.into()).collect(),
                            results
                                .distances
                                .into_iter()
                                .map(|value| value.into())
                                .collect(),
                        ))
                    } else {
                        Err(anyhow!("index ann query: search failed"))
                    })
                    .unwrap_or_else(|_| warn!("index::new: Index::Ann: unable to send response"));
                }
                Index::Stop => {
                    rx.close();
                }
            }
        }
    });
    Ok((tx, task))
}
