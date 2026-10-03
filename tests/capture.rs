// SPDX-License-Identifier: Apache-2.0

//! What a book can say about itself, end to end.
//!
//! The parsers are unit-tested in `src/nav.rs`, `src/smil.rs` and `src/opf.rs`
//! against bytes the tests hand them directly. What is tested here is the
//! *wiring*: that a real archive going through a real server produces the
//! navigation and overlay events, that they arrive where the contract says
//! they do, that they reach `Document.outline` and `Document.source_meta`, and
//! that the options which turn them off turn them all the way off.

mod common;

use grpc_epub::document_fold::integrity_errors;
use grpc_epub::proto::v1 as pb;
use prost_types::value::Kind;

/// Options that ask for the projection and nothing else.
fn with_document() -> pb::ParseOptions {
    pb::ParseOptions {
        emit_document: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn the_navigation_document_arrives_after_info_and_before_the_chapters() {
    let harness = common::start().await;
    let events = harness.parse_ok(&common::with_nav()).await;

    assert_eq!(
        common::shape(&events),
        [
            "info",
            "navigation",
            "resource",
            "chapter",
            "chapter",
            "resource",
            "status"
        ],
        "navigation is contracted to precede the chapters; the nav document's own bytes \
         still go out as a resource when its entry is reached"
    );

    let navigation = common::navigation(&events).expect("a navigation event");
    assert_eq!(navigation.source_href, "OEBPS/nav.xhtml");
    assert!(!navigation.from_ncx);
    assert_eq!(navigation.toc.len(), 1, "one top-level entry, one nested");
    assert_eq!(navigation.toc[0].label, "Chapter One");
    assert_eq!(navigation.toc[0].href, "OEBPS/text/chap1.xhtml");
    assert_eq!(navigation.toc[0].children.len(), 1);
    assert_eq!(
        navigation.toc[0].children[0].href,
        "OEBPS/text/chap2.xhtml#part2"
    );
    assert_eq!(navigation.toc[0].children[0].depth, 1);
}

#[tokio::test]
async fn the_nav_document_bytes_are_still_delivered_verbatim() {
    let harness = common::start().await;
    let events = harness.parse_ok(&common::with_nav()).await;

    let nav = common::resources(&events)
        .into_iter()
        .find(|resource| resource.href == "OEBPS/nav.xhtml")
        .expect("the nav document is an ordinary resource too");
    assert_eq!(
        nav.content,
        common::nav_xhtml().into_bytes(),
        "parsing a document is not a licence to stop shipping it"
    );
    assert_eq!(nav.kind, pb::ResourceKind::Document as i32);
    assert_eq!(nav.properties, ["nav"]);
}

#[tokio::test]
async fn reading_the_navigation_charges_the_budget_once() {
    let harness = common::start().await;
    let with = harness.parse_ok(&common::with_nav()).await;
    let without = harness
        .parse(
            &common::with_nav(),
            pb::ParseOptions {
                parse_navigation: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");

    // The nav document is inflated before the chapters so it can be parsed,
    // and handed back rather than re-inflated when the walk reaches it. If it
    // were inflated twice, the parsing run would show it.
    assert_eq!(
        common::status(&with).uncompressed_bytes,
        common::status(&without).uncompressed_bytes,
        "the same archive costs the same to unpack whether or not its outline is read"
    );
    assert_eq!(
        common::status(&with).entries_read,
        common::status(&without).entries_read
    );
}

/// An EPUB 3 book that lists its navigation document in the spine, as many
/// do so a reader can page to the table of contents.
fn nav_in_spine() -> Vec<u8> {
    common::shell()
        .add(
            common::OPF_PATH,
            common::package(
                "    <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" \
                 properties=\"nav\"/>\n    \
                 <item id=\"ch1\" href=\"text/chap1.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n    \
                 <item id=\"ch2\" href=\"text/chap2.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n",
                "    <itemref idref=\"nav\"/>\n    <itemref idref=\"ch1\"/>\n    \
                 <itemref idref=\"ch2\"/>\n",
            ),
        )
        .add(common::NAV, common::nav_xhtml())
        .add(
            common::CHAP1,
            common::chapter_xhtml("Chapter One", "The first chapter."),
        )
        .add(
            common::CHAP2,
            common::chapter_xhtml("Chapter Two", "The second chapter."),
        )
        .build()
}

/// A navigation document in the spine is read ahead to be parsed and then
/// sent as its chapter from those same bytes, not inflated a second time.
#[tokio::test]
async fn a_navigation_document_in_the_spine_is_inflated_once() {
    let harness = common::start().await;
    let with = harness.parse_ok(&nav_in_spine()).await;
    let without = harness
        .parse(
            &nav_in_spine(),
            pb::ParseOptions {
                parse_navigation: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");

    assert!(common::navigation(&with).is_some());
    let chapters = common::chapters(&with);
    assert_eq!(chapters[0].href, common::NAV);
    assert_eq!(chapters[0].content, common::nav_xhtml().into_bytes());
    assert!(
        common::resources(&with).is_empty(),
        "the nav document is a chapter here, so it is not a resource as well"
    );
    assert_eq!(
        common::status(&with).uncompressed_bytes,
        common::status(&without).uncompressed_bytes,
        "reading the outline must not charge the nav document twice"
    );
    assert_eq!(
        common::status(&with).entries_read,
        common::status(&without).entries_read
    );
}

/// The default book narrated chapter by chapter from one shared overlay.
fn shared_overlay() -> Vec<u8> {
    let manifest = "\
    <item id=\"ch1\" href=\"text/chap1.xhtml\" media-type=\"application/xhtml+xml\" \
     media-overlay=\"ov\"/>\n\
    <item id=\"ch2\" href=\"text/chap2.xhtml\" media-type=\"application/xhtml+xml\" \
     media-overlay=\"ov\"/>\n\
    <item id=\"ov\" href=\"overlays/chap1.smil\" media-type=\"application/smil+xml\"/>\n";
    common::shell()
        .add(
            common::OPF_PATH,
            common::package(
                manifest,
                "    <itemref idref=\"ch1\"/>\n    <itemref idref=\"ch2\"/>\n",
            ),
        )
        .add(common::OVERLAY, common::smil_xml())
        .add(
            common::CHAP1,
            common::chapter_xhtml("Chapter One", "The first chapter."),
        )
        .add(
            common::CHAP2,
            common::chapter_xhtml("Chapter Two", "The second chapter."),
        )
        .build()
}

/// An overlay read for its cues is not held for the rest of the call when
/// the walk will not send it, and is inflated once however many chapters it
/// narrates.
///
/// SMIL classifies as `Other`, which the default options do not send, so
/// held for the walk it would have sat in memory until the call ended, every
/// overlay of a narrated book at once. Debug builds assert at the end of
/// every parse that nothing read ahead is still held.
#[tokio::test]
async fn an_overlay_read_for_its_cues_is_inflated_once_and_not_held() {
    let harness = common::start().await;
    let overlays_on = pb::ParseOptions {
        parse_media_overlays: Some(true),
        ..Default::default()
    };

    let parsed = harness
        .parse(&shared_overlay(), overlays_on)
        .await
        .expect("the book should parse");
    assert_eq!(
        common::overlays(&parsed).len(),
        2,
        "one per narrated chapter"
    );
    assert!(
        common::resources(&parsed).is_empty(),
        "SMIL is not sent by default"
    );
    let unparsed = harness.parse_ok(&shared_overlay()).await;
    assert_eq!(
        common::status(&parsed).entries_read,
        common::status(&unparsed).entries_read + 1,
        "the shared overlay is inflated once, not once per chapter"
    );

    // Asked for as well, its bytes go out once, from the same inflation.
    let everything = pb::ParseOptions {
        parse_media_overlays: Some(true),
        include_all_resources: Some(true),
        ..Default::default()
    };
    let sent = harness
        .parse(&shared_overlay(), everything)
        .await
        .expect("the book should parse");
    let smil: Vec<&pb::Resource> = common::resources(&sent)
        .into_iter()
        .filter(|resource| resource.href == common::OVERLAY)
        .collect();
    assert_eq!(smil.len(), 1);
    assert_eq!(smil[0].content, common::smil_xml().into_bytes());
    assert_eq!(
        common::status(&sent).entries_read,
        common::status(&parsed).entries_read
    );
}

#[tokio::test]
async fn an_epub_2_book_falls_back_to_its_ncx() {
    let harness = common::start().await;
    let events = harness.parse_ok(&common::with_ncx()).await;

    let navigation = common::navigation(&events).expect("a navigation event");
    assert!(
        navigation.from_ncx,
        "no manifest carries a `nav` property, so the spine's `toc` is the only pointer"
    );
    assert_eq!(navigation.source_href, "OEBPS/toc.ncx");
    assert_eq!(navigation.toc[0].label, "Chapter One");
    assert_eq!(navigation.toc[0].children[0].label, "Chapter Two");

    let info = common::info(&events);
    assert_eq!(info.ncx_href, "OEBPS/toc.ncx");
    assert_eq!(info.nav_href, "", "this book has no navigation document");
}

#[tokio::test]
async fn turning_navigation_off_removes_the_event_and_the_outline() {
    let harness = common::start().await;
    let events = harness
        .parse(
            &common::with_nav(),
            pb::ParseOptions {
                emit_document: true,
                parse_navigation: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");

    assert!(common::navigation(&events).is_none());
    let document = common::documents(&events)[0];
    assert!(document.outline.is_empty());
    assert!(integrity_errors(document).is_empty());
}

#[tokio::test]
async fn the_outline_points_at_the_chapter_groups_it_names() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::with_nav(), with_document())
        .await
        .expect("the book should parse");
    let document = common::documents(&events)[0];

    let titles: Vec<&str> = document
        .outline
        .iter()
        .map(|entry| entry.title.as_str())
        .collect();
    assert_eq!(titles, ["Chapter One", "Chapter Two"]);
    assert_eq!(document.outline[0].level, 0);
    assert_eq!(document.outline[1].level, 1);

    assert_eq!(
        document.outline[0]
            .target
            .as_ref()
            .map(|t| t.r#ref.as_str()),
        Some("#/groups/0")
    );
    assert_eq!(
        document.outline[1]
            .target
            .as_ref()
            .map(|t| t.r#ref.as_str()),
        Some("#/groups/1"),
        "the entry's fragment names a place in chapter two, which is still chapter two"
    );
    assert!(integrity_errors(document).is_empty());
}

#[tokio::test]
async fn the_document_carries_the_books_own_metadata_typed() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::minimal(), with_document())
        .await
        .expect("the book should parse");
    let info = common::info(&events);
    let document = common::documents(&events)[0];

    let meta = document.source_meta.as_ref().expect("source meta");
    assert_eq!(meta.title.as_deref(), Some(info.title.as_str()));
    assert_eq!(meta.authors, info.creators);
    assert_eq!(
        meta.created,
        grpc_epub::datetime::parse("1843-10-01"),
        "the book's dc:date, read as the instant it names"
    );
    assert_eq!(
        meta.created_raw.as_deref(),
        Some("1843-10-01"),
        "and its own spelling, kept beside the reading"
    );
    assert_eq!(meta.language.as_deref(), Some("en-GB"));
    assert_eq!(meta.keywords, ["Computing"]);

    // Dublin Core's open half, which no typed field claims.
    assert_eq!(meta.extra["epub.publisher"], "Analytical Press");
    assert_eq!(meta.extra["epub.identifier.0"], "urn:isbn:9780000000000");
    assert_eq!(meta.extra["epub.identifier.0.scheme"], "ISBN");

    let body = document
        .body
        .as_ref()
        .expect("a body")
        .meta
        .as_ref()
        .expect("body meta");
    let language = body.language.as_ref().expect("a typed language");
    assert_eq!(language.code_raw.as_deref(), Some("en-GB"));
    assert_eq!(
        body.keywords.as_ref().expect("typed keywords").values,
        ["Computing"]
    );

    // The string copies the typed fields replaced are gone from both maps.
    for typed in [
        "epub.language",
        "epub.subjects",
        "epub.creators",
        "epub.date",
    ] {
        assert!(!body.custom_fields.contains_key(typed), "{typed} is typed");
        assert!(!meta.extra.contains_key(typed), "{typed} is typed");
    }
}

#[tokio::test]
async fn a_book_whose_dates_parse_reaches_the_document_as_two_instants() {
    let harness = common::start().await;
    let events = harness
        .parse(
            &common::dated("1843-10-01", "2026-08-25T09:41:07Z"),
            with_document(),
        )
        .await
        .expect("the book should parse");
    let meta = common::documents(&events)[0]
        .source_meta
        .as_ref()
        .expect("source meta");

    assert_eq!(
        meta.created.expect("dc:date parses").seconds,
        -3_984_163_200
    );
    assert_eq!(meta.created_raw.as_deref(), Some("1843-10-01"));
    assert_eq!(
        meta.modified.expect("dcterms:modified parses").seconds,
        1_787_650_867
    );
    assert_eq!(meta.modified_raw.as_deref(), Some("2026-08-25T09:41:07Z"));
}

#[tokio::test]
async fn a_book_whose_dates_are_prose_reaches_the_document_raw_and_only_raw() {
    let harness = common::start().await;
    let events = harness
        .parse(
            &common::dated("sometime in the 1840s", "whenever this was last touched"),
            with_document(),
        )
        .await
        .expect("the book should parse");
    let meta = common::documents(&events)[0]
        .source_meta
        .as_ref()
        .expect("source meta");

    assert!(
        meta.created.is_none() && meta.modified.is_none(),
        "a producer's prose is not an instant and is not turned into one"
    );
    assert_eq!(meta.created_raw.as_deref(), Some("sometime in the 1840s"));
    assert_eq!(
        meta.modified_raw.as_deref(),
        Some("whenever this was last touched"),
        "nothing the book wrote is dropped for failing to parse"
    );
}

#[tokio::test]
async fn the_origin_hash_is_the_archive_and_two_uploads_of_it_agree() {
    let harness = common::start().await;
    let archive = common::minimal();

    let first = harness
        .parse(&archive, with_document())
        .await
        .expect("the book should parse");
    let second = harness
        .parse(&archive, with_document())
        .await
        .expect("the book should parse");

    let hash = |events: &[pb::parse_epub_response::Event]| {
        common::documents(events)[0]
            .origin
            .as_ref()
            .expect("an origin")
            .binary_hash
    };
    assert_ne!(hash(&first), 0, "the bytes are in hand; the key is free");
    assert_eq!(hash(&first), hash(&second));

    let other = harness
        .parse(&common::with_nav(), with_document())
        .await
        .expect("the book should parse");
    assert_ne!(
        hash(&first),
        hash(&other),
        "a different archive is a different key"
    );
}

#[tokio::test]
async fn a_narrated_chapter_says_so_without_the_overlay_being_parsed() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::narrated(), with_document())
        .await
        .expect("the book should parse");

    assert!(
        common::overlays(&events).is_empty(),
        "parsing the SMIL is opt-in"
    );

    // Which chapter has narration is a manifest fact, so it costs nothing.
    let chapters = common::chapters(&events);
    assert_eq!(chapters[0].media_overlay_href, "OEBPS/overlays/chap1.smil");
    assert_eq!(chapters[1].media_overlay_href, "");

    let document = common::documents(&events)[0];
    let fields = &document.groups[0]
        .meta
        .as_ref()
        .expect("chapter meta")
        .custom_fields;
    assert_eq!(
        fields["epub.media_overlay_href"].kind,
        Some(Kind::StringValue("OEBPS/overlays/chap1.smil".to_owned()))
    );
    assert!(
        !fields.contains_key("epub.media_overlay_cues"),
        "no cues were read, so none are claimed"
    );
    assert!(document.media.is_none());
}

#[tokio::test]
async fn asking_for_media_overlays_yields_cue_level_timings() {
    let harness = common::start().await;
    let events = harness
        .parse(
            &common::narrated(),
            pb::ParseOptions {
                emit_document: true,
                parse_media_overlays: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");

    let overlays = common::overlays(&events);
    assert_eq!(overlays.len(), 1, "one narrated chapter, one overlay");
    let overlay = overlays[0];
    assert_eq!(overlay.source_href, "OEBPS/overlays/chap1.smil");
    assert_eq!(overlay.chapter_href, "OEBPS/text/chap1.xhtml");
    assert_eq!(overlay.cues.len(), 2);
    assert_eq!(overlay.cues[0].text_href, "OEBPS/text/chap1.xhtml#s1");
    assert_eq!(
        overlay.cues[0].audio_href, "OEBPS/audio/chap1.mp3",
        "resolved against the overlay's own directory, not the OPF's"
    );
    assert!((overlay.cues[0].end_time - 12.5).abs() < f64::EPSILON);
    assert!((overlay.cues[1].start_time - 12.5).abs() < f64::EPSILON);

    // The alignment is available even though the recording itself was not
    // requested: audio is excluded by the default include options.
    assert!(
        !common::resources(&events)
            .iter()
            .any(|resource| resource.href == "OEBPS/audio/chap1.mp3"),
        "the audio bytes are still opt-in"
    );

    let document = common::documents(&events)[0];
    assert_eq!(
        document
            .media
            .as_ref()
            .expect("media meta")
            .duration_ms
            .expect("a duration"),
        20_000.0
    );
    let fields = &document.groups[0]
        .meta
        .as_ref()
        .expect("chapter meta")
        .custom_fields;
    let Some(Kind::ListValue(cues)) = fields["epub.media_overlay_cues"].kind.as_ref() else {
        panic!("the cues are a list");
    };
    assert_eq!(cues.values.len(), 2);
    assert!(integrity_errors(document).is_empty());
}

#[tokio::test]
async fn a_book_with_neither_navigation_nor_overlays_streams_exactly_as_before() {
    let harness = common::start().await;
    let events = harness.parse_ok(&common::minimal()).await;
    assert_eq!(
        common::shape(&events),
        ["info", "chapter", "chapter", "resource", "status"],
        "the new events are added where there is something to say and nowhere else"
    );
}

#[tokio::test]
async fn the_build_advertises_what_it_can_now_read() {
    let harness = common::start().await;
    let info = harness
        .client
        .clone()
        .get_service_info(pb::GetServiceInfoRequest {})
        .await
        .expect("service info")
        .into_inner();

    for capability in ["navigation", "media-overlays"] {
        assert!(
            info.features.contains(&capability.to_owned()),
            "a client should branch on the capability, not the version: {:?}",
            info.features
        );
    }
}
