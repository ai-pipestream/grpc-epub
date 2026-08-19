// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a real tonic server on an ephemeral port, the generated
//! client, and EPUBs the tests build in memory.
//!
//! These cover the happy path and the wire contract. Hostile input lives in
//! `security.rs`, malformed input in `errors.rs`, and the live-stream property
//! in `streaming.rs`.

mod common;

use grpc_epub::proto::v1 as pb;

#[tokio::test]
async fn a_minimal_epub_yields_metadata_chapters_and_the_image() {
    let harness = common::start().await;
    let events = harness.parse_ok(&common::minimal()).await;

    let info = common::info(&events);
    assert_eq!(info.title, "A Tale of Two Chapters");
    assert_eq!(info.creators, ["Ada Lovelace", "Charles Babbage"]);
    assert_eq!(info.language, "en-GB");
    assert_eq!(info.publisher, "Analytical Press");
    assert_eq!(info.date, "1843-10-01");
    assert_eq!(info.subjects, ["Computing"]);
    assert_eq!(info.epub_version, "3.0");
    assert_eq!(info.opf_href, common::OPF_PATH);
    assert_eq!(info.spine_item_count, 2);
    assert_eq!(info.unique_identifier, "urn:isbn:9780000000000");
    assert_eq!(info.identifiers.len(), 1);
    assert_eq!(info.identifiers[0].scheme, "ISBN");
    assert_eq!(info.cover_href, common::COVER);

    let chapters = common::chapters(&events);
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].spine_index, 0);
    assert_eq!(chapters[0].idref, "ch1");
    assert_eq!(chapters[0].href, common::CHAP1);
    assert_eq!(chapters[0].media_type, "application/xhtml+xml");
    assert!(chapters[0].linear, "absent `linear` means linear");
    assert_eq!(
        chapters[0].content,
        common::chapter_xhtml("Chapter One", "The first chapter.").into_bytes(),
        "chapter bytes go out verbatim; nothing here rewrites XHTML"
    );
    assert_eq!(chapters[1].spine_index, 1);
    assert_eq!(chapters[1].href, common::CHAP2);

    let resources = common::resources(&events);
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].href, common::COVER);
    assert_eq!(resources[0].media_type, "image/png");
    assert_eq!(resources[0].kind, pb::ResourceKind::Image as i32);
    assert_eq!(resources[0].manifest_id, "cover-img");
    assert_eq!(resources[0].properties, ["cover-image"]);
    assert_eq!(
        resources[0].content,
        common::IMAGE,
        "image bytes round-trip"
    );

    let status = common::status(&events);
    assert_eq!(status.chapters_emitted, 2);
    assert_eq!(status.resources_emitted, 1);
    assert_eq!(status.resources_skipped, 0);
    // mimetype, container.xml, the OPF, two chapters and the image.
    assert_eq!(status.entries_read, 6);
    assert!(status.uncompressed_bytes > 0);
    assert!(
        status.warnings.is_empty(),
        "a conforming book warns about nothing: {:?}",
        status.warnings
    );
}

/// A resource is emitted when its archive entry is reached, not at the end.
///
/// The load-bearing test for the emission-order contract on the `Resource`
/// message. Two books with identical manifests and identical spines, differing
/// only in where the image sits in the archive, must produce the image event
/// in different places. An implementation that collected resources and flushed
/// them at the end would make both streams identical, and this is what says
/// so.
#[tokio::test]
async fn a_resource_arrives_when_its_archive_entry_is_reached() {
    let harness = common::start().await;

    let last = harness.parse_ok(&common::minimal()).await;
    assert_eq!(
        common::shape(&last),
        ["info", "chapter", "chapter", "resource", "status"],
        "image stored after the chapters must arrive after them"
    );

    let first = harness.parse_ok(&common::image_first()).await;
    assert_eq!(
        common::shape(&first),
        ["info", "resource", "chapter", "chapter", "status"],
        "image stored before the chapters must arrive before them"
    );

    // Same book either way, only differently packed.
    assert_eq!(
        common::chapters(&last)
            .iter()
            .map(|chapter| chapter.href.clone())
            .collect::<Vec<_>>(),
        common::chapters(&first)
            .iter()
            .map(|chapter| chapter.href.clone())
            .collect::<Vec<_>>()
    );
}

/// Spine order is reading order, whatever order the archive happens to use.
#[tokio::test]
async fn chapters_follow_the_spine_not_the_archive() {
    let harness = common::start().await;

    // Three chapters written to the archive backwards, and a spine that puts
    // them right.
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(
                &[
                    ("ch1", "text/chap1.xhtml"),
                    ("ch2", "text/chap2.xhtml"),
                    ("ch3", "text/chap3.xhtml"),
                ],
                &[],
            ),
        )
        .add(
            "OEBPS/text/chap3.xhtml",
            common::chapter_xhtml("Three", "c"),
        )
        .add("OEBPS/text/chap2.xhtml", common::chapter_xhtml("Two", "b"))
        .add("OEBPS/text/chap1.xhtml", common::chapter_xhtml("One", "a"))
        .build();

    let events = harness.parse_ok(&archive).await;
    let chapters = common::chapters(&events);
    assert_eq!(
        chapters
            .iter()
            .map(|chapter| chapter.idref.as_str())
            .collect::<Vec<_>>(),
        ["ch1", "ch2", "ch3"]
    );
    assert_eq!(
        chapters
            .iter()
            .map(|chapter| chapter.spine_index)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

#[tokio::test]
async fn a_non_linear_spine_item_is_reported_as_such() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::package(
                "    <item id=\"ch1\" href=\"text/chap1.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n    \
                 <item id=\"note\" href=\"text/note.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n",
                "    <itemref idref=\"ch1\"/>\n    <itemref idref=\"note\" linear=\"no\"/>\n",
            ),
        )
        .add(common::CHAP1, common::chapter_xhtml("One", "a"))
        .add("OEBPS/text/note.xhtml", common::chapter_xhtml("Note", "n"))
        .build();

    let events = harness.parse_ok(&archive).await;
    let chapters = common::chapters(&events);
    assert!(chapters[0].linear);
    assert!(!chapters[1].linear, "linear=\"no\" is auxiliary content");
}

#[tokio::test]
async fn stylesheets_are_excluded_by_default_and_included_on_request() {
    let harness = common::start().await;
    let archive = common::with_stylesheet();

    let events = harness.parse_ok(&archive).await;
    let hrefs: Vec<&str> = common::resources(&events)
        .iter()
        .map(|resource| resource.href.as_str())
        .collect();
    assert_eq!(hrefs, [common::COVER], "css is off unless asked for");
    let status = common::status(&events);
    assert_eq!(status.resources_skipped, 1);
    assert_eq!(
        status.warnings.len(),
        1,
        "one warning per excluded kind, not one per file"
    );
    assert_eq!(
        status.warnings[0].code,
        pb::ParseWarningCode::ResourceKindExcluded as i32
    );

    let events = harness
        .parse(
            &archive,
            pb::ParseOptions {
                include_stylesheets: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");
    let mut hrefs: Vec<&str> = common::resources(&events)
        .iter()
        .map(|resource| resource.href.as_str())
        .collect();
    hrefs.sort_unstable();
    assert_eq!(hrefs, [common::COVER, common::STYLESHEET]);
    assert_eq!(common::status(&events).resources_skipped, 0);
}

#[tokio::test]
async fn include_images_false_drops_the_image_but_keeps_the_chapters() {
    let harness = common::start().await;
    let events = harness
        .parse(
            &common::minimal(),
            pb::ParseOptions {
                include_images: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");

    assert!(common::resources(&events).is_empty());
    assert_eq!(common::chapters(&events).len(), 2);
    assert_eq!(common::status(&events).resources_skipped, 1);
    // The cover is still *named*, so a client can ask for it another way.
    assert_eq!(common::info(&events).cover_href, common::COVER);
}

/// Where the caller splits the upload must be invisible in the output.
#[tokio::test]
async fn chunk_boundaries_do_not_change_the_events() {
    let harness = common::start().await;
    let archive = common::minimal();
    let whole = harness.parse_ok(&archive).await;

    for chunk_size in [1, 7, 64, 512, 4096] {
        let split = harness
            .parse_chunked(&archive, pb::ParseOptions::default(), chunk_size)
            .await
            .unwrap_or_else(|e| panic!("chunk size {chunk_size} failed: {e}"));
        assert_eq!(
            split, whole,
            "a {chunk_size}-byte chunking produced a different stream"
        );
    }
}

/// An EPUB 2 book: no `properties`, cover named by `<meta name="cover">`, NCX
/// in the manifest but not in the spine.
#[tokio::test]
async fn an_epub_2_book_parses_with_its_own_conventions() {
    let harness = common::start().await;
    let manifest = "    <item id=\"ch1\" href=\"chap1.xhtml\" \
                    media-type=\"application/xhtml+xml\"/>\n    \
                    <item id=\"ncx\" href=\"toc.ncx\" \
                    media-type=\"application/x-dtbncx+xml\"/>\n    \
                    <item id=\"cov\" href=\"cover.png\" media-type=\"image/png\"/>\n";
    let opf = common::package(manifest, "    <itemref idref=\"ch1\"/>\n").replace(
        "<dc:subject>Computing</dc:subject>",
        "<dc:subject>Computing</dc:subject>\n    <meta name=\"cover\" content=\"cov\"/>",
    );

    let archive = common::Builder::new()
        .add_stored("mimetype", "application/epub+zip")
        .add("META-INF/container.xml", common::container_xml("book.opf"))
        .add("book.opf", opf)
        .add("chap1.xhtml", common::chapter_xhtml("One", "a"))
        .add("toc.ncx", "<ncx><navMap/></ncx>")
        .add("cover.png", common::IMAGE)
        .build();

    let events = harness.parse_ok(&archive).await;
    assert_eq!(common::info(&events).cover_href, "cover.png");
    assert_eq!(common::info(&events).opf_href, "book.opf");

    // The NCX is markup, so it is emitted by default even though it is not in
    // the spine; the cover is an image, so it is too.
    let mut kinds: Vec<i32> = common::resources(&events)
        .iter()
        .map(|resource| resource.kind)
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            pb::ResourceKind::Document as i32,
            pb::ResourceKind::Image as i32
        ]
    );
}

/// Percent-encoded hrefs resolve to the entry names they name.
#[tokio::test]
async fn a_percent_encoded_href_finds_its_entry() {
    let harness = common::start().await;
    let archive = common::shell()
        .add(
            common::OPF_PATH,
            common::opf_xml(&[("ch1", "text/chapter%20one.xhtml")], &[]),
        )
        .add(
            "OEBPS/text/chapter one.xhtml",
            common::chapter_xhtml("One", "a"),
        )
        .build();

    let events = harness.parse_ok(&archive).await;
    assert_eq!(
        common::chapters(&events)[0].href,
        "OEBPS/text/chapter one.xhtml"
    );
}

#[tokio::test]
async fn get_service_info_reports_the_limits_in_force() {
    let harness = common::start().await;
    let mut client = harness.client.clone();
    let info = client
        .get_service_info(pb::GetServiceInfoRequest {})
        .await
        .expect("service info")
        .into_inner();

    assert_eq!(info.name, "grpc-epub");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert!(info.features.contains(&"diskless".to_owned()));

    let limits = info.limits.expect("limits are always reported");
    assert_eq!(
        limits.max_document_mib,
        grpc_epub::limits::DEFAULT_MAX_DOCUMENT_MIB
    );
    assert_eq!(
        limits.max_uncompressed_mib,
        grpc_epub::limits::DEFAULT_MAX_UNCOMPRESSED_MIB
    );
    assert_eq!(limits.max_entries, grpc_epub::limits::DEFAULT_MAX_ENTRIES);
    assert_eq!(
        limits.max_compression_ratio,
        grpc_epub::limits::DEFAULT_MAX_COMPRESSION_RATIO
    );
    assert!(limits.max_chunk_bytes > 0);

    let ui = info.ui.expect("ui advertisement is always reported");
    assert_eq!(ui.title, "EPUB");
    assert_eq!(ui.path, "/ui/epub");
    assert!(!ui.description.is_empty());
}
