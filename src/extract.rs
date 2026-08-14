// SPDX-License-Identifier: Apache-2.0

//! The parse driver: one EPUB in, a live event stream out.
//!
//! # Why the upload is buffered and the output still is not
//!
//! A ZIP's central directory is at the *end* of the file, so nothing in an
//! archive can be located until the last byte has arrived. Buffering the
//! upload is therefore forced by the format, and no amount of protocol design
//! removes it.
//!
//! What is not forced is buffering the *output*, and that is where Docling's
//! EPUB backend and this one part company. Docling unpacks to a temp
//! directory, runs every chapter through an HTML backend, and returns one
//! document at the end. Here, `info` goes out as soon as the OPF is parsed —
//! before a single chapter has been inflated — and each `chapter` goes out as
//! its entry comes off the archive. Nothing is accumulated: the only chapter
//! in memory at any moment is the one being sent. A reader can paint chapter 1
//! while chapter 12 is still compressed.
//!
//! `tests/streaming.rs` holds the test that fails if someone turns this back
//! into a batch.
//!
//! # Emission order
//!
//! Chapters go out in spine order, which is reading order and the only order
//! that means anything to a reader. Resources go out **at the point their
//! archive entry is reached during that walk**: a cover image stored before
//! the text arrives before chapter 1, and one stored after the text arrives
//! after the last chapter. This is what "as its entry is hit" means, and it is
//! deterministic for a given archive.
//!
//! The consequence for clients is stated on the `Resource` message: a chapter
//! may reference a resource that has not arrived yet. Buffer by href and
//! resolve when the stream ends.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::Status;

use crate::archive::{self, Budget, EntryInfo, MemoryArchive};
use crate::document_fold::DocumentFold;
use crate::href::{self, Target};
use crate::limits::Effective;
use crate::metrics::Metrics;
use crate::opf::{self, ManifestItem, Package};
use crate::proto::v1 as pb;

/// Archive path of the container document, fixed by the EPUB specification.
const CONTAINER_PATH: &str = "META-INF/container.xml";

/// Archive path of the encryption descriptor. Its presence means DRM or font
/// obfuscation, neither of which this service implements.
const ENCRYPTION_PATH: &str = "META-INF/encryption.xml";

/// Archive path of the media-type marker.
const MIMETYPE_PATH: &str = "mimetype";

/// The only value `mimetype` may hold in a conforming EPUB.
///
/// Also what [`DocumentFold`](crate::document_fold::DocumentFold) records as
/// `DocumentOrigin.mimetype`, so the two cannot drift apart.
pub const EPUB_MIMETYPE: &str = "application/epub+zip";

/// Largest `mimetype` entry worth reading. The conforming value is 20 bytes;
/// anything past this is not a marker.
const MAX_MIMETYPE_BYTES: u64 = 256;

/// Largest number of warnings carried on `ParseStatus`.
///
/// A book with ten thousand excluded resources would otherwise put ten
/// thousand warnings in the trailer, which turns a receipt into a payload.
/// Excluded-kind warnings are already collapsed to one per kind; this bounds
/// everything else.
const MAX_WARNINGS: usize = 64;

/// How the call ended.
#[derive(Debug)]
pub enum Outcome {
    /// A `status` trailer was delivered. The stream ends cleanly.
    Complete,
    /// The client stopped reading. Nothing further can be said to it.
    Abandoned,
    /// The call must end with this gRPC status.
    Failed(Box<Status>),
}

/// Internal control flow: either the client is gone or the call has failed.
enum Abort {
    /// The client stopped reading.
    Gone,
    /// The call must end with this status.
    Failed(Box<Status>),
}

impl From<Status> for Abort {
    fn from(status: Status) -> Self {
        Self::Failed(Box::new(status))
    }
}

/// The outbound half of a response stream, usable from a blocking thread.
///
/// Backpressure lives here. The fast path is a non-blocking `try_send`; only a
/// consumer that is genuinely behind reaches the waiting path, and that wait
/// is bounded, because a client that has stopped reading altogether would
/// otherwise pin a blocking-pool thread forever.
///
/// The optional Document fold also lives here, because this is the one place
/// every outbound event passes through: an event that never reaches the client
/// must not reach the projection either, and putting the fold anywhere else
/// would mean remembering to feed it at each `emit` call site.
pub struct Sink {
    /// The channel feeding the tonic response stream.
    tx: mpsc::Sender<Result<pb::ParseEpubResponse, Status>>,
    /// How long to wait on a full channel before giving the call up.
    stall: Duration,
    /// The Document projection, present only when the caller asked for one.
    ///
    /// `RefCell` rather than a lock: a parse runs on exactly one blocking
    /// thread, and the whole emission path holds `&Sink`, so this is interior
    /// mutability without contention rather than shared state.
    fold: Option<RefCell<DocumentFold>>,
}

impl Sink {
    /// Wrap a response channel, with a Document fold when one was asked for.
    #[must_use]
    pub fn new(
        tx: mpsc::Sender<Result<pb::ParseEpubResponse, Status>>,
        stall: Duration,
        fold: Option<DocumentFold>,
    ) -> Self {
        Self {
            tx,
            stall,
            fold: fold.map(RefCell::new),
        }
    }

    /// Send one event, folding it into the Document projection on the way.
    ///
    /// The trailer is the trigger: when `status` is emitted the fold has seen
    /// everything, so the `document` event goes out first and `status` stays
    /// last. With no fold this is exactly [`Sink::send`] and costs nothing.
    fn emit(&self, event: pb::parse_epub_response::Event) -> Result<(), Abort> {
        if let Some(fold) = self.fold.as_ref() {
            let document = {
                let mut fold = fold.borrow_mut();
                fold.consume(&event);
                matches!(event, pb::parse_epub_response::Event::Status(_)).then(|| fold.take())
            };
            if let Some(document) = document {
                self.send(pb::parse_epub_response::Event::Document(document))?;
            }
        }
        self.send(event)
    }

    /// Put one event on the wire, waiting for the client if the channel is
    /// full.
    fn send(&self, event: pb::parse_epub_response::Event) -> Result<(), Abort> {
        let message = Ok(pb::ParseEpubResponse { event: Some(event) });
        let message = match self.tx.try_send(message) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(Abort::Gone),
            Err(mpsc::error::TrySendError::Full(message)) => message,
        };
        // Inside `spawn_blocking` there is always a runtime handle; be
        // defensive anyway, because blocking without one panics.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Err(Abort::Gone);
        };
        match runtime.block_on(self.tx.send_timeout(message, self.stall)) {
            Ok(()) => Ok(()),
            // A client that has read nothing for the whole stall window has
            // abandoned the call. Reporting a status to it would mean sending
            // on the channel it is not draining, so there is nothing useful
            // left to do but free the thread.
            Err(_) => Err(Abort::Gone),
        }
    }
}

/// Unpack `bytes` and stream the book into `sink`.
///
/// Synchronous on purpose: inflating is CPU-bound, so the caller runs this on
/// [`tokio::task::spawn_blocking`] and [`Sink`] bridges back to the async
/// channel. Everything stays in memory; nothing is written anywhere.
#[must_use]
pub fn run(bytes: &[u8], limits: &Effective, metrics: &Metrics, sink: &Sink) -> Outcome {
    match parse(bytes, limits, metrics, sink) {
        Ok(()) => Outcome::Complete,
        Err(Abort::Gone) => Outcome::Abandoned,
        Err(Abort::Failed(status)) => Outcome::Failed(status),
    }
}

/// One manifest item, resolved against the archive.
struct Resolved<'a> {
    /// The manifest item as the OPF declared it.
    item: &'a ManifestItem,
    /// Normalized archive path, or the absolute URI for a remote resource.
    path: String,
    /// Position in `entries`, absent when the archive has no such file.
    entry: Option<usize>,
    /// Whether the href was an absolute URI rather than an archive path.
    remote: bool,
}

/// Collects the non-fatal observations that end up on `ParseStatus`.
#[derive(Default)]
struct Warnings {
    /// Warnings gathered so far, capped at [`MAX_WARNINGS`].
    entries: Vec<pb::ParseWarning>,
    /// Resource kinds already reported as excluded, so the trailer carries one
    /// warning per kind rather than one per file.
    reported_kinds: HashSet<i32>,
}

impl Warnings {
    /// Record a warning, dropping it once the cap is reached.
    fn push(&mut self, code: pb::ParseWarningCode, href: &str, message: impl Into<String>) {
        if self.entries.len() >= MAX_WARNINGS {
            return;
        }
        self.entries.push(pb::ParseWarning {
            code: code as i32,
            message: message.into(),
            href: href.to_owned(),
        });
    }

    /// Record an excluded resource kind, at most once per kind.
    fn exclude_kind(&mut self, kind: pb::ResourceKind) {
        if !self.reported_kinds.insert(kind as i32) {
            return;
        }
        self.push(
            pb::ParseWarningCode::ResourceKindExcluded,
            "",
            format!(
                "{} resources were not emitted; see ParseOptions",
                kind.as_str_name()
            ),
        );
    }
}

/// Classify a manifest item by its declared media type.
///
/// The declared type, never the extension and never the bytes: the manifest is
/// the book's own statement about its files, and disagreeing with it here
/// would make the include options mean something different from what the OPF
/// says.
fn classify(media_type: &str) -> pb::ResourceKind {
    let media_type = media_type.trim().to_ascii_lowercase();
    let base = media_type.split(';').next().unwrap_or("").trim();
    match base {
        "" => pb::ResourceKind::Unspecified,
        "text/css" => pb::ResourceKind::Stylesheet,
        "application/xhtml+xml"
        | "text/html"
        | "application/x-dtbncx+xml"
        | "text/x-oeb1-document" => pb::ResourceKind::Document,
        "application/vnd.ms-opentype" | "application/font-woff" | "application/font-sfnt" => {
            pb::ResourceKind::Font
        }
        other if other.starts_with("image/") => pb::ResourceKind::Image,
        other if other.starts_with("font/") => pb::ResourceKind::Font,
        other if other.starts_with("audio/") => pb::ResourceKind::Audio,
        other if other.starts_with("video/") => pb::ResourceKind::Video,
        other if other.starts_with("application/x-font") => pb::ResourceKind::Font,
        _ => pb::ResourceKind::Other,
    }
}

/// Whether a resource of this kind is emitted under these options.
const fn selected(kind: pb::ResourceKind, limits: &Effective) -> bool {
    if limits.include_all_resources {
        return true;
    }
    match kind {
        // Navigation and non-spine markup are small and are what a reader
        // needs to build a table of contents, so they are never excluded.
        pb::ResourceKind::Document => true,
        pb::ResourceKind::Image => limits.include_images,
        pb::ResourceKind::Stylesheet => limits.include_stylesheets,
        _ => false,
    }
}

/// True for a media type or path that would be another archive.
///
/// ZIP-in-ZIP is a non-goal, and recursing into one is exactly how a
/// decompression bomb hides from a single-level cap. The inner archive is
/// never opened; it is reported and skipped.
fn is_nested_archive(media_type: &str, path: &str) -> bool {
    let media_type = media_type.trim().to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    matches!(
        media_type.as_str(),
        "application/zip" | "application/epub+zip" | "application/x-zip-compressed"
    ) || std::path::Path::new(&path)
        .extension()
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("epub")
        })
}

/// Look an archive path up, tolerating a producer that never percent-encoded.
///
/// `resolve` decodes `%20` to a space because that is what an IRI reference
/// means. Some producers write the literal `%20` into both the href and the
/// entry name, and for those the decoded form matches nothing. Trying the raw
/// form second costs one hash lookup and reads those books.
fn lookup(
    index: &HashMap<String, usize>,
    decoded: &str,
    raw_base: &str,
    raw_href: &str,
) -> Option<usize> {
    if let Some(found) = index.get(decoded) {
        return Some(*found);
    }
    let raw = href::normalize(raw_base, raw_href.split('#').next().unwrap_or_default()).ok()?;
    index.get(&raw).copied()
}

/// The whole parse, with every failure as a `?`.
#[allow(clippy::too_many_lines)] // A pipeline; splitting it would hide the order.
fn parse(bytes: &[u8], limits: &Effective, metrics: &Metrics, sink: &Sink) -> Result<(), Abort> {
    let mut archive = archive::open(bytes, limits)?;
    let entries = archive::scan(&mut archive)?;
    let index: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.name.clone(), position))
        .collect();

    let mut budget = Budget::new(limits.max_uncompressed_bytes);
    let mut warnings = Warnings::default();

    // --- Is this an EPUB at all? -------------------------------------------
    if index.contains_key(ENCRYPTION_PATH) {
        return Err(Status::unimplemented(
            "the archive carries META-INF/encryption.xml; DRM and font obfuscation are not \
             supported",
        )
        .into());
    }

    let declared = read_named(
        &mut archive,
        &entries,
        &index,
        MIMETYPE_PATH,
        limits,
        &mut budget,
    )?
    .filter(|bytes| bytes.len() as u64 <= MAX_MIMETYPE_BYTES)
    .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned());
    let claims_epub = declared.as_deref() == Some(EPUB_MIMETYPE);

    let Some(&container_position) = index.get(CONTAINER_PATH) else {
        // The one place the two "wrong input" statuses have to be told apart.
        // A file that says it is an EPUB and then has no container is a broken
        // EPUB, which is the caller's to fix; a ZIP that never claimed to be
        // one is a format this service does not implement.
        return Err(if claims_epub {
            Status::invalid_argument(
                "the archive declares itself an EPUB but has no META-INF/container.xml",
            )
        } else {
            Status::unimplemented(format!(
                "this is a ZIP archive but not an EPUB: no META-INF/container.xml, and \
                 `mimetype` is {}",
                declared
                    .as_deref()
                    .map_or("absent".to_owned(), |m| format!("{m:?}"))
            ))
        }
        .into());
    };
    if !claims_epub {
        warnings.push(
            pb::ParseWarningCode::Metadata,
            MIMETYPE_PATH,
            match declared.as_deref() {
                Some(found) => format!("`mimetype` is {found:?}, expected {EPUB_MIMETYPE:?}"),
                None => "the archive has no `mimetype` entry".to_owned(),
            },
        );
    }

    // --- container.xml -> OPF ----------------------------------------------
    let container = read_at(
        &mut archive,
        &entries[container_position],
        limits,
        &mut budget,
        metrics,
    )?;
    let opf_href = opf::parse_container(&container)
        .map_err(|e| Status::invalid_argument(format!("{CONTAINER_PATH}: {e}")))?;
    let Ok(Target::Entry(opf_path)) = href::resolve("", &opf_href) else {
        return Err(Status::invalid_argument(format!(
            "{CONTAINER_PATH} names an unusable rootfile {opf_href:?}"
        ))
        .into());
    };
    let Some(opf_position) = lookup(&index, &opf_path, "", &opf_href) else {
        return Err(Status::invalid_argument(format!(
            "{CONTAINER_PATH} names the rootfile {opf_path:?}, which the archive does not contain"
        ))
        .into());
    };

    let opf_bytes = read_at(
        &mut archive,
        &entries[opf_position],
        limits,
        &mut budget,
        metrics,
    )?;
    let package: Package = opf::parse_package(&opf_bytes)
        .map_err(|e| Status::invalid_argument(format!("{opf_path}: {e}")))?;
    let opf_dir = href::parent_dir(&opf_path).to_owned();

    // --- Resolve the manifest ----------------------------------------------
    let mut resolved: Vec<Resolved<'_>> = Vec::with_capacity(package.manifest.len());
    let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(package.manifest.len());
    for item in &package.manifest {
        let target = href::resolve(&opf_dir, &item.href).map_err(|e| {
            Status::invalid_argument(format!(
                "manifest item {:?} has an unusable href {:?}: {e}",
                item.id, item.href
            ))
        })?;
        let entry = match &target {
            Target::Entry(path) => lookup(&index, path, &opf_dir, &item.href),
            Target::Remote(_) => None,
        };
        let (path, remote) = match target {
            Target::Entry(path) => (path, false),
            Target::Remote(uri) => (uri, true),
        };
        by_id.entry(item.id.as_str()).or_insert(resolved.len());
        resolved.push(Resolved {
            item,
            path,
            entry,
            remote,
        });
    }

    // --- Resolve the spine, before a single event goes out ------------------
    //
    // The central directory gives the whole file list up front, so a broken
    // spine can be diagnosed before the stream opens. That matters: a client
    // that has already been told "eight chapters" and then gets an error after
    // three has to unwind, while a call that never opened is just a failure.
    let mut spine: Vec<(usize, &opf::SpineItem)> = Vec::with_capacity(package.spine.len());
    for itemref in &package.spine {
        let Some(&position) = by_id.get(itemref.idref.as_str()) else {
            return Err(Status::invalid_argument(format!(
                "spine itemref {:?} names no manifest item",
                itemref.idref
            ))
            .into());
        };
        let target = &resolved[position];
        if target.remote {
            return Err(Status::invalid_argument(format!(
                "spine item {:?} points outside the archive at {:?}; remote chapters are not \
                 supported",
                itemref.idref, target.path
            ))
            .into());
        }
        if target.entry.is_none() {
            return Err(Status::invalid_argument(format!(
                "spine item {:?} names {:?}, which the archive does not contain",
                itemref.idref, target.path
            ))
            .into());
        }
        spine.push((position, itemref));
    }

    // --- info ---------------------------------------------------------------
    let spine_ids: HashSet<usize> = spine.iter().map(|(position, _)| *position).collect();
    let cover_href = cover(&resolved, &package);
    let info = pb::EpubInfo {
        title: package.metadata.title.clone(),
        creators: package.metadata.creators.clone(),
        contributors: package.metadata.contributors.clone(),
        language: package.metadata.language.clone(),
        identifiers: package
            .metadata
            .identifiers
            .iter()
            .map(|identifier| pb::DublinCoreIdentifier {
                value: identifier.value.clone(),
                id: identifier.id.clone(),
                scheme: identifier.scheme.clone(),
            })
            .collect(),
        unique_identifier: package
            .metadata
            .identifiers
            .iter()
            .find(|identifier| {
                !package.unique_identifier_id.is_empty()
                    && identifier.id == package.unique_identifier_id
            })
            .map(|identifier| identifier.value.clone())
            .unwrap_or_default(),
        publisher: package.metadata.publisher.clone(),
        description: package.metadata.description.clone(),
        date: package.metadata.date.clone(),
        subjects: package.metadata.subjects.clone(),
        spine_item_count: u32::try_from(spine.len()).unwrap_or(u32::MAX),
        opf_href: opf_path.clone(),
        epub_version: package.version.clone(),
        cover_href,
    };
    sink.emit(pb::parse_epub_response::Event::Info(info))?;

    // --- Plan the resource interleave ---------------------------------------
    let mut pending: Vec<usize> = Vec::new();
    let mut skipped = 0u32;
    for (position, target) in resolved.iter().enumerate() {
        if spine_ids.contains(&position) {
            continue;
        }
        let kind = classify(&target.item.media_type);
        if target.remote {
            skipped += 1;
            warnings.push(
                pb::ParseWarningCode::MissingManifestEntry,
                &target.path,
                "the manifest points at a remote resource; nothing is fetched over the network",
            );
            continue;
        }
        let Some(entry) = target.entry else {
            skipped += 1;
            warnings.push(
                pb::ParseWarningCode::MissingManifestEntry,
                &target.path,
                "the manifest names a file the archive does not contain",
            );
            continue;
        };
        if is_nested_archive(&target.item.media_type, &target.path) {
            skipped += 1;
            warnings.push(
                pb::ParseWarningCode::NestedArchive,
                &target.path,
                "nested archives are not opened",
            );
            continue;
        }
        if !selected(kind, limits) {
            skipped += 1;
            warnings.exclude_kind(kind);
            continue;
        }
        pending.push(entry);
    }
    // Archive order, which is what makes a resource arrive when its entry is
    // hit rather than at the end.
    pending.sort_unstable();
    let mut pending = pending.into_iter().peekable();
    let position_of_entry: HashMap<usize, usize> = resolved
        .iter()
        .enumerate()
        .filter_map(|(position, target)| target.entry.map(|entry| (entry, position)))
        .collect();

    // --- Walk the spine ------------------------------------------------------
    let mut chapters = 0u32;
    let mut emitted = 0u32;
    for (spine_index, (position, itemref)) in spine.iter().enumerate() {
        let target = &resolved[*position];
        let chapter_entry = target.entry.expect("checked while resolving the spine");

        while pending.peek().is_some_and(|entry| *entry < chapter_entry) {
            let entry = pending.next().expect("peeked");
            emit_resource(
                &mut archive,
                &entries[entry],
                &resolved[position_of_entry[&entry]],
                limits,
                &mut budget,
                metrics,
                sink,
            )?;
            emitted += 1;
        }

        let content = read_at(
            &mut archive,
            &entries[chapter_entry],
            limits,
            &mut budget,
            metrics,
        )?;
        sink.emit(pb::parse_epub_response::Event::Chapter(pb::Chapter {
            spine_index: u32::try_from(spine_index).unwrap_or(u32::MAX),
            idref: itemref.idref.clone(),
            href: target.path.clone(),
            media_type: target.item.media_type.clone(),
            content,
            linear: itemref.linear,
            properties: target.item.properties.clone(),
        }))?;
        metrics.chapter_emitted();
        chapters += 1;
    }

    for entry in pending {
        emit_resource(
            &mut archive,
            &entries[entry],
            &resolved[position_of_entry[&entry]],
            limits,
            &mut budget,
            metrics,
            sink,
        )?;
        emitted += 1;
    }

    // --- status --------------------------------------------------------------
    sink.emit(pb::parse_epub_response::Event::Status(pb::ParseStatus {
        chapters_emitted: chapters,
        resources_emitted: emitted,
        resources_skipped: skipped,
        uncompressed_bytes: budget.consumed(),
        entries_read: budget.entries(),
        warnings: warnings.entries,
    }))?;
    Ok(())
}

/// Find the cover image's archive path, if the book names one.
fn cover(resolved: &[Resolved<'_>], package: &Package) -> String {
    // EPUB 3: a manifest property.
    if let Some(target) = resolved.iter().find(|target| {
        target
            .item
            .properties
            .iter()
            .any(|property| property == "cover-image")
    }) {
        return target.path.clone();
    }
    // EPUB 2: `<meta name="cover" content="…">` naming a manifest item.
    if package.metadata.cover_id.is_empty() {
        return String::new();
    }
    resolved
        .iter()
        .find(|target| target.item.id == package.metadata.cover_id)
        .map(|target| target.path.clone())
        .unwrap_or_default()
}

/// Inflate an entry and send it as a `resource` event.
fn emit_resource(
    archive: &mut MemoryArchive<'_>,
    entry: &EntryInfo,
    target: &Resolved<'_>,
    limits: &Effective,
    budget: &mut Budget,
    metrics: &Metrics,
    sink: &Sink,
) -> Result<(), Abort> {
    let content = read_at(archive, entry, limits, budget, metrics)?;
    sink.emit(pb::parse_epub_response::Event::Resource(pb::Resource {
        href: target.path.clone(),
        media_type: target.item.media_type.clone(),
        kind: classify(&target.item.media_type) as i32,
        content,
        manifest_id: target.item.id.clone(),
        properties: target.item.properties.clone(),
    }))?;
    metrics.resource_emitted();
    Ok(())
}

/// Inflate one entry, counting what it cost.
fn read_at(
    archive: &mut MemoryArchive<'_>,
    entry: &EntryInfo,
    limits: &Effective,
    budget: &mut Budget,
    metrics: &Metrics,
) -> Result<Vec<u8>, Status> {
    let bytes = archive::read_entry(archive, entry, limits, budget)?;
    metrics.inflated(bytes.len() as u64);
    Ok(bytes)
}

/// Inflate an entry by name, if the archive has it.
fn read_named(
    archive: &mut MemoryArchive<'_>,
    entries: &[EntryInfo],
    index: &HashMap<String, usize>,
    name: &str,
    limits: &Effective,
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>, Status> {
    let Some(&position) = index.get(name) else {
        return Ok(None);
    };
    archive::read_entry(archive, &entries[position], limits, budget).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types_classify_by_what_the_manifest_declares() {
        assert_eq!(classify("image/png"), pb::ResourceKind::Image);
        assert_eq!(classify("image/svg+xml"), pb::ResourceKind::Image);
        assert_eq!(classify("text/css"), pb::ResourceKind::Stylesheet);
        assert_eq!(
            classify("application/xhtml+xml"),
            pb::ResourceKind::Document
        );
        assert_eq!(
            classify("application/x-dtbncx+xml"),
            pb::ResourceKind::Document
        );
        assert_eq!(classify("font/woff2"), pb::ResourceKind::Font);
        assert_eq!(
            classify("application/vnd.ms-opentype"),
            pb::ResourceKind::Font
        );
        assert_eq!(classify("audio/mpeg"), pb::ResourceKind::Audio);
        assert_eq!(classify("application/smil+xml"), pb::ResourceKind::Other);
        assert_eq!(classify(""), pb::ResourceKind::Unspecified);
        // Parameters and case are noise.
        assert_eq!(
            classify("TEXT/CSS; charset=utf-8"),
            pb::ResourceKind::Stylesheet
        );
    }

    #[test]
    fn defaults_emit_markup_and_images_and_nothing_else() {
        let limits = Effective::default();
        assert!(selected(pb::ResourceKind::Document, &limits));
        assert!(selected(pb::ResourceKind::Image, &limits));
        assert!(!selected(pb::ResourceKind::Stylesheet, &limits));
        assert!(!selected(pb::ResourceKind::Font, &limits));
        assert!(!selected(pb::ResourceKind::Other, &limits));
    }

    #[test]
    fn include_all_resources_overrides_every_exclusion() {
        let limits = Effective {
            include_images: false,
            include_all_resources: true,
            ..Effective::default()
        };
        assert!(selected(pb::ResourceKind::Font, &limits));
        assert!(selected(pb::ResourceKind::Image, &limits));
        assert!(selected(pb::ResourceKind::Other, &limits));
    }

    #[test]
    fn nested_archives_are_recognized_by_type_or_name() {
        assert!(is_nested_archive("application/zip", "extra/bundle.dat"));
        assert!(is_nested_archive(
            "application/octet-stream",
            "extra/inner.EPUB"
        ));
        assert!(!is_nested_archive("image/png", "images/cover.png"));
    }

    #[test]
    fn warnings_are_capped_and_kinds_collapse() {
        let mut warnings = Warnings::default();
        for _ in 0..10 {
            warnings.exclude_kind(pb::ResourceKind::Font);
        }
        assert_eq!(warnings.entries.len(), 1, "one warning per excluded kind");

        for i in 0..MAX_WARNINGS * 2 {
            warnings.push(pb::ParseWarningCode::Metadata, "x", format!("{i}"));
        }
        assert_eq!(warnings.entries.len(), MAX_WARNINGS);
    }
}
