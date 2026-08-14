// SPDX-License-Identifier: Apache-2.0

//! The tests that fail if this service is ever turned back into a batch API.
//!
//! Everything else in the suite would still pass if someone rewrote the driver
//! to inflate the whole book, collect the events in a `Vec`, and send them at
//! the end: the same events would arrive in the same order. That rewrite is
//! the single most likely way to lose the property this service exists for, so
//! it needs a test that can tell the difference, and "the events looked right"
//! cannot.
//!
//! What can tell the difference is *how far the server has got* when an early
//! event reaches the client. The counters in [`grpc_epub::Metrics`] answer
//! that, and [`the_stream_is_not_a_batch`] asks: at the moment `info` arrives,
//! a streaming implementation has inflated almost nothing, while a batching
//! one has inflated the entire book — because it could not have sent `info`
//! until it had.
//!
//! [`the_stream_is_not_a_batch`] drives [`grpc_epub::extract`] in-process with
//! a one-slot channel, so the bound is exact: the parser can be at most one
//! event ahead of the reader, with no transport buffer in between to blur it.
//! The socket-level test that follows repeats the question end to end, where
//! HTTP/2 flow control makes the bound looser but the batch case still stands
//! out.

mod common;

use std::sync::Arc;
use std::time::Duration;

use grpc_epub::extract::{self, Sink};
use grpc_epub::limits::Effective;
use grpc_epub::metrics::Metrics;
use grpc_epub::proto::v1 as pb;
use tokio::sync::mpsc;

/// Chapters in the fixtures below.
const CHAPTERS: usize = 12;

/// Bytes of body text per chapter.
///
/// Comfortably under the per-entry compression ratio floor, so this
/// deliberately repetitive filler is not mistaken for a decompression bomb,
/// and large enough that "the server inflated one" and "the server inflated
/// all twelve" are far apart in the counters.
const CHAPTER_BYTES: usize = 256 * 1024;

/// Total body text across the book, the number a batching implementation would
/// have inflated before it could send anything at all.
const BOOK_BYTES: u64 = (CHAPTERS * CHAPTER_BYTES) as u64;

/// `info` must reach the client before the book has been inflated.
///
/// The load-bearing test of this repository. Driven in-process against a
/// one-slot channel so the parser can be at most one event ahead of the
/// reader: when `info` is received, a streaming implementation has inflated
/// only `mimetype`, `container.xml` and the OPF, plus at most the one or two
/// chapters it could get ahead by. A batching implementation has inflated all
/// twelve, because that is what it would have had to do before it could send
/// the first event.
#[tokio::test]
async fn the_stream_is_not_a_batch() {
    let archive = common::long_book(CHAPTERS, CHAPTER_BYTES);
    let metrics = Metrics::new();
    let (tx, mut rx) = mpsc::channel(1);

    let counters = Arc::clone(&metrics);
    let parser = tokio::task::spawn_blocking(move || {
        let sink = Sink::new(tx, Duration::from_secs(10), None);
        extract::run(&archive, &Effective::default(), &counters, &sink)
    });

    let first = rx
        .recv()
        .await
        .expect("a stream always opens")
        .expect("no error");
    let Some(pb::parse_epub_response::Event::Info(info)) = first.event else {
        panic!("the first event must be `info`");
    };
    assert_eq!(info.spine_item_count, CHAPTERS as u32);

    // The whole assertion, in one line: a batch would read BOOK_BYTES before
    // sending anything.
    let opened = metrics.snapshot();
    assert!(
        opened.bytes_inflated < BOOK_BYTES / 3,
        "`info` arrived only after {} of {BOOK_BYTES} body bytes had been inflated, which is \
         what a batched implementation looks like",
        opened.bytes_inflated
    );
    assert!(
        opened.chapters_emitted <= 2,
        "with a one-slot channel the parser can be at most one chapter ahead, saw {}",
        opened.chapters_emitted
    );

    // Backpressure: while the reader does nothing, the parser must not run
    // ahead into a buffer of its own.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let idle = metrics.snapshot();
    assert!(
        idle.chapters_emitted <= 2,
        "the parser kept going while nobody was reading, reaching chapter {}; the outbound \
         channel is supposed to be the only buffer",
        idle.chapters_emitted
    );

    // Now drain, and check the shape while we are here.
    let mut shape = vec!["info"];
    while let Some(event) = rx.recv().await {
        let event = event.expect("no error");
        shape.push(
            match event.event.expect("every response carries an event") {
                pb::parse_epub_response::Event::Info(_) => "info",
                pb::parse_epub_response::Event::Chapter(_) => "chapter",
                pb::parse_epub_response::Event::Resource(_) => "resource",
                pb::parse_epub_response::Event::Status(_) => "status",
                pb::parse_epub_response::Event::Document(_) => "document",
            },
        );
    }
    assert!(matches!(
        parser.await.expect("the parser thread"),
        extract::Outcome::Complete
    ));

    assert_eq!(shape.first(), Some(&"info"));
    assert_eq!(shape.last(), Some(&"status"));
    assert_eq!(
        shape.iter().filter(|kind| **kind == "chapter").count(),
        CHAPTERS
    );
}

/// The same question end to end, over a socket.
///
/// Looser than the in-process bound — HTTP/2 flow control lets the server run
/// a little ahead of the client — but a batch implementation would still show
/// every chapter already inflated by the time the first one is read, and this
/// says it must not.
#[tokio::test]
async fn a_chapter_reaches_the_client_while_later_ones_are_still_compressed() {
    let harness = common::start().await;
    let archive = common::long_book(CHAPTERS, CHAPTER_BYTES);

    let mut client = harness.client.clone();
    let frames = vec![
        pb::ParseEpubRequest {
            frame: Some(pb::parse_epub_request::Frame::Options(
                pb::ParseOptions::default(),
            )),
        },
        pb::ParseEpubRequest {
            frame: Some(pb::parse_epub_request::Frame::Chunk(archive)),
        },
    ];

    let mut stream = client
        .parse_epub(tokio_stream::iter(frames))
        .await
        .expect("open the call")
        .into_inner();

    let info = stream.message().await.expect("no error").expect("an event");
    assert!(matches!(
        info.event,
        Some(pb::parse_epub_response::Event::Info(_))
    ));

    let first = stream.message().await.expect("no error").expect("an event");
    let Some(pb::parse_epub_response::Event::Chapter(chapter)) = first.event else {
        panic!("the second event of a book with no early resources must be a chapter");
    };
    assert_eq!(chapter.spine_index, 0);

    assert!(
        harness.metrics.snapshot().chapters_emitted < CHAPTERS as u64,
        "chapter 0 reached the client only after the whole book had been inflated"
    );

    // Drain and confirm the trailer really is last.
    let mut seen = 1;
    let mut trailer = None;
    while let Some(event) = stream.message().await.expect("no error") {
        match event.event.expect("every response carries an event") {
            pb::parse_epub_response::Event::Chapter(_) => {
                assert!(trailer.is_none(), "a chapter arrived after `status`");
                seen += 1;
            }
            pb::parse_epub_response::Event::Status(status) => trailer = Some(status),
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(seen, CHAPTERS);
    assert_eq!(
        trailer.expect("a trailer").chapters_emitted,
        CHAPTERS as u32
    );
}

/// A client that hangs up mid-stream must not wedge the server.
#[tokio::test]
async fn dropping_the_stream_early_frees_the_parser() {
    let harness = common::start().await;
    let archive = common::long_book(CHAPTERS, CHAPTER_BYTES);

    let mut client = harness.client.clone();
    let frames = vec![
        pb::ParseEpubRequest {
            frame: Some(pb::parse_epub_request::Frame::Options(
                pb::ParseOptions::default(),
            )),
        },
        pb::ParseEpubRequest {
            frame: Some(pb::parse_epub_request::Frame::Chunk(archive)),
        },
    ];
    let mut stream = client
        .parse_epub(tokio_stream::iter(frames))
        .await
        .expect("open the call")
        .into_inner();

    let _info = stream.message().await.expect("no error").expect("an event");
    drop(stream);

    // The parser notices a closed channel on its next send and stops. If it
    // did not, this counter would keep climbing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped = harness.metrics.snapshot();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.metrics.snapshot().chapters_emitted,
        stopped.chapters_emitted,
        "the parser kept working for a client that had gone away"
    );
    assert!(
        stopped.chapters_emitted < CHAPTERS as u64,
        "the parser finished the whole book for a client that had gone away"
    );
}
