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
//! - the OPF metadata, on the body group's `meta.custom_fields`;
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
//! - **Root meta is first-writer-wins.** The `epub.*` metadata lives on the
//!   *body* group's `meta.custom_fields`, and the coordinator does not merge
//!   competing root metas: whichever collector's fragment lands first keeps
//!   its keys. Per-item meta (on the chapter groups and pictures) does not
//!   have this problem.
//! - **No provenance.** An EPUB is reflowable and has no pages and no
//!   bounding boxes, so `prov` is left empty everywhere rather than filled
//!   with invented coordinates. Source locators — spine index, href, manifest
//!   id — go in `meta.custom_fields` instead.

use std::collections::{HashMap, HashSet};

use prost_types::value::Kind;
use prost_types::{ListValue, Struct, Value};

use crate::extract::EPUB_MIMETYPE;
use crate::proto::document_v1 as doc;
use crate::proto::v1 as pb;

/// `CollectorSource.collector` for everything this fold creates.
pub const COLLECTOR: &str = "epub";

/// The upstream schema this projection tracks, recorded on every Document.
pub const SCHEMA_NAME: &str = "docling_document_v2";

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
}

impl DocumentFold {
    /// A fold that attributes its items to `version` of this collector.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            document: skeleton(),
            version: version.into(),
            cover_href: String::new(),
        }
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
    ///   audio, video, nav documents and anything else have no docling slot to
    ///   go in — inventing one would mean lying with a label — and they are
    ///   already on the typed stream in full, so they are deliberately not
    ///   projected;
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
        std::mem::replace(&mut self.document, skeleton())
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
            binary_hash: 0,
            filename: String::new(),
            uri: None,
        });

        // Everything the OPF said, under `epub.` keys, because none of it has
        // a docling slot: the schema's origin block holds a mimetype and a
        // filename and nothing else. Empty fields are omitted rather than
        // written as empty strings, so a reader can tell "the book did not say"
        // from "the book said nothing useful".
        let mut fields = HashMap::new();
        insert_text(&mut fields, "epub.language", &info.language);
        insert_text(
            &mut fields,
            "epub.unique_identifier",
            &info.unique_identifier,
        );
        insert_text(&mut fields, "epub.publisher", &info.publisher);
        insert_text(&mut fields, "epub.description", &info.description);
        // Verbatim, as the event carries it: EPUB 2 permits any W3CDTF
        // profile and real books are looser still, so normalizing here would
        // be guessing.
        insert_text(&mut fields, "epub.date", &info.date);
        insert_text(&mut fields, "epub.epub_version", &info.epub_version);
        insert_text(&mut fields, "epub.opf_href", &info.opf_href);
        insert_strings(&mut fields, "epub.creators", &info.creators);
        insert_strings(&mut fields, "epub.contributors", &info.contributors);
        insert_strings(&mut fields, "epub.subjects", &info.subjects);
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

        if !fields.is_empty() {
            self.group_mut(BODY_REF).meta = Some(doc::BaseMeta {
                custom_fields: fields,
                ..doc::BaseMeta::default()
            });
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
        });
        self.link_child(BODY_REF, &self_ref);
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
    /// information.
    fn collector_source(&self) -> doc::SourceType {
        doc::SourceType {
            source: Some(doc::source_type::Source::Collector(doc::CollectorSource {
                collector: COLLECTOR.to_owned(),
                model: None,
                version: Some(self.version.clone()),
                confidence: None,
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
/// check, ported from `docling_integrity_errors` in gRParse's
/// `src/docling_map.cpp`, and every test that builds a document asserts the
/// result is empty:
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
            field(fields, "epub.language"),
            &Kind::StringValue("en-GB".to_owned())
        );
        assert_eq!(
            field(fields, "epub.publisher"),
            &Kind::StringValue("Analytical Press".to_owned())
        );
        assert_eq!(
            field(fields, "epub.date"),
            &Kind::StringValue("1843-10-01".to_owned()),
            "the date is verbatim, never normalized"
        );
        assert_eq!(
            field(fields, "epub.opf_href"),
            &Kind::StringValue("OEBPS/content.opf".to_owned())
        );

        let Kind::ListValue(creators) = field(fields, "epub.creators") else {
            panic!("creators is a list");
        };
        assert_eq!(creators.values.len(), 2);

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
            "only images have a docling slot; the rest stay on the typed stream"
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
