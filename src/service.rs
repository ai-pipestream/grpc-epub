// SPDX-License-Identifier: Apache-2.0

//! The tonic service: upload handling, concurrency bound, and the supervisor
//! that turns a panicking parse into an `INTERNAL` status instead of a stream
//! that just stops.
//!
//! The parse itself runs on [`tokio::task::spawn_blocking`], because inflating
//! is CPU-bound and would otherwise occupy an async worker for the length of a
//! book. [`crate::extract::Sink`] carries backpressure across that boundary:
//! the outbound channel is bounded, and the blocking thread waits on it, so a
//! slow reader slows the parser rather than growing a queue behind it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::extract::{self, Outcome, Sink};
use crate::limits::Limits;
use crate::metrics::Metrics;
use crate::proto::v1 as pb;

/// Events buffered on the outbound channel before the parser has to wait.
///
/// Small on purpose. The channel is a smoothing buffer, not a place to store
/// the book: a chapter can be megabytes, so a deep queue would quietly
/// reintroduce the whole-document buffering this service exists to avoid.
const OUTBOUND_BUFFER: usize = 4;

/// How long the parser waits on a full outbound channel before giving up.
///
/// A client that has read nothing for this long has abandoned the call, and
/// waiting without a bound would pin a blocking-pool thread until the process
/// restarted.
const CONSUMER_STALL: Duration = Duration::from_secs(30);

/// The `ai.pipestream.epub.v1.EpubParseService` implementation.
pub struct EpubGrpc {
    /// The ceilings this server enforces.
    limits: Limits,
    /// Process counters.
    metrics: Arc<Metrics>,
    /// Bounds how many calls may inflate at once, capping heap rather than
    /// shedding load: a call past the bound waits for a permit.
    parse_slots: Arc<tokio::sync::Semaphore>,
}

impl EpubGrpc {
    /// Build a service with the given limits and a fresh set of counters.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self::with_metrics(limits, Metrics::new())
    }

    /// Build a service sharing an existing set of counters.
    ///
    /// Tests use this to watch a parse from outside: the counters are the only
    /// honest way to ask "how far has the server actually got", which is what
    /// distinguishes a live stream from a batch that was buffered and then
    /// handed over all at once.
    #[must_use]
    pub fn with_metrics(limits: Limits, metrics: Arc<Metrics>) -> Self {
        Self {
            limits,
            metrics,
            parse_slots: Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent_parses)),
        }
    }

    /// The counters this service reports into.
    #[must_use]
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// Wrap this service in its generated tonic server.
    ///
    /// tonic's decoding limit is set to twice the chunk cap, deliberately
    /// above it rather than equal to it. The two mean different things: the
    /// cap is advice, refused with an `INVALID_ARGUMENT` naming the number and
    /// saying to split the upload, while tonic's is a hard backstop against a
    /// hostile length prefix. Equal limits would make the backstop fire first
    /// for every ordinary overshoot, and the caller would get `OutOfRange` and
    /// a sentence about decoded message lengths instead of the one telling
    /// them what to do.
    #[must_use]
    pub fn into_service(self) -> pb::epub_parse_service_server::EpubParseServiceServer<Self> {
        let backstop =
            usize::try_from(self.limits.max_chunk_bytes.saturating_mul(2)).unwrap_or(usize::MAX);
        pb::epub_parse_service_server::EpubParseServiceServer::new(self)
            .max_decoding_message_size(backstop)
    }
}

#[tonic::async_trait]
impl pb::epub_parse_service_server::EpubParseService for EpubGrpc {
    type ParseEpubStream = ReceiverStream<Result<pb::ParseEpubResponse, Status>>;

    async fn parse_epub(
        &self,
        request: Request<Streaming<pb::ParseEpubRequest>>,
    ) -> Result<Response<Self::ParseEpubStream>, Status> {
        let mut inbound = request.into_inner();

        // Options first, so every way the request can be rejected outright is
        // resolved before the response stream opens. Once it is open, only the
        // parse can end it badly.
        let options = match inbound.message().await? {
            Some(pb::ParseEpubRequest {
                frame: Some(pb::parse_epub_request::Frame::Options(options)),
            }) => options,
            Some(_) => {
                return Err(Status::invalid_argument(
                    "the first frame must carry `options`",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "the request stream closed before sending `options`",
                ));
            }
        };
        let effective = self.limits.resolve(&options);

        let bytes = self
            .receive(&mut inbound, effective.max_document_bytes)
            .await?;

        // Acquired before the upload is handed to a thread, so the memory
        // ceiling counts calls that are actually inflating.
        let permit = Arc::clone(&self.parse_slots)
            .acquire_owned()
            .await
            .map_err(|_| Status::unavailable("the server is shutting down"))?;

        self.metrics.parse_started();
        let metrics = Arc::clone(&self.metrics);
        let (tx, rx) = mpsc::channel(OUTBOUND_BUFFER);
        let supervisor = tx.clone();

        let handle = tokio::task::spawn_blocking(move || {
            let sink = Sink::new(tx, CONSUMER_STALL);
            extract::run(&bytes, &effective, &metrics, &sink)
        });

        let metrics = Arc::clone(&self.metrics);
        tokio::spawn(async move {
            // A panic drops the parser's sender, and without this the stream
            // would end *successfully* with whatever had been delivered — a
            // truncated book indistinguishable from a short one. The
            // supervisor's own sender is what makes the difference reportable.
            let status = match handle.await {
                Ok(Outcome::Complete) => {
                    metrics.parse_succeeded();
                    None
                }
                Ok(Outcome::Abandoned) => {
                    metrics.parse_failed();
                    None
                }
                Ok(Outcome::Failed(status)) => {
                    metrics.parse_failed();
                    Some(*status)
                }
                Err(join) => {
                    metrics.parse_failed();
                    Some(Status::internal(panic_detail(join)))
                }
            };
            if let Some(status) = status {
                let _ = supervisor.send(Err(status)).await;
            }
            drop(permit);
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_service_info(
        &self,
        _request: Request<pb::GetServiceInfoRequest>,
    ) -> Result<Response<pb::GetServiceInfoResponse>, Status> {
        Ok(Response::new(pb::GetServiceInfoResponse {
            name: "grpc-epub".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            limits: Some(self.limits.to_proto()),
            features: vec![
                "diskless".to_owned(),
                "spine-stream".to_owned(),
                "zip-bomb-guard".to_owned(),
                "health".to_owned(),
                "reflection".to_owned(),
            ],
        }))
    }
}

impl EpubGrpc {
    /// Drain the request stream into one buffer, enforcing the upload cap.
    ///
    /// The buffer is unavoidable: a ZIP is unreadable until its central
    /// directory, which is the last thing to arrive. The cap is checked as
    /// bytes land rather than at the end, so a hostile upload is cut off at
    /// the limit instead of after it.
    async fn receive(
        &self,
        inbound: &mut Streaming<pb::ParseEpubRequest>,
        max_document_bytes: u64,
    ) -> Result<Vec<u8>, Status> {
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(request) = inbound.message().await? {
            let chunk = match request.frame {
                Some(pb::parse_epub_request::Frame::Chunk(chunk)) => chunk,
                Some(pb::parse_epub_request::Frame::Options(_)) => {
                    return Err(Status::invalid_argument(
                        "`options` may only be sent once, as the first frame",
                    ));
                }
                // An empty frame carries nothing and means nothing; skip it
                // rather than treat it as the end of the upload.
                None => continue,
            };
            if chunk.len() as u64 > self.limits.max_chunk_bytes {
                return Err(Status::invalid_argument(format!(
                    "chunk of {} bytes exceeds the {} byte frame limit; split the upload into \
                     more, smaller chunks",
                    chunk.len(),
                    self.limits.max_chunk_bytes
                )));
            }
            if bytes.len() as u64 + chunk.len() as u64 > max_document_bytes {
                self.metrics.uploaded(bytes.len() as u64);
                return Err(Status::resource_exhausted(format!(
                    "the upload passed its {max_document_bytes} byte limit; raise \
                     max_document_mib if the book is genuinely this large"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        self.metrics.uploaded(bytes.len() as u64);
        Ok(bytes)
    }
}

/// Render a `JoinError` into something worth putting on the wire.
fn panic_detail(join: tokio::task::JoinError) -> String {
    if !join.is_panic() {
        return "the parse task was cancelled".to_owned();
    }
    let payload = join.into_panic();
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned());
    format!("the parser panicked, which is a bug in grpc-epub: {detail}")
}
