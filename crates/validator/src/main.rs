/*
 * Copyright 2025-present ScyllaDB
 * SPDX-License-Identifier: LicenseRef-ScyllaDB-Source-Available-1.0
 */

mod dns;
mod tests;

use clap::Parser;
use dns::DnsExt;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tests::TestActors;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

#[derive(Debug, Parser)]
#[clap(version)]
struct Args {
    #[arg(short, long, default_value = "127.0.1.1")]
    dns_ip: Ipv4Addr,

    #[arg(short, long, default_value = "127.0.2.1")]
    base_ip: Ipv4Addr,
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

    info!(
        "{} version: {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    info!("dns version: {}", dns.version().await);

    let test_cases = tests::register().await;

    assert!(
        tests::run(
            TestActors {
                services_subnet,
                dns,
            },
            test_cases,
            Arc::new(HashMap::new())
        )
        .await
    );
}
