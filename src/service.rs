// SPDX-License-Identifier: Apache-2.0

//! The tonic service: upload handling, concurrency bound, and the supervisor
//! that turns a panicking parse into an `INTERNAL` status instead of a stream
//! that just stops.
//!
//! Two bounds share the heap between calls. The parse slots cap how many
//! calls inflate at once. The upload budget caps the bytes of upload buffer
//! the whole process holds, across every call, whether it is still uploading,
//! waiting for a slot, or parsing: without it the slots bounded the parses
//! and not the buffers, and a client with many streams open could make the
//! server hold an upload's worth of memory for each of them. A call whose
//! upload would take the process past the budget fails with
//! `RESOURCE_EXHAUSTED` on the spot instead of waiting for room, because a
//! call that waited would stop reading its stream, and on a connection it
//! shares with other calls its unread frames would hold the HTTP/2
//! connection window that those calls need to finish their uploads and give
//! the budget back. For the same reason the parse slot is taken only after
//! the upload is complete: every upload keeps being read.
//!
//! Refusing rather than waiting makes the budget something a client can
//! hold, so two clocks bound how long it is held. The idle timeout ends a
//! stream that sends nothing; the upload timeout ends one that keeps sending
//! too slowly to finish, a byte at a time just inside the idle timeout, which
//! the idle timeout alone would let hold its share forever.
//!
//! The parse itself runs on [`tokio::task::spawn_blocking`], because inflating
//! is CPU-bound and would otherwise occupy an async worker for the length of a
//! book. [`crate::extract::Sink`] carries backpressure across that boundary:
//! the outbound channel is bounded, and the blocking thread waits on it, so a
//! slow reader slows the parser rather than growing a queue behind it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, mpsc};
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

/// Default for how long the server waits for the next request frame.
///
/// A call holds its share of the upload budget for as long as its upload is
/// open, so a client that opens a stream, sends part of a book and then
/// nothing would otherwise keep that share for as long as HTTP/2 keepalive
/// kept the connection up.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default for how long a call's whole upload may take, from the call's
/// first frame to its last.
///
/// Five minutes moves a full 256 MiB upload at under 1 MiB/s, slower than any
/// link a collector in this fleet sits behind, and still stops a client that
/// trickles a frame in just under the idle timeout from holding its share of
/// the upload budget indefinitely.
pub const DEFAULT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Stand-in for "no deadline" when a timeout is too large to add to now.
///
/// `Instant + Duration` panics on overflow, and a timeout read from the
/// environment can be anything; a year is as good as forever for one call.
const FOREVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Bytes of upload buffer one permit of the upload budget stands for.
///
/// Counted in KiB rather than bytes because one reservation is a `u32`, and
/// in bytes that would cap a single growth step of one upload at 4 GiB.
const BUDGET_UNIT: usize = 1024;

/// The `ai.pipestream.epub.v1.EpubParseService` implementation.
pub struct EpubGrpc {
    /// The ceilings this server enforces.
    limits: Limits,
    /// Process counters.
    metrics: Arc<Metrics>,
    /// Bounds how many calls may inflate at once, capping heap rather than
    /// shedding load: a call past the bound waits for a permit.
    parse_slots: Arc<tokio::sync::Semaphore>,
    /// Upload buffer the whole process may hold, in [`BUDGET_UNIT`]s. Each
    /// call reserves what its buffer grows to and gives it back when its
    /// stream ends.
    upload_budget: Arc<tokio::sync::Semaphore>,
    /// Longest wait for the next request frame before the call is ended.
    idle_timeout: Duration,
    /// Longest a call's whole upload may take before the call is ended.
    upload_timeout: Duration,
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
        // A budget smaller than one upload would refuse the largest upload a
        // call is allowed to send, every time.
        let limits = Limits {
            max_buffered_upload_bytes: limits
                .max_buffered_upload_bytes
                .max(limits.max_document_bytes),
            ..limits
        };
        Self {
            limits,
            metrics,
            parse_slots: Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent_parses)),
            upload_budget: Arc::new(tokio::sync::Semaphore::new(
                usize::try_from(limits.max_buffered_upload_bytes)
                    .unwrap_or(usize::MAX)
                    .div_ceil(BUDGET_UNIT)
                    .min(tokio::sync::Semaphore::MAX_PERMITS),
            )),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            upload_timeout: DEFAULT_UPLOAD_TIMEOUT,
        }
    }

    /// Override how long the server waits for the next request frame.
    ///
    /// Past it the call ends with `DEADLINE_EXCEEDED` and gives its share of
    /// the upload budget back. Raised to one millisecond if smaller: an idle
    /// stream is always bounded.
    #[must_use]
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Override how long a call's whole upload may take, options frame
    /// included.
    ///
    /// Past it the call ends with `DEADLINE_EXCEEDED` and gives its share of
    /// the upload budget back, however steadily it was sending. Raised to one
    /// millisecond if smaller: an upload is always bounded.
    #[must_use]
    pub fn with_upload_timeout(mut self, timeout: Duration) -> Self {
        self.upload_timeout = timeout.max(Duration::from_millis(1));
        self
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
        let clock = UploadClock::start(self.idle_timeout, self.upload_timeout);

        // Options first, so every way the request can be rejected outright is
        // resolved before the response stream opens. Once it is open, only the
        // parse can end it badly.
        let options = match clock.next_frame(&mut inbound).await? {
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

        // The upload is bounded by the process-wide budget as it is read; see
        // the module documentation for why it is read before the slot is
        // taken rather than after.
        let (bytes, upload) = self
            .receive(&mut inbound, &clock, effective.max_document_bytes)
            .await?;

        // Acquired before the upload is handed to a thread, so the slots
        // count calls that are actually inflating.
        let permit = Arc::clone(&self.parse_slots)
            .acquire_owned()
            .await
            .map_err(|_| Status::unavailable("the server is shutting down"))?;

        self.metrics.parse_started();
        let metrics = Arc::clone(&self.metrics);
        let (tx, rx) = mpsc::channel(OUTBOUND_BUFFER);
        let supervisor = tx.clone();

        // Built here rather than inside the parse so the cost of the option is
        // visible: no fold, no allocation, and the emission path is unchanged.
        let fold = effective
            .emit_document
            .then(crate::document_fold::DocumentFold::for_this_build);

        let handle = tokio::task::spawn_blocking(move || {
            let sink = Sink::new(tx, CONSUMER_STALL, fold);
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
            // The parse thread has returned and dropped the upload with it,
            // so its share of the budget is free again.
            drop(upload);
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
                // This build can fold a call into an
                // `ai.pipestream.document.v1.Document`; an older one cannot,
                // and a client that needs the projection can branch on this
                // rather than on the version string.
                "document-fold".to_owned(),
                // This build reads the navigation document and the NCX into a
                // `navigation` event and the Document's outline, and can read
                // SMIL media overlays into `media_overlay` events. A client
                // that needs either can branch on the capability rather than
                // on the version string.
                "navigation".to_owned(),
                "media-overlays".to_owned(),
                "health".to_owned(),
                "reflection".to_owned(),
            ],
            ui: Some(pb::UiInfo {
                title: "EPUB".to_owned(),
                path: "/ui/epub".to_owned(),
                description: "Unpacks EPUB archives in memory and streams the spine".to_owned(),
            }),
        }))
    }
}

impl EpubGrpc {
    /// Drain the request stream into one buffer, enforcing the upload cap and
    /// the process-wide upload budget.
    ///
    /// The buffer is unavoidable: a ZIP is unreadable until its central
    /// directory, which is the last thing to arrive. Both bounds are checked
    /// as bytes land rather than at the end, so a hostile upload is cut off at
    /// the limit instead of after it. The buffer never grows past the cap
    /// either: `Vec`'s own doubling would otherwise reserve up to twice the
    /// cap for an upload just under it. The budget is charged for capacity,
    /// not length, because capacity is what the allocator handed out.
    ///
    /// Returns the upload and the share of the budget it holds, which the
    /// caller keeps until the upload is dropped.
    async fn receive(
        &self,
        inbound: &mut Streaming<pb::ParseEpubRequest>,
        clock: &UploadClock,
        max_document_bytes: u64,
    ) -> Result<(Vec<u8>, Option<OwnedSemaphorePermit>), Status> {
        let cap = usize::try_from(max_document_bytes).unwrap_or(usize::MAX);
        let mut bytes: Vec<u8> = Vec::new();
        let mut held: Option<OwnedSemaphorePermit> = None;
        while let Some(request) = clock.next_frame(inbound).await? {
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
            let needed = bytes.len() + chunk.len();
            if needed > bytes.capacity() {
                // Geometric growth, as `Vec` would do it, stopped at the cap.
                // Near the budget the doubling is given up before the upload
                // is: growing to exactly what is needed may still fit.
                let doubled = bytes.capacity().saturating_mul(2).max(needed).min(cap);
                let target = if self.reserve_upload(&mut held, doubled).is_ok() {
                    doubled
                } else {
                    self.reserve_upload(&mut held, needed)?;
                    needed
                };
                bytes.reserve_exact(target - bytes.len());
            }
            bytes.extend_from_slice(&chunk);
        }
        self.metrics.uploaded(bytes.len() as u64);
        Ok((bytes, held))
    }

    /// Grow this call's share of the upload budget to cover `capacity`
    /// bytes, or fail without waiting.
    ///
    /// # Errors
    ///
    /// `RESOURCE_EXHAUSTED` when the process already holds as much upload as
    /// the budget allows. See the module documentation for why this fails
    /// rather than waits.
    fn reserve_upload(
        &self,
        held: &mut Option<OwnedSemaphorePermit>,
        capacity: usize,
    ) -> Result<(), Status> {
        let have = held.as_ref().map_or(0, OwnedSemaphorePermit::num_permits);
        let more = capacity.div_ceil(BUDGET_UNIT).saturating_sub(have);
        if more == 0 {
            return Ok(());
        }
        let permit = u32::try_from(more)
            .ok()
            .and_then(|more| {
                Arc::clone(&self.upload_budget)
                    .try_acquire_many_owned(more)
                    .ok()
            })
            .ok_or_else(|| {
                Status::resource_exhausted(format!(
                    "the server is already holding the {} MiB of uploads it allows across all \
                     calls; retry once calls in progress have finished",
                    self.limits.max_buffered_upload_bytes / crate::limits::MIB
                ))
            })?;
        match held {
            Some(held) => held.merge(permit),
            None => *held = Some(permit),
        }
        Ok(())
    }
}

/// The two clocks one call's upload runs against.
#[derive(Clone, Copy, Debug)]
struct UploadClock {
    /// Longest wait for any one frame.
    idle: Duration,
    /// Longest the whole upload may take, for the message.
    total: Duration,
    /// When the whole upload must be in.
    deadline: tokio::time::Instant,
}

impl UploadClock {
    /// Start both clocks now, at the call's first frame.
    fn start(idle: Duration, total: Duration) -> Self {
        Self {
            idle,
            total,
            deadline: after(total),
        }
    }

    /// Wait for the next request frame, until the idle timeout or the upload
    /// deadline, whichever comes first.
    ///
    /// # Errors
    ///
    /// `DEADLINE_EXCEEDED` when nothing arrives in time, naming which clock
    /// ran out, or whatever status the transport reports for a broken stream.
    async fn next_frame(
        &self,
        inbound: &mut Streaming<pb::ParseEpubRequest>,
    ) -> Result<Option<pb::ParseEpubRequest>, Status> {
        let idle_at = after(self.idle);
        if idle_at < self.deadline {
            tokio::time::timeout_at(idle_at, inbound.message())
                .await
                .map_err(|_| {
                    Status::deadline_exceeded(format!(
                        "no request frame arrived for {} ms; an idle stream may not hold server \
                         memory",
                        self.idle.as_millis()
                    ))
                })?
        } else {
            tokio::time::timeout_at(self.deadline, inbound.message())
                .await
                .map_err(|_| {
                    Status::deadline_exceeded(format!(
                        "the upload was not complete within {} ms; send the book faster or raise \
                         the server's upload timeout",
                        self.total.as_millis()
                    ))
                })?
        }
    }
}

/// The instant `wait` from now, or [`FOREVER`] from now if that overflows.
fn after(wait: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(wait).unwrap_or(now + FOREVER)
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
