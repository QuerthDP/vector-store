/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

mod common;
mod dns;
mod scylla_cluster;
mod tests;
mod vector_store_cluster;

use clap::Parser;
use dns::DnsExt;
use scylla_cluster::ScyllaClusterExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tests::TestActors;
use tests::TestCase;
use tokio::fs;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use vector_store_cluster::VectorStoreClusterExt;

#[derive(Debug, Parser)]
#[clap(version)]
struct Args {
    #[arg(short, long, default_value = "127.0.1.1")]
    dns_ip: Ipv4Addr,

    #[arg(short, long, default_value = "127.0.2.1")]
    base_ip: Ipv4Addr,

    #[arg(short, long, default_value = "conf/scylla.yaml")]
    scylla_default_conf: PathBuf,

    #[arg(short, long, default_value = "false")]
    verbose: bool,

    scylla: PathBuf,

    vector_store: PathBuf,

    /// Test filters - can be test file names (like 'crud') or specific test names (like 'simple_create_drop_index').
    /// Supports partial matching - 'create' will match tests containing 'create' in their name.
    #[arg(value_name = "FILTER")]
    filters: Vec<String>,
}

async fn file_exists(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    metadata.is_file()
}

async fn executable_exists(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0)
}

/// Represents a subnet for services, derived from a base IP address.
struct ServicesSubnet([u8; 3]);

impl ServicesSubnet {
    fn new(ip: Ipv4Addr) -> Self {
        assert!(
            ip.is_loopback(),
            "Base IP for services must be a loopback address"
        );

        let octets = ip.octets();
        assert!(
            octets[3] == 1,
            "Base IP for services must have the last octet set to 1"
        );

        Self([octets[0], octets[1], octets[2]])
    }

    /// Returns an IP address in the subnet with the specified last octet.
    fn ip(&self, octet: u8) -> Ipv4Addr {
        [self.0[0], self.0[1], self.0[2], octet].into()
    }
}

fn validate_different_subnet(dns_ip: Ipv4Addr, base_ip: Ipv4Addr) {
    let dns_octets = dns_ip.octets();
    let base_octets = base_ip.octets();
    assert!(
        dns_octets[1] != base_octets[1] || dns_octets[2] != base_octets[2],
        "DNS server should serve addresses from a different subnet than its own"
    );
}

/// Parse command line filters into the expected filter format for test execution.
/// Returns a HashMap where:
/// - Key: test file name (e.g., "crud", "full_scan")
/// - Value: HashSet of specific test names within that file (empty means run all tests in file)
fn parse_test_filters(
    filters: &[String],
    test_cases: &[(String, TestCase)],
) -> HashMap<String, HashSet<String>> {
    if filters.is_empty() {
        return HashMap::new(); // Run all tests
    }

    let mut filter_map = HashMap::new();

    for filter in filters {
        let mut found_exact_file = false;

        // First, check if the filter matches a test file name exactly
        for (file_name, _) in test_cases {
            if file_name == filter {
                filter_map.insert(file_name.clone(), HashSet::new());
                found_exact_file = true;
                break;
            }
        }

        if !found_exact_file {
            // If no exact file match, search for specific tests containing the filter
            for (file_name, test_case) in test_cases {
                let mut matching_tests = HashSet::new();

                // Check each test in the test case
                for (test_name, _, _) in test_case.tests() {
                    if test_name.contains(filter) {
                        matching_tests.insert(test_name.clone());
                    }
                }

                if !matching_tests.is_empty() {
                    // If this file already has some tests selected, merge them
                    if let Some(existing_tests) = filter_map.get_mut(file_name) {
                        if existing_tests.is_empty() {
                            // If existing is empty (run all), keep it empty
                            continue;
                        }
                        existing_tests.extend(matching_tests);
                    } else {
                        filter_map.insert(file_name.clone(), matching_tests);
                    }
                }
            }
        }
    }

    filter_map
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new("info"))
                .expect("Failed to create EnvFilter"),
        )
        .with(fmt::layer().with_target(false))
        .init();

    let args = Args::parse();

    validate_different_subnet(args.dns_ip, args.base_ip);

    let services_subnet = Arc::new(ServicesSubnet::new(args.base_ip));
    let dns = dns::new(args.dns_ip).await;
    let db = scylla_cluster::new(args.scylla, args.scylla_default_conf, args.verbose).await;
    let vs = vector_store_cluster::new(args.vector_store, args.verbose).await;

    info!(
        "{} version: {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    info!("dns version: {}", dns.version().await);
    info!("scylla version: {}", db.version().await);
    info!("vector-store version: {}", vs.version().await);

    let test_cases = tests::register().await;

    // Parse command line filters 
    let filter_map = parse_test_filters(&args.filters, &test_cases);

    assert!(
        tests::run(
            TestActors {
                services_subnet,
                dns,
                db,
                vs,
            },
            test_cases,
            Arc::new(filter_map)
        )
        .await
    );
}
