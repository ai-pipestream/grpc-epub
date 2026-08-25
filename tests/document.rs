// SPDX-License-Identifier: Apache-2.0

//! The `emit_document` option, end to end: a real server, the generated
//! client, and the Document event that folds a whole book.
//!
//! The fold itself is unit-tested in `src/document_fold.rs` against
//! synthesized events. What is tested here is the *wiring*: that the server
//! folds the same events it puts on the wire, that the Document arrives once
//! and immediately before the trailer, and that asking for nothing changes
//! nothing.

mod common;

use grpc_epub::document_fold::integrity_errors;
use grpc_epub::proto::document_v1 as doc;
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
async fn the_document_arrives_once_immediately_before_the_trailer() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::minimal(), with_document())
        .await
        .expect("the book should parse");

    assert_eq!(
        common::shape(&events),
        [
            "info", "chapter", "chapter", "resource", "document", "status"
        ],
        "the Document is folded from the events above it and still leaves the trailer last"
    );

    let documents = common::documents(&events);
    assert_eq!(documents.len(), 1, "exactly one Document per stream");
    assert_eq!(
        integrity_errors(documents[0]),
        Vec::<String>::new(),
        "the fragment must survive the coordinator's additive merge"
    );
}

#[tokio::test]
async fn the_document_matches_the_typed_events_it_was_folded_from() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::minimal(), with_document())
        .await
        .expect("the book should parse");

    let info = common::info(&events);
    let chapters = common::chapters(&events);
    let resources = common::resources(&events);
    let document = common::documents(&events)[0];

    assert_eq!(document.name, info.title);
    assert_eq!(document.schema_name.as_deref(), Some("docling_document_v2"));
    assert_eq!(
        document
            .origin
            .as_ref()
            .map(|origin| origin.mimetype.as_str()),
        Some("application/epub+zip")
    );

    // One chapter group per spine item, named by href, in spine order and
    // empty: the XHTML belongs to the HTML collector.
    assert_eq!(document.groups.len(), chapters.len());
    for (group, chapter) in document.groups.iter().zip(&chapters) {
        assert_eq!(group.label, doc::GroupLabel::Chapter as i32);
        assert_eq!(group.name.as_deref(), Some(chapter.href.as_str()));
        assert!(group.children.is_empty());
    }

    // One picture per image resource, pointing back at the stream that
    // carried the bytes rather than repeating them.
    assert_eq!(document.pictures.len(), resources.len());
    let image = document.pictures[0]
        .image
        .as_ref()
        .expect("a picture has an image ref");
    assert_eq!(image.uri, format!("epub:{}", resources[0].href));
    assert_eq!(image.mimetype, resources[0].media_type);
    assert!(
        !image.uri.contains("base64"),
        "bytes stay on the typed stream; the Document is one bounded message"
    );

    let cover = &document.pictures[0]
        .meta
        .as_ref()
        .expect("picture meta")
        .custom_fields;
    assert_eq!(
        cover["epub.cover"].kind,
        Some(Kind::BoolValue(true)),
        "info names the cover before the resource arrives, so the match works in one pass"
    );

    // The metadata the OPF carried. What the schema types is on the typed
    // field; the body group's custom fields hold only what it does not.
    let meta = document
        .body
        .as_ref()
        .expect("a body")
        .meta
        .as_ref()
        .expect("body meta");
    assert_eq!(
        meta.language
            .as_ref()
            .expect("a typed language")
            .code_raw
            .as_deref(),
        Some(info.language.as_str())
    );
    assert!(
        !meta.custom_fields.contains_key("epub.language"),
        "the language has a typed home, so it is not a string beside it too"
    );
    assert_eq!(
        meta.custom_fields["epub.opf_href"].kind,
        Some(Kind::StringValue(info.opf_href.clone()))
    );
}

#[tokio::test]
async fn the_source_stamp_names_this_collector_and_build() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::minimal(), with_document())
        .await
        .expect("the book should parse");
    let document = common::documents(&events)[0];

    let source = &document.pictures[0].source;
    assert_eq!(source.len(), 1);
    let Some(doc::source_type::Source::Collector(collector)) = source[0].source.as_ref() else {
        panic!("the only source is a collector");
    };
    assert_eq!(collector.collector, "epub");
    assert_eq!(
        collector.version.as_deref(),
        Some(env!("CARGO_PKG_VERSION")),
        "the stamp is the running build, the same string GetServiceInfo reports"
    );
    assert!(collector.model.is_none());
    assert!(collector.confidence.is_none());
    assert!(
        collector.raw_score.is_none() && collector.raw_score_kind.is_none(),
        "a raw score is the signal behind a confidence there is none of"
    );
}

#[tokio::test]
async fn a_book_whose_images_were_excluded_still_yields_its_chapter_groups() {
    let harness = common::start().await;
    let events = harness
        .parse(
            &common::minimal(),
            pb::ParseOptions {
                emit_document: true,
                include_images: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("the book should parse");

    assert_eq!(
        common::shape(&events),
        ["info", "chapter", "chapter", "document", "status"],
        "no resource event, so nothing to project a picture from"
    );

    let document = common::documents(&events)[0];
    assert_eq!(document.groups.len(), 2, "the spine is still the spine");
    assert!(
        document.pictures.is_empty(),
        "the fold projects what was emitted, not what the manifest listed"
    );
    assert!(integrity_errors(document).is_empty());
}

#[tokio::test]
async fn without_the_option_the_stream_is_unchanged() {
    let harness = common::start().await;
    let asked = harness
        .parse(&common::minimal(), with_document())
        .await
        .expect("the book should parse");
    let plain = harness.parse_ok(&common::minimal()).await;

    assert!(
        common::documents(&plain).is_empty(),
        "the projection is opt-in"
    );
    assert_eq!(
        common::shape(&plain),
        ["info", "chapter", "chapter", "resource", "status"]
    );

    // Same events, in the same order, with one added: the option adds to the
    // stream and changes nothing that was already there.
    let without_document: Vec<_> = asked
        .into_iter()
        .filter(|event| !matches!(event, pb::parse_epub_response::Event::Document(_)))
        .collect();
    assert_eq!(without_document, plain);
}

#[tokio::test]
async fn a_resource_stored_before_the_chapters_folds_in_arrival_order() {
    let harness = common::start().await;
    let events = harness
        .parse(&common::image_first(), with_document())
        .await
        .expect("the book should parse");

    assert_eq!(
        common::shape(&events),
        [
            "info", "resource", "chapter", "chapter", "document", "status"
        ]
    );

    // The same book packed the other way round: the fold appends as events
    // arrive and never reorders, so the picture leads the body here and
    // trails it in `the_document_matches_the_typed_events_it_was_folded_from`.
    let document = common::documents(&events)[0];
    let children: Vec<&str> = document
        .body
        .as_ref()
        .expect("a body")
        .children
        .iter()
        .map(|child| child.r#ref.as_str())
        .collect();
    assert_eq!(children, ["#/pictures/0", "#/groups/0", "#/groups/1"]);
    assert!(integrity_errors(document).is_empty());
}

#[tokio::test]
async fn the_build_advertises_the_projection() {
    let harness = common::start().await;
    let info = harness
        .client
        .clone()
        .get_service_info(pb::GetServiceInfoRequest {})
        .await
        .expect("service info")
        .into_inner();

    assert!(
        info.features.contains(&"document-fold".to_owned()),
        "a client should be able to branch on the capability, not the version: {:?}",
        info.features
    );
}

/// A stream that fails carries no Document.
///
/// The fold is fed from the emission path, so a call that never reaches its
/// trailer never reaches the projection either: a half-folded book would be a
/// fragment claiming to be a book.
#[tokio::test]
async fn a_failed_parse_emits_no_document() {
    let harness = common::start().await;
    let archive = common::Builder::new()
        .add("notes.txt", "just a zip of some files")
        .build();

    let status = harness
        .parse(&archive, with_document())
        .await
        .expect_err("a ZIP that is not an EPUB is refused");
    assert_eq!(status.code(), tonic::Code::Unimplemented, "{status:?}");
}
