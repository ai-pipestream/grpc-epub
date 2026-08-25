// SPDX-License-Identifier: Apache-2.0

//! The two XML documents this service parses: `META-INF/container.xml` and
//! the OPF package document.
//!
//! Nothing else in an EPUB is parsed as XML. Chapter XHTML goes out as bytes,
//! because HTML semantics belong to the HTML collector and reimplementing them
//! here would be a second, worse answer to a question already answered.
//!
//! # External entities
//!
//! quick-xml has no DTD processor, so it cannot fetch a `SYSTEM` entity even
//! if asked. That is a property of the library rather than of a setting, and a
//! property nobody wrote down is a property that gets regressed, so this
//! module makes it explicit and testable in two ways:
//!
//! 1. A `<!DOCTYPE …>` whose internal subset declares an `<!ENTITY …>` is
//!    refused outright ([`XmlError::EntityDeclaration`]). A package document
//!    has no honest reason to declare entities, and refusing is a clearer
//!    answer than silently producing a title with a hole in it.
//! 2. quick-xml surfaces every other general reference as its own
//!    `Event::GeneralRef` rather than expanding it. [`Text::push_ref`] resolves
//!    character references and the five XML built-ins and copies anything else
//!    through **verbatim**, so `&xxe;` reaches the client as the four
//!    characters `&xxe;` and can never be the contents of a file.
//!
//! `tests/security.rs` asserts both.

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Why an XML document could not be used. Every variant is a caller error and
/// maps to `INVALID_ARGUMENT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XmlError {
    /// The bytes are not well-formed XML, or are not valid UTF-8.
    Malformed(String),
    /// A DTD internal subset declared an entity. Refused; see the module
    /// documentation.
    EntityDeclaration,
    /// `container.xml` named no rootfile.
    NoRootfile,
    /// The OPF root element is not `<package>`.
    NotAPackage,
    /// The OPF declared no `<spine>`, or the spine held no `<itemref>`.
    EmptySpine,
}

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "malformed XML: {detail}"),
            Self::EntityDeclaration => f.write_str(
                "the document declares a DTD entity; entity declarations are refused because \
                 they are the shape an XXE attack arrives in and no package document needs one",
            ),
            Self::NoRootfile => {
                f.write_str("META-INF/container.xml names no <rootfile full-path=…>")
            }
            Self::NotAPackage => f.write_str("the OPF root element is not <package>"),
            Self::EmptySpine => f.write_str("the OPF declares no spine items"),
        }
    }
}

impl From<quick_xml::Error> for XmlError {
    fn from(err: quick_xml::Error) -> Self {
        Self::Malformed(err.to_string())
    }
}

/// One `<item>` of the OPF manifest: a file the book contains.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestItem {
    /// The `id` attribute. Spine itemrefs point at this.
    pub id: String,
    /// The `href` attribute, raw, still relative to the OPF's directory and
    /// still percent-encoded.
    pub href: String,
    /// The `media-type` attribute.
    pub media_type: String,
    /// EPUB 3 `properties` tokens, whitespace-split. Empty for EPUB 2.
    pub properties: Vec<String>,
    /// The `media-overlay` attribute: the `id` of the manifest item holding
    /// this item's SMIL narration. Empty when the item has no overlay.
    ///
    /// This attribute is the *only* link an EPUB draws between a chapter and
    /// the recorded reading of it. Dropping it severs the connection at the
    /// packaging layer, before anything downstream gets to decide what to do
    /// with the alignment, so it is read here even though the SMIL itself is
    /// parsed elsewhere.
    pub media_overlay: String,
}

/// One `<itemref>` of the OPF spine: a position in reading order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpineItem {
    /// The `idref` attribute, naming a [`ManifestItem::id`].
    pub idref: String,
    /// The `linear` attribute. Absent means true, per the EPUB spec.
    pub linear: bool,
}

/// One `<dc:identifier>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identifier {
    /// The element's text content.
    pub value: String,
    /// The `id` attribute, empty when absent.
    pub id: String,
    /// EPUB 2's `opf:scheme` attribute, empty when absent.
    pub scheme: String,
}

/// One EPUB 3 `<meta refines="#id" property="p">v</meta>`, or the EPUB 2
/// attribute that plays the same role.
///
/// Refinements are how EPUB 3 says everything interesting about a metadata
/// element: which creator is the author and which the illustrator, how to sort
/// a name, whether a title is the main one or a subtitle. The two dialects
/// spell it differently (`opf:role` on the element in EPUB 2, a refining
/// `<meta>` in EPUB 3) and both land here, so consumers never have to know
/// which one the producer used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refinement {
    /// The `property` name: `role`, `file-as`, `title-type`, `identifier-type`.
    pub property: String,
    /// The refinement's value.
    pub value: String,
    /// The `scheme` attribute, naming the vocabulary `value` is drawn from.
    pub scheme: String,
}

/// One metadata element, verbatim, with whatever refines it.
///
/// The typed fields on [`Metadata`] are a reading of these; this is the record
/// they were read from. Keeping it means a consumer that wants an element this
/// service has no opinion about does not need this service to grow an opinion
/// first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaEntry {
    /// Local name for a Dublin Core element (`title`, `rights`), or the
    /// `property` for a bare EPUB 3 `<meta>` (`dcterms:modified`).
    pub element: String,
    /// The element's text content, verbatim and trimmed.
    pub value: String,
    /// The `id` attribute, empty when absent. Refinements name this.
    pub id: String,
    /// EPUB 2's `opf:scheme` attribute, empty when absent.
    pub scheme: String,
    /// Everything refining this element, in document order.
    pub refinements: Vec<Refinement>,
}

impl MetaEntry {
    /// The value of the first refinement with this property, or `""`.
    #[must_use]
    pub fn refined(&self, property: &str) -> &str {
        self.refinements
            .iter()
            .find(|refinement| refinement.property == property)
            .map_or("", |refinement| refinement.value.as_str())
    }
}

/// A person or organization the OPF credits, with the role it credits them in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contributor {
    /// The element's text content: the name as the book displays it.
    pub name: String,
    /// MARC relator code from a `role` refinement (`aut`, `ill`, `trl`), or
    /// EPUB 2's `opf:role`. Empty when the book did not say.
    pub role: String,
    /// Sort form from a `file-as` refinement or EPUB 2's `opf:file-as`
    /// (`Lovelace, Ada`). Empty when the book did not say.
    pub file_as: String,
}

/// One `<dc:title>` with the role EPUB 3 gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Title {
    /// The title text.
    pub value: String,
    /// A `title-type` refinement: `main`, `subtitle`, `collection`, `edition`.
    /// Empty when the book did not say, which is every EPUB 2 book.
    pub title_type: String,
}

/// The Dublin Core metadata of an OPF package document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    /// The title marked `main` by a refinement, or the first `dc:title` when
    /// none is marked.
    pub title: String,
    /// Every `dc:title`, in document order, with its declared type.
    pub titles: Vec<Title>,
    /// Every `dc:creator`, in document order, with role and sort name.
    pub creators: Vec<Contributor>,
    /// Every `dc:contributor`, in document order, with role and sort name.
    pub contributors: Vec<Contributor>,
    /// The first `dc:language`.
    pub language: String,
    /// Every `dc:language`, in document order. A bilingual edition declares
    /// more than one and the first alone misrepresents it.
    pub languages: Vec<String>,
    /// Every `dc:identifier`, in document order.
    pub identifiers: Vec<Identifier>,
    /// The first `dc:publisher`.
    pub publisher: String,
    /// The first `dc:description`.
    pub description: String,
    /// The first `dc:date`, verbatim.
    pub date: String,
    /// `<meta property="dcterms:modified">`, verbatim. Mandatory in EPUB 3 and
    /// the natural freshness signal for anything that re-crawls a book.
    pub modified: String,
    /// Every `dc:subject`, in document order.
    pub subjects: Vec<String>,
    /// The first `dc:rights`.
    pub rights: String,
    /// The first `dc:source`.
    pub source: String,
    /// The first `dc:type`.
    pub resource_type: String,
    /// The first `dc:format`.
    pub format: String,
    /// The first `dc:coverage`.
    pub coverage: String,
    /// The first `dc:relation`.
    pub relation: String,
    /// The manifest item id from EPUB 2's `<meta name="cover" content="…">`.
    pub cover_id: String,
    /// Every metadata element the parser kept, in document order, with its
    /// refinements attached. The lossless tail the typed fields above are a
    /// reading of.
    pub entries: Vec<MetaEntry>,
}

/// A parsed OPF package document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    /// The `version` attribute of `<package>`, empty when absent.
    pub version: String,
    /// The `unique-identifier` attribute, naming an [`Identifier::id`].
    pub unique_identifier_id: String,
    /// Dublin Core metadata.
    pub metadata: Metadata,
    /// Every manifest item, in document order.
    pub manifest: Vec<ManifestItem>,
    /// Every spine itemref, in reading order.
    pub spine: Vec<SpineItem>,
    /// The `<spine toc="…">` attribute: the manifest `id` of the EPUB 2 NCX.
    /// Empty for a book that ships only an EPUB 3 nav document.
    pub toc_idref: String,
}

/// Build a reader with the settings both documents are parsed under.
fn reader(bytes: &[u8]) -> Reader<&[u8]> {
    let mut reader = Reader::from_reader(bytes);
    let config = reader.config_mut();
    // Well-formedness is worth enforcing: a package document whose tags do not
    // nest is a document whose spine order cannot be trusted either.
    config.check_end_names = true;
    config.allow_unmatched_ends = false;
    // `<item …/>` and `<item …></item>` must reach the same arm.
    config.expand_empty_elements = false;
    reader
}

/// Look up one attribute by local name.
///
/// Local rather than qualified because EPUB 2 writes `opf:scheme` where EPUB 3
/// writes nothing at all, and because both documents bind their default
/// namespace differently in the wild. `xmlns` declarations are skipped so an
/// `xmlns:content="…"` cannot be mistaken for a `content` attribute.
fn attribute(start: &BytesStart<'_>, name: &str) -> String {
    for attr in start.attributes().with_checks(false).flatten() {
        if attr.key.as_ref().starts_with(b"xmlns") {
            continue;
        }
        if attr.key.local_name().as_ref() == name.as_bytes() {
            // `normalized_value` resolves the five predefined entities and
            // character references and nothing else — quick-xml documents the
            // replacement set as non-recursive — so an attribute cannot be a
            // way in for an entity that text is protected from.
            return match attr.normalized_value(XmlVersion::Implicit1_0) {
                Ok(value) => value.into_owned(),
                // An unresolvable reference in an attribute value: keep the
                // bytes as written rather than guess. Same rule as text.
                Err(_) => String::from_utf8_lossy(attr.value.as_ref()).into_owned(),
            };
        }
    }
    String::new()
}

/// Accumulator for an element's text content.
///
/// Exists so the entity policy lives in exactly one place. See the module
/// documentation.
#[derive(Default)]
struct Text(String);

impl Text {
    /// Append a text fragment.
    fn push_text(&mut self, raw: &[u8]) {
        self.0.push_str(&String::from_utf8_lossy(raw));
    }

    /// Append a general reference.
    ///
    /// Character references (`&#8212;`) and the five XML built-ins resolve.
    /// Everything else is copied through as the literal `&name;` it was
    /// written as, because resolving it would mean consulting a DTD and
    /// consulting a DTD is the whole of XXE.
    fn push_ref(&mut self, reference: &BytesRef<'_>) {
        if let Ok(Some(resolved)) = reference.resolve_char_ref() {
            self.0.push(resolved);
            return;
        }
        let name = String::from_utf8_lossy(reference.as_ref()).into_owned();
        match name.as_str() {
            "amp" => self.0.push('&'),
            "lt" => self.0.push('<'),
            "gt" => self.0.push('>'),
            "quot" => self.0.push('"'),
            "apos" => self.0.push('\''),
            _ => {
                self.0.push('&');
                self.0.push_str(&name);
                self.0.push(';');
            }
        }
    }

    /// Take the accumulated text, trimmed, and reset.
    fn take(&mut self) -> String {
        let value = self.0.trim().to_owned();
        self.0.clear();
        value
    }
}

/// Refuse a DTD that declares entities.
fn check_doctype(raw: &[u8]) -> Result<(), XmlError> {
    let text = String::from_utf8_lossy(raw).to_ascii_uppercase();
    if text.contains("<!ENTITY") || text.contains("!ENTITY") {
        return Err(XmlError::EntityDeclaration);
    }
    Ok(())
}

/// Parse `META-INF/container.xml` and return the OPF's archive path.
///
/// A container may name several rootfiles; the one with media type
/// `application/oebps-package+xml` wins, and if none declares that type the
/// first rootfile is used. That is the reading every EPUB reader implements,
/// and the alternative — refusing a book whose producer omitted a media type —
/// helps nobody.
///
/// # Errors
///
/// [`XmlError::Malformed`] if the XML does not parse, [`XmlError::NoRootfile`]
/// if it names none, [`XmlError::EntityDeclaration`] if it declares entities.
pub fn parse_container(bytes: &[u8]) -> Result<String, XmlError> {
    let mut reader = reader(bytes);
    let mut buf = Vec::new();
    let mut first: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::DocType(dtd) => check_doctype(dtd.as_ref())?,
            Event::Start(start) | Event::Empty(start) => {
                if start.local_name().as_ref() == b"rootfile" {
                    let path = attribute(&start, "full-path");
                    if path.is_empty() {
                        continue;
                    }
                    if attribute(&start, "media-type") == "application/oebps-package+xml" {
                        return Ok(path);
                    }
                    first.get_or_insert(path);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    first.ok_or(XmlError::NoRootfile)
}

/// Which part of the package document the parser is inside.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    /// Outside `<metadata>`, `<manifest>` and `<spine>`.
    #[default]
    Outside,
    /// Inside `<metadata>`.
    Metadata,
    /// Inside `<manifest>`.
    Manifest,
    /// Inside `<spine>`.
    Spine,
}

/// The Dublin Core elements whose text content is worth keeping.
///
/// All fifteen of the Dublin Core Metadata Element Set that an OPF may carry.
/// The list used to stop at nine, which dropped rights, source, type, format,
/// coverage and relation on the floor; `dc:rights` in particular is the one an
/// ingest pipeline is asked about, and keeping an element costs a match arm.
const DUBLIN_CORE: &[&[u8]] = &[
    b"title",
    b"creator",
    b"contributor",
    b"language",
    b"identifier",
    b"publisher",
    b"description",
    b"date",
    b"subject",
    b"rights",
    b"source",
    b"type",
    b"format",
    b"coverage",
    b"relation",
];

/// A metadata element whose text is still being collected.
struct Pending {
    /// Local name of the element, so the matching end tag can be recognized.
    local: Vec<u8>,
    /// The entry as far as its start tag described it.
    entry: MetaEntry,
    /// The `refines` target with its leading `#` removed. Non-empty only for a
    /// refining `<meta>`, which becomes a [`Refinement`] rather than an entry.
    refines: String,
}

/// Streaming state for one package document.
///
/// A struct rather than a pile of locals because the event loop has to hand
/// the same state to the start, end and text arms, and threading it through
/// free functions reads worse than owning it.
#[derive(Default)]
struct PackageParser {
    /// What has been recognized so far.
    package: Package,
    /// Whether the root `<package>` element has been seen.
    saw_package: bool,
    /// Which top-level section the parser is in.
    section: Section,
    /// Nesting depth, counting only non-empty elements.
    depth: usize,
    /// The depth of the element that opened the current section.
    section_depth: usize,
    /// The metadata element currently collecting text.
    collecting: Option<Pending>,
    /// Text collected for `collecting`.
    text: Text,
    /// Refinements as `(refines target, refinement)`, held until the whole
    /// metadata block has been read.
    ///
    /// A refinement may appear before or after the element it names, so they
    /// cannot be attached while streaming; the EPUB specification puts no
    /// ordering constraint on them and real books use both orders.
    refinements: Vec<(String, Refinement)>,
}

impl PackageParser {
    /// Handle a start or empty-element tag.
    fn open(&mut self, start: &BytesStart<'_>, empty: bool) -> Result<(), XmlError> {
        if !empty {
            self.depth += 1;
        }
        let local = start.local_name();
        let name = local.as_ref();

        if !self.saw_package {
            if name != b"package" {
                return Err(XmlError::NotAPackage);
            }
            self.saw_package = true;
            self.package.version = attribute(start, "version");
            self.package.unique_identifier_id = attribute(start, "unique-identifier");
            return Ok(());
        }

        match self.section {
            Section::Outside if !empty => match name {
                b"metadata" => self.enter(Section::Metadata),
                b"manifest" => self.enter(Section::Manifest),
                b"spine" => {
                    // `toc` names the NCX, which is the only way to find it in
                    // an EPUB 2 book: the manifest gives it no property.
                    self.package.toc_idref = attribute(start, "toc");
                    self.enter(Section::Spine);
                }
                _ => {}
            },
            Section::Outside => {}
            Section::Metadata => self.open_metadata(start, name, empty),
            Section::Manifest => {
                if name == b"item" {
                    self.package.manifest.push(ManifestItem {
                        id: attribute(start, "id"),
                        href: attribute(start, "href"),
                        media_type: attribute(start, "media-type"),
                        properties: attribute(start, "properties")
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect(),
                        media_overlay: attribute(start, "media-overlay"),
                    });
                }
            }
            Section::Spine => {
                if name == b"itemref" {
                    self.package.spine.push(SpineItem {
                        idref: attribute(start, "idref"),
                        // Absent means linear, per the EPUB spec; only the
                        // literal `no` opts out.
                        linear: attribute(start, "linear") != "no",
                    });
                }
            }
        }
        Ok(())
    }

    /// Enter a top-level section, remembering the depth to leave it at.
    fn enter(&mut self, section: Section) {
        self.section = section;
        self.section_depth = self.depth;
    }

    /// Handle a start tag inside `<metadata>`.
    fn open_metadata(&mut self, start: &BytesStart<'_>, name: &[u8], empty: bool) {
        let pending = if name == b"meta" {
            // EPUB 2's cover convention. EPUB 3 uses a manifest property
            // instead, which the manifest arm already collects.
            if attribute(start, "name").eq_ignore_ascii_case("cover") {
                let content = attribute(start, "content");
                if !content.is_empty() && self.package.metadata.cover_id.is_empty() {
                    self.package.metadata.cover_id = content;
                }
                return;
            }
            // EPUB 3's `<meta property="…">`, which is both the refinement
            // mechanism and the home of package-level properties such as
            // `dcterms:modified`. A `<meta>` with neither `name` nor
            // `property` says nothing anyone can read.
            let property = attribute(start, "property");
            if property.is_empty() {
                return;
            }
            Pending {
                local: name.to_vec(),
                entry: MetaEntry {
                    element: property,
                    id: attribute(start, "id"),
                    scheme: attribute(start, "scheme"),
                    ..MetaEntry::default()
                },
                refines: attribute(start, "refines")
                    .trim_start_matches('#')
                    .to_owned(),
            }
        } else if DUBLIN_CORE.contains(&name) {
            Pending {
                local: name.to_vec(),
                entry: MetaEntry {
                    element: String::from_utf8_lossy(name).into_owned(),
                    id: attribute(start, "id"),
                    scheme: attribute(start, "scheme"),
                    // EPUB 2 spells role and sort name as attributes on the
                    // element rather than as refining `<meta>` elements. Both
                    // dialects land in `refinements` so nothing downstream has
                    // to know which one the producer used.
                    refinements: epub2_refinements(start),
                    ..MetaEntry::default()
                },
                refines: String::new(),
            }
        } else {
            return;
        };

        self.text.take();
        if empty {
            // `<dc:title/>` or `<meta property="…"/>`: no text will follow, so
            // close it now.
            self.store(pending, String::new());
        } else {
            self.collecting = Some(pending);
        }
    }

    /// Handle an end tag.
    fn close(&mut self, name: &[u8]) {
        let closes_collection = self
            .collecting
            .as_ref()
            .is_some_and(|pending| pending.local.as_slice() == name);
        if closes_collection {
            let pending = self.collecting.take().expect("just matched");
            let value = self.text.take();
            self.store(pending, value);
        }
        if self.section != Section::Outside && self.depth == self.section_depth {
            self.section = Section::Outside;
        }
        self.depth = self.depth.saturating_sub(1);
    }

    /// File one finished metadata element, or hold it as a refinement.
    fn store(&mut self, pending: Pending, value: String) {
        let Pending {
            entry,
            refines,
            local: _,
        } = pending;
        if refines.is_empty() {
            self.package
                .metadata
                .entries
                .push(MetaEntry { value, ..entry });
        } else {
            self.refinements.push((
                refines,
                Refinement {
                    property: entry.element,
                    value,
                    scheme: entry.scheme,
                },
            ));
        }
    }

    /// Attach refinements to what they refine, then read the typed fields out.
    ///
    /// Deferred to the end because a refinement may precede or follow its
    /// target, so no single-pass reading of `<metadata>` can resolve one as it
    /// arrives.
    fn finish_metadata(&mut self) {
        for (target, refinement) in std::mem::take(&mut self.refinements) {
            if let Some(entry) = self
                .package
                .metadata
                .entries
                .iter_mut()
                .find(|entry| entry.id == target)
            {
                entry.refinements.push(refinement);
            }
        }
        derive(&mut self.package.metadata);
    }
}

/// The EPUB 2 refinement attributes, as the refinements they are equivalent to.
fn epub2_refinements(start: &BytesStart<'_>) -> Vec<Refinement> {
    [("role", "role"), ("file-as", "file-as")]
        .into_iter()
        .filter_map(|(attribute_name, property)| {
            let value = attribute(start, attribute_name);
            (!value.is_empty()).then(|| Refinement {
                property: property.to_owned(),
                value,
                scheme: String::new(),
            })
        })
        .collect()
}

/// Read the typed metadata fields out of the collected entries.
///
/// Repeatable elements keep every value; single-valued ones keep the first,
/// which is the reading every EPUB reader implements for a book that declares
/// two publishers.
fn derive(metadata: &mut Metadata) {
    /// The first entry with this element name, or `""`.
    fn first<'a>(entries: &'a [MetaEntry], element: &str) -> &'a str {
        entries
            .iter()
            .find(|entry| entry.element == element)
            .map_or("", |entry| entry.value.as_str())
    }
    /// Every value with this element name, in document order.
    fn every(entries: &[MetaEntry], element: &str) -> Vec<String> {
        entries
            .iter()
            .filter(|entry| entry.element == element)
            .map(|entry| entry.value.clone())
            .collect()
    }
    /// Every entry with this element name, read as a credited contributor.
    fn credited(entries: &[MetaEntry], element: &str) -> Vec<Contributor> {
        entries
            .iter()
            .filter(|entry| entry.element == element)
            .map(|entry| Contributor {
                name: entry.value.clone(),
                role: entry.refined("role").to_owned(),
                file_as: entry.refined("file-as").to_owned(),
            })
            .collect()
    }

    let entries = std::mem::take(&mut metadata.entries);

    metadata.titles = entries
        .iter()
        .filter(|entry| entry.element == "title")
        .map(|entry| Title {
            value: entry.value.clone(),
            title_type: entry.refined("title-type").to_owned(),
        })
        .collect();
    // A book that marks one of several titles `main` means it; a book that
    // marks none is answered by document order, which is what EPUB 2 had.
    metadata.title = metadata
        .titles
        .iter()
        .find(|title| title.title_type == "main")
        .or_else(|| metadata.titles.first())
        .map(|title| title.value.clone())
        .unwrap_or_default();

    metadata.creators = credited(&entries, "creator");
    metadata.contributors = credited(&entries, "contributor");
    metadata.languages = every(&entries, "language");
    metadata.language = metadata.languages.first().cloned().unwrap_or_default();
    metadata.subjects = every(&entries, "subject");
    metadata.identifiers = entries
        .iter()
        .filter(|entry| entry.element == "identifier")
        .map(|entry| Identifier {
            value: entry.value.clone(),
            id: entry.id.clone(),
            scheme: entry.scheme.clone(),
        })
        .collect();

    metadata.publisher = first(&entries, "publisher").to_owned();
    metadata.description = first(&entries, "description").to_owned();
    metadata.date = first(&entries, "date").to_owned();
    metadata.modified = first(&entries, "dcterms:modified").to_owned();
    metadata.rights = first(&entries, "rights").to_owned();
    metadata.source = first(&entries, "source").to_owned();
    metadata.resource_type = first(&entries, "type").to_owned();
    metadata.format = first(&entries, "format").to_owned();
    metadata.coverage = first(&entries, "coverage").to_owned();
    metadata.relation = first(&entries, "relation").to_owned();

    metadata.entries = entries;
}

/// Parse an OPF package document.
///
/// # Errors
///
/// [`XmlError::Malformed`] if the XML does not parse,
/// [`XmlError::NotAPackage`] if the root element is not `<package>`,
/// [`XmlError::EmptySpine`] if there are no spine items,
/// [`XmlError::EntityDeclaration`] if it declares entities.
pub fn parse_package(bytes: &[u8]) -> Result<Package, XmlError> {
    let mut reader = reader(bytes);
    let mut buf = Vec::new();
    let mut parser = PackageParser::default();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::DocType(dtd) => check_doctype(dtd.as_ref())?,
            Event::Start(start) => parser.open(&start, false)?,
            Event::Empty(start) => parser.open(&start, true)?,
            Event::End(end) => parser.close(end.local_name().as_ref()),
            Event::Text(text) if parser.collecting.is_some() => {
                parser.text.push_text(text.as_ref());
            }
            Event::CData(data) if parser.collecting.is_some() => {
                parser.text.push_text(data.as_ref());
            }
            Event::GeneralRef(reference) if parser.collecting.is_some() => {
                parser.text.push_ref(&reference);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if !parser.saw_package {
        return Err(XmlError::NotAPackage);
    }
    if parser.package.spine.is_empty() {
        return Err(XmlError::EmptySpine);
    }
    parser.finish_metadata();
    Ok(parser.package)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTAINER: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    const PACKAGE: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>A Tale of Two Chapters</dc:title>
    <dc:title>A subtitle that must not win</dc:title>
    <dc:creator>Ada Lovelace</dc:creator>
    <dc:creator>Charles Babbage</dc:creator>
    <dc:contributor>The Typesetter</dc:contributor>
    <dc:language>en-GB</dc:language>
    <dc:identifier id="bookid" opf:scheme="ISBN">urn:isbn:9780000000000</dc:identifier>
    <dc:publisher>Analytical Press</dc:publisher>
    <dc:description>Short &amp; sweet</dc:description>
    <dc:date>1843-10-01</dc:date>
    <dc:subject>Computing</dc:subject>
    <dc:subject>History</dc:subject>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest>
    <item id="ch1" href="text/chap1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="text/chap2.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover-img" href="images/cover.png" media-type="image/png" properties="cover-image"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
    <itemref idref="ch2" linear="no"/>
  </spine>
</package>"#;

    #[test]
    fn container_yields_the_opf_path() {
        assert_eq!(parse_container(CONTAINER).unwrap(), "OEBPS/content.opf");
    }

    #[test]
    fn container_without_a_rootfile_is_refused() {
        let empty = br#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <rootfiles/></container>"#;
        assert_eq!(parse_container(empty), Err(XmlError::NoRootfile));
    }

    #[test]
    fn package_metadata_manifest_and_spine_round_trip() {
        let package = parse_package(PACKAGE).unwrap();
        assert_eq!(package.version, "3.0");
        assert_eq!(package.unique_identifier_id, "bookid");

        let metadata = &package.metadata;
        assert_eq!(metadata.title, "A Tale of Two Chapters");
        let names: Vec<&str> = metadata
            .creators
            .iter()
            .map(|creator| creator.name.as_str())
            .collect();
        assert_eq!(names, ["Ada Lovelace", "Charles Babbage"]);
        assert_eq!(metadata.contributors[0].name, "The Typesetter");
        assert_eq!(metadata.language, "en-GB");
        assert_eq!(metadata.description, "Short & sweet");
        assert_eq!(metadata.date, "1843-10-01");
        assert_eq!(metadata.subjects, ["Computing", "History"]);
        assert_eq!(metadata.cover_id, "cover-img");
        assert_eq!(metadata.identifiers.len(), 1);
        assert_eq!(metadata.identifiers[0].value, "urn:isbn:9780000000000");
        assert_eq!(metadata.identifiers[0].id, "bookid");
        assert_eq!(metadata.identifiers[0].scheme, "ISBN");

        assert_eq!(package.manifest.len(), 3);
        assert_eq!(package.manifest[2].properties, ["cover-image"]);

        assert_eq!(package.spine.len(), 2);
        assert_eq!(package.spine[0].idref, "ch1");
        assert!(package.spine[0].linear, "absent linear means linear");
        assert!(!package.spine[1].linear, "linear=\"no\" is auxiliary");
        assert_eq!(package.toc_idref, "ncx", "the spine names the EPUB 2 NCX");
    }

    /// EPUB 3 metadata expression: refinements before and after their target,
    /// a main title that is not the first, and the six Dublin Core elements
    /// the old nine-element allow-list walked past.
    const EXPRESSIVE: &[u8] = br##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <meta refines="#t-main" property="title-type">main</meta>
    <dc:title id="t-sub">Being an Account of Two Chapters</dc:title>
    <meta refines="#t-sub" property="title-type">subtitle</meta>
    <dc:title id="t-main">A Tale of Two Chapters</dc:title>
    <dc:creator id="c1">Ada Lovelace</dc:creator>
    <meta refines="#c1" property="role" scheme="marc:relators">aut</meta>
    <meta refines="#c1" property="file-as">Lovelace, Ada</meta>
    <dc:creator id="c2">Charles Babbage</dc:creator>
    <meta refines="#c2" property="role">ill</meta>
    <dc:language>en-GB</dc:language>
    <dc:language>fr</dc:language>
    <dc:identifier id="bookid">urn:isbn:9780000000000</dc:identifier>
    <dc:rights>Public domain</dc:rights>
    <dc:source>urn:isbn:9781111111111</dc:source>
    <dc:type>monograph</dc:type>
    <dc:format>application/epub+zip</dc:format>
    <dc:coverage>England, 1843</dc:coverage>
    <dc:relation>urn:isbn:9782222222222</dc:relation>
    <meta property="dcterms:modified">2026-08-25T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml" media-overlay="ov1"/>
    <item id="ov1" href="c1.smil" media-type="application/smil+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"##;

    #[test]
    fn refinements_attach_whichever_side_of_their_target_they_sit_on() {
        let metadata = parse_package(EXPRESSIVE).unwrap().metadata;

        assert_eq!(
            metadata.title, "A Tale of Two Chapters",
            "the title marked `main` wins over the one that merely came first"
        );
        assert_eq!(metadata.titles.len(), 2);
        assert_eq!(metadata.titles[0].title_type, "subtitle");
        assert_eq!(metadata.titles[1].title_type, "main");

        // `#c1` is refined by elements that follow it, `#t-main` by one that
        // precedes it. Both must resolve.
        assert_eq!(metadata.creators[0].role, "aut");
        assert_eq!(metadata.creators[0].file_as, "Lovelace, Ada");
        assert_eq!(metadata.creators[1].role, "ill");
        assert_eq!(
            metadata.creators[1].file_as, "",
            "a refinement the book omitted stays empty rather than guessed"
        );
    }

    #[test]
    fn the_dublin_core_tail_survives_the_allow_list() {
        let metadata = parse_package(EXPRESSIVE).unwrap().metadata;
        assert_eq!(metadata.rights, "Public domain");
        assert_eq!(metadata.source, "urn:isbn:9781111111111");
        assert_eq!(metadata.resource_type, "monograph");
        assert_eq!(metadata.format, "application/epub+zip");
        assert_eq!(metadata.coverage, "England, 1843");
        assert_eq!(metadata.relation, "urn:isbn:9782222222222");
        assert_eq!(metadata.modified, "2026-08-25T00:00:00Z");
        assert_eq!(
            metadata.languages,
            ["en-GB", "fr"],
            "a bilingual edition declares both and the first alone misreports it"
        );
    }

    #[test]
    fn the_manifest_keeps_the_link_from_a_chapter_to_its_narration() {
        let package = parse_package(EXPRESSIVE).unwrap();
        assert_eq!(package.manifest[0].media_overlay, "ov1");
        assert_eq!(
            package.manifest[1].media_overlay, "",
            "the overlay itself has none"
        );
    }

    #[test]
    fn epub2_role_attributes_read_as_the_refinements_they_are() {
        let epub2 = br#"<package xmlns:opf="http://www.idpf.org/2007/opf" version="2.0">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
  <dc:creator opf:role="aut" opf:file-as="Lovelace, Ada">Ada Lovelace</dc:creator>
</metadata><spine><itemref idref="a"/></spine></package>"#;
        let metadata = parse_package(epub2).unwrap().metadata;
        assert_eq!(metadata.creators[0].role, "aut");
        assert_eq!(metadata.creators[0].file_as, "Lovelace, Ada");
    }

    #[test]
    fn a_refinement_naming_nothing_is_dropped_rather_than_guessed_at() {
        let dangling = br##"<package version="3.0"><metadata>
  <dc:title id="t">Only Title</dc:title>
  <meta refines="#nobody" property="title-type">main</meta>
</metadata><spine><itemref idref="a"/></spine></package>"##;
        let metadata = parse_package(dangling).unwrap().metadata;
        assert_eq!(metadata.title, "Only Title");
        assert_eq!(metadata.titles[0].title_type, "");
    }

    #[test]
    fn a_package_without_a_spine_is_refused() {
        let no_spine = br#"<package version="3.0"><manifest/></package>"#;
        assert_eq!(parse_package(no_spine), Err(XmlError::EmptySpine));
        let empty_spine = br#"<package version="3.0"><spine></spine></package>"#;
        assert_eq!(parse_package(empty_spine), Err(XmlError::EmptySpine));
    }

    #[test]
    fn a_root_that_is_not_package_is_refused() {
        let html = br"<html><body>not an OPF</body></html>";
        assert_eq!(parse_package(html), Err(XmlError::NotAPackage));
    }

    #[test]
    fn malformed_xml_is_refused() {
        let broken = br"<package><metadata></package>";
        assert!(matches!(parse_package(broken), Err(XmlError::Malformed(_))));
    }

    #[test]
    fn an_entity_declaration_is_refused_outright() {
        let hostile = br#"<?xml version="1.0"?>
<!DOCTYPE package [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]>
<package version="3.0"><metadata><dc:title>&xxe;</dc:title></metadata>
<spine><itemref idref="a"/></spine></package>"#;
        assert_eq!(parse_package(hostile), Err(XmlError::EntityDeclaration));
        assert_eq!(parse_container(hostile), Err(XmlError::EntityDeclaration));
    }

    #[test]
    fn an_undeclared_entity_is_copied_through_never_resolved() {
        // No DOCTYPE, so nothing above rejects this. quick-xml has no DTD
        // processor; the reference must survive as four literal characters.
        let sneaky = br#"<package version="3.0">
<metadata><dc:title>&xxe;</dc:title></metadata>
<spine><itemref idref="a"/></spine></package>"#;
        let package = parse_package(sneaky).unwrap();
        assert_eq!(package.metadata.title, "&xxe;");
    }

    #[test]
    fn character_references_and_builtins_still_resolve() {
        let refs = br#"<package version="3.0">
<metadata><dc:title>Caf&#233; &amp; Bar &#x2014; open</dc:title></metadata>
<spine><itemref idref="a"/></spine></package>"#;
        let package = parse_package(refs).unwrap();
        assert_eq!(package.metadata.title, "Café & Bar — open");
    }
}
