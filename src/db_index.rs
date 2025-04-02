/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

use crate::ColumnName;
use crate::Embeddings;
use crate::IndexMetadata;
use crate::Key;
use crate::TableName;
use anyhow::Context;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use scylla::client::session::Session;
use scylla::errors::NextRowError;
use scylla::statement::prepared::PreparedStatement;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::Instrument;
use tracing::debug_span;
use tracing::warn;

pub(crate) enum DbIndex {
    GetProcessedIds {
        tx: oneshot::Sender<anyhow::Result<BoxStream<'static, Result<Key, NextRowError>>>>,
    },

    GetItems {
        #[allow(clippy::type_complexity)]
        tx: oneshot::Sender<
            anyhow::Result<BoxStream<'static, Result<(Key, Embeddings), NextRowError>>>,
        >,
    },
}

pub(crate) trait DbIndexExt {
    async fn get_processed_ids(
        &self,
    ) -> anyhow::Result<BoxStream<'static, Result<Key, NextRowError>>>;

    async fn get_items(
        &self,
    ) -> anyhow::Result<BoxStream<'static, Result<(Key, Embeddings), NextRowError>>>;
}

impl DbIndexExt for mpsc::Sender<DbIndex> {
    async fn get_processed_ids(
        &self,
    ) -> anyhow::Result<BoxStream<'static, Result<Key, NextRowError>>> {
        let (tx, rx) = oneshot::channel();
        self.send(DbIndex::GetProcessedIds { tx }).await?;
        rx.await?
    }

    async fn get_items(
        &self,
    ) -> anyhow::Result<BoxStream<'static, Result<(Key, Embeddings), NextRowError>>> {
        let (tx, rx) = oneshot::channel();
        self.send(DbIndex::GetItems { tx }).await?;
        rx.await?
    }
}

pub(crate) async fn new(
    db_session: Arc<Session>,
    metadata: IndexMetadata,
) -> anyhow::Result<mpsc::Sender<DbIndex>> {
    let statements = Arc::new(Statements::new(db_session, metadata).await?);
    let (tx, mut rx) = mpsc::channel(10);
    tokio::spawn(
        async move {
            while let Some(msg) = rx.recv().await {
                tokio::spawn(process(Arc::clone(&statements), msg));
            }
        }
        .instrument(debug_span!("db_index")),
    );
    Ok(tx)
}

async fn process(statements: Arc<Statements>, msg: DbIndex) {
    match msg {
        DbIndex::GetProcessedIds { tx } => tx
            .send(statements.get_processed_ids().await)
            .unwrap_or_else(|_| {
                warn!("db_index::process: Db::GetProcessedIds: unable to send response")
            }),

        DbIndex::GetItems { tx } => tx
            .send(statements.get_items().await)
            .unwrap_or_else(|_| warn!("db_index::process: Db::GetItems: unable to send response")),
    }
}

struct Statements {
    session: Arc<Session>,
    st_get_processed_ids: PreparedStatement,
    st_get_items: PreparedStatement,
}

impl Statements {
    async fn new(session: Arc<Session>, metadata: IndexMetadata) -> anyhow::Result<Self> {
        Ok(Self {
            st_get_processed_ids: session
                .prepare(Self::get_processed_ids_query(
                    &metadata.table_name,
                    &metadata.key_name,
                ))
                .await
                .context("get_processed_ids_query")?,

            st_get_items: session
                .prepare(Self::get_items_query(
                    &metadata.table_name,
                    &metadata.key_name,
                    &metadata.target_name,
                ))
                .await
                .context("get_items_query")?,

            session,
        })
    }

    fn get_processed_ids_query(table: &TableName, col_id: &ColumnName) -> String {
        format!(
            "
            SELECT {col_id}
            FROM {table}
            WHERE processed = TRUE
            LIMIT 1000
            "
        )
    }

    async fn get_processed_ids(
        &self,
    ) -> anyhow::Result<BoxStream<'static, Result<Key, NextRowError>>> {
        Ok(self
            .session
            .execute_iter(self.st_get_processed_ids.clone(), ())
            .await?
            .rows_stream::<(i64,)>()?
            .map_ok(|(key,)| (key as u64).into())
            .boxed())
    }

    fn get_items_query(table: &TableName, col_id: &ColumnName, col_emb: &ColumnName) -> String {
        format!(
            "
            SELECT {col_id}, {col_emb}
            FROM {table}
            WHERE processed = FALSE
            "
        )
    }

    async fn get_items(
        &self,
    ) -> anyhow::Result<BoxStream<'static, Result<(Key, Embeddings), NextRowError>>> {
        Ok(self
            .session
            .execute_iter(self.st_get_items.clone(), ())
            .await?
            .rows_stream::<(i64, Vec<f32>)>()?
            .map_ok(|(key, embeddings)| ((key as u64).into(), embeddings.into()))
            .boxed())
    }
}
