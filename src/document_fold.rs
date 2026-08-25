// SPDX-License-Identifier: Apache-2.0

//! The Document fold: this stream's own events projected into one
//! [`ai.pipestream.document.v1.Document`](crate::proto::document_v1::Document).
//!
//! # What this is and is not
//!
//! The typed event stream is the lossless wire. The Document is a **lossy
//! structural projection** of it, produced only when the caller sets
//! `ParseOptions.emit_document`, and sent as one `document` event immediately
//! before the `status` trailer.
//!
//! What it projects is the **skeleton of the book**, and nothing below it:
//!
//! - the OPF metadata, typed into `Document.source_meta` and the body group's
//!   `BaseMeta` wherever the schema types it, and the open-vocabulary
//!   remainder of Dublin Core on the body group's `meta.custom_fields`;
//! - the book's own table of contents, as `Document.outline`, with each entry
//!   pointing at the chapter group it names;
//! - one `GROUP_LABEL_CHAPTER` group per spine item, in spine order, **with no
//!   children**;
//! - one `PictureItem` per emitted image resource, pointing at the bytes
//!   rather than carrying them.
//!
//! The chapter groups are empty on purpose. Chapter XHTML is never parsed
//! here — that is the HTML collector's job, and this service exists precisely
//! not to reimplement it (see `docs/design.md` §4). The groups are the
//! *sockets* the HTML collector's items are merged into downstream, which is
//! why they are emitted at all: without them the coordinator would have
//! nowhere to hang the chapters' contents and no record of reading order. An
//! empty-children group is therefore valid output, and
//! [`integrity_errors`] accepts it.
//!
//! # Shape of the fold
//!
//! Single pass, O(1) per event, no buffering and no reordering:
//! [`DocumentFold::consume`] takes the same `ParseEpubResponse` events the
//! server writes to the wire, in the order it writes them, and
//! [`DocumentFold::take`] hands back the Document at the end. Chapters and
//! resources arrive interleaved in archive order and are appended in arrival
//! order; the cover image is still recognised, because `info` — which names
//! it — always precedes both.
//!
//! # Merge safety
//!
//! The document is a self-contained fragment for the coordinator's additive
//! merge: dense 0-based `self_ref`s local to this fold, `parent` and
//! `children` always set in both directions, and no reference to anything
//! this fold did not create except the two roots `#/body` and `#/furniture`.
//! [`integrity_errors`] is the check, and every test in this module asserts it
//! is empty.
//!
//! Two caveats worth knowing at the call site:
//!
//! - **Root custom fields are first-writer-wins.** The `epub.*` keys live on
//!   the *body* group's `meta.custom_fields`, and the coordinator does not
//!   merge competing root metas: whichever collector's fragment lands first
//!   keeps its keys. That is why everything with a typed home is written to
//!   that home, in `Document.source_meta` and in `BaseMeta`'s own fields,
//!   which the merge and the query layer understand, and to nothing else:
//!   the string copies that used to sit beside them under `epub.*` are gone,
//!   and what remains under those keys is the part of Dublin Core that is
//!   genuinely open vocabulary.
//! - **No provenance.** An EPUB is reflowable and has no pages and no
//!   bounding boxes, so `prov` is left empty everywhere rather than filled
//!   with invented coordinates. Source locators — spine index, href, manifest
//!   id — go in `meta.custom_fields` instead.

use std::collections::{HashMap, HashSet};

use prost_types::value::Kind;
use prost_types::{ListValue, Struct, Value};

use crate::datetime;
use crate::extract::EPUB_MIMETYPE;
use crate::proto::document_v1 as doc;
use crate::proto::v1 as pb;

/// `CollectorSource.collector` for everything this fold creates.
pub const COLLECTOR: &str = "epub";

/// The upstream schema identifier this projection stamps on every Document.
///
/// A wire value rather than a description: consumers match on it, so it is
/// fixed by the schema this fold targets and not by anything about this
/// service.
pub const SCHEMA_NAME: &str = "docling_document_v2";

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The integrity key recorded as `DocumentOrigin.binary_hash`.
///
/// FNV-1a over the whole archive. Not a cryptographic digest and not offered
/// as one: the field is 64 bits, which rules that out by construction. What it
/// is good for is what the schema asks of it, deduplicating identical uploads
/// and noticing that two calls did not receive the same bytes.
///
/// Spelled out here rather than taken from `std`, because
/// `std::collections::hash_map::DefaultHasher` is explicitly not stable across
/// releases, and a hash that changes when the toolchain does is worse than no
/// hash at all: it would silently stop matching yesterday's records.
#[must_use]
pub fn source_hash(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Scheme of an [`ImageRef`](doc::ImageRef) `uri` produced by this fold.
///
/// A Document is one gRPC message and clients commonly cap receives at 4 MiB,
/// so image bytes never go inside it — not even the cover. `epub:` plus the
/// resolved archive path is a **pointer into this call's own typed stream**:
/// the `resource` event with that `href` carries the bytes. It is not a
/// registered URI scheme and nothing dereferences it over a network.
pub const URI_SCHEME: &str = "epub:";

/// JSON Pointer of the body group, the parent of everything this fold makes.
const BODY_REF: &str = "#/body";

/// JSON Pointer of the furniture group. Nothing is put in it: an EPUB has no
/// page chrome to speak of, having no pages.
const FURNITURE_REF: &str = "#/furniture";

/// Folds a `ParseEpub` response stream into one Document.
///
/// Feed it every event the server emits, in emission order, then call
/// [`take`](Self::take):
///
/// ```
/// use grpc_epub::document_fold::DocumentFold;
/// use grpc_epub::proto::v1 as pb;
///
/// let mut fold = DocumentFold::new("0.1.0");
/// fold.consume(&pb::parse_epub_response::Event::Info(pb::EpubInfo {
///     title: "A Tale of Two Chapters".to_owned(),
///     ..Default::default()
/// }));
/// let document = fold.take();
/// assert_eq!(document.name, "A Tale of Two Chapters");
/// ```
#[derive(Clone, Debug)]
pub struct DocumentFold {
    /// The document built so far.
    document: doc::Document,
    /// This server's version string, stamped on every `CollectorSource`.
    version: String,
    /// `EpubInfo.cover_href`, remembered so the matching picture can be
    /// flagged when its resource event arrives later in the stream.
    cover_href: String,
    /// Hash of the archive bytes, set before the first event.
    source_hash: u64,
    /// Chapter group `self_ref` by resolved href, so an outline entry can
    /// point at the group it names.
    ///
    /// The navigation event arrives before any chapter, so the targets cannot
    /// be resolved as the entries are read; the outline is projected at the
    /// end of the fold, when the spine is known.
    chapter_refs: HashMap<String, String>,
    /// The parsed table of contents, held until the chapter groups exist.
    outline: Vec<pb::NavPoint>,
    /// The parsed media overlays, held for the same reason: they name the
    /// chapter they narrate and arrive before it.
    overlays: Vec<pb::MediaOverlay>,
}

impl DocumentFold {
    /// A fold that attributes its items to `version` of this collector.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            document: skeleton(),
            version: version.into(),
            cover_href: String::new(),
            source_hash: 0,
            chapter_refs: HashMap::new(),
            outline: Vec::new(),
            overlays: Vec::new(),
        }
    }

    /// Record the archive's integrity key, before the first event.
    pub const fn set_source_hash(&mut self, hash: u64) {
        self.source_hash = hash;
    }

    /// A fold attributed to the running build, which is what the server uses.
    #[must_use]
    pub fn for_this_build() -> Self {
        Self::new(env!("CARGO_PKG_VERSION"))
    }

    /// Fold one outbound event in.
    ///
    /// Every variant is handled, and the ones that map to nothing say so:
    ///
    /// - `info` fills the document's name, origin and body metadata;
    /// - `chapter` appends one empty chapter group;
    /// - `resource` appends a picture **only for images**. Stylesheets, fonts,
    ///   audio, video, nav documents and anything else have no honest slot in
    ///   the schema to go in — inventing one would mean lying with a label —
    ///   and they are already on the typed stream in full, so they are
    ///   deliberately not projected;
    /// - `navigation` is held until the chapter groups exist, then becomes
    ///   `Document.outline`;
    /// - `media_overlay` contributes the narration's length; its cues stay on
    ///   the chapter group that owns them;
    /// - `status` is a receipt of counts already implied by the items above,
    ///   so there is nothing to map. It is still fed in, because it is the
    ///   signal that the fold is complete;
    /// - `document` is this fold's own output and is never folded again.
    pub fn consume(&mut self, event: &pb::parse_epub_response::Event) {
        match event {
            pb::parse_epub_response::Event::Info(info) => self.info(info),
            pb::parse_epub_response::Event::Chapter(chapter) => self.chapter(chapter),
            pb::parse_epub_response::Event::Resource(resource)
                if resource.kind == pb::ResourceKind::Image as i32 =>
            {
                self.picture(resource);
            }
            pb::parse_epub_response::Event::Navigation(navigation) => {
                self.outline.clone_from(&navigation.toc);
            }
            pb::parse_epub_response::Event::MediaOverlay(overlay) => {
                self.overlays.push(overlay.clone());
            }
            pb::parse_epub_response::Event::Resource(_)
            | pb::parse_epub_response::Event::Status(_)
            | pb::parse_epub_response::Event::Document(_) => {}
        }
    }

    /// The document built so far, without ending the fold.
    #[must_use]
    pub const fn document(&self) -> &doc::Document {
        &self.document
    }

    /// Take the folded document, leaving an empty one behind.
    ///
    /// The fold stays usable rather than being consumed, so it can live behind
    /// a shared reference in the emission path; what comes back is the whole
    /// book, and a second call returns an empty skeleton.
    pub fn take(&mut self) -> doc::Document {
        self.project_outline();
        self.project_overlays();
        self.cover_href.clear();
        self.chapter_refs.clear();
        std::mem::replace(&mut self.document, skeleton())
    }

    /// Turn the held navigation entries into `Document.outline`.
    ///
    /// Done at the end rather than as the event arrives, because `navigation`
    /// precedes every chapter and an entry's target is the chapter group it
    /// names: nothing to point at exists yet when it is read.
    fn project_outline(&mut self) {
        /// Append `point` and its children, depth-first in reading order.
        fn walk(
            point: &pb::NavPoint,
            chapters: &HashMap<String, String>,
            out: &mut Vec<doc::OutlineEntry>,
        ) {
            out.push(doc::OutlineEntry {
                title: point.label.clone(),
                level: i32::try_from(point.depth).unwrap_or(i32::MAX),
                // An EPUB is reflowable and has no pages, so there is no page
                // number to give. `page-list` books declare printed pages, but
                // that nav is not read yet.
                page_no: None,
                // An entry whose href names no spine item is a link to front
                // matter outside the spine, or to a file the archive does not
                // have. It keeps its title and gets no target rather than a
                // target that resolves to nothing.
                target: chapter_target(&point.href, chapters),
            });
            for child in &point.children {
                walk(child, chapters, out);
            }
        }

        let outline = std::mem::take(&mut self.outline);
        for point in &outline {
            walk(point, &self.chapter_refs, &mut self.document.outline);
        }
    }

    /// Fold the held overlays: cues onto their chapter, length onto the
    /// document.
    ///
    /// The cues cannot be attached where they belong. A cue addresses a
    /// fragment *inside* a chapter, and this fold emits chapter groups with no
    /// children, so the items a `SourceType::Track` would hang on do not exist
    /// until the HTML collector contributes them downstream. Until then the
    /// cues ride on the chapter group as the lossless tail, in the same shape
    /// `epub.identifiers` uses, and the typed `media_overlay` event carries
    /// them properly.
    ///
    /// What the Document *can* say in a typed slot is how long the narration
    /// runs, which is `MediaMeta.duration_ms`.
    fn project_overlays(&mut self) {
        let overlays = std::mem::take(&mut self.overlays);
        let mut narration_ms = 0.0f64;

        for overlay in &overlays {
            narration_ms = narration_ms.max(
                overlay
                    .cues
                    .iter()
                    .map(|cue| cue.end_time)
                    .fold(0.0, f64::max)
                    * 1000.0,
            );

            let Some(group_ref) = self.chapter_refs.get(&overlay.chapter_href).cloned() else {
                continue;
            };
            let cues = list(
                overlay
                    .cues
                    .iter()
                    .map(|cue| {
                        let mut entry = std::collections::BTreeMap::new();
                        entry.insert("text_href".to_owned(), text(&cue.text_href));
                        entry.insert("audio_href".to_owned(), text(&cue.audio_href));
                        entry.insert("start_time".to_owned(), number(cue.start_time));
                        entry.insert("end_time".to_owned(), number(cue.end_time));
                        if !cue.identifier.is_empty() {
                            entry.insert("id".to_owned(), text(&cue.identifier));
                        }
                        Value {
                            kind: Some(Kind::StructValue(Struct { fields: entry })),
                        }
                    })
                    .collect(),
            );
            if let Some(meta) = self.group_mut(&group_ref).meta.as_mut() {
                meta.custom_fields
                    .insert("epub.media_overlay_cues".to_owned(), cues);
            }
        }

        if narration_ms > 0.0 {
            self.document.media = Some(doc::MediaMeta {
                duration_ms: Some(narration_ms),
                // Who reads the book aloud is not something an overlay states;
                // it aligns text with audio and names no voice. `codec` would
                // mean decoding the audio, which nothing here does.
                speakers: Vec::new(),
                codec: None,
            });
        }
    }

    /// Map the opening `info` event: name, origin, and the OPF metadata.
    fn info(&mut self, info: &pb::EpubInfo) {
        self.cover_href.clone_from(&info.cover_href);
        self.document.name.clone_from(&info.title);
        self.document.origin = Some(doc::DocumentOrigin {
            // The archive's own declared media type. `filename` stays empty
            // and `uri` unset: this service is handed bytes on a gRPC stream
            // and is never told what the file was called.
            mimetype: EPUB_MIMETYPE.to_owned(),
            binary_hash: self.source_hash,
            filename: String::new(),
            uri: None,
            // Retrieval provenance belongs to a crawler; this service is
            // handed an archive over gRPC and never fetches anything.
            web: None,
        });
        self.document.source_meta = Some(self.source_meta(info));

        // The OPF facts that have no typed home, under `epub.` keys. A fact
        // the schema types is NOT written here as well: the title, the
        // creators, the language, the subjects and the two dates all have
        // first-class slots now, and a string copy beside a typed field is a
        // second answer that can disagree with the first. Empty fields are
        // omitted rather than written as empty strings, so a reader can tell
        // "the book did not say" from "the book said nothing useful".
        let mut fields = HashMap::new();
        insert_text(
            &mut fields,
            "epub.unique_identifier",
            &info.unique_identifier,
        );
        insert_text(&mut fields, "epub.publisher", &info.publisher);
        insert_text(&mut fields, "epub.description", &info.description);
        insert_text(&mut fields, "epub.epub_version", &info.epub_version);
        insert_text(&mut fields, "epub.opf_href", &info.opf_href);
        insert_strings(&mut fields, "epub.contributors", &info.contributors);
        if !info.identifiers.is_empty() {
            fields.insert(
                "epub.identifiers".to_owned(),
                list(
                    info.identifiers
                        .iter()
                        .map(|identifier| {
                            let mut entry = std::collections::BTreeMap::new();
                            entry.insert("value".to_owned(), text(&identifier.value));
                            if !identifier.scheme.is_empty() {
                                entry.insert("scheme".to_owned(), text(&identifier.scheme));
                            }
                            Value {
                                kind: Some(Kind::StructValue(Struct { fields: entry })),
                            }
                        })
                        .collect(),
                ),
            );
        }

        insert_text(&mut fields, "epub.nav_href", &info.nav_href);
        insert_text(&mut fields, "epub.ncx_href", &info.ncx_href);
        // The six Dublin Core elements the old allow-list dropped, plus the
        // creator roles the EPUB 3 expression model carries. Nothing in the
        // schema has a typed home for any of them.
        for (key, element) in [
            ("epub.rights", "rights"),
            ("epub.source", "source"),
            ("epub.type", "type"),
            ("epub.format", "format"),
            ("epub.coverage", "coverage"),
            ("epub.relation", "relation"),
        ] {
            insert_text(&mut fields, key, first(info, element));
        }
        if let Some(credits) = credits(info) {
            fields.insert("epub.creator_roles".to_owned(), credits);
        }

        // `LanguageMetaField` and `KeywordsMetaField` are what the merge and
        // the query layer understand, and they are now the only place the
        // language and the subjects are written: the `epub.language` and
        // `epub.subjects` custom fields they replaced are gone.
        self.group_mut(BODY_REF).meta = Some(doc::BaseMeta {
            language: (!info.language.is_empty()).then(|| doc::LanguageMetaField {
                code: language_code(&info.language) as i32,
                // Always set, whether or not the enum had a variant: the tag
                // the book wrote is the fact, and the enum is a convenience
                // over it.
                code_raw: Some(info.language.clone()),
                created_by: Some(COLLECTOR.to_owned()),
                // The book declared it; nothing here detected it, so there is
                // no confidence to report.
                confidence: None,
                custom_fields: HashMap::new(),
            }),
            keywords: (!info.subjects.is_empty()).then(|| doc::KeywordsMetaField {
                values: info.subjects.clone(),
                custom_fields: HashMap::new(),
            }),
            custom_fields: fields,
            ..doc::BaseMeta::default()
        });
    }

    /// The book's own account of itself, typed.
    ///
    /// Everything with a first-class slot goes in one, and *only* in one.
    /// `extra` is for what Dublin Core leaves genuinely open: a publisher, a
    /// rights statement, a MARC relator code, a producer-invented `<meta>`
    /// property. The schema says as much at the field itself, so a value that
    /// has a typed home and is written here too would be a second answer with
    /// no tiebreaker.
    ///
    /// The two dates are the shape the schema asks for everywhere it carries
    /// one: the typed instant plus a `_raw` twin holding the source's own
    /// spelling. The twin is written whenever the book stated a date at all,
    /// not only when the date failed to parse, because reading `1843-10-01` as
    /// an instant means choosing a timezone the book never wrote and the twin
    /// is what makes that choice reversible. A value that does not parse gets
    /// the twin and nothing else.
    fn source_meta(&self, info: &pb::EpubInfo) -> doc::DocumentMeta {
        let mut extra: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        /// Record a key, skipping it when the book did not supply a value.
        fn record(extra: &mut std::collections::HashMap<String, String>, key: &str, value: &str) {
            if !value.is_empty() {
                extra.insert(key.to_owned(), value.to_owned());
            }
        }

        record(&mut extra, "epub.publisher", &info.publisher);
        record(&mut extra, "epub.description", &info.description);
        record(
            &mut extra,
            "epub.unique_identifier",
            &info.unique_identifier,
        );
        record(&mut extra, "epub.version", &info.epub_version);
        record(&mut extra, "epub.opf_href", &info.opf_href);
        for element in ["rights", "source", "type", "format", "coverage", "relation"] {
            record(&mut extra, &format!("epub.{element}"), first(info, element));
        }

        // Subtitles and the rest of the titles, which `title` can only hold
        // one of.
        for entry in info
            .metadata
            .iter()
            .filter(|entry| entry.element == "title")
        {
            let kind = refined(entry, "title-type");
            if !kind.is_empty() && kind != "main" {
                record(&mut extra, &format!("epub.title.{kind}"), &entry.value);
            }
        }
        // Sort names and roles, keyed by the name they qualify, so a consumer
        // can answer "who is the illustrator" without re-reading the OPF.
        for entry in info
            .metadata
            .iter()
            .filter(|entry| matches!(entry.element.as_str(), "creator" | "contributor"))
        {
            let role = refined(entry, "role");
            let file_as = refined(entry, "file-as");
            if !role.is_empty() {
                record(&mut extra, &format!("epub.role.{}", entry.value), role);
            }
            if !file_as.is_empty() {
                record(
                    &mut extra,
                    &format!("epub.file-as.{}", entry.value),
                    file_as,
                );
            }
        }
        // One key per identifier, in document order, never one key holding all
        // of them joined: a reader after the third identifier should not have
        // to know this fold's separator, and an identifier is free to contain
        // one. The scheme rides on its own key for the same reason.
        for (index, identifier) in info.identifiers.iter().enumerate() {
            record(
                &mut extra,
                &format!("epub.identifier.{index}"),
                &identifier.value,
            );
            record(
                &mut extra,
                &format!("epub.identifier.{index}.scheme"),
                &identifier.scheme,
            );
        }

        let created = declared_creation(info);
        let modified = info.modified.as_str();

        doc::DocumentMeta {
            title: (!info.title.is_empty()).then(|| info.title.clone()),
            // Authors in the order the book credits them, with the ones it
            // marked as authors first: a book that says which creator wrote it
            // and which drew it should not have them read out in file order.
            authors: authors(info),
            created: datetime::parse(created),
            modified: datetime::parse(modified),
            language: (!info.language.is_empty()).then(|| info.language.clone()),
            // The producing software is not something an OPF states. A `bkp`
            // credit names a book producer, which is as often a person or an
            // imprint as it is a program, so reading one as a generator would
            // be an invention rather than a reading.
            generator: None,
            keywords: info.subjects.clone(),
            // A package document declares no grammar for itself: which one it
            // follows is fixed by the EPUB version, which is `epub.version`
            // below, not by an `xsi:schemaLocation`.
            schema_location: None,
            created_raw: (!created.is_empty()).then(|| created.to_owned()),
            modified_raw: (!modified.is_empty()).then(|| modified.to_owned()),
            extra,
        }
    }

    /// Map one `chapter` event to one empty chapter group under the body.
    fn chapter(&mut self, chapter: &pb::Chapter) {
        let self_ref = format!("#/groups/{}", self.document.groups.len());

        let mut fields = HashMap::new();
        fields.insert("epub.idref".to_owned(), text(&chapter.idref));
        fields.insert("epub.media_type".to_owned(), text(&chapter.media_type));
        // `linear=false` marks auxiliary content a linear read should skip,
        // which is a reading-order fact the group label cannot carry.
        fields.insert("epub.linear".to_owned(), flag(chapter.linear));
        fields.insert(
            "epub.spine_index".to_owned(),
            number(f64::from(chapter.spine_index)),
        );
        if !chapter.properties.is_empty() {
            insert_strings(&mut fields, "epub.properties", &chapter.properties);
        }
        // Which chapters have a recorded reading is a manifest fact, so it is
        // known whether or not the SMIL was parsed.
        insert_text(
            &mut fields,
            "epub.media_overlay_href",
            &chapter.media_overlay_href,
        );
        self.attribute(&mut fields);

        self.document.groups.push(doc::GroupItem {
            self_ref: self_ref.clone(),
            parent: Some(reference(BODY_REF)),
            // Empty by design: the XHTML is not parsed here. See the module
            // docs.
            children: Vec::new(),
            content_layer: doc::ContentLayer::Body as i32,
            meta: Some(doc::BaseMeta {
                custom_fields: fields,
                ..doc::BaseMeta::default()
            }),
            // The resolved archive path, which is the key the HTML collector's
            // items are matched back to.
            name: Some(chapter.href.clone()),
            label: doc::GroupLabel::Chapter as i32,
            // The typed label says everything; the raw fallback is for
            // vocabularies this schema does not enumerate.
            label_raw: None,
        });
        self.link_child(BODY_REF, &self_ref);
        // The key an outline entry and a media overlay both name their target
        // by. Recorded here because this is where the group's ref is known.
        self.chapter_refs.insert(chapter.href.clone(), self_ref);
    }

    /// Map one image `resource` event to one picture under the body.
    ///
    /// Pictures hang off the body rather than off a chapter group: which
    /// chapter references an image is a fact about the XHTML, and this fold
    /// does not read XHTML. The coordinator learns it from the HTML
    /// collector's own picture items.
    fn picture(&mut self, resource: &pb::Resource) {
        let self_ref = format!("#/pictures/{}", self.document.pictures.len());

        let mut fields = HashMap::new();
        fields.insert("epub.href".to_owned(), text(&resource.href));
        fields.insert("epub.manifest_id".to_owned(), text(&resource.manifest_id));
        if !self.cover_href.is_empty() && resource.href == self.cover_href {
            fields.insert("epub.cover".to_owned(), flag(true));
        }

        self.document.pictures.push(doc::PictureItem {
            self_ref: self_ref.clone(),
            parent: Some(reference(BODY_REF)),
            content_layer: doc::ContentLayer::Body as i32,
            label: doc::DocItemLabel::Picture as i32,
            image: Some(doc::ImageRef {
                mimetype: resource.media_type.clone(),
                // Nothing here decodes an image — the manifest's declared
                // media type is all this service knows about it — so there
                // are no pixel dimensions and no dpi to report. An unset
                // `size` says "unknown"; a `Size` of 0x0 would be a claim.
                dpi: 0,
                size: None,
                uri: format!("{URI_SCHEME}{}", resource.href),
            }),
            meta: Some(doc::PictureMeta {
                custom_fields: fields,
                ..doc::PictureMeta::default()
            }),
            source: vec![self.collector_source()],
            ..doc::PictureItem::default()
        });
        self.link_child(BODY_REF, &self_ref);
    }

    /// This collector's attribution, for the items that have a slot for it.
    ///
    /// `model` is unset: there is one engine here and naming it would invent a
    /// distinction. `confidence` is unset too — the mapping is a declarative
    /// walk of the OPF, so a confidence would be noise rather than
    /// information, and `raw_score` is the uncalibrated signal behind a
    /// confidence there is none of.
    fn collector_source(&self) -> doc::SourceType {
        doc::SourceType {
            source: Some(doc::source_type::Source::Collector(doc::CollectorSource {
                collector: COLLECTOR.to_owned(),
                model: None,
                version: Some(self.version.clone()),
                confidence: None,
                raw_score: None,
                raw_score_kind: None,
            })),
        }
    }

    /// Attribute a *group* to this collector, through its custom fields.
    ///
    /// `GroupItem` has no `source` field in this schema — only text, picture
    /// and table items do — so a chapter group cannot carry a `CollectorSource`
    /// the way a picture does. Dropping the attribution would leave the one
    /// kind of item this fold always produces unattributable, so it goes in
    /// the group's own meta under `epub.collector` as `{collector, version}`.
    fn attribute(&self, fields: &mut HashMap<String, Value>) {
        let mut entry = std::collections::BTreeMap::new();
        entry.insert("collector".to_owned(), text(COLLECTOR));
        entry.insert("version".to_owned(), text(&self.version));
        fields.insert(
            "epub.collector".to_owned(),
            Value {
                kind: Some(Kind::StructValue(Struct { fields: entry })),
            },
        );
    }

    /// The group a JSON Pointer names, falling back to the body.
    ///
    /// Only `#/body` is ever passed in by this fold; the general form is kept
    /// so a later mapping (a nav document, say) can nest groups without
    /// rewriting the linking primitives.
    fn group_mut(&mut self, group_ref: &str) -> &mut doc::GroupItem {
        if group_ref == FURNITURE_REF {
            return self
                .document
                .furniture
                .get_or_insert_with(|| root(FURNITURE_REF, doc::ContentLayer::Furniture));
        }
        if let Some(index) = group_ref
            .strip_prefix("#/groups/")
            .and_then(|index| index.parse::<usize>().ok())
            && index < self.document.groups.len()
        {
            return &mut self.document.groups[index];
        }
        self.document
            .body
            .get_or_insert_with(|| root(BODY_REF, doc::ContentLayer::Body))
    }

    /// Record a child on its parent, the other half of every `parent` set.
    fn link_child(&mut self, parent_ref: &str, child_ref: &str) {
        self.group_mut(parent_ref)
            .children
            .push(reference(child_ref));
    }
}

impl Default for DocumentFold {
    fn default() -> Self {
        Self::for_this_build()
    }
}

/// An empty document with its two roots in place.
fn skeleton() -> doc::Document {
    doc::Document {
        schema_name: Some(SCHEMA_NAME.to_owned()),
        body: Some(root(BODY_REF, doc::ContentLayer::Body)),
        furniture: Some(root(FURNITURE_REF, doc::ContentLayer::Furniture)),
        ..doc::Document::default()
    }
}

/// One of the two root groups.
fn root(self_ref: &str, layer: doc::ContentLayer) -> doc::GroupItem {
    doc::GroupItem {
        self_ref: self_ref.to_owned(),
        content_layer: layer as i32,
        ..doc::GroupItem::default()
    }
}

/// A JSON Pointer reference to another item.
fn reference(target: &str) -> doc::RefItem {
    doc::RefItem {
        r#ref: target.to_owned(),
    }
}

/// A string `google.protobuf.Value`.
fn text(value: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(value.to_owned())),
    }
}

/// A boolean `google.protobuf.Value`.
const fn flag(value: bool) -> Value {
    Value {
        kind: Some(Kind::BoolValue(value)),
    }
}

/// A numeric `google.protobuf.Value`.
const fn number(value: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(value)),
    }
}

/// A list `google.protobuf.Value`.
fn list(values: Vec<Value>) -> Value {
    Value {
        kind: Some(Kind::ListValue(ListValue { values })),
    }
}

/// Record a string field, skipping it when the book did not supply one.
fn insert_text(fields: &mut HashMap<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        fields.insert(key.to_owned(), text(value));
    }
}

/// The value of the first metadata entry with this element name, or `""`.
fn first<'a>(info: &'a pb::EpubInfo, element: &str) -> &'a str {
    info.metadata
        .iter()
        .find(|entry| entry.element == element)
        .map_or("", |entry| entry.value.as_str())
}

/// The creation date the book states, whichever way it states it.
///
/// EPUB 3 spells it `<meta property="dcterms:created">`, which is a claim
/// about the work's creation and nothing else, so it wins where a book makes
/// it. `dc:date` is the older and vaguer spelling, meaning "a date associated
/// with an event in the life cycle of the resource", and is what every EPUB 2
/// book has instead; it is the fallback rather than the first choice.
fn declared_creation(info: &pb::EpubInfo) -> &str {
    let created = first(info, "dcterms:created");
    if created.is_empty() {
        info.date.as_str()
    } else {
        created
    }
}

/// The value of the first refinement of `entry` with this property, or `""`.
fn refined<'a>(entry: &'a pb::MetadataEntry, property: &str) -> &'a str {
    entry
        .refinements
        .iter()
        .find(|refinement| refinement.property == property)
        .map_or("", |refinement| refinement.value.as_str())
}

/// The book's authors, the ones it marked as such first.
///
/// A book that says which of its creators wrote it and which illustrated it
/// deserves better than file order, and `DocumentMeta.authors` is a plain list
/// with no room for the distinction. Creators marked `aut` lead; the rest
/// follow in document order, because a creator with no stated role is still a
/// creator and dropping them would lose the only credit an EPUB 2 book gives.
fn authors(info: &pb::EpubInfo) -> Vec<String> {
    let creators: Vec<&pb::MetadataEntry> = info
        .metadata
        .iter()
        .filter(|entry| entry.element == "creator")
        .collect();
    if creators.is_empty() {
        // An EPUB whose metadata tail did not survive, or a caller folding
        // events it built by hand: the scalar list is still the truth.
        return info.creators.clone();
    }

    let (authored, rest): (Vec<_>, Vec<_>) = creators
        .into_iter()
        .partition(|entry| refined(entry, "role") == "aut");
    authored
        .into_iter()
        .chain(rest)
        .map(|entry| entry.value.clone())
        .collect()
}

/// Creator and contributor credits as `{name, role, file_as}` structs.
///
/// Returns `None` when no credit carries a role or a sort name, so a book that
/// said nothing beyond the names does not get an empty structure implying it
/// did.
fn credits(info: &pb::EpubInfo) -> Option<Value> {
    let entries: Vec<&pb::MetadataEntry> = info
        .metadata
        .iter()
        .filter(|entry| matches!(entry.element.as_str(), "creator" | "contributor"))
        .filter(|entry| !refined(entry, "role").is_empty() || !refined(entry, "file-as").is_empty())
        .collect();
    if entries.is_empty() {
        return None;
    }

    Some(list(
        entries
            .into_iter()
            .map(|entry| {
                let mut fields = std::collections::BTreeMap::new();
                fields.insert("name".to_owned(), text(&entry.value));
                fields.insert("element".to_owned(), text(&entry.element));
                let role = refined(entry, "role");
                if !role.is_empty() {
                    fields.insert("role".to_owned(), text(role));
                }
                let file_as = refined(entry, "file-as");
                if !file_as.is_empty() {
                    fields.insert("file_as".to_owned(), text(file_as));
                }
                Value {
                    kind: Some(Kind::StructValue(Struct { fields })),
                }
            })
            .collect(),
    ))
}

/// Map a BCP 47 tag onto the schema's language enum.
///
/// Only the primary subtag is looked up, because the enum is ISO 639-1 and
/// `en-GB` is `en` as far as it can say. A tag with no matching variant is
/// `UNSPECIFIED`; the tag itself always survives in `code_raw`, so nothing is
/// lost by the enum not knowing a language.
fn language_code(tag: &str) -> doc::HumanLanguageLabel {
    let primary = tag.split(['-', '_']).next().unwrap_or_default();
    if primary.is_empty() {
        return doc::HumanLanguageLabel::Unspecified;
    }
    doc::HumanLanguageLabel::from_str_name(&format!(
        "HUMAN_LANGUAGE_LABEL_{}",
        primary.to_ascii_uppercase()
    ))
    .unwrap_or(doc::HumanLanguageLabel::Unspecified)
}

/// The chapter group an outline entry points at, when one matches.
///
/// The fragment is dropped for the lookup: `chap1.xhtml#part2` and
/// `chap1.xhtml` are the same spine item, and the item inside it that the
/// fragment names does not exist on this plane. It is kept on the reference's
/// own range only when a chapter matches, so an entry never points at a group
/// this fold did not create.
fn chapter_target(href: &str, chapters: &HashMap<String, String>) -> Option<doc::FineRef> {
    if href.is_empty() {
        return None;
    }
    let path = href.split('#').next().unwrap_or(href);
    chapters.get(path).map(|group_ref| doc::FineRef {
        r#ref: group_ref.clone(),
        // A range is a character span into an item's text, and a chapter group
        // has no text of its own.
        range: None,
    })
}

/// Record a repeated string field as a `ListValue`, skipping it when empty.
fn insert_strings(fields: &mut HashMap<String, Value>, key: &str, values: &[String]) {
    if !values.is_empty() {
        fields.insert(
            key.to_owned(),
            list(values.iter().map(|value| text(value)).collect()),
        );
    }
}

/// Everything structurally wrong with `document`, as human-readable lines.
///
/// The merge downstream is additive and renumbers refs, and it can only do
/// that if the fragment it is handed is internally consistent. This is the
/// check, ported from the equivalent in gRParse's own Document mapper, and
/// every test that builds a document asserts the result is empty:
///
/// - a `self_ref` that is empty or repeated;
/// - a `children` entry naming something that is not in the document;
/// - a `parent` naming something that is not in the document;
/// - a parent that does not list a child which claims it;
/// - an arena item with no parent at all, which nothing can reach;
/// - a missing or misnamed root.
///
/// A group with no children is **not** an error: chapter groups are emitted
/// empty for the HTML collector to fill.
#[must_use]
pub fn integrity_errors(document: &doc::Document) -> Vec<String> {
    let mut inventory = Inventory::default();

    for (root_ref, group) in [
        (BODY_REF, document.body.as_ref()),
        (FURNITURE_REF, document.furniture.as_ref()),
    ] {
        match group {
            None => inventory
                .errors
                .push(format!("the document has no {root_ref} group")),
            Some(group) => {
                // The roots are the two refs a fragment may name without
                // having created them: the merge resolves them to the
                // coordinator's own body and furniture.
                inventory.refs.insert(root_ref);
                if group.self_ref != root_ref {
                    inventory.errors.push(format!(
                        "the {root_ref} group calls itself {:?}",
                        group.self_ref
                    ));
                }
                inventory.edges(root_ref, &group.children);
            }
        }
    }

    for group in &document.groups {
        inventory.item(&group.self_ref, &group.children, group.parent.as_ref());
    }
    for item in &document.texts {
        match text_base(item) {
            None => inventory
                .errors
                .push("text item with an unset variant".to_owned()),
            Some((self_ref, children, parent)) => inventory.item(self_ref, children, parent),
        }
    }
    for picture in &document.pictures {
        inventory.item(
            &picture.self_ref,
            &picture.children,
            picture.parent.as_ref(),
        );
    }
    for table in &document.tables {
        inventory.item(&table.self_ref, &table.children, table.parent.as_ref());
    }

    inventory.finish()
}

/// The identifying fields of a text item, whichever variant it is.
///
/// `CodeItem` is the trap: it does not wrap a `TextItemBase`, its base fields
/// are inlined, so it cannot be handled by the same arm as the others.
fn text_base(item: &doc::BaseTextItem) -> Option<(&str, &[doc::RefItem], Option<&doc::RefItem>)> {
    /// Pull the three fields out of a wrapped base.
    fn from_base(
        base: Option<&doc::TextItemBase>,
    ) -> Option<(&str, &[doc::RefItem], Option<&doc::RefItem>)> {
        let base = base?;
        Some((&base.self_ref, &base.children, base.parent.as_ref()))
    }

    match item.item.as_ref()? {
        doc::base_text_item::Item::Title(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::SectionHeader(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::ListItem(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::Formula(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::Text(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::FieldHeading(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::FieldValue(item) => from_base(item.base.as_ref()),
        doc::base_text_item::Item::Code(item) => {
            Some((&item.self_ref, &item.children, item.parent.as_ref()))
        }
    }
}

/// One walk's worth of refs and links, checked once the walk is done.
#[derive(Default)]
struct Inventory<'a> {
    /// What has gone wrong so far.
    errors: Vec<String>,
    /// Every `self_ref` seen, including the two roots.
    refs: HashSet<&'a str>,
    /// `(parent, child)` for every entry of every `children` list.
    down: Vec<(&'a str, &'a str)>,
    /// `(child, parent)` for every `parent` back-pointer.
    up: Vec<(&'a str, &'a str)>,
}

impl<'a> Inventory<'a> {
    /// Record one arena item.
    fn item(
        &mut self,
        self_ref: &'a str,
        children: &'a [doc::RefItem],
        parent: Option<&'a doc::RefItem>,
    ) {
        if self_ref.is_empty() {
            self.errors.push("item with an empty self_ref".to_owned());
            return;
        }
        if !self.refs.insert(self_ref) {
            self.errors.push(format!("duplicate self_ref {self_ref}"));
        }
        self.edges(self_ref, children);
        match parent {
            Some(parent) => self.up.push((self_ref, parent.r#ref.as_str())),
            None => self
                .errors
                .push(format!("{self_ref} has no parent, so nothing reaches it")),
        }
    }

    /// Record a `children` list.
    fn edges(&mut self, parent_ref: &'a str, children: &'a [doc::RefItem]) {
        for child in children {
            self.down.push((parent_ref, child.r#ref.as_str()));
        }
    }

    /// Resolve every link now that every ref is known.
    fn finish(mut self) -> Vec<String> {
        let listed: HashSet<(&str, &str)> = self.down.iter().copied().collect();
        for (parent, child) in &self.down {
            if !self.refs.contains(child) {
                self.errors
                    .push(format!("child {child} of {parent} does not resolve"));
            }
        }
        for (child, parent) in &self.up {
            if !self.refs.contains(parent) {
                self.errors
                    .push(format!("parent {parent} of {child} does not resolve"));
            } else if !listed.contains(&(*parent, *child)) {
                self.errors
                    .push(format!("{parent} does not list its child {child}"));
            }
        }
        self.errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The events a two-chapter book with a cover produces, in the order the
    /// server emits them for the default fixture: the cover is stored after
    /// the chapters, so its resource event arrives last.
    fn default_book() -> Vec<pb::parse_epub_response::Event> {
        vec![
            pb::parse_epub_response::Event::Info(pb::EpubInfo {
                title: "A Tale of Two Chapters".to_owned(),
                creators: vec!["Ada Lovelace".to_owned(), "Charles Babbage".to_owned()],
                language: "en-GB".to_owned(),
                identifiers: vec![pb::DublinCoreIdentifier {
                    value: "urn:isbn:9780000000000".to_owned(),
                    id: "bookid".to_owned(),
                    scheme: "ISBN".to_owned(),
                }],
                unique_identifier: "urn:isbn:9780000000000".to_owned(),
                publisher: "Analytical Press".to_owned(),
                date: "1843-10-01".to_owned(),
                subjects: vec!["Computing".to_owned()],
                spine_item_count: 2,
                opf_href: "OEBPS/content.opf".to_owned(),
                epub_version: "3.0".to_owned(),
                cover_href: "OEBPS/images/cover.png".to_owned(),
                ..Default::default()
            }),
            chapter(0, "ch1", "OEBPS/text/chap1.xhtml"),
            chapter(1, "ch2", "OEBPS/text/chap2.xhtml"),
            resource(
                "OEBPS/images/cover.png",
                "image/png",
                pb::ResourceKind::Image,
            ),
            pb::parse_epub_response::Event::Status(pb::ParseStatus {
                chapters_emitted: 2,
                resources_emitted: 1,
                ..Default::default()
            }),
        ]
    }

    /// One `chapter` event.
    fn chapter(index: u32, idref: &str, href: &str) -> pb::parse_epub_response::Event {
        pb::parse_epub_response::Event::Chapter(pb::Chapter {
            spine_index: index,
            idref: idref.to_owned(),
            href: href.to_owned(),
            media_type: "application/xhtml+xml".to_owned(),
            content: b"<html/>".to_vec(),
            linear: true,
            properties: Vec::new(),
            media_overlay_href: String::new(),
        })
    }

    /// One `resource` event.
    fn resource(
        href: &str,
        media_type: &str,
        kind: pb::ResourceKind,
    ) -> pb::parse_epub_response::Event {
        pb::parse_epub_response::Event::Resource(pb::Resource {
            href: href.to_owned(),
            media_type: media_type.to_owned(),
            kind: kind as i32,
            content: b"bytes".to_vec(),
            manifest_id: "res".to_owned(),
            properties: Vec::new(),
            media_overlay_href: String::new(),
        })
    }

    /// Fold a whole event stream.
    fn fold(events: &[pb::parse_epub_response::Event]) -> doc::Document {
        let mut fold = DocumentFold::new("0.1.0");
        for event in events {
            fold.consume(event);
        }
        fold.take()
    }

    /// The string behind a custom field, for terse assertions.
    fn field<'a>(fields: &'a HashMap<String, Value>, key: &str) -> &'a Kind {
        fields
            .get(key)
            .unwrap_or_else(|| panic!("custom field {key:?} is missing"))
            .kind
            .as_ref()
            .expect("a value with no kind is not a value")
    }

    /// The body group of a folded document.
    fn body(document: &doc::Document) -> &doc::GroupItem {
        document.body.as_ref().expect("every document has a body")
    }

    #[test]
    fn the_book_becomes_a_named_document_with_an_origin() {
        let document = fold(&default_book());
        assert_eq!(document.name, "A Tale of Two Chapters");
        assert_eq!(document.schema_name.as_deref(), Some(SCHEMA_NAME));
        let origin = document.origin.as_ref().expect("an origin");
        assert_eq!(origin.mimetype, "application/epub+zip");
        assert!(
            origin.filename.is_empty(),
            "the server is handed bytes, never a filename"
        );
    }

    #[test]
    fn opf_metadata_lands_in_body_custom_fields() {
        let document = fold(&default_book());
        let fields = &body(&document)
            .meta
            .as_ref()
            .expect("body meta")
            .custom_fields;

        assert_eq!(
            field(fields, "epub.publisher"),
            &Kind::StringValue("Analytical Press".to_owned())
        );
        assert_eq!(
            field(fields, "epub.opf_href"),
            &Kind::StringValue("OEBPS/content.opf".to_owned())
        );
        for typed in [
            "epub.language",
            "epub.subjects",
            "epub.creators",
            "epub.date",
            "epub.modified",
        ] {
            assert!(
                !fields.contains_key(typed),
                "{typed} has a typed home now, so it is not a string here as well"
            );
        }

        let Kind::ListValue(identifiers) = field(fields, "epub.identifiers") else {
            panic!("identifiers is a list");
        };
        let Some(Kind::StructValue(first)) = identifiers.values[0].kind.as_ref() else {
            panic!("an identifier is an object");
        };
        assert_eq!(
            first.fields["value"].kind,
            Some(Kind::StringValue("urn:isbn:9780000000000".to_owned()))
        );
        assert_eq!(
            first.fields["scheme"].kind,
            Some(Kind::StringValue("ISBN".to_owned()))
        );

        assert!(
            !fields.contains_key("epub.description"),
            "a field the book did not supply is absent, not empty"
        );
    }

    #[test]
    fn chapters_become_empty_groups_in_spine_order() {
        let document = fold(&default_book());
        assert_eq!(document.groups.len(), 2);

        for (index, group) in document.groups.iter().enumerate() {
            assert_eq!(group.self_ref, format!("#/groups/{index}"));
            assert_eq!(group.label, doc::GroupLabel::Chapter as i32);
            assert_eq!(group.content_layer, doc::ContentLayer::Body as i32);
            assert_eq!(
                group.parent.as_ref().map(|p| p.r#ref.as_str()),
                Some(BODY_REF)
            );
            assert!(
                group.children.is_empty(),
                "the XHTML is the HTML collector's to parse; the group is a socket"
            );

            let fields = &group.meta.as_ref().expect("chapter meta").custom_fields;
            assert_eq!(
                field(fields, "epub.spine_index"),
                &Kind::NumberValue(f64::from(u32::try_from(index).expect("two chapters")))
            );
            assert_eq!(field(fields, "epub.linear"), &Kind::BoolValue(true));
            assert_eq!(
                field(fields, "epub.media_type"),
                &Kind::StringValue("application/xhtml+xml".to_owned())
            );
        }

        assert_eq!(
            document.groups[0].name.as_deref(),
            Some("OEBPS/text/chap1.xhtml")
        );
        assert_eq!(
            document.groups[1].name.as_deref(),
            Some("OEBPS/text/chap2.xhtml")
        );
        assert_eq!(
            document.groups[0].meta.as_ref().unwrap().custom_fields["epub.idref"].kind,
            Some(Kind::StringValue("ch1".to_owned()))
        );
    }

    #[test]
    fn the_cover_image_becomes_a_flagged_picture_pointing_at_the_stream() {
        let document = fold(&default_book());
        assert_eq!(document.pictures.len(), 1);
        let picture = &document.pictures[0];

        assert_eq!(picture.self_ref, "#/pictures/0");
        assert_eq!(picture.label, doc::DocItemLabel::Picture as i32);
        assert!(picture.prov.is_empty(), "an EPUB has no pages and no boxes");

        let image = picture.image.as_ref().expect("an image ref");
        assert_eq!(image.mimetype, "image/png");
        assert_eq!(image.uri, "epub:OEBPS/images/cover.png");
        assert!(image.size.is_none(), "nothing here decodes an image");

        let fields = &picture.meta.as_ref().expect("picture meta").custom_fields;
        assert_eq!(field(fields, "epub.cover"), &Kind::BoolValue(true));
        assert_eq!(
            field(fields, "epub.href"),
            &Kind::StringValue("OEBPS/images/cover.png".to_owned())
        );
    }

    #[test]
    fn a_picture_that_is_not_the_cover_is_not_flagged() {
        let mut events = default_book();
        events.insert(
            3,
            resource(
                "OEBPS/images/plate.png",
                "image/png",
                pb::ResourceKind::Image,
            ),
        );
        let document = fold(&events);

        assert_eq!(document.pictures.len(), 2);
        let plate = &document.pictures[0].meta.as_ref().unwrap().custom_fields;
        assert!(!plate.contains_key("epub.cover"));
        let cover = &document.pictures[1].meta.as_ref().unwrap().custom_fields;
        assert_eq!(field(cover, "epub.cover"), &Kind::BoolValue(true));
    }

    #[test]
    fn items_carry_this_collector_and_its_version() {
        let document = fold(&default_book());

        let source = &document.pictures[0].source;
        assert_eq!(source.len(), 1);
        let Some(doc::source_type::Source::Collector(collector)) = source[0].source.as_ref() else {
            panic!("the only source is a collector");
        };
        assert_eq!(collector.collector, "epub");
        assert_eq!(collector.version.as_deref(), Some("0.1.0"));
        assert!(collector.model.is_none(), "one engine, so no model");
        assert!(
            collector.confidence.is_none(),
            "a declarative mapping has no confidence to report"
        );

        // Groups have no `source` field in this schema, so the same
        // attribution rides in the group's meta.
        let fields = &document.groups[0].meta.as_ref().unwrap().custom_fields;
        let Kind::StructValue(stamp) = field(fields, "epub.collector") else {
            panic!("the group stamp is an object");
        };
        assert_eq!(
            stamp.fields["collector"].kind,
            Some(Kind::StringValue("epub".to_owned()))
        );
        assert_eq!(
            stamp.fields["version"].kind,
            Some(Kind::StringValue("0.1.0".to_owned()))
        );
    }

    #[test]
    fn arrival_order_is_document_order() {
        // The same book packed with the cover first: the fold appends as
        // events arrive and never reorders, so the picture precedes the
        // chapters in `body.children`.
        let events = vec![
            default_book()[0].clone(),
            resource(
                "OEBPS/images/cover.png",
                "image/png",
                pb::ResourceKind::Image,
            ),
            chapter(0, "ch1", "OEBPS/text/chap1.xhtml"),
            chapter(1, "ch2", "OEBPS/text/chap2.xhtml"),
        ];
        let document = fold(&events);
        let children: Vec<&str> = body(&document)
            .children
            .iter()
            .map(|child| child.r#ref.as_str())
            .collect();
        assert_eq!(children, ["#/pictures/0", "#/groups/0", "#/groups/1"]);
        assert!(integrity_errors(&document).is_empty());
    }

    #[test]
    fn non_image_resources_are_not_projected() {
        let mut events = default_book();
        events.insert(
            3,
            resource(
                "OEBPS/style/main.css",
                "text/css",
                pb::ResourceKind::Stylesheet,
            ),
        );
        events.insert(
            4,
            resource(
                "OEBPS/nav.xhtml",
                "application/xhtml+xml",
                pb::ResourceKind::Document,
            ),
        );
        events.insert(
            5,
            resource("OEBPS/fonts/serif.otf", "font/otf", pb::ResourceKind::Font),
        );
        let document = fold(&events);

        assert_eq!(
            document.pictures.len(),
            1,
            "only images have an honest slot; the rest stay on the typed stream"
        );
        assert_eq!(
            body(&document).children.len(),
            3,
            "two chapters, one picture"
        );
    }

    #[test]
    fn a_book_with_no_images_still_yields_its_chapter_groups() {
        let events: Vec<_> = default_book()
            .into_iter()
            .filter(|event| !matches!(event, pb::parse_epub_response::Event::Resource(_)))
            .collect();
        let document = fold(&events);

        assert_eq!(document.groups.len(), 2);
        assert!(document.pictures.is_empty());
        assert!(integrity_errors(&document).is_empty());
    }

    #[test]
    fn the_default_book_folds_to_a_document_with_no_integrity_errors() {
        let document = fold(&default_book());
        assert_eq!(integrity_errors(&document), Vec::<String>::new());

        // Both directions of every link, spelled out.
        let children: Vec<&str> = body(&document)
            .children
            .iter()
            .map(|child| child.r#ref.as_str())
            .collect();
        assert_eq!(children, ["#/groups/0", "#/groups/1", "#/pictures/0"]);
    }

    #[test]
    fn taking_twice_yields_an_empty_second_document() {
        let mut fold = DocumentFold::new("0.1.0");
        for event in &default_book() {
            fold.consume(event);
        }
        assert_eq!(fold.take().groups.len(), 2);

        let second = fold.take();
        assert!(second.groups.is_empty());
        assert!(second.name.is_empty());
        assert!(
            integrity_errors(&second).is_empty(),
            "an empty fold is valid"
        );
    }

    #[test]
    fn the_status_trailer_adds_nothing_and_a_document_event_is_never_refolded() {
        let mut without = DocumentFold::new("0.1.0");
        let mut with = DocumentFold::new("0.1.0");
        for event in &default_book() {
            with.consume(event);
            if !matches!(event, pb::parse_epub_response::Event::Status(_)) {
                without.consume(event);
            }
        }
        let folded = with.take();
        assert_eq!(folded, without.take());

        let mut again = DocumentFold::new("0.1.0");
        again.consume(&pb::parse_epub_response::Event::Document(folded));
        let empty = again.take();
        assert!(empty.groups.is_empty() && empty.pictures.is_empty());
    }

    #[test]
    fn integrity_errors_catch_a_broken_fragment() {
        let mut document = fold(&default_book());

        // A child nobody can resolve.
        document
            .body
            .as_mut()
            .unwrap()
            .children
            .push(reference("#/groups/99"));
        // A duplicate self_ref.
        let clone = document.groups[0].clone();
        document.groups.push(clone);
        // A picture whose parent does not list it.
        document.pictures.push(doc::PictureItem {
            self_ref: "#/pictures/1".to_owned(),
            parent: Some(reference(BODY_REF)),
            ..doc::PictureItem::default()
        });
        // An orphan.
        document.pictures.push(doc::PictureItem {
            self_ref: "#/pictures/2".to_owned(),
            parent: None,
            ..doc::PictureItem::default()
        });

        let errors = integrity_errors(&document);
        assert!(
            errors.iter().any(|e| e.contains("#/groups/99")),
            "dangling child: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.starts_with("duplicate self_ref")),
            "duplicate: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("does not list its child")),
            "asymmetric link: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("has no parent")),
            "orphan: {errors:?}"
        );
    }

    /// The metadata tail a book with an expressive OPF produces, as the server
    /// would put it on `EpubInfo.metadata`.
    fn expressive_info() -> pb::EpubInfo {
        /// One metadata entry with its refinements.
        fn entry(element: &str, value: &str, refinements: &[(&str, &str)]) -> pb::MetadataEntry {
            pb::MetadataEntry {
                element: element.to_owned(),
                value: value.to_owned(),
                refinements: refinements
                    .iter()
                    .map(|(property, value)| pb::MetadataRefinement {
                        property: (*property).to_owned(),
                        value: (*value).to_owned(),
                        scheme: String::new(),
                    })
                    .collect(),
                ..Default::default()
            }
        }

        pb::EpubInfo {
            title: "A Tale of Two Chapters".to_owned(),
            creators: vec!["Ada Lovelace".to_owned(), "Charles Babbage".to_owned()],
            language: "en-GB".to_owned(),
            subjects: vec!["Computing".to_owned(), "History".to_owned()],
            date: "1843-10-01".to_owned(),
            modified: "2026-08-25T00:00:00Z".to_owned(),
            publisher: "Analytical Press".to_owned(),
            metadata: vec![
                entry("title", "A Tale of Two Chapters", &[("title-type", "main")]),
                entry(
                    "title",
                    "Being an Account of Two Chapters",
                    &[("title-type", "subtitle")],
                ),
                // The illustrator is listed first, so file order and credit
                // order disagree and the reading has to choose.
                entry("creator", "Charles Babbage", &[("role", "ill")]),
                entry(
                    "creator",
                    "Ada Lovelace",
                    &[("role", "aut"), ("file-as", "Lovelace, Ada")],
                ),
                entry("rights", "Public domain", &[]),
                entry("source", "urn:isbn:9781111111111", &[]),
                entry("type", "monograph", &[]),
                entry("format", "application/epub+zip", &[]),
                entry("coverage", "England, 1843", &[]),
                entry("relation", "urn:isbn:9782222222222", &[]),
                entry("dcterms:modified", "2026-08-25T00:00:00Z", &[]),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn dublin_core_lands_in_the_typed_document_metadata() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(expressive_info()));
        let document = fold.take();

        let meta = document.source_meta.as_ref().expect("source meta");
        assert_eq!(meta.title.as_deref(), Some("A Tale of Two Chapters"));
        assert_eq!(
            meta.authors,
            ["Ada Lovelace", "Charles Babbage"],
            "the creator the book marked `aut` leads, whatever order the OPF listed them in"
        );
        assert_eq!(
            meta.created,
            datetime::parse("1843-10-01"),
            "dc:date is the creation date this book states"
        );
        assert_eq!(meta.created_raw.as_deref(), Some("1843-10-01"));
        assert_eq!(meta.modified, datetime::parse("2026-08-25T00:00:00Z"));
        assert_eq!(meta.modified_raw.as_deref(), Some("2026-08-25T00:00:00Z"));
        assert_eq!(meta.language.as_deref(), Some("en-GB"));
        assert_eq!(meta.keywords, ["Computing", "History"]);
        assert!(
            meta.schema_location.is_none(),
            "a package document declares no grammar for itself"
        );

        // The six elements the old allow-list dropped, and the expression
        // model's roles, in the untyped tail because nothing types them.
        assert_eq!(meta.extra["epub.rights"], "Public domain");
        assert_eq!(meta.extra["epub.source"], "urn:isbn:9781111111111");
        assert_eq!(meta.extra["epub.type"], "monograph");
        assert_eq!(meta.extra["epub.format"], "application/epub+zip");
        assert_eq!(meta.extra["epub.coverage"], "England, 1843");
        assert_eq!(meta.extra["epub.relation"], "urn:isbn:9782222222222");
        assert_eq!(
            meta.extra["epub.title.subtitle"],
            "Being an Account of Two Chapters"
        );
        assert_eq!(meta.extra["epub.role.Ada Lovelace"], "aut");
        assert_eq!(meta.extra["epub.file-as.Ada Lovelace"], "Lovelace, Ada");
        assert_eq!(meta.extra["epub.role.Charles Babbage"], "ill");
        assert_eq!(meta.extra["epub.publisher"], "Analytical Press");
    }

    #[test]
    fn language_and_subjects_reach_the_typed_meta_fields() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(expressive_info()));
        let document = fold.take();
        let meta = body(&document).meta.as_ref().expect("body meta");

        let language = meta.language.as_ref().expect("a language field");
        assert_eq!(language.code, doc::HumanLanguageLabel::En as i32);
        assert_eq!(
            language.code_raw.as_deref(),
            Some("en-GB"),
            "the enum is ISO 639-1, so the region only survives in the raw tag"
        );
        assert_eq!(language.created_by.as_deref(), Some("epub"));
        assert!(
            language.confidence.is_none(),
            "the book declared it; nothing detected it"
        );

        let keywords = meta.keywords.as_ref().expect("a keywords field");
        assert_eq!(keywords.values, ["Computing", "History"]);
    }

    #[test]
    fn a_language_the_enum_does_not_know_still_keeps_its_tag() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(pb::EpubInfo {
            language: "x-klingon".to_owned(),
            ..Default::default()
        }));
        let document = fold.take();
        let language = body(&document)
            .meta
            .as_ref()
            .expect("body meta")
            .language
            .as_ref()
            .expect("a language field");
        assert_eq!(language.code, doc::HumanLanguageLabel::Unspecified as i32);
        assert_eq!(language.code_raw.as_deref(), Some("x-klingon"));
    }

    #[test]
    fn the_custom_fields_keep_only_what_the_schema_does_not_type() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(expressive_info()));
        let document = fold.take();
        let fields = &body(&document)
            .meta
            .as_ref()
            .expect("body meta")
            .custom_fields;

        // Open vocabulary, and the map is the honest type for it.
        assert_eq!(
            field(fields, "epub.rights"),
            &Kind::StringValue("Public domain".to_owned())
        );
        let Kind::ListValue(credits) = field(fields, "epub.creator_roles") else {
            panic!("creator roles is a list");
        };
        assert_eq!(credits.values.len(), 2);

        // The facts that have a first-class slot are only in that slot.
        for typed in [
            "epub.language",
            "epub.subjects",
            "epub.creators",
            "epub.date",
            "epub.modified",
            "epub.title",
        ] {
            assert!(
                !fields.contains_key(typed),
                "{typed} is typed, not a string"
            );
        }
    }

    #[test]
    fn a_parseable_date_lands_typed_with_its_spelling_kept_beside_it() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(pb::EpubInfo {
            // A whole-day date and a full instant, the two spellings a real
            // book uses for the two fields.
            date: "1843-10-01".to_owned(),
            modified: "2026-08-25T09:41:07Z".to_owned(),
            ..Default::default()
        }));
        let meta = fold.take().source_meta.expect("source meta");

        let created = meta.created.expect("dc:date parses");
        assert_eq!(created.seconds, -3_984_163_200);
        assert_eq!(created.nanos, 0);
        assert_eq!(
            meta.created_raw.as_deref(),
            Some("1843-10-01"),
            "the twin keeps the book's own spelling, not this fold's reading of it"
        );

        let modified = meta.modified.expect("dcterms:modified parses");
        assert_eq!(modified.seconds, 1_787_650_867);
        assert_eq!(meta.modified_raw.as_deref(), Some("2026-08-25T09:41:07Z"));
    }

    #[test]
    fn a_date_that_is_not_a_date_is_kept_raw_and_claimed_as_nothing() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(pb::EpubInfo {
            date: "sometime in the 1840s".to_owned(),
            modified: "Last Tuesday".to_owned(),
            ..Default::default()
        }));
        let meta = fold.take().source_meta.expect("source meta");

        assert!(
            meta.created.is_none(),
            "an unreadable date is not an instant"
        );
        assert!(meta.modified.is_none());
        assert_eq!(meta.created_raw.as_deref(), Some("sometime in the 1840s"));
        assert_eq!(
            meta.modified_raw.as_deref(),
            Some("Last Tuesday"),
            "the raw twin is the only field set, and nothing is lost"
        );
    }

    #[test]
    fn a_book_that_states_no_date_claims_neither_field() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(pb::EpubInfo {
            title: "Undated".to_owned(),
            ..Default::default()
        }));
        let meta = fold.take().source_meta.expect("source meta");

        assert!(meta.created.is_none() && meta.created_raw.is_none());
        assert!(meta.modified.is_none() && meta.modified_raw.is_none());
    }

    #[test]
    fn dcterms_created_outranks_dc_date_where_a_book_states_both() {
        let mut info = expressive_info();
        info.metadata.push(pb::MetadataEntry {
            element: "dcterms:created".to_owned(),
            value: "1843-10-15T00:00:00Z".to_owned(),
            ..Default::default()
        });
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(info));
        let meta = fold.take().source_meta.expect("source meta");

        assert_eq!(
            meta.created_raw.as_deref(),
            Some("1843-10-15T00:00:00Z"),
            "dcterms:created is a claim about creation; dc:date is a claim about anything"
        );
        assert_eq!(meta.created, datetime::parse("1843-10-15T00:00:00Z"));
    }

    #[test]
    fn every_identifier_gets_its_own_key_rather_than_one_joined_string() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.consume(&pb::parse_epub_response::Event::Info(pb::EpubInfo {
            identifiers: vec![
                pb::DublinCoreIdentifier {
                    value: "urn:isbn:9780000000000".to_owned(),
                    id: "bookid".to_owned(),
                    scheme: "ISBN".to_owned(),
                },
                pb::DublinCoreIdentifier {
                    value: "urn:uuid:0000 0000".to_owned(),
                    id: String::new(),
                    scheme: String::new(),
                },
            ],
            ..Default::default()
        }));
        let extra = fold.take().source_meta.expect("source meta").extra;

        assert_eq!(extra["epub.identifier.0"], "urn:isbn:9780000000000");
        assert_eq!(extra["epub.identifier.0.scheme"], "ISBN");
        assert_eq!(
            extra["epub.identifier.1"], "urn:uuid:0000 0000",
            "an identifier may contain a separator, which is why there is no separator"
        );
        assert!(!extra.contains_key("epub.identifier.1.scheme"));
        assert!(
            !extra.contains_key("epub.identifiers"),
            "the joined list is gone"
        );
    }

    /// One `navigation` event over the default book's two chapters.
    fn navigation() -> pb::parse_epub_response::Event {
        pb::parse_epub_response::Event::Navigation(pb::Navigation {
            source_href: "OEBPS/nav.xhtml".to_owned(),
            from_ncx: false,
            toc: vec![
                pb::NavPoint {
                    label: "Chapter One".to_owned(),
                    href: "OEBPS/text/chap1.xhtml".to_owned(),
                    depth: 0,
                    children: vec![pb::NavPoint {
                        label: "The Second Part".to_owned(),
                        href: "OEBPS/text/chap1.xhtml#part2".to_owned(),
                        depth: 1,
                        children: Vec::new(),
                    }],
                },
                pb::NavPoint {
                    label: "Afterword".to_owned(),
                    href: "OEBPS/text/after.xhtml".to_owned(),
                    depth: 0,
                    children: Vec::new(),
                },
            ],
        })
    }

    #[test]
    fn the_table_of_contents_becomes_the_outline_pointing_at_chapter_groups() {
        let mut events = default_book();
        events.insert(1, navigation());
        let document = fold(&events);

        let titles: Vec<&str> = document
            .outline
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(
            titles,
            ["Chapter One", "The Second Part", "Afterword"],
            "depth-first in reading order, nesting flattened into levels"
        );
        assert_eq!(document.outline[0].level, 0);
        assert_eq!(document.outline[1].level, 1);
        assert_eq!(document.outline[2].level, 0);

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
            Some("#/groups/0"),
            "a fragment names a place inside a chapter, which is still that chapter's group"
        );
        assert!(
            document.outline[2].target.is_none(),
            "the afterword is not in the spine, so there is no group to point at"
        );
        assert!(
            document.outline.iter().all(|entry| entry.page_no.is_none()),
            "an EPUB is reflowable and has no pages"
        );
        assert!(integrity_errors(&document).is_empty());
    }

    #[test]
    fn a_book_with_no_navigation_event_has_an_empty_outline() {
        let document = fold(&default_book());
        assert!(document.outline.is_empty());
    }

    #[test]
    fn media_overlays_give_the_chapter_its_cues_and_the_document_its_length() {
        let mut events = default_book();
        // The manifest link rides on the chapter whether or not the SMIL was
        // parsed, so both halves are exercised here.
        let pb::parse_epub_response::Event::Chapter(first) = &mut events[1] else {
            panic!("the second event is the first chapter");
        };
        first.media_overlay_href = "OEBPS/overlays/ch1.smil".to_owned();
        events.insert(
            1,
            pb::parse_epub_response::Event::MediaOverlay(pb::MediaOverlay {
                source_href: "OEBPS/overlays/ch1.smil".to_owned(),
                chapter_href: "OEBPS/text/chap1.xhtml".to_owned(),
                cues: vec![
                    pb::MediaOverlayCue {
                        text_href: "OEBPS/text/chap1.xhtml#s1".to_owned(),
                        audio_href: "OEBPS/audio/ch1.mp3".to_owned(),
                        start_time: 0.0,
                        end_time: 12.5,
                        identifier: "p1".to_owned(),
                    },
                    pb::MediaOverlayCue {
                        text_href: "OEBPS/text/chap1.xhtml#s2".to_owned(),
                        audio_href: "OEBPS/audio/ch1.mp3".to_owned(),
                        start_time: 12.5,
                        end_time: 20.0,
                        identifier: "p2".to_owned(),
                    },
                ],
            }),
        );
        let document = fold(&events);

        let media = document.media.as_ref().expect("media meta");
        assert_eq!(
            media.duration_ms,
            Some(20_000.0),
            "the narration runs to the end of the last cue"
        );

        let fields = &document.groups[0]
            .meta
            .as_ref()
            .expect("chapter meta")
            .custom_fields;
        assert_eq!(
            field(fields, "epub.media_overlay_href"),
            &Kind::StringValue("OEBPS/overlays/ch1.smil".to_owned())
        );
        let Kind::ListValue(cues) = field(fields, "epub.media_overlay_cues") else {
            panic!("cues are a list");
        };
        assert_eq!(cues.values.len(), 2);
        let Some(Kind::StructValue(first)) = cues.values[0].kind.as_ref() else {
            panic!("a cue is an object");
        };
        assert_eq!(
            first.fields["text_href"].kind,
            Some(Kind::StringValue("OEBPS/text/chap1.xhtml#s1".to_owned()))
        );
        assert_eq!(first.fields["end_time"].kind, Some(Kind::NumberValue(12.5)));

        // The second chapter is not narrated and says so by omission.
        let second = &document.groups[1]
            .meta
            .as_ref()
            .expect("chapter meta")
            .custom_fields;
        assert!(!second.contains_key("epub.media_overlay_href"));
        assert!(!second.contains_key("epub.media_overlay_cues"));
        assert!(integrity_errors(&document).is_empty());
    }

    #[test]
    fn a_book_with_no_overlays_claims_no_media() {
        let document = fold(&default_book());
        assert!(
            document.media.is_none(),
            "a duration of zero would be a claim; absence is the fact"
        );
    }

    #[test]
    fn the_origin_carries_the_archive_hash_rather_than_a_zero() {
        let mut fold = DocumentFold::new("0.1.0");
        fold.set_source_hash(source_hash(b"an archive"));
        for event in &default_book() {
            fold.consume(event);
        }
        let origin = fold.take().origin.expect("an origin");
        assert_eq!(origin.binary_hash, source_hash(b"an archive"));
        assert_ne!(origin.binary_hash, 0);
    }

    #[test]
    fn the_hash_is_stable_and_separates_different_bytes() {
        // Pinned: the value is a record consumers may have stored, so a change
        // to the algorithm has to be a deliberate one that fails here first.
        assert_eq!(source_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(source_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(source_hash(b"epub"), source_hash(b"epuc"));
        assert_eq!(source_hash(b"epub"), source_hash(b"epub"));
    }

    #[test]
    fn a_second_fold_starts_clean_of_the_first_book_s_navigation() {
        let mut fold = DocumentFold::new("0.1.0");
        let mut events = default_book();
        events.insert(1, navigation());
        for event in &events {
            fold.consume(event);
        }
        assert_eq!(fold.take().outline.len(), 3);

        for event in &default_book() {
            fold.consume(event);
        }
        let second = fold.take();
        assert!(
            second.outline.is_empty(),
            "the held navigation must not leak into the next book"
        );
    }

    #[test]
    fn integrity_errors_want_both_roots() {
        let mut document = fold(&default_book());
        document.furniture = None;
        assert!(
            integrity_errors(&document)
                .iter()
                .any(|e| e.contains("#/furniture")),
        );
    }
}
