/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: Apache-2.0
 */

mod actor;
mod engine;
mod httproutes;
mod httpserver;
mod index;
mod modify_indexes;
mod monitor_indexes;
mod monitor_items;
mod supervisor;

use {
    crate::{actor::ActorStop, supervisor::SupervisorExt},
    anyhow::anyhow,
    scylla::{
        frame::response::result::ColumnType,
        serialize::{
            value::SerializeValue,
            writers::{CellWriter, WrittenCellProof},
            SerializationError,
        },
    },
    std::net::{SocketAddr, ToSocketAddrs},
    tokio::signal,
    tracing_subscriber::{fmt, prelude::*, EnvFilter},
};

#[derive(Clone, derive_more::From, derive_more::Display)]
pub(crate) struct ScyllaDbUri(String);

#[derive(
    Copy, Clone, Hash, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize, derive_more::From,
)]
struct IndexId(i32);

impl SerializeValue for IndexId {
    fn serialize<'b>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        use {
            scylla::serialize::value::{
                BuiltinSerializationError, BuiltinSerializationErrorKind, BuiltinTypeCheckError,
                BuiltinTypeCheckErrorKind,
            },
            std::any,
        };

        match typ {
            ColumnType::Int => writer
                .set_value(self.0.to_be_bytes().as_slice())
                .map_err(|_| {
                    SerializationError::new(BuiltinSerializationError {
                        rust_name: any::type_name::<Self>(),
                        got: typ.clone().into_owned(),
                        kind: BuiltinSerializationErrorKind::ValueOverflow,
                    })
                }),
            _ => Err(SerializationError::new(BuiltinTypeCheckError {
                rust_name: any::type_name::<Self>(),
                got: typ.clone().into_owned(),
                kind: BuiltinTypeCheckErrorKind::MismatchedType {
                    expected: &[ColumnType::Int],
                },
            })),
        }
    }
}

#[derive(Clone, derive_more::From, serde::Serialize, serde::Deserialize, derive_more::Display)]
struct TableName(String);

#[derive(Clone, derive_more::From, serde::Serialize, serde::Deserialize, derive_more::Display)]
struct ColumnName(String);

#[derive(
    Copy, Clone, serde::Serialize, serde::Deserialize, derive_more::From, derive_more::Display,
)]
struct Key(u64);

impl SerializeValue for Key {
    fn serialize<'b>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        use {
            scylla::serialize::value::{
                BuiltinSerializationError, BuiltinSerializationErrorKind, BuiltinTypeCheckError,
                BuiltinTypeCheckErrorKind,
            },
            std::any,
        };

        match typ {
            ColumnType::BigInt => writer
                .set_value(self.0.to_be_bytes().as_slice())
                .map_err(|_| {
                    SerializationError::new(BuiltinSerializationError {
                        rust_name: any::type_name::<Self>(),
                        got: typ.clone().into_owned(),
                        kind: BuiltinSerializationErrorKind::ValueOverflow,
                    })
                }),
            _ => Err(SerializationError::new(BuiltinTypeCheckError {
                rust_name: any::type_name::<Self>(),
                got: typ.clone().into_owned(),
                kind: BuiltinTypeCheckErrorKind::MismatchedType {
                    expected: &[ColumnType::BigInt],
                },
            })),
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize, derive_more::From)]
struct Distance(f32);

#[derive(
    Copy, Clone, serde::Serialize, serde::Deserialize, derive_more::From, derive_more::Display,
)]
struct IndexItemsCount(u32);

impl SerializeValue for IndexItemsCount {
    fn serialize<'b>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        use {
            scylla::serialize::value::{
                BuiltinSerializationError, BuiltinSerializationErrorKind, BuiltinTypeCheckError,
                BuiltinTypeCheckErrorKind,
            },
            std::any,
        };

        match typ {
            ColumnType::Int => writer
                .set_value(self.0.to_be_bytes().as_slice())
                .map_err(|_| {
                    SerializationError::new(BuiltinSerializationError {
                        rust_name: any::type_name::<Self>(),
                        got: typ.clone().into_owned(),
                        kind: BuiltinSerializationErrorKind::ValueOverflow,
                    })
                }),
            _ => Err(SerializationError::new(BuiltinTypeCheckError {
                rust_name: any::type_name::<Self>(),
                got: typ.clone().into_owned(),
                kind: BuiltinTypeCheckErrorKind::MismatchedType {
                    expected: &[ColumnType::Int],
                },
            })),
        }
    }
}

#[derive(
    Copy, Clone, serde::Serialize, serde::Deserialize, derive_more::From, derive_more::Display,
)]
struct Dimensions(usize);

#[derive(
    Copy, Clone, serde::Serialize, serde::Deserialize, derive_more::From, derive_more::Display,
)]
struct Connectivity(usize);

#[derive(
    Copy, Clone, serde::Serialize, serde::Deserialize, derive_more::From, derive_more::Display,
)]
struct ExpansionAdd(usize);

#[derive(
    Copy, Clone, serde::Serialize, serde::Deserialize, derive_more::From, derive_more::Display,
)]
struct ParamM(usize);

#[derive(Clone, serde::Serialize, serde::Deserialize, derive_more::From)]
struct Embeddings(Vec<f32>);

#[derive(Clone, serde::Serialize, serde::Deserialize, derive_more::Display)]
struct Limit(usize);

#[derive(derive_more::From)]
struct HttpServerAddr(SocketAddr);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    _ = dotenvy::dotenv();
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?)
        .with(fmt::layer().with_target(false))
        .init();
    let scylla_usearch_addr = dotenvy::var("SCYLLA_USEARCH_URI")
        .unwrap_or("127.0.0.1:6080".to_string())
        .to_socket_addrs()?
        .next()
        .ok_or(anyhow!(
            "Unable to parse SCYLLA_USEARCH_URI env (host:port)"
        ))?
        .into();
    let scylladb_uri = dotenvy::var("SCYLLADB_URI")
        .unwrap_or("127.0.0.1:9042".to_string())
        .into();
    let (supervisor_actor, supervisor_handle) = supervisor::new();
    let (engine_actor, engine_task) = engine::new(scylladb_uri, supervisor_actor.clone()).await?;
    supervisor_actor
        .attach(engine_actor.clone(), engine_task)
        .await;
    let (server_actor, server_task) = httpserver::new(scylla_usearch_addr, engine_actor).await?;
    supervisor_actor.attach(server_actor, server_task).await;
    wait_for_shutdown().await;
    supervisor_actor.actor_stop().await;
    supervisor_handle.await?;
    Ok(())
}

async fn wait_for_shutdown() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
