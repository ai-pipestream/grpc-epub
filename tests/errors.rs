// SPDX-License-Identifier: Apache-2.0

//! What the server says when the input is wrong, and why each answer is the
//! code it is.
//!
//! The split the fleet contract asks for, restated as the question each code
//! answers:
//!
//! - `INVALID_ARGUMENT` — "you gave me a broken EPUB." Fixable by sending a
//!   different file; the server would behave the same tomorrow.
//! - `UNIMPLEMENTED` — "that is not an EPUB, or not one I can open." Retrying
//!   changes nothing until this service grows a feature.
//! - `RESOURCE_EXHAUSTED` — lives in `security.rs`, where the caps are.
//! - `INTERNAL` — a bug here. Nothing in this file should produce one, and a
//!   test that starts to is reporting a real defect.

mod common;

use tonic::Code;

#[tokio::test]
async fn an_empty_upload_is_a_caller_error() {
    let harness = common::start().await;
    let status = harness.parse_err(&[]).await;
    assert_eq!(status.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_truncated_archive_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::minimal();

    // The central directory lives at the end, so losing the tail is exactly
    // the "upload cut short" case.
    let truncated = &archive[..archive.len() / 2];
    let status = harness.parse_err(truncated).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");

    // Losing only the very end is subtler and must land the same way.
    let clipped = &archive[..archive.len() - 8];
    let status = harness.parse_err(clipped).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}

#[tokio::test]
async fn bytes_that_are_not_a_zip_at_all_are_a_caller_error() {
    let harness = common::start().await;
    let status = harness.parse_err(b"<html>not even a zip</html>").await;
    assert_eq!(status.code(), Code::InvalidArgument);
}

/// A ZIP that never claimed to be an EPUB is a format this service does not
/// implement, not a broken book.
#[tokio::test]
async fn a_plain_zip_is_unimplemented() {
    let harness = common::start().await;
    let archive = common::Builder::new()
        .add("notes.txt", "just a zip of some files")
        .add("photo.png", common::IMAGE)
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
    assert!(
        status.message().contains("not an EPUB"),
        "the message should say what it is: {}",
        status.message()
    );
}

/// A file that says `application/epub+zip` and then has no container is a
/// broken EPUB, which is the caller's to fix.
#[tokio::test]
async fn an_epub_without_a_container_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::Builder::new()
        .add_stored("mimetype", "application/epub+zip")
        .add("OEBPS/content.opf", "<package/>")
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("container.xml"));
}

#[tokio::test]
async fn a_container_naming_a_missing_opf_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::Builder::new()
        .add_stored("mimetype", "application/epub+zip")
        .add(
            "META-INF/container.xml",
            common::container_xml("OEBPS/nowhere.opf"),
        )
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("nowhere.opf"), "{status:?}");
}

#[tokio::test]
async fn a_container_with_no_rootfile_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::Builder::new()
        .add_stored("mimetype", "application/epub+zip")
        .add(
            "META-INF/container.xml",
            r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
                 <rootfiles/></container>"#,
        )
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}

#[tokio::test]
async fn an_unparseable_opf_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(common::OPF_PATH, "<package><manifest></package>")
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}

#[tokio::test]
async fn an_empty_spine_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(common::OPF_PATH, common::opf_xml(&[], &[]))
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("spine"), "{status:?}");
}

/// A spine that points at a manifest id nobody declared.
#[tokio::test]
async fn a_dangling_spine_idref_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::package(
                "    <item id=\"ch1\" href=\"text/chap1.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n",
                "    <itemref idref=\"ch1\"/>\n    <itemref idref=\"ghost\"/>\n",
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("ghost"), "{status:?}");
}

/// A spine item whose file is simply not in the archive.
///
/// Diagnosed from the central directory before the stream opens, so the client
/// never sees a partial book it has to unwind. The assertion that no events
/// arrived is the part worth keeping.
#[tokio::test]
async fn a_spine_item_missing_from_the_archive_fails_before_any_event() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(
                &[("ch1", "text/chap1.xhtml"), ("ch2", "text/chap2.xhtml")],
                &[],
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let result = harness
        .parse(&archive, grpc_epub::proto::v1::ParseOptions::default())
        .await;
    let status = result.expect_err("a book missing a chapter must fail");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("chap2.xhtml"), "{status:?}");
}

/// A *resource* the archive lacks is a warning, not a failure: the book is
/// still readable without its cover.
#[tokio::test]
async fn a_missing_resource_is_a_warning_not_a_failure() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(
                &[("ch1", "text/chap1.xhtml")],
                &[("cover-img", "images/gone.png", "image/png", "")],
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let events = harness.parse_ok(&archive).await;
    let status = common::status(&events);
    assert_eq!(status.chapters_emitted, 1);
    assert_eq!(status.resources_skipped, 1);
    assert_eq!(
        status.warnings[0].code,
        grpc_epub::proto::v1::ParseWarningCode::MissingManifestEntry as i32
    );
    assert_eq!(status.warnings[0].href, "OEBPS/images/gone.png");
}

/// A remote manifest resource is recorded and never fetched. No network.
#[tokio::test]
async fn a_remote_resource_is_recorded_and_never_fetched() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(
                &[("ch1", "text/chap1.xhtml")],
                &[(
                    "remote",
                    "https://example.invalid/cover.png",
                    "image/png",
                    "",
                )],
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let events = harness.parse_ok(&archive).await;
    assert!(common::resources(&events).is_empty());
    let status = common::status(&events);
    assert_eq!(status.resources_skipped, 1);
    assert!(
        status.warnings[0].message.contains("network"),
        "{:?}",
        status.warnings
    );
}

/// A *spine* item that is remote is fatal: there is no chapter to stream.
#[tokio::test]
async fn a_remote_spine_item_is_a_caller_error() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "https://example.invalid/chap1.xhtml")], &[]),
        )
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(
        status.message().contains("outside the archive"),
        "{status:?}"
    );
}

/// DRM and font obfuscation both announce themselves with this file.
#[tokio::test]
async fn an_encrypted_book_is_unimplemented() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            "META-INF/encryption.xml",
            r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"/>"#,
        )
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chap1.xhtml")], &[]),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .build();

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
    assert!(status.message().contains("DRM"), "{status:?}");
}

/// An entry with the encryption bit set, which is what a DRM'd book's chapters
/// actually look like on the wire.
#[tokio::test]
async fn an_encrypted_entry_is_unimplemented() {
    let harness = common::start().await;
    let mut archive = common::minimal();
    common::patch_encrypted(&mut archive, common::CHAP1);

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
}

/// Store and deflate are the only methods this build decodes, and the refusal
/// is a missing feature rather than a broken file.
#[tokio::test]
async fn an_unsupported_compression_method_is_unimplemented() {
    let harness = common::start().await;
    let mut archive = common::minimal();
    // 93 is zstd. The build has no zstd decoder, on purpose.
    common::patch_compression_method(&mut archive, common::CHAP1, 93);

    let status = harness.parse_err(&archive).await;
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
}

#[tokio::test]
async fn the_first_frame_must_carry_options() {
    let harness = common::start().await;
    let mut client = harness.client.clone();
    let frames = vec![grpc_epub::proto::v1::ParseEpubRequest {
        frame: Some(grpc_epub::proto::v1::parse_epub_request::Frame::Chunk(
            common::minimal(),
        )),
    }];

    let status = client
        .parse_epub(tokio_stream::iter(frames))
        .await
        .expect_err("a chunk before options must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("options"));
}

#[tokio::test]
async fn options_may_not_be_sent_twice() {
    let harness = common::start().await;
    let mut client = harness.client.clone();
    let options = || grpc_epub::proto::v1::ParseEpubRequest {
        frame: Some(grpc_epub::proto::v1::parse_epub_request::Frame::Options(
            grpc_epub::proto::v1::ParseOptions::default(),
        )),
    };

    // The whole upload is consumed before the response stream opens (a ZIP
    // cannot be read until its last byte), so an upload-level mistake arrives
    // as a status on the call rather than in-band.
    let status = client
        .parse_epub(tokio_stream::iter(vec![options(), options()]))
        .await
        .expect_err("a second options frame must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("only be sent once"));
}
