// SPDX-License-Identifier: Apache-2.0

//! The two standard services the fleet contract requires, asserted over a live
//! socket rather than by reading `main.rs`.
//!
//! Health and reflection are the kind of wiring that is easy to add, easy to
//! drop in a refactor, and invisible until an orchestrator marks the pod
//! unready or `grpcurl` cannot describe the contract.

use std::net::SocketAddr;

use grpc_epub::proto::v1::epub_parse_service_server::EpubParseServiceServer;
use grpc_epub::service::EpubGrpc;
use grpc_epub::{Limits, proto};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Endpoint, Server};

/// Start a server carrying the same service set as the binary.
async fn start_full_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local address");

    let (health, health_service) = tonic_health::server::health_reporter();
    health
        .set_serving::<EpubParseServiceServer<EpubGrpc>>()
        .await;
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1()
        .expect("reflection");

    tokio::spawn(async move {
        Server::builder()
            .add_service(health_service)
            .add_service(reflection)
            .add_service(EpubGrpc::new(Limits::default()).into_service())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("server failed");
    });
    addr
}

/// Connect a channel to the test server.
async fn channel(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

#[tokio::test]
async fn health_reports_the_parse_service_as_serving() {
    let addr = start_full_server().await;
    let mut client = tonic_health::pb::health_client::HealthClient::new(channel(addr).await);

    let response = client
        .check(tonic_health::pb::HealthCheckRequest {
            service: "ai.pipestream.epub.v1.EpubParseService".to_owned(),
        })
        .await
        .expect("health check")
        .into_inner();
    assert_eq!(
        response.status,
        tonic_health::pb::health_check_response::ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn reflection_lists_the_parse_service() {
    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::{
        ServerReflectionRequest, server_reflection_request::MessageRequest,
        server_reflection_response::MessageResponse,
    };

    let addr = start_full_server().await;
    let mut client = ServerReflectionClient::new(channel(addr).await);

    let mut stream = client
        .server_reflection_info(tokio_stream::iter(vec![ServerReflectionRequest {
            host: String::new(),
            message_request: Some(MessageRequest::ListServices(String::new())),
        }]))
        .await
        .expect("reflection call")
        .into_inner();

    let response = stream
        .message()
        .await
        .expect("no error")
        .expect("one response");
    let Some(MessageResponse::ListServicesResponse(services)) = response.message_response else {
        panic!("expected a service listing, got {response:?}");
    };
    let names: Vec<&str> = services
        .service
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    assert!(
        names.contains(&"ai.pipestream.epub.v1.EpubParseService"),
        "reflection did not advertise the parse service: {names:?}"
    );
    // Registering the health service is not the same as advertising it.
    // `grpcurl` resolves a method through reflection, so without the health
    // descriptor a probe fails with "no such service" against a server that
    // implements it. This is the assertion that keeps the probe usable.
    assert!(
        names.contains(&"grpc.health.v1.Health"),
        "reflection did not advertise the health service: {names:?}"
    );
}
