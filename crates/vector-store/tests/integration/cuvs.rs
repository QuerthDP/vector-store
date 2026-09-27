/*
 * Copyright 2026-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.1
 */

use crate::db_basic;
use crate::db_basic::ScanFn;
use crate::vs_index::cuvs_test_config;
use crate::vs_index::setup_store;
use crate::vs_index::setup_store_and_wait_for_index;
use crate::wait_for_value;
use futures::FutureExt;
use httpapi::IndexStatus;
use scylla::cluster::metadata::NativeType;
use scylla::value::CqlValue;
use std::time::Duration;
use vector_store::DbIndexPartitioning;
use vector_store::Timestamp;

#[tokio::test]
async fn full_scan_is_serving_only_once_its_rows_are_built() {
    crate::enable_tracing();

    // The last row comes after a build, so it waits for the next one.
    let first = db_basic::scan_fn_vectors([
        (
            [CqlValue::Int(1)].into(),
            Some(vec![1., 1., 1.].into()),
            [].into(),
            Timestamp::from_millis(10),
        ),
        (
            [CqlValue::Int(2)].into(),
            Some(vec![2., -2., 2.].into()),
            [].into(),
            Timestamp::from_millis(10),
        ),
        (
            [CqlValue::Int(3)].into(),
            Some(vec![3., 3., 3.].into()),
            [].into(),
            Timestamp::from_millis(10),
        ),
    ]);
    let last = db_basic::scan_fn_vectors([(
        [CqlValue::Int(4)].into(),
        Some(vec![4., 4., 4.].into()),
        [].into(),
        Timestamp::from_millis(10),
    )]);
    let fullscan_fn: ScanFn = Box::new(|tx| {
        async move {
            first(tx.clone()).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            last(tx).await;
        }
        .boxed()
    });
    let (run, index, _db, _node_state) = setup_store(
        cuvs_test_config(),
        DbIndexPartitioning::Global,
        ["pk".into()],
        1,
        [("pk".to_string().into(), NativeType::Int)],
        Some(fullscan_fn),
        None,
    )
    .await;
    let (client, _server, _config_tx) = run.await;

    let keyspace_name = index.keyspace_name.clone().into();
    let index_name = index.index_name.clone().into();
    let status = wait_for_value(
        || async {
            client
                .index_status(&keyspace_name, &index_name)
                .await
                .ok()
                .filter(|status| status.status == IndexStatus::Serving)
        },
        "Waiting for index to be serving",
    )
    .await;
    assert_eq!(status.count, 4);
}

#[tokio::test]
async fn a_non_finite_vector_does_not_stall_the_full_scan() {
    crate::enable_tracing();

    setup_store_and_wait_for_index(
        cuvs_test_config(),
        DbIndexPartitioning::Global,
        ["pk".into()],
        1,
        [("pk".to_string().into(), NativeType::Int)],
        Some(db_basic::scan_fn_vectors([
            (
                [CqlValue::Int(1)].into(),
                Some(vec![1., 1., 1.].into()),
                [].into(),
                Timestamp::from_millis(10),
            ),
            (
                [CqlValue::Int(2)].into(),
                Some(vec![2., -2., 2.].into()),
                [].into(),
                Timestamp::from_millis(10),
            ),
            (
                [CqlValue::Int(3)].into(),
                Some(vec![3., 3., 3.].into()),
                [].into(),
                Timestamp::from_millis(10),
            ),
            (
                [CqlValue::Int(4)].into(),
                Some(vec![f32::NAN; 3].into()),
                [].into(),
                Timestamp::from_millis(10),
            ),
        ])),
        None,
        Some(3),
    )
    .await;
}

#[tokio::test]
async fn a_non_finite_update_is_served_only_once_the_old_row_is_built_out() {
    crate::enable_tracing();

    // The update comes after a build, so the old row waits for the next one.
    let first = db_basic::scan_fn_vectors([
        (
            [CqlValue::Int(1)].into(),
            Some(vec![1., 1., 1.].into()),
            [].into(),
            Timestamp::from_millis(10),
        ),
        (
            [CqlValue::Int(2)].into(),
            Some(vec![2., -2., 2.].into()),
            [].into(),
            Timestamp::from_millis(10),
        ),
        (
            [CqlValue::Int(3)].into(),
            Some(vec![3., 3., 3.].into()),
            [].into(),
            Timestamp::from_millis(10),
        ),
    ]);
    let update = db_basic::scan_fn_vectors([(
        [CqlValue::Int(1)].into(),
        Some(vec![f32::NAN; 3].into()),
        [].into(),
        Timestamp::from_millis(20),
    )]);
    let fullscan_fn: ScanFn = Box::new(|tx| {
        async move {
            first(tx.clone()).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            update(tx).await;
        }
        .boxed()
    });
    let (run, index, _db, _node_state) = setup_store(
        cuvs_test_config(),
        DbIndexPartitioning::Global,
        ["pk".into()],
        1,
        [("pk".to_string().into(), NativeType::Int)],
        Some(fullscan_fn),
        None,
    )
    .await;
    let (client, _server, _config_tx) = run.await;

    let keyspace_name = index.keyspace_name.clone().into();
    let index_name = index.index_name.clone().into();
    let status = wait_for_value(
        || async {
            client
                .index_status(&keyspace_name, &index_name)
                .await
                .ok()
                .filter(|status| status.status == IndexStatus::Serving)
        },
        "Waiting for index to be serving",
    )
    .await;
    assert_eq!(status.count, 2);
}
