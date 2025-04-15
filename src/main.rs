/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

use anyhow::anyhow;
use std::net::ToSocketAddrs;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

// Index creating/querying is CPU bound task, so that vector-store uses rayon ThreadPool for them.
// From the start there was no need (network traffic seems to be not so high) to support more than
// one thread per network IO bound tasks.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    _ = dotenvy::dotenv();
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?)
        .with(fmt::layer().with_target(false))
        .init();

    #[cfg(not(feature = "opensearch"))]
    let search_tool_addr = dotenvy::var("SCYLLA_USEARCH_URI")
        .unwrap_or("127.0.0.1:6080".to_string())
        .to_socket_addrs()?
        .next()
        .ok_or(anyhow!(
            "Unable to parse SCYLLA_USEARCH_URI env (host:port)"
        ))?
        .into();

    #[cfg(feature = "opensearch")]
    let search_tool_addr = {
        let addr = dotenvy::var("OPENSEARCH_ADDRESS").unwrap_or("127.0.0.1".to_string());
        let port = dotenvy::var("OPENSEARCH_PORT").unwrap_or("9200".to_string());
        format!("{addr}:{port}")
            .to_socket_addrs()?
            .next()
            .ok_or(anyhow!(
                "Unable to parse opensearch URI"
            ))?
            .into()
    };

    let scylladb_uri = dotenvy::var("SCYLLADB_URI")
        .unwrap_or("127.0.0.1:9042".to_string())
        .into();

    let background_threads = dotenvy::var("SCYLLA_USEARCH_BACKGROUND_THREADS")
        .ok()
        .and_then(|v| v.parse().ok());

    let db_actor = vector_store::new_db(scylladb_uri).await?;
    let (_server_actor, addr) =
        vector_store::run(search_tool_addr, background_threads, db_actor).await?;
    tracing::info!("listening on {addr}");
    vector_store::wait_for_shutdown().await;

    Ok(())
}
