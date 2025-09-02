/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

use crate::dns::DnsExt;
use crate::scylla_cluster::ScyllaClusterExt;
use crate::tests::*;
use crate::vector_store_cluster::VectorStoreClusterExt;
use httpclient::HttpClient;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::sync::Arc;
use std::time::Duration;
use tokio::task;
use tokio::time;
use tracing::info;

const VS_NAME: &str = "vs";

pub(crate) const VS_PORT: u16 = 6080;
pub(crate) const DB_PORT: u16 = 9042;

pub(crate) const VS_OCTET: u8 = 1;
pub(crate) const DB_OCTET: u8 = 2;

pub(crate) async fn init(actors: TestActors) {
    info!("started");

    let vs_ip = actors.services_subnet.ip(VS_OCTET);

    actors.dns.upsert(VS_NAME.to_string(), vs_ip).await;

    let vs_url = format!(
        "http://{}.{}:{}",
        VS_NAME,
        actors.dns.domain().await,
        VS_PORT
    );

    let db_ip = actors.services_subnet.ip(DB_OCTET);

    actors.db.start(vs_url, db_ip, None).await;
    assert!(actors.db.wait_for_ready().await);

    actors
        .vs
        .start((vs_ip, VS_PORT).into(), (db_ip, DB_PORT).into())
        .await;
    assert!(actors.vs.wait_for_ready().await);

    info!("finished");
}

pub(crate) async fn cleanup(actors: TestActors) {
    info!("started");
    actors.dns.remove(VS_NAME.to_string()).await;
    actors.vs.stop().await;
    actors.db.stop().await;
    info!("finished");
}

pub(crate) async fn prepare_connection(actors: TestActors) -> (Arc<Session>, HttpClient) {
    let session = Arc::new(
        SessionBuilder::new()
            .known_node(actors.services_subnet.ip(DB_OCTET).to_string())
            .build()
            .await
            .expect("failed to create session"),
    );
    let client = HttpClient::new((actors.services_subnet.ip(VS_OCTET), VS_PORT).into());
    (session, client)
}

pub(crate) async fn wait_for<F, Fut>(mut condition: F, msg: &str, timeout: Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    time::timeout(timeout, async {
        while !condition().await {
            task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Timeout on: {msg}"))
}
