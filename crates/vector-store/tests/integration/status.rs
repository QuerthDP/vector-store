/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

use crate::usearch::{setup_store, setup_store_and_wait_for_index};
use crate::wait_for;
use vector_store::httproutes::Status;

#[tokio::test]
async fn status_is_serving_after_creation() {
    crate::enable_tracing();
    let (_index, client, _db, _server, _node_state) = setup_store_and_wait_for_index().await;

    let result = client.status().await;
    assert_eq!(result.unwrap(), Status::Serving);
}

#[tokio::test]
async fn status_is_bootstrapping_while_discovering_indexes() {
    crate::enable_tracing();
    let (run, _index, db, _node_state) = setup_store().await;
    db.simulate_endless_get_indexes_processing();
    let (client, _server) = run.await;

    // assert that status is Bootstrapping while indexes are being discovered
    wait_for(
        || async { client.status().await.unwrap() == Status::Bootstrapping },
        "Waiting for status to be Bootstrapping",
    )
    .await;
}
