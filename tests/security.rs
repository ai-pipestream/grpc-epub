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

use grpc_epub::Limits;
use grpc_epub::proto::v1 as pb;
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

/// An archive entry named `../../etc/passwd`.
///
/// Nothing here writes to disk, so this cannot overwrite anything in *this*
/// process. It is refused because the name would go out on the wire as a
/// `Chapter.href`, and a client that does write files would inherit the
/// traversal from us.
#[tokio::test]
async fn an_entry_name_that_escapes_the_archive_is_refused() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add("../../etc/passwd", "root:x:0:0::/root:/bin/sh")
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(
        status.message().contains("escapes the archive root"),
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
