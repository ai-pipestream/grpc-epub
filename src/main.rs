// SPDX-License-Identifier: Apache-2.0

//! Binary entry point for the EPUB gRPC server.
//!
//! Serves `ai.pipestream.epub.v1.EpubParseService`, the standard
//! `grpc.health.v1.Health` service, and v1 server reflection until SIGINT or
//! SIGTERM, then drains open streams rather than cutting them mid-book.
//!
//! Every setting is an optional environment override; see the README for the
//! table. Sizing knobs live here, safety limits live in
//! [`grpc_epub::limits`].

use std::sync::Arc;
use std::time::Duration;

use tonic::transport::Server;

use grpc_epub::limits::Limits;
use grpc_epub::metrics::{self, Metrics};
use grpc_epub::proto::{self, v1 as pb};
use grpc_epub::service::EpubGrpc;

/// Default listen address when `GRPC_EPUB_ADDR` is not set.
const DEFAULT_ADDR: &str = "0.0.0.0:50051";

/// Default HTTP/2 initial window, for both the stream and the connection.
///
/// hyper defaults to 1 MiB. An EPUB upload is a bulk transfer and a chapter
/// event can be large, so this is sized to keep both off the
/// one-window-per-round-trip floor.
const DEFAULT_WINDOW_BYTES: u32 = 4 * 1024 * 1024;

/// Default seconds between metrics lines. Zero disables them.
const DEFAULT_METRICS_INTERVAL_SECS: u64 = 60;

/// Read a `usize` environment variable, falling back to `default`.
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workers = env_usize(
        "GRPC_EPUB_WORKERS",
        std::thread::available_parallelism().map_or(4, usize::from),
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(workers)
        .build()?;

    runtime.block_on(serve())
}

/// Bind the listener and serve until a shutdown signal arrives.
async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("GRPC_EPUB_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_owned())
        .parse()?;
    let limits = Limits::from_env();

    let counters: Arc<Metrics> = Metrics::new();
    metrics::spawn_reporter(
        &counters,
        Duration::from_secs(
            u64::try_from(env_usize(
                "GRPC_EPUB_METRICS_INTERVAL_SECS",
                usize::try_from(DEFAULT_METRICS_INTERVAL_SECS).unwrap_or(60),
            ))
            .unwrap_or(DEFAULT_METRICS_INTERVAL_SECS),
        ),
    );

    let epub = EpubGrpc::with_metrics(limits, Arc::clone(&counters)).into_service();

    // Health, so an orchestrator can tell "listening" from "ready". This
    // server has no dependency to warm up, so it is serving from the moment it
    // binds; the service is registered because the fleet contract requires a
    // uniform probe, not because the answer is ever interesting.
    let (health, health_service) = tonic_health::server::health_reporter();
    health
        .set_serving::<pb::epub_parse_service_server::EpubParseServiceServer<EpubGrpc>>()
        .await;

    // Reflection, so grpcurl and friends can discover the contract from a live
    // server instead of shipping the .proto files around.
    //
    // The health descriptor is registered alongside the parse service on
    // purpose. Registering the health *service* is not enough for a reflective
    // client: grpcurl asks reflection what exists, and without this it answers
    // "no such service grpc.health.v1.Health" for a probe the server would
    // have answered perfectly well.
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    let window = u32::try_from(env_usize(
        "GRPC_EPUB_WINDOW_BYTES",
        DEFAULT_WINDOW_BYTES as usize,
    ))
    .unwrap_or(DEFAULT_WINDOW_BYTES);

    eprintln!(
        "grpc-epub listening on {addr} (http2 window {window} bytes, \
         max upload {} MiB, max inflated {} MiB, max entries {})",
        limits.max_document_bytes / grpc_epub::limits::MIB,
        limits.max_uncompressed_bytes / grpc_epub::limits::MIB,
        limits.max_entries,
    );

    Server::builder()
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .initial_stream_window_size(window)
        .initial_connection_window_size(window)
        .max_concurrent_streams(1024)
        .add_service(health_service)
        .add_service(reflection)
        .add_service(epub)
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    println!("{}", counters.snapshot());
    eprintln!("grpc-epub shut down");
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM, so open
/// streams can drain instead of being cut mid-book.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}
