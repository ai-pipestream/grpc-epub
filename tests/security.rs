// SPDX-License-Identifier: Apache-2.0

//! Attacks the EPUB format can actually express, and what the server does
//! about each.
//!
//! An EPUB is a ZIP full of XML supplied by whoever made the file, so the
//! interesting inputs are not malformed books but *well-formed hostile* ones:
//! a chapter that inflates to gigabytes, an entry named `../../etc/passwd`, an
//! OPF whose title is the contents of a file on the server. Each of those has
//! a test here, and each fixture is built by the test rather than committed,
//! so the attack is legible in the source.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use grpc_epub::Limits;
use grpc_epub::proto::v1 as pb;
use grpc_epub::service::EpubGrpc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;

/// Roughly 8 MiB of a repeating byte, which deflate stores in a few kilobytes.
///
/// A thousandfold amplification, which is what a decompression bomb is. It sits
/// above the per-entry ratio floor, so the ratio rule is the one that catches
/// it and the total cap never has to.
fn bomb_payload() -> Vec<u8> {
    vec![b'A'; 8 * 1024 * 1024]
}

/// A book whose second chapter is a decompression bomb.
fn bomb_book() -> Vec<u8> {
    common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(
                &[("ch1", "text/chap1.xhtml"), ("ch2", "text/chap2.xhtml")],
                &[],
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add(common::CHAP2, bomb_payload())
        .build()
}

/// The per-entry ratio catches an entry that inflates far beyond what its
/// upload paid for, even though the total cap has plenty of room left.
#[tokio::test]
async fn a_decompression_bomb_is_refused_on_its_ratio() {
    let harness = common::start().await;
    let archive = bomb_book();

    // The upload is tiny; the payload is not. That gap is the attack.
    assert!(
        archive.len() < 64 * 1024,
        "an 8 MiB bomb should upload in well under 64 KiB, got {}",
        archive.len()
    );

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    assert!(
        status.message().contains("bomb"),
        "the message should name the problem: {}",
        status.message()
    );
}

/// A bomb whose central directory understates it is stopped while it
/// inflates, as soon as it passes the ratio.
///
/// The header claims a thousand bytes, so nothing checked before inflating
/// sees a problem. The ratio rule used to be applied only once the entry was
/// whole, so the entry inflated until the total budget stopped it; with the
/// budget at 4 MiB that is the decompressed-size cap firing. Checked on every
/// chunk, the ratio stops the same entry at about 1.6 MiB, long before.
#[tokio::test]
async fn a_bomb_with_a_lying_header_is_stopped_while_it_inflates() {
    let harness = common::start().await;
    let mut archive = bomb_book();
    common::patch_declared_size(&mut archive, common::CHAP2, 1000);

    let status = harness
        .parse(
            &archive,
            pb::ParseOptions {
                max_uncompressed_mib: 4,
                ..Default::default()
            },
        )
        .await
        .expect_err("a bomb is a bomb whatever its header says");
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    assert!(
        status.message().contains("bomb"),
        "the ratio, not the budget, must be what stopped it: {}",
        status.message()
    );
}

/// A bomb whose header claims to be stored in more bytes than the archive
/// holds is measured against the bytes the archive actually has.
///
/// Claiming a two-gigabyte stored size makes any entry look barely
/// compressed. Without a ceiling on the claim, this 8 MiB bomb passed the
/// ratio rule outright and went to the client as a chapter.
#[tokio::test]
async fn a_bomb_claiming_a_huge_stored_size_is_still_a_bomb() {
    let harness = common::start().await;
    let mut archive = bomb_book();
    common::patch_stored_size(&mut archive, common::CHAP2, 0x7fff_ffff);

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    assert!(status.message().contains("bomb"), "{}", status.message());
}

/// A caller may raise the ratio; the total cap then stops the same file.
///
/// Two rules rather than one, because either alone has a hole: the ratio
/// alone lets a thousand ordinary entries add up, and the total alone lets one
/// entry sit just under it with a tiny upload.
#[tokio::test]
async fn the_total_cap_stops_what_a_raised_ratio_lets_through() {
    let harness = common::start().await;
    let status = harness
        .parse(
            &bomb_book(),
            pb::ParseOptions {
                max_compression_ratio: u32::MAX,
                max_uncompressed_mib: 1,
                ..Default::default()
            },
        )
        .await
        .expect_err("8 MiB does not fit in a 1 MiB budget");

    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    assert!(status.message().contains("cap"), "{}", status.message());
}

/// The upload cap is enforced as bytes land, not after the last one.
#[tokio::test]
async fn the_upload_cap_is_enforced_while_uploading() {
    let harness = common::start().await;
    // Incompressible-ish padding so the upload itself is over a megabyte.
    let padding: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add_stored("OEBPS/big.bin", padding)
        .build();

    let status = harness
        .parse(
            &archive,
            pb::ParseOptions {
                max_document_mib: 1,
                ..Default::default()
            },
        )
        .await
        .expect_err("a 3 MiB upload does not fit in a 1 MiB limit");
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
}

/// The entry count is checked against the central directory before anything is
/// inflated, so the archive whose payload is a million names costs one parse.
#[tokio::test]
async fn too_many_entries_are_refused_before_inflating_anything() {
    let harness = common::start().await;
    let mut builder = common::shell().add(
        common::OPF_PATH,
        common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
    );
    builder = builder.add(common::CHAP1, common::chapter_xhtml("One", "a"));
    for i in 0..50 {
        builder = builder.add(&format!("OEBPS/junk/{i}.txt"), "x");
    }
    let archive = builder.build();

    let status = harness
        .parse(
            &archive,
            pb::ParseOptions {
                max_entries: 10,
                ..Default::default()
            },
        )
        .await
        .expect_err("54 entries do not fit under a limit of 10");
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    assert_eq!(
        harness.metrics.snapshot().bytes_inflated,
        0,
        "the count is checked from the central directory, before any inflation"
    );
}

/// An archive entry named `../../etc/passwd`, and another named absolutely.
///
/// Nothing here writes to disk, so neither can overwrite anything in *this*
/// process. Neither may go out on the wire either, because a client that does
/// write files would inherit the traversal from us. But nothing can name
/// such an entry, so leaving it out is enough: the book around it, which a
/// careless zip tool did not make any less readable, still parses.
#[tokio::test]
async fn an_entry_name_that_escapes_the_archive_is_left_out() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add("../../etc/passwd", "root:x:0:0::/root:/bin/sh")
        .add("/stray.txt", "left by a zip tool")
        .build();

    let events = harness
        .parse(
            &archive,
            pb::ParseOptions {
                include_all_resources: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("the book around the bad names still parses");
    assert_eq!(common::chapters(&events).len(), 1);
    assert!(common::resources(&events).is_empty());
    assert!(
        !format!("{events:?}").contains("root:x"),
        "nothing from the escaping entry may reach the client"
    );

    let status = common::status(&events);
    let left_out: Vec<&pb::ParseWarning> = status
        .warnings
        .iter()
        .filter(|warning| warning.code == pb::ParseWarningCode::UnusableEntryName as i32)
        .collect();
    assert_eq!(left_out.len(), 2, "{:?}", status.warnings);
    assert!(left_out[0].message.contains("escapes the archive root"));
    assert!(left_out[1].message.contains("absolute"));
    assert!(
        left_out.iter().all(|warning| warning.href.is_empty()),
        "a name that is no archive path is not offered as one"
    );
}

/// The escaping name is still fatal where the book needs it: as the spine
/// item's file it is simply not there.
#[tokio::test]
async fn a_spine_item_that_only_an_escaping_entry_could_supply_fails() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(
            "../OEBPS/text/chap1.xhtml",
            common::chapter_xhtml("One", "a"),
        )
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(
        status.message().contains("does not contain"),
        "{}",
        status.message()
    );
}

/// The same traversal in an OPF href rather than an entry name.
#[tokio::test]
async fn a_manifest_href_that_escapes_the_archive_is_refused() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "../../../../etc/passwd")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("escapes"), "{}", status.message());
}

/// Percent-encoding does not smuggle a traversal past the check, because the
/// href is decoded before it is normalized rather than after.
#[tokio::test]
async fn a_percent_encoded_traversal_is_refused() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "%2e%2e/%2e%2e/%2e%2e/etc/passwd")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}

/// An OPF that declares an external entity and uses it in the title.
///
/// The canonical XXE. quick-xml has no DTD processor and cannot fetch the
/// file, and on top of that the declaration itself is refused, so there are
/// two independent reasons this cannot leak `/etc/passwd`. The test pins the
/// outer one.
#[tokio::test]
async fn an_external_entity_declaration_is_refused() {
    let harness = common::start().await;
    let hostile = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE package [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]>
{}"#,
        common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[])
            .replace("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n", "")
            .replace("A Tale of Two Chapters", "&xxe;")
    );

    let archive = common::shell()
        .add(common::OPF_PATH, hostile)
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(
        status.message().contains("entity"),
        "the message should name the reason: {}",
        status.message()
    );
}

/// The same attack with the declaration removed, which is the version that
/// gets through the outer check.
///
/// This is the assertion that quick-xml never resolves an entity: the title
/// must come back as the four literal characters `&xxe;`, not as the contents
/// of a file and not as an empty string that hides what happened.
#[tokio::test]
async fn an_undeclared_entity_reaches_the_client_verbatim() {
    let harness = common::start().await;
    let opf = common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[])
        .replace("A Tale of Two Chapters", "&xxe;");
    let archive = common::shell()
        .add(common::OPF_PATH, opf)
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let events = harness.parse_ok(&archive).await;
    let title = &common::info(&events).title;
    assert_eq!(title, "&xxe;", "an entity must never be resolved");
    assert!(!title.contains("root:"), "no file contents may appear");
}

/// The same check on the container document, which is parsed first and is just
/// as much attacker-supplied XML.
#[tokio::test]
async fn an_external_entity_in_the_container_is_refused() {
    let harness = common::start().await;
    let archive = common::Builder::new()
        .add_stored("mimetype", "application/epub+zip")
        .add(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?>
<!DOCTYPE container [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf"
    media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("entity"), "{}", status.message());
}

/// A ZIP inside the EPUB is reported and never opened.
///
/// Recursing is how a bomb hides from a single-level cap: an inner archive can
/// be small, pass every check, and inflate to gigabytes once opened. The
/// non-goal in `docs/design.md` is therefore also a control.
#[tokio::test]
async fn a_nested_archive_is_reported_and_never_opened() {
    let harness = common::start().await;
    let inner = common::minimal();
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(
                &[("ch1", "text/chap1.xhtml")],
                &[("inner", "extra/inner.epub", "application/epub+zip", "")],
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add("OEBPS/extra/inner.epub", inner)
        .build();

    let events = harness
        .parse(
            &archive,
            pb::ParseOptions {
                // Even when the caller asks for everything.
                include_all_resources: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("the outer book still parses");

    assert!(
        common::resources(&events).is_empty(),
        "the inner archive must not be emitted"
    );
    let status = common::status(&events);
    assert_eq!(
        status.warnings[0].code,
        pb::ParseWarningCode::NestedArchive as i32
    );
    assert_eq!(status.warnings[0].href, "OEBPS/extra/inner.epub");
}

/// A request cannot raise a limit past the server's own.
#[tokio::test]
async fn a_request_cannot_widen_the_servers_caps() {
    let harness = common::start_with(Limits {
        max_uncompressed_bytes: 64 * 1024,
        ..Limits::default()
    })
    .await;

    let status = harness
        .parse(
            &bomb_book(),
            pb::ParseOptions {
                max_uncompressed_mib: u32::MAX,
                max_compression_ratio: u32::MAX,
                ..Default::default()
            },
        )
        .await
        .expect_err("asking for more headroom must not grant it");
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
}

/// A frame larger than the server's chunk cap is refused with advice, not with
/// the transport's opaque length-prefix error.
#[tokio::test]
async fn an_oversized_chunk_frame_is_refused_with_advice() {
    let harness = common::start_with(Limits {
        max_chunk_bytes: 1024,
        ..Limits::default()
    })
    .await;

    let status = harness.parse_err(&common::minimal()).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(
        status.message().contains("smaller chunks"),
        "{}",
        status.message()
    );
}

/// One mebibyte, for the upload sizes below.
const MIB: usize = 1024 * 1024;

/// Upload frame size for the paced uploads below.
const FRAME: usize = 64 * 1024;

/// A conforming one-chapter book padded with `filler` bytes of stored,
/// incompressible data that no manifest item names.
///
/// The padding is never inflated. It makes the upload large while the parse
/// stays trivial, which is what a test about uploads wants.
fn padded_book(filler: usize) -> Vec<u8> {
    let padding: Vec<u8> = (0..filler)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add_stored("OEBPS/padding.bin", padding)
        .build()
}

/// The `options` frame with default options.
fn options_frame() -> pb::ParseEpubRequest {
    pb::ParseEpubRequest {
        frame: Some(pb::parse_epub_request::Frame::Options(
            pb::ParseOptions::default(),
        )),
    }
}

/// One `chunk` frame.
fn chunk_frame(bytes: &[u8]) -> pb::ParseEpubRequest {
    pb::ParseEpubRequest {
        frame: Some(pb::parse_epub_request::Frame::Chunk(bytes.to_vec())),
    }
}

/// Run one call over a request stream the test feeds by hand, collecting
/// every event.
async fn call(
    harness: &common::Harness,
    frames: mpsc::Receiver<pb::ParseEpubRequest>,
) -> tokio::task::JoinHandle<Result<Vec<pb::parse_epub_response::Event>, tonic::Status>> {
    let mut client = harness.client.clone();
    tokio::spawn(async move {
        let mut stream = client
            .parse_epub(ReceiverStream::new(frames))
            .await?
            .into_inner();
        let mut events = Vec::new();
        while let Some(response) = stream.message().await? {
            events.push(response.event.expect("every response carries an event"));
        }
        Ok(events)
    })
}

/// Limits with a 10 MiB process-wide upload budget and an 8 MiB document
/// cap: one 7.5 MiB upload fits, and so does one 3 MiB upload, but not both
/// at once.
fn tight_budget() -> Limits {
    Limits {
        max_document_bytes: 8 * MIB as u64,
        max_buffered_upload_bytes: 10 * MIB as u64,
        ..Limits::default()
    }
}

/// Start a call, push the first 7 MiB of a 7.5 MiB book through it, and leave
/// it open. Returns the frame sender, the frames not yet sent, and the call.
///
/// The server reads every upload as it arrives, so once these sends have gone
/// through it holds about 7 MiB of this call's upload, in an 8 MiB buffer.
async fn stalled_upload(
    harness: &common::Harness,
    book: &[u8],
) -> (
    mpsc::Sender<pb::ParseEpubRequest>,
    Vec<Vec<u8>>,
    tokio::task::JoinHandle<Result<Vec<pb::parse_epub_response::Event>, tonic::Status>>,
) {
    let (tx, rx) = mpsc::channel(1);
    let handle = call(harness, rx).await;
    tx.send(options_frame()).await.expect("options");
    let mut frames = book.chunks(FRAME);
    for frame in frames.by_ref().take(7 * MIB / FRAME) {
        tx.send(chunk_frame(frame))
            .await
            .expect("the upload is read");
    }
    (tx, frames.map(<[u8]>::to_vec).collect(), handle)
}

/// The process never holds more upload than its budget, summed over calls.
///
/// The per-call cap bounds one upload and the parse slots bound the
/// inflating, and before the budget nothing bounded how many uploads were
/// buffered at once: a client with many streams open could make the server
/// hold an upload's worth of memory for every one of them. Here one call
/// holds 8 MiB of a 10 MiB budget, so a second call's 3 MiB upload is refused
/// while it arrives, although it is well under its own cap; once the first
/// call has finished, the same upload goes through.
#[tokio::test]
async fn the_process_holds_no_more_upload_than_its_budget() {
    let harness = common::start_with(tight_budget()).await;
    let held_book = padded_book(7 * MIB + MIB / 2);
    let (tx, rest, held) = stalled_upload(&harness, &held_book).await;

    let small = padded_book(3 * MIB);
    let status = harness
        .parse(&small, pb::ParseOptions::default())
        .await
        .expect_err("the budget is spoken for");
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    assert!(
        status.message().contains("across all calls"),
        "the refusal should say it is the process-wide budget, not this call's cap: {}",
        status.message()
    );

    for frame in rest {
        tx.send(chunk_frame(&frame)).await.expect("frame");
    }
    drop(tx);
    let events = held.await.expect("task").expect("the first call parses");
    assert_eq!(common::status(&events).chapters_emitted, 1);

    let events = harness
        .parse(&small, pb::ParseOptions::default())
        .await
        .expect("the budget was given back when the first call ended");
    assert_eq!(common::status(&events).chapters_emitted, 1);
}

/// A client that sends part of an upload and goes quiet gives its share of
/// the budget back.
///
/// Without the idle timeout the share would be held for as long as HTTP/2
/// keepalive kept the connection up.
#[tokio::test]
async fn an_idle_upload_gives_its_budget_back() {
    let harness = common::start_service(
        EpubGrpc::new(tight_budget()).with_idle_timeout(Duration::from_millis(200)),
    )
    .await;
    let book = padded_book(7 * MIB + MIB / 2);
    let (tx, _rest, idle) = stalled_upload(&harness, &book).await;

    let status = tokio::time::timeout(Duration::from_secs(10), idle)
        .await
        .expect("the server gives up on an idle stream")
        .expect("task")
        .expect_err("an idle call is ended");
    assert_eq!(status.code(), Code::DeadlineExceeded, "{status:?}");
    drop(tx);

    let events = harness
        .parse(&padded_book(3 * MIB), pb::ParseOptions::default())
        .await
        .expect("the idle call's share of the budget was given back");
    assert_eq!(common::status(&events).chapters_emitted, 1);
}

/// A call waiting for a parse slot still has its upload read.
///
/// The budget fails a call rather than making it wait, and this is why. A
/// call that stopped reading its stream while it waited would leave its
/// frames in the HTTP/2 connection window it shares with every other call
/// on the connection, and the calls already being read could stall behind
/// them. So nothing waits with an unread upload: a call waits for its slot
/// only once its upload is in.
#[tokio::test]
async fn an_upload_is_read_while_its_call_waits_for_a_slot() {
    let harness = common::start_with(Limits {
        max_concurrent_parses: 1,
        ..Limits::default()
    })
    .await;

    // The first call takes the only slot, and keeps it: its client reads the
    // first event and then nothing, so the parse waits on its outbound
    // channel with the slot in hand. Its 10 MiB of chapters is far more than
    // the client's receive window and the outbound channel can absorb.
    let (first_tx, first_rx) = mpsc::channel(4);
    let mut client = harness.client.clone();
    first_tx.send(options_frame()).await.expect("options");
    first_tx
        .send(chunk_frame(&common::long_book(40, 256 * 1024)))
        .await
        .expect("upload");
    drop(first_tx);
    let mut first = client
        .parse_epub(ReceiverStream::new(first_rx))
        .await
        .expect("the first call opens")
        .into_inner();
    let opening = first.message().await.expect("no error").expect("an event");
    assert!(matches!(
        opening.event,
        Some(pb::parse_epub_response::Event::Info(_))
    ));

    // The second call's whole 4 MiB upload goes in although it has no slot.
    let book = padded_book(4 * MIB);
    let total = book.len();
    let taken = Arc::new(AtomicUsize::new(0));
    let (second_tx, second_rx) = mpsc::channel(1);
    let second = call(&harness, second_rx).await;
    let producer = {
        let taken = Arc::clone(&taken);
        tokio::spawn(async move {
            second_tx.send(options_frame()).await.expect("options");
            for frame in book.chunks(FRAME) {
                second_tx.send(chunk_frame(frame)).await.expect("frame");
                taken.fetch_add(frame.len(), Ordering::Relaxed);
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(20), producer)
        .await
        .expect("the waiting call's upload was read in full")
        .expect("producer");
    assert_eq!(taken.load(Ordering::Relaxed), total);
    assert!(!second.is_finished(), "the second call has no slot yet");

    // Drain the first call; its slot passes to the second.
    while first.message().await.expect("no error").is_some() {}
    let events = second.await.expect("task").expect("the second call parses");
    assert_eq!(common::status(&events).chapters_emitted, 1);
}
