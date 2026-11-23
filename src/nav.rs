// SPDX-License-Identifier: Apache-2.0

//! The book's own table of contents: the EPUB 3 navigation document and the
//! EPUB 2 NCX.
//!
//! # Why this is not a breach of the thin-packager rule
//!
//! This service does not parse XHTML, on the grounds that HTML semantics
//! belong to the HTML collector. A navigation document is XHTML, so reading
//! one looks like the exception that swallows the rule. It is not, for a
//! reason worth writing down: a nav document is *navigation metadata*, a
//! `<nav>` holding nested `<ol>`/`<li>`/`<a>` and nothing else the format
//! permits to matter. Its labels and targets are statements the book makes
//! about its own structure, in the same category as the spine.
//!
//! The alternative is worse in a specific way. The bytes are already inflated
//! here in order to be emitted, so leaving them unparsed does not save the
//! work; it moves it downstream and duplicates it, and until someone does it a
//! forty-chapter book projects as forty identically shaped groups whose only
//! human-readable identity is a file path.
//!
//! # What is deliberately not read
//!
//! Only the `toc` nav is read. EPUB 3 also defines `page-list` (printed page
//! numbers) and `landmarks` (where the body matter starts); both are useful,
//! both are the same shape, and neither has a Document slot wired up yet, so
//! reading them now would produce facts with nowhere to go.
//!
//! # Failure policy
//!
//! Nothing here fails a call. A nav document that does not parse, names no
//! entries, or points at files the archive does not contain is a defect in the
//! book, and a book with a broken table of contents is still a book. Every
//! failure path returns an empty list and the caller warns.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::href::{self, Target};
use crate::opf::Text;

/// Largest number of entries read from one navigation document.
///
/// A table of contents is a human artefact: a thousand entries is a reference
/// work with a line per section, and ten thousand is not a book. The cap
/// bounds what a crafted nav document can make the server allocate, since the
/// entries end up in one response message.
const MAX_ENTRIES: usize = 5_000;

/// Deepest nesting followed.
///
/// Past this, a list item opens no new level and its entry joins the deepest
/// one that is open. The tree is walked recursively when it is renumbered and
/// again when it is dropped, so an unbounded depth is a stack-overflow the
/// book gets to choose; a table of contents nested thirty-two deep is not one
/// anybody wrote for a reader.
const MAX_DEPTH: usize = 32;

/// One entry of a book's navigation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavPoint {
    /// The link text, with markup stripped and whitespace collapsed.
    pub label: String,
    /// Archive path with the fragment preserved, or empty when the entry is a
    /// heading that links nowhere.
    pub href: String,
    /// Zero-based nesting depth.
    pub depth: usize,
    /// Entries nested under this one, in document order.
    pub children: Vec<NavPoint>,
}

/// A parsed table of contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Navigation {
    /// Archive path of the document this was read from.
    pub source_href: String,
    /// Top-level entries, in document order.
    pub toc: Vec<NavPoint>,
    /// Whether this came from an EPUB 2 NCX rather than a nav document.
    pub from_ncx: bool,
    /// The XML error the read stopped on, when it stopped before the end of
    /// the document. The entries read before it are kept in `toc`, and the
    /// ones after it are missing.
    pub broken_at: Option<String>,
}

impl Navigation {
    /// Whether the parse found nothing worth emitting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.toc.is_empty()
    }

    /// Every entry, depth-first in document order, flattened.
    ///
    /// The nesting is what a reader draws; a flat walk in reading order is
    /// what a projection into a linear outline needs, and deriving one from
    /// the other in two places would be two chances to differ.
    #[must_use]
    pub fn flatten(&self) -> Vec<&NavPoint> {
        /// Push `point` and everything under it.
        fn walk<'a>(point: &'a NavPoint, out: &mut Vec<&'a NavPoint>) {
            out.push(point);
            for child in &point.children {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        for point in &self.toc {
            walk(point, &mut out);
        }
        out
    }
}

/// Resolve a navigation href against the document that declared it.
///
/// The fragment is what distinguishes two entries pointing into the same
/// chapter, so unlike [`href::resolve`] it is kept: the path is resolved
/// without it and it is appended back afterwards. A remote or unusable target
/// yields `None` and the entry keeps its label with no href.
fn resolve_with_fragment(base_dir: &str, raw: &str) -> Option<String> {
    let (path, fragment) = match raw.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (raw, None),
    };
    // A bare `#id` points inside the navigation document itself, which is not
    // a chapter and is not somewhere an outline entry can usefully point.
    if path.is_empty() {
        return None;
    }
    let Ok(Target::Entry(resolved)) = href::resolve(base_dir, path) else {
        return None;
    };
    Some(match fragment {
        Some(fragment) if !fragment.is_empty() => format!("{resolved}#{fragment}"),
        _ => resolved,
    })
}

/// Read one attribute by local name, resolving the predefined entities.
///
/// The same policy as [`crate::opf`]: an attribute cannot be a way in for an
/// entity that text is protected from.
fn attribute(start: &BytesStart<'_>, name: &str) -> String {
    for attr in start.attributes().with_checks(false).flatten() {
        if attr.key.as_ref().starts_with("xmlns") {
            continue;
        }
        if attr.key.local_name().as_ref() == name {
            return match attr.normalized_value(XmlVersion::Implicit1_0) {
                Ok(value) => value.into_owned(),
                Err(_) => attr.value.into_owned(),
            };
        }
    }
    String::new()
}

/// Collapse runs of whitespace and trim, which is what a rendered label is.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A reader with the same settings the OPF is parsed under, except that
/// unmatched end tags are tolerated.
///
/// Nav documents are XHTML written by ebook tooling and a stray `</br>` is
/// common enough that refusing the whole table of contents over one would be
/// choosing purity over the book. The OPF is held to the stricter standard
/// because a package document whose tags do not nest is a package document
/// whose spine order cannot be trusted.
fn reader(bytes: &[u8]) -> Reader<&[u8]> {
    let mut reader = Reader::from_reader(bytes);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.expand_empty_elements = false;
    reader
}

/// A partly-built entry, waiting for the text and children that follow it.
struct Open {
    /// The entry so far.
    point: NavPoint,
    /// Nesting depth of the element that opened it, so the matching close can
    /// be recognized without trusting tag names to nest.
    depth: usize,
}

/// Attach a finished entry to its parent, or to the top level.
fn attach(stack: &mut [Open], roots: &mut Vec<NavPoint>, finished: NavPoint) {
    match stack.last_mut() {
        Some(parent) => parent.point.children.push(finished),
        None => roots.push(finished),
    }
}

/// Parse an EPUB 3 navigation document.
///
/// `source_href` is the document's own archive path, used both to resolve its
/// relative links and to report where the entries came from. Only the `toc`
/// nav is read; see the module documentation.
#[must_use]
pub fn parse_nav(bytes: &[u8], source_href: &str) -> Navigation {
    let base_dir = href::parent_dir(source_href).to_owned();
    let mut reader = reader(bytes);
    let mut buf = Vec::new();

    let mut navigation = Navigation {
        source_href: source_href.to_owned(),
        toc: Vec::new(),
        from_ncx: false,
        broken_at: None,
    };
    // Which `<nav>` the parser is inside, if any. A nav document holds several
    // and only the `toc` one is wanted; `epub:type` is the discriminator, and
    // a `<nav>` with none is treated as the toc when no explicit one has been
    // seen, because that is what a single-nav document means.
    let mut in_toc_nav = false;
    let mut nav_depth = 0usize;
    let mut depth = 0usize;
    let mut stack: Vec<Open> = Vec::new();
    let mut roots: Vec<NavPoint> = Vec::new();
    // Set while inside the `<a>` or `<span>` that titles the innermost open
    // list item. Nesting comes from `<ol>`/`<li>`, never from these: an
    // anchor closes before the sublist that follows it opens.
    let mut in_label = false;
    let mut label = Text::default();
    let mut entries = 0usize;
    // List items nested past MAX_DEPTH, which are not read.
    let mut too_deep = 0usize;

    // A malformed nav document costs the book the rest of its outline and
    // nothing else. Whatever was read before the error is kept, because a
    // truncated table of contents is more useful than none, and the error is
    // kept with it so the caller is told the outline is not whole.
    loop {
        let event = match reader.read_event_into(&mut buf) {
            Ok(event) => event,
            Err(err) => {
                navigation.broken_at = Some(read_error(&reader, &err));
                break;
            }
        };
        match event {
            Event::Start(start) => {
                depth += 1;
                let local = start.local_name();
                match local.as_ref() {
                    "nav" if !in_toc_nav => {
                        // `epub:type` picks the toc out of the several navs a
                        // document may hold. A `<nav>` with no type is taken
                        // as the toc, because that is what a single-nav
                        // document means.
                        let kind = attribute(&start, "type");
                        if kind.is_empty() || kind.split_whitespace().any(|token| token == "toc") {
                            in_toc_nav = true;
                            nav_depth = depth;
                        }
                    }
                    "li" if in_toc_nav => {
                        if stack.len() < MAX_DEPTH {
                            stack.push(Open {
                                point: NavPoint::default(),
                                depth,
                            });
                        } else {
                            too_deep += 1;
                        }
                    }
                    // The first titling element wins: a list item may hold an
                    // anchor and then a whole sublist of them.
                    "a" | "span" if in_toc_nav => {
                        if let Some(open) = stack.last_mut()
                            && open.point.label.is_empty()
                        {
                            let raw = attribute(&start, "href");
                            open.point.href =
                                resolve_with_fragment(&base_dir, &raw).unwrap_or_default();
                            in_label = true;
                            label.clear();
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(text) if in_label => label.push_text(text.as_ref()),
            Event::CData(data) if in_label => label.push_text(data.as_ref()),
            Event::GeneralRef(reference) if in_label => label.push_ref(&reference),
            Event::End(end) => {
                if in_label && matches!(end.local_name().as_ref(), "a" | "span") {
                    in_label = false;
                    let text = label.take();
                    if let Some(open) = stack.last_mut() {
                        open.point.label = collapse(&text);
                    }
                }
                if let Some(open) = stack.pop_if(|open| open.depth == depth)
                    && !open.point.label.is_empty()
                {
                    if entries >= MAX_ENTRIES {
                        navigation.broken_at = Some(entry_cap(&reader));
                        break;
                    }
                    entries += 1;
                    attach(&mut stack, &mut roots, open.point);
                }
                if in_toc_nav && depth == nav_depth {
                    in_toc_nav = false;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    // Anything still open was truncated; keep what it had rather than lose the
    // whole branch to one unclosed tag.
    while let Some(open) = stack.pop() {
        if !open.point.label.is_empty() {
            attach(&mut stack, &mut roots, open.point);
        }
    }

    if navigation.broken_at.is_none() {
        navigation.broken_at = left_out_deep(too_deep);
    }
    navigation.toc = nest(roots);
    navigation
}

/// Parse an EPUB 2 NCX document.
///
/// `<navMap>` holds `<navPoint>` elements that nest directly, each with a
/// `<navLabel><text>` and a `<content src="…">`, so the shape is the same as
/// the nav document's and only the tag names differ.
#[must_use]
pub fn parse_ncx(bytes: &[u8], source_href: &str) -> Navigation {
    let base_dir = href::parent_dir(source_href).to_owned();
    let mut reader = reader(bytes);
    let mut buf = Vec::new();

    let mut depth = 0usize;
    let mut stack: Vec<Open> = Vec::new();
    let mut roots: Vec<NavPoint> = Vec::new();
    let mut in_map = false;
    let mut map_depth = 0usize;
    let mut in_text = false;
    let mut label = Text::default();
    let mut entries = 0usize;
    // Nav points nested past MAX_DEPTH, which are not read.
    let mut too_deep = 0usize;

    // Same policy as the nav document: read as far as the bytes allow, and
    // say where they stopped allowing it.
    let mut broken_at = None;
    loop {
        let event = match reader.read_event_into(&mut buf) {
            Ok(event) => event,
            Err(err) => {
                broken_at = Some(read_error(&reader, &err));
                break;
            }
        };
        match event {
            // `<content src="…"/>` is an empty element and `<navPoint>` is
            // not, so the two arms differ only in whether they open a level.
            Event::Empty(start) => {
                if in_map
                    && start.local_name().as_ref() == "content"
                    && let Some(open) = stack.last_mut()
                {
                    let raw = attribute(&start, "src");
                    open.point.href = resolve_with_fragment(&base_dir, &raw).unwrap_or_default();
                }
            }
            Event::Start(start) => {
                depth += 1;
                match start.local_name().as_ref() {
                    "navMap" => {
                        in_map = true;
                        map_depth = depth;
                    }
                    "navPoint" if in_map => {
                        if stack.len() < MAX_DEPTH {
                            stack.push(Open {
                                point: NavPoint::default(),
                                depth,
                            });
                        } else {
                            too_deep += 1;
                        }
                    }
                    // Only a `<text>` inside a nav point is a label. The NCX
                    // header carries `<docTitle><text>` too, and reading that
                    // as an entry would put the book's title in its own table
                    // of contents.
                    "text" if in_map && !stack.is_empty() => {
                        in_text = true;
                        label.clear();
                    }
                    // A producer that wrote `<content>…</content>` rather than
                    // the empty form still means the same thing.
                    "content" if in_map => {
                        if let Some(open) = stack.last_mut() {
                            let raw = attribute(&start, "src");
                            open.point.href =
                                resolve_with_fragment(&base_dir, &raw).unwrap_or_default();
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(text) if in_text => label.push_text(text.as_ref()),
            Event::CData(data) if in_text => label.push_text(data.as_ref()),
            Event::GeneralRef(reference) if in_text => label.push_ref(&reference),
            Event::End(end) => {
                if end.local_name().as_ref() == "text" && in_text {
                    in_text = false;
                    let text = label.take();
                    if let Some(open) = stack.last_mut()
                        && open.point.label.is_empty()
                    {
                        open.point.label = collapse(&text);
                    }
                }
                if let Some(open) = stack.pop_if(|open| open.depth == depth)
                    && !open.point.label.is_empty()
                {
                    if entries >= MAX_ENTRIES {
                        broken_at = Some(entry_cap(&reader));
                        break;
                    }
                    entries += 1;
                    attach(&mut stack, &mut roots, open.point);
                }
                if in_map && depth == map_depth {
                    in_map = false;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Navigation {
        source_href: source_href.to_owned(),
        toc: nest(roots),
        from_ncx: true,
        broken_at: broken_at.or_else(|| left_out_deep(too_deep)),
    }
}

/// Why a table of contents stopped at [`MAX_ENTRIES`], and where.
fn entry_cap(reader: &Reader<&[u8]>) -> String {
    format!(
        "stopped at the cap of {MAX_ENTRIES} entries (at byte {})",
        reader.buffer_position()
    )
}

/// The entries left out for nesting past [`MAX_DEPTH`], if any were.
fn left_out_deep(too_deep: usize) -> Option<String> {
    (too_deep > 0)
        .then(|| format!("{too_deep} entries nested deeper than {MAX_DEPTH} levels were left out"))
}

/// Describe a read error with the byte offset it was found at.
pub(crate) fn read_error(reader: &Reader<&[u8]>, err: &quick_xml::Error) -> String {
    format!("{err} (at byte {})", reader.error_position())
}

/// Renumber depths after the tree is built.
///
/// Entries record their depth as they are opened, which is right for a nav
/// document and wrong for anything that skipped a level; recomputing from the
/// finished tree makes the field mean exactly "how far down this sits".
fn nest(mut roots: Vec<NavPoint>) -> Vec<NavPoint> {
    /// Set `point.depth` and recurse.
    fn renumber(point: &mut NavPoint, depth: usize) {
        point.depth = depth;
        for child in &mut point.children {
            renumber(child, depth + 1);
        }
    }
    for point in &mut roots {
        renumber(point, 0);
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nav document with an XML error after its second entry. Before the
    /// fix the outline came back with two entries and nothing to say there
    /// were more.
    #[test]
    fn a_nav_document_that_breaks_part_way_says_where() {
        let nav = br#"<html><body><nav epub:type="toc"><ol>
            <li><a href="c1.xhtml">One</a></li>
            <li><a href="c2.xhtml">Two</a></li>
            <!x>
            <li><a href="c3.xhtml">Three</a></li>
        </ol></nav></body></html>"#;
        let navigation = parse_nav(nav, "OEBPS/nav.xhtml");
        assert_eq!(
            navigation.toc.len(),
            2,
            "what came before the error is kept"
        );
        let broken = navigation.broken_at.expect("the break is reported");
        assert!(broken.contains("at byte"), "{broken}");

        let whole = parse_nav(NAV, "OEBPS/nav.xhtml");
        assert_eq!(whole.broken_at, None, "a sound document reports no break");
    }

    #[test]
    fn an_ncx_that_breaks_part_way_says_where() {
        let ncx = br#"<ncx><navMap>
            <navPoint id="a"><navLabel><text>One</text></navLabel><content src="c1.xhtml"/></navPoint>
            <!x>
            <navPoint id="b"><navLabel><text>Two</text></navLabel><content src="c2.xhtml"/></navPoint>
        </navMap></ncx>"#;
        let navigation = parse_ncx(ncx, "OEBPS/toc.ncx");
        assert_eq!(navigation.toc.len(), 1);
        assert!(navigation.broken_at.is_some());
    }

    /// A table of contents past the entry cap. It used to come back cut
    /// at 5,000 entries with nothing saying more existed.
    #[test]
    fn a_table_of_contents_past_its_cap_says_so() {
        use std::fmt::Write as _;
        let mut items = String::new();
        for n in 0..MAX_ENTRIES + 10 {
            write!(items, r#"<li><a href="c{n}.xhtml">Entry {n}</a></li>"#)
                .expect("a String takes any write");
        }
        let nav =
            format!(r#"<html><body><nav epub:type="toc"><ol>{items}</ol></nav></body></html>"#);
        let navigation = parse_nav(nav.as_bytes(), "OEBPS/nav.xhtml");
        assert_eq!(navigation.toc.len(), MAX_ENTRIES);
        let cut = navigation.broken_at.expect("the cap is reported");
        assert!(cut.contains("5000"), "{cut}");

        let mut points = String::new();
        for n in 0..MAX_ENTRIES + 10 {
            write!(
                points,
                r#"<navPoint id="p{n}"><navLabel><text>E{n}</text></navLabel><content src="c{n}.xhtml"/></navPoint>"#
            )
            .expect("a String takes any write");
        }
        let ncx = format!("<ncx><navMap>{points}</navMap></ncx>");
        let navigation = parse_ncx(ncx.as_bytes(), "OEBPS/toc.ncx");
        assert_eq!(navigation.toc.len(), MAX_ENTRIES);
        assert!(navigation.broken_at.is_some_and(|cut| cut.contains("5000")));
    }

    /// Entries nested past the depth cap. They were left out silently.
    #[test]
    fn entries_nested_past_the_depth_cap_are_reported() {
        let open: String = (0..MAX_DEPTH + 3)
            .map(|n| format!(r#"<li><a href="c{n}.xhtml">Level {n}</a><ol>"#))
            .collect();
        let close = "</ol></li>".repeat(MAX_DEPTH + 3);
        let nav = format!(
            r#"<html><body><nav epub:type="toc"><ol>{open}{close}</ol></nav></body></html>"#
        );
        let navigation = parse_nav(nav.as_bytes(), "OEBPS/nav.xhtml");
        let left_out = navigation.broken_at.expect("the deep entries are reported");
        assert!(left_out.contains("3 entries"), "{left_out}");
        assert!(left_out.contains("32"), "{left_out}");
    }

    const NAV: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body>
  <nav epub:type="toc">
    <h1>Contents</h1>
    <ol>
      <li><a href="text/chap1.xhtml">Chapter One</a>
        <ol>
          <li><a href="text/chap1.xhtml#part2">  The   Second Part </a></li>
        </ol>
      </li>
      <li><a href="text/chap2.xhtml">Chapter Two</a></li>
    </ol>
  </nav>
  <nav epub:type="landmarks">
    <ol><li><a href="text/chap1.xhtml">Start of content</a></li></ol>
  </nav>
</body></html>"#;

    const NCX: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <docTitle><text>A Tale of Two Chapters</text></docTitle>
  <navMap>
    <navPoint id="np1" playOrder="1">
      <navLabel><text>Chapter One</text></navLabel>
      <content src="text/chap1.xhtml"/>
      <navPoint id="np1a" playOrder="2">
        <navLabel><text>The Second Part</text></navLabel>
        <content src="text/chap1.xhtml#part2"/>
      </navPoint>
    </navPoint>
    <navPoint id="np2" playOrder="3">
      <navLabel><text>Chapter Two</text></navLabel>
      <content src="text/chap2.xhtml"/>
    </navPoint>
  </navMap>
</ncx>"#;

    #[test]
    fn the_nav_document_yields_a_nested_table_of_contents() {
        let navigation = parse_nav(NAV, "OEBPS/nav.xhtml");
        assert!(!navigation.from_ncx);
        assert_eq!(navigation.source_href, "OEBPS/nav.xhtml");
        assert_eq!(navigation.toc.len(), 2, "two top-level chapters");

        assert_eq!(navigation.toc[0].label, "Chapter One");
        assert_eq!(navigation.toc[0].href, "OEBPS/text/chap1.xhtml");
        assert_eq!(navigation.toc[0].depth, 0);

        let nested = &navigation.toc[0].children[0];
        assert_eq!(
            nested.label, "The Second Part",
            "whitespace in the link text is collapsed the way a renderer would"
        );
        assert_eq!(
            nested.href, "OEBPS/text/chap1.xhtml#part2",
            "the fragment is what tells two entries in one chapter apart"
        );
        assert_eq!(nested.depth, 1);

        assert_eq!(navigation.toc[1].label, "Chapter Two");
    }

    #[test]
    fn only_the_toc_nav_is_read() {
        let navigation = parse_nav(NAV, "OEBPS/nav.xhtml");
        let labels: Vec<&str> = navigation
            .flatten()
            .iter()
            .map(|point| point.label.as_str())
            .collect();
        assert!(
            !labels.contains(&"Start of content"),
            "landmarks are a different nav and have no slot yet: {labels:?}"
        );
    }

    #[test]
    fn a_nav_document_with_no_epub_type_is_taken_as_the_toc() {
        let bare = br#"<html><body><nav><ol>
            <li><a href="c1.xhtml">One</a></li></ol></nav></body></html>"#;
        let navigation = parse_nav(bare, "OEBPS/nav.xhtml");
        assert_eq!(navigation.toc.len(), 1);
        assert_eq!(navigation.toc[0].href, "OEBPS/c1.xhtml");
    }

    #[test]
    fn the_ncx_yields_the_same_shape_as_the_nav_document() {
        let navigation = parse_ncx(NCX, "OEBPS/toc.ncx");
        assert!(navigation.from_ncx);
        assert_eq!(navigation.toc.len(), 2);
        assert_eq!(navigation.toc[0].label, "Chapter One");
        assert_eq!(navigation.toc[0].href, "OEBPS/text/chap1.xhtml");
        assert_eq!(navigation.toc[0].children.len(), 1);
        assert_eq!(
            navigation.toc[0].children[0].href,
            "OEBPS/text/chap1.xhtml#part2"
        );
        assert_eq!(navigation.toc[0].children[0].depth, 1);
        assert_eq!(navigation.toc[1].label, "Chapter Two");

        // `docTitle` also holds a `<text>`, and it is not a nav point.
        let labels: Vec<&str> = navigation
            .flatten()
            .iter()
            .map(|point| point.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["Chapter One", "The Second Part", "Chapter Two"],
            "the document title is not an entry"
        );
    }

    #[test]
    fn flatten_is_depth_first_in_reading_order() {
        let navigation = parse_nav(NAV, "OEBPS/nav.xhtml");
        let labels: Vec<&str> = navigation
            .flatten()
            .iter()
            .map(|point| point.label.as_str())
            .collect();
        assert_eq!(labels, ["Chapter One", "The Second Part", "Chapter Two"]);
    }

    #[test]
    fn a_broken_nav_document_costs_the_outline_and_nothing_else() {
        // Truncated mid-element: whatever was read stays, and no panic.
        let truncated = br#"<html><body><nav epub:type="toc"><ol>
            <li><a href="c1.xhtml">One</a></li>
            <li><a href="c2.xhtml">Tw"#;
        let navigation = parse_nav(truncated, "OEBPS/nav.xhtml");
        assert_eq!(navigation.toc.len(), 1);
        assert_eq!(navigation.toc[0].label, "One");

        // Not markup at all.
        assert!(parse_nav(b"not xml at all", "OEBPS/nav.xhtml").is_empty());
        assert!(parse_ncx(b"", "OEBPS/toc.ncx").is_empty());
    }

    #[test]
    fn entries_that_escape_the_archive_keep_their_label_and_lose_their_target() {
        let hostile = br##"<html><body><nav epub:type="toc"><ol>
            <li><a href="../../etc/passwd">Escape</a></li>
            <li><a href="https://example.com/x">Remote</a></li>
            <li><a href="#local">Self</a></li>
        </ol></nav></body></html>"##;
        let navigation = parse_nav(hostile, "OEBPS/nav.xhtml");
        assert_eq!(navigation.toc.len(), 3);
        for point in &navigation.toc {
            assert_eq!(
                point.href, "",
                "{:?} must not carry an unusable target",
                point.label
            );
        }
    }

    #[test]
    fn nav_labels_keep_references_and_cdata() {
        // A reference arrives as its own event, not inside the text around
        // it: a label reader that takes text alone loses every one of them.
        let nav = br#"<html><body><nav epub:type="toc"><ol>
            <li><a href="c1.xhtml">Crime &amp; Punishment</a></li>
            <li><a href="c2.xhtml">Caf&#233; &#x2014; menu</a></li>
            <li><a href="c3.xhtml"><![CDATA[Q & A]]></a></li>
            <li><a href="c4.xhtml">Tom&nbsp;Jones</a></li>
        </ol></nav></body></html>"#;
        let navigation = parse_nav(nav, "OEBPS/nav.xhtml");
        let labels: Vec<&str> = navigation
            .toc
            .iter()
            .map(|point| point.label.as_str())
            .collect();
        // The last is an entity only a DTD could resolve, and no DTD is ever
        // consulted: it stays as written, as it does in the OPF.
        assert_eq!(
            labels,
            [
                "Crime & Punishment",
                "Caf\u{e9} \u{2014} menu",
                "Q & A",
                "Tom&nbsp;Jones"
            ]
        );
    }

    #[test]
    fn ncx_labels_keep_references_and_cdata() {
        let ncx = br#"<ncx><navMap>
            <navPoint id="a" playOrder="1">
                <navLabel><text>Crime &amp; Punishment</text></navLabel>
                <content src="c1.xhtml"/>
            </navPoint>
            <navPoint id="b" playOrder="2">
                <navLabel><text>Caf&#233; &#x2014; menu</text></navLabel>
                <content src="c2.xhtml"/>
            </navPoint>
            <navPoint id="c" playOrder="3">
                <navLabel><text><![CDATA[Q & A]]></text></navLabel>
                <content src="c3.xhtml"/>
            </navPoint>
        </navMap></ncx>"#;
        let navigation = parse_ncx(ncx, "OEBPS/toc.ncx");
        let labels: Vec<&str> = navigation
            .toc
            .iter()
            .map(|point| point.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["Crime & Punishment", "Caf\u{e9} \u{2014} menu", "Q & A"]
        );
    }

    #[test]
    fn an_entry_with_no_label_is_not_an_entry() {
        let empty_label = br#"<html><body><nav epub:type="toc"><ol>
            <li><a href="c1.xhtml"></a></li>
            <li><a href="c2.xhtml">Two</a></li>
        </ol></nav></body></html>"#;
        let navigation = parse_nav(empty_label, "OEBPS/nav.xhtml");
        assert_eq!(navigation.toc.len(), 1);
        assert_eq!(navigation.toc[0].label, "Two");
    }
}
