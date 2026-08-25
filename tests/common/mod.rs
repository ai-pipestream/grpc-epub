// SPDX-License-Identifier: Apache-2.0

//! In-memory EPUB fixtures, authored by the tests.
//!
//! Nothing binary is committed. Every archive in this suite is built here with
//! the same `zip` crate the server reads with, which means a fixture is a
//! *program* rather than a blob: "the same book with the image stored first"
//! is one call, and so is "the same book with a decompression bomb where
//! chapter two should be". A committed `.epub` cannot express either without a
//! second committed `.epub`.
//!
//! Structure of the default book, in archive order, which matters because
//! resources are emitted when their entry is reached:
//!
//! ```text
//! mimetype                    stored, first, as the specification requires
//! META-INF/container.xml
//! OEBPS/content.opf
//! OEBPS/text/chap1.xhtml
//! OEBPS/text/chap2.xhtml
//! OEBPS/images/cover.png
//! ```

#![allow(dead_code)] // Each test binary uses a different part of this module.

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Archive path of the OPF in every fixture built here.
pub const OPF_PATH: &str = "OEBPS/content.opf";

/// Archive path of the first chapter in the default book.
pub const CHAP1: &str = "OEBPS/text/chap1.xhtml";

/// Archive path of the second chapter in the default book.
pub const CHAP2: &str = "OEBPS/text/chap2.xhtml";

/// Archive path of the cover image in the default book.
pub const COVER: &str = "OEBPS/images/cover.png";

/// Archive path of the stylesheet in the default book.
pub const STYLESHEET: &str = "OEBPS/style/main.css";

/// Stand-in image bytes.
///
/// A real PNG signature followed by filler. Nothing in this service decodes an
/// image — the manifest's declared media type is what classifies a resource —
/// so a decodable PNG would test nothing extra, and inventing valid CRCs by
/// hand would only add a way for the fixture itself to be wrong. What the
/// tests assert is that these exact bytes come back.
pub const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\ncover-image-bytes-for-the-round-trip-assertion";

/// One entry to write into a fixture archive.
pub struct Entry {
    /// Archive path.
    pub name: String,
    /// Contents.
    pub data: Vec<u8>,
    /// How to store it. `mimetype` must be stored; everything else is
    /// deflated, which is also what the zip-bomb fixtures need.
    pub method: CompressionMethod,
}

/// An archive under construction. Entries are written in the order added,
/// which is the order the server will reach them in.
#[derive(Default)]
pub struct Builder {
    /// Entries in archive order.
    entries: Vec<Entry>,
}

impl Builder {
    /// An empty archive.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a deflated entry.
    #[must_use]
    pub fn add(mut self, name: &str, data: impl Into<Vec<u8>>) -> Self {
        self.entries.push(Entry {
            name: name.to_owned(),
            data: data.into(),
            method: CompressionMethod::Deflated,
        });
        self
    }

    /// Append a stored (uncompressed) entry.
    #[must_use]
    pub fn add_stored(mut self, name: &str, data: impl Into<Vec<u8>>) -> Self {
        self.entries.push(Entry {
            name: name.to_owned(),
            data: data.into(),
            method: CompressionMethod::Stored,
        });
        self
    }

    /// Serialize the archive.
    #[must_use]
    pub fn build(self) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for entry in self.entries {
            let options = SimpleFileOptions::default().compression_method(entry.method);
            writer
                .start_file(entry.name.as_str(), options)
                .expect("start entry");
            writer.write_all(&entry.data).expect("write entry");
        }
        writer.finish().expect("finish archive").into_inner()
    }
}

/// A builder already carrying `mimetype` and `META-INF/container.xml`.
///
/// `mimetype` is stored and written first, as the EPUB specification requires,
/// so the fixtures are conforming books rather than merely acceptable ones.
#[must_use]
pub fn shell() -> Builder {
    Builder::new()
        .add_stored("mimetype", "application/epub+zip")
        .add("META-INF/container.xml", container_xml(OPF_PATH))
}

/// A `META-INF/container.xml` naming `opf_path` as the package document.
#[must_use]
pub fn container_xml(opf_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="{opf_path}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
    )
}

/// Build an OPF package document.
///
/// `spine` is `(id, href)` in reading order; `resources` is
/// `(id, href, media-type[, properties])` for everything the manifest names
/// that is not a chapter. Hrefs are relative to the OPF's own directory, as
/// the format requires.
#[must_use]
pub fn opf_xml(spine: &[(&str, &str)], resources: &[(&str, &str, &str, &str)]) -> String {
    let mut manifest = String::new();
    for (id, href) in spine {
        manifest.push_str(&format!(
            "    <item id=\"{id}\" href=\"{href}\" media-type=\"application/xhtml+xml\"/>\n"
        ));
    }
    for (id, href, media_type, properties) in resources {
        let properties = if properties.is_empty() {
            String::new()
        } else {
            format!(" properties=\"{properties}\"")
        };
        manifest.push_str(&format!(
            "    <item id=\"{id}\" href=\"{href}\" media-type=\"{media_type}\"{properties}/>\n"
        ));
    }

    let mut itemrefs = String::new();
    for (id, _) in spine {
        itemrefs.push_str(&format!("    <itemref idref=\"{id}\"/>\n"));
    }

    package(&manifest, &itemrefs)
}

/// Wrap a manifest and a spine in the metadata every fixture shares.
#[must_use]
pub fn package(manifest: &str, itemrefs: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>A Tale of Two Chapters</dc:title>
    <dc:creator>Ada Lovelace</dc:creator>
    <dc:creator>Charles Babbage</dc:creator>
    <dc:language>en-GB</dc:language>
    <dc:identifier id="bookid" opf:scheme="ISBN">urn:isbn:9780000000000</dc:identifier>
    <dc:publisher>Analytical Press</dc:publisher>
    <dc:date>1843-10-01</dc:date>
    <dc:subject>Computing</dc:subject>
  </metadata>
  <manifest>
{manifest}  </manifest>
  <spine>
{itemrefs}  </spine>
</package>"#
    )
}

/// An XHTML chapter with the given heading and body text.
#[must_use]
pub fn chapter_xhtml(heading: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>{heading}</title></head>
<body><h1>{heading}</h1><p>{body}</p><img src="../images/cover.png" alt="cover"/></body></html>"#
    )
}

/// The default two-chapter book: cover image stored **after** the chapters.
#[must_use]
pub fn minimal() -> Vec<u8> {
    shell()
        .add(
            OPF_PATH,
            opf_xml(
                &[("ch1", "text/chap1.xhtml"), ("ch2", "text/chap2.xhtml")],
                &[("cover-img", "images/cover.png", "image/png", "cover-image")],
            ),
        )
        .add(CHAP1, chapter_xhtml("Chapter One", "The first chapter."))
        .add(CHAP2, chapter_xhtml("Chapter Two", "The second chapter."))
        .add(COVER, IMAGE)
        .build()
}

/// The same book with the cover image stored **before** the chapters.
///
/// The pair exists to pin emission order: a resource is emitted when its
/// archive entry is reached, so this book must produce the `resource` event
/// first and [`minimal`] must produce it last.
#[must_use]
pub fn image_first() -> Vec<u8> {
    shell()
        .add(
            OPF_PATH,
            opf_xml(
                &[("ch1", "text/chap1.xhtml"), ("ch2", "text/chap2.xhtml")],
                &[("cover-img", "images/cover.png", "image/png", "cover-image")],
            ),
        )
        .add(COVER, IMAGE)
        .add(CHAP1, chapter_xhtml("Chapter One", "The first chapter."))
        .add(CHAP2, chapter_xhtml("Chapter Two", "The second chapter."))
        .build()
}

/// The default book plus a stylesheet, which the defaults exclude.
#[must_use]
pub fn with_stylesheet() -> Vec<u8> {
    shell()
        .add(
            OPF_PATH,
            opf_xml(
                &[("ch1", "text/chap1.xhtml"), ("ch2", "text/chap2.xhtml")],
                &[
                    ("cover-img", "images/cover.png", "image/png", "cover-image"),
                    ("css", "style/main.css", "text/css", ""),
                ],
            ),
        )
        .add(CHAP1, chapter_xhtml("Chapter One", "The first chapter."))
        .add(CHAP2, chapter_xhtml("Chapter Two", "The second chapter."))
        .add(STYLESHEET, "body { margin: 0 }")
        .add(COVER, IMAGE)
        .build()
}

/// Archive path of the EPUB 3 navigation document.
pub const NAV: &str = "OEBPS/nav.xhtml";

/// Archive path of the EPUB 2 NCX.
pub const NCX: &str = "OEBPS/toc.ncx";

/// Archive path of the first chapter's media overlay.
pub const OVERLAY: &str = "OEBPS/overlays/chap1.smil";

/// An EPUB 3 navigation document over the default book's two chapters.
///
/// The second chapter is nested under the first, so a test can tell a flat
/// reading of the table of contents from a nested one.
#[must_use]
pub fn nav_xhtml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body>
  <nav epub:type="toc">
    <h1>Contents</h1>
    <ol>
      <li><a href="text/chap1.xhtml">Chapter One</a>
        <ol><li><a href="text/chap2.xhtml#part2">Chapter Two</a></li></ol>
      </li>
    </ol>
  </nav>
</body></html>"#
        .to_owned()
}

/// An EPUB 2 NCX over the same two chapters, nested the same way.
#[must_use]
pub fn ncx_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <docTitle><text>A Tale of Two Chapters</text></docTitle>
  <navMap>
    <navPoint id="np1" playOrder="1">
      <navLabel><text>Chapter One</text></navLabel>
      <content src="text/chap1.xhtml"/>
      <navPoint id="np2" playOrder="2">
        <navLabel><text>Chapter Two</text></navLabel>
        <content src="text/chap2.xhtml#part2"/>
      </navPoint>
    </navPoint>
  </navMap>
</ncx>"#
        .to_owned()
}

/// A SMIL media overlay narrating the first chapter.
#[must_use]
pub fn smil_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<smil xmlns="http://www.w3.org/ns/SMIL" version="3.0">
  <body>
    <seq id="s1">
      <par id="p1">
        <text src="../text/chap1.xhtml#s1"/>
        <audio src="../audio/chap1.mp3" clipBegin="0:00:00.000" clipEnd="0:00:12.500"/>
      </par>
      <par id="p2">
        <text src="../text/chap1.xhtml#s2"/>
        <audio src="../audio/chap1.mp3" clipBegin="12.5s" clipEnd="20s"/>
      </par>
    </seq>
  </body>
</smil>"#
        .to_owned()
}

/// The default book plus an EPUB 3 navigation document.
#[must_use]
pub fn with_nav() -> Vec<u8> {
    shell()
        .add(
            OPF_PATH,
            opf_xml(
                &[("ch1", "text/chap1.xhtml"), ("ch2", "text/chap2.xhtml")],
                &[
                    ("cover-img", "images/cover.png", "image/png", "cover-image"),
                    ("nav", "nav.xhtml", "application/xhtml+xml", "nav"),
                ],
            ),
        )
        .add(NAV, nav_xhtml())
        .add(CHAP1, chapter_xhtml("Chapter One", "The first chapter."))
        .add(CHAP2, chapter_xhtml("Chapter Two", "The second chapter."))
        .add(COVER, IMAGE)
        .build()
}

/// The default book plus an EPUB 2 NCX, found through `<spine toc="ncx">`.
///
/// No `nav` property anywhere, so this is the fallback path: a book that
/// predates the navigation document entirely.
#[must_use]
pub fn with_ncx() -> Vec<u8> {
    let manifest = "\
    <item id=\"ch1\" href=\"text/chap1.xhtml\" media-type=\"application/xhtml+xml\"/>\n\
    <item id=\"ch2\" href=\"text/chap2.xhtml\" media-type=\"application/xhtml+xml\"/>\n\
    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n";
    let opf = package(
        manifest,
        "    <itemref idref=\"ch1\"/>\n    <itemref idref=\"ch2\"/>\n",
    )
    .replace("<spine>", "<spine toc=\"ncx\">");

    shell()
        .add(OPF_PATH, opf)
        .add(NCX, ncx_xml())
        .add(CHAP1, chapter_xhtml("Chapter One", "The first chapter."))
        .add(CHAP2, chapter_xhtml("Chapter Two", "The second chapter."))
        .build()
}

/// The default book with the first chapter narrated by a media overlay.
///
/// The audio itself is a manifest entry the default options exclude, which is
/// the point: the *alignment* is available without asking for the recording.
#[must_use]
pub fn narrated() -> Vec<u8> {
    let manifest = "\
    <item id=\"ch1\" href=\"text/chap1.xhtml\" media-type=\"application/xhtml+xml\" \
     media-overlay=\"ov1\"/>\n\
    <item id=\"ch2\" href=\"text/chap2.xhtml\" media-type=\"application/xhtml+xml\"/>\n\
    <item id=\"ov1\" href=\"overlays/chap1.smil\" media-type=\"application/smil+xml\"/>\n\
    <item id=\"aud1\" href=\"audio/chap1.mp3\" media-type=\"audio/mpeg\"/>\n";

    shell()
        .add(
            OPF_PATH,
            package(
                manifest,
                "    <itemref idref=\"ch1\"/>\n    <itemref idref=\"ch2\"/>\n",
            ),
        )
        .add(OVERLAY, smil_xml())
        .add(CHAP1, chapter_xhtml("Chapter One", "The first chapter."))
        .add(CHAP2, chapter_xhtml("Chapter Two", "The second chapter."))
        .add("OEBPS/audio/chap1.mp3", b"not really an mp3".to_vec())
        .build()
}

/// A book of `count` chapters, each padded to roughly `size` bytes.
///
/// Used by the streaming tests, where the point is that a chapter reaches the
/// client while later ones are still compressed. The padding is repetitive and
/// therefore compresses hard, which is fine: it stays under the per-entry
/// ratio floor, so it is not mistaken for a bomb.
#[must_use]
pub fn long_book(count: usize, size: usize) -> Vec<u8> {
    let spine: Vec<(String, String)> = (0..count)
        .map(|i| (format!("ch{i}"), format!("text/chap{i}.xhtml")))
        .collect();
    let borrowed: Vec<(&str, &str)> = spine
        .iter()
        .map(|(id, href)| (id.as_str(), href.as_str()))
        .collect();

    let mut builder = shell().add(OPF_PATH, opf_xml(&borrowed, &[]));
    for (i, (_, href)) in spine.iter().enumerate() {
        let filler = format!("Chapter {i} body. ").repeat(size / 18 + 1);
        builder = builder.add(
            &format!("OEBPS/{href}"),
            chapter_xhtml(&format!("Chapter {i}"), &filler),
        );
    }
    builder.build()
}

/// One of the two ZIP header kinds, and where its fields sit.
///
/// Offsets are from the signature, per APPNOTE.TXT 4.3.7 (local file header)
/// and 4.3.12 (central directory record).
struct Header {
    /// Four-byte signature that starts the record.
    signature: &'static [u8; 4],
    /// Offset of the general-purpose bit flag.
    flags_at: usize,
    /// Offset of the compression method.
    method_at: usize,
    /// Offset of the file name length.
    name_length_at: usize,
    /// Offset of the file name itself.
    name_at: usize,
}

/// The two header kinds a reader consults, so a patch has to reach both.
const HEADERS: [Header; 2] = [
    Header {
        signature: b"PK\x03\x04",
        flags_at: 6,
        method_at: 8,
        name_length_at: 26,
        name_at: 30,
    },
    Header {
        signature: b"PK\x01\x02",
        flags_at: 8,
        method_at: 10,
        name_length_at: 28,
        name_at: 46,
    },
];

/// Overwrite the compression method of every header naming `name`.
///
/// The `zip` crate is built here without the features that would let it
/// *write* bzip2 or zstd, which is the point: this build cannot produce an
/// archive it cannot read. Patching the headers is how a test still gets one,
/// and it is exactly the archive an attacker would hand the server.
pub fn patch_compression_method(archive: &mut [u8], name: &str, method: u16) {
    patch(archive, name, |header| header.method_at, method);
}

/// Set the encryption flag (general-purpose bit 0) on every header naming
/// `name`, producing the archive a DRM'd or obfuscated entry actually is.
pub fn patch_encrypted(archive: &mut [u8], name: &str) {
    patch(archive, name, |header| header.flags_at, 1);
}

/// Write `value` into one 16-bit field of every header naming `name`.
fn patch(archive: &mut [u8], name: &str, field: fn(&Header) -> usize, value: u16) {
    for header in &HEADERS {
        let mut position = 0;
        while position + header.name_at + name.len() <= archive.len() {
            if &archive[position..position + 4] != header.signature.as_slice() {
                position += 1;
                continue;
            }
            let length = u16::from_le_bytes([
                archive[position + header.name_length_at],
                archive[position + header.name_length_at + 1],
            ]) as usize;
            let start = position + header.name_at;
            if length == name.len()
                && start + length <= archive.len()
                && &archive[start..start + length] == name.as_bytes()
            {
                let at = position + field(header);
                archive[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
            position += 4;
        }
    }
}

// --- Server harness -------------------------------------------------------

use std::sync::Arc;

use grpc_epub::Limits;
use grpc_epub::metrics::Metrics;
use grpc_epub::proto::v1 as pb;
use grpc_epub::proto::v1::epub_parse_service_client::EpubParseServiceClient;
use grpc_epub::service::EpubGrpc;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Endpoint, Server};

/// A running server plus a client connected to it.
pub struct Harness {
    /// Connected client for the server below.
    pub client: EpubParseServiceClient<Channel>,
    /// The counters the server reports into, so a test can ask how far the
    /// parse has actually got rather than inferring it from timing.
    pub metrics: Arc<Metrics>,
}

/// Start a server on an ephemeral localhost port with the default limits.
pub async fn start() -> Harness {
    start_with(Limits::default()).await
}

/// Start a server on an ephemeral localhost port with the given limits.
pub async fn start_with(limits: Limits) -> Harness {
    start_inner(limits, None).await
}

/// Start a server with the client's HTTP/2 receive windows pinned to
/// `window` bytes.
///
/// The streaming tests use this to turn "how far has the parser run ahead
/// of the reader" back into a question the wire answers deterministically:
/// with the window capped, flow control stalls the server once about one
/// chapter is in flight, and every chapter the client consumes returns
/// credit for about one more. Left unpinned, the whole book can cross into
/// transport buffers before anyone reads it — whether that happens depends
/// on kernel buffers and scheduling, not on the design under test.
pub async fn start_with_window(window: u32) -> Harness {
    start_inner(Limits::default(), Some(window)).await
}

async fn start_inner(limits: Limits, window: Option<u32>) -> Harness {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local address");

    let metrics = Metrics::new();
    let service = EpubGrpc::with_metrics(limits, Arc::clone(&metrics)).into_service();
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("server failed");
    });

    let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    let endpoint = match window {
        Some(window) => endpoint
            .initial_stream_window_size(window)
            .initial_connection_window_size(window),
        None => endpoint,
    };
    let channel = endpoint.connect().await.expect("connect to server");
    Harness {
        client: EpubParseServiceClient::new(channel),
        metrics,
    }
}

impl Harness {
    /// Upload `archive` in one frame and collect every event.
    pub async fn parse(
        &self,
        archive: &[u8],
        options: pb::ParseOptions,
    ) -> Result<Vec<pb::parse_epub_response::Event>, tonic::Status> {
        self.parse_chunked(archive, options, usize::MAX).await
    }

    /// Upload `archive` in `chunk_size` slices and collect every event.
    pub async fn parse_chunked(
        &self,
        archive: &[u8],
        options: pb::ParseOptions,
        chunk_size: usize,
    ) -> Result<Vec<pb::parse_epub_response::Event>, tonic::Status> {
        let mut client = self.client.clone();
        let mut frames = vec![pb::ParseEpubRequest {
            frame: Some(pb::parse_epub_request::Frame::Options(options)),
        }];
        frames.extend(
            archive
                .chunks(chunk_size.min(archive.len().max(1)))
                .map(|chunk| pb::ParseEpubRequest {
                    frame: Some(pb::parse_epub_request::Frame::Chunk(chunk.to_vec())),
                }),
        );

        let mut stream = client
            .parse_epub(tokio_stream::iter(frames))
            .await?
            .into_inner();
        let mut events = Vec::new();
        while let Some(response) = stream.message().await? {
            events.push(response.event.expect("every response carries an event"));
        }
        Ok(events)
    }

    /// Parse with the default options, expecting success.
    pub async fn parse_ok(&self, archive: &[u8]) -> Vec<pb::parse_epub_response::Event> {
        self.parse(archive, pb::ParseOptions::default())
            .await
            .expect("the book should parse")
    }

    /// Parse with the default options, expecting a failure status.
    pub async fn parse_err(&self, archive: &[u8]) -> tonic::Status {
        self.parse(archive, pb::ParseOptions::default())
            .await
            .expect_err("the book should be refused")
    }
}

/// The `info` event, which must be first.
#[must_use]
pub fn info(events: &[pb::parse_epub_response::Event]) -> &pb::EpubInfo {
    match events.first() {
        Some(pb::parse_epub_response::Event::Info(info)) => info,
        other => panic!("the first event must be `info`, got {other:?}"),
    }
}

/// The `status` trailer, which must be last.
#[must_use]
pub fn status(events: &[pb::parse_epub_response::Event]) -> &pb::ParseStatus {
    match events.last() {
        Some(pb::parse_epub_response::Event::Status(status)) => status,
        other => panic!("the last event must be `status`, got {other:?}"),
    }
}

/// Every `chapter` event, in the order received.
#[must_use]
pub fn chapters(events: &[pb::parse_epub_response::Event]) -> Vec<&pb::Chapter> {
    events
        .iter()
        .filter_map(|event| match event {
            pb::parse_epub_response::Event::Chapter(chapter) => Some(chapter),
            _ => None,
        })
        .collect()
}

/// Every `document` event, in the order received.
///
/// A conforming stream has at most one, and only when `emit_document` was set;
/// the tests assert that by counting rather than by taking the first.
#[must_use]
pub fn documents(
    events: &[pb::parse_epub_response::Event],
) -> Vec<&grpc_epub::proto::document_v1::Document> {
    events
        .iter()
        .filter_map(|event| match event {
            pb::parse_epub_response::Event::Document(document) => Some(document),
            _ => None,
        })
        .collect()
}

/// The `navigation` event, when the stream carried one.
#[must_use]
pub fn navigation(events: &[pb::parse_epub_response::Event]) -> Option<&pb::Navigation> {
    events.iter().find_map(|event| match event {
        pb::parse_epub_response::Event::Navigation(navigation) => Some(navigation),
        _ => None,
    })
}

/// Every `media_overlay` event, in the order received.
#[must_use]
pub fn overlays(events: &[pb::parse_epub_response::Event]) -> Vec<&pb::MediaOverlay> {
    events
        .iter()
        .filter_map(|event| match event {
            pb::parse_epub_response::Event::MediaOverlay(overlay) => Some(overlay),
            _ => None,
        })
        .collect()
}

/// Every `resource` event, in the order received.
#[must_use]
pub fn resources(events: &[pb::parse_epub_response::Event]) -> Vec<&pb::Resource> {
    events
        .iter()
        .filter_map(|event| match event {
            pb::parse_epub_response::Event::Resource(resource) => Some(resource),
            _ => None,
        })
        .collect()
}

/// A one-word name per event, for order assertions that read like the
/// contract: `["info", "chapter", "chapter", "resource", "status"]`.
#[must_use]
pub fn shape(events: &[pb::parse_epub_response::Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            pb::parse_epub_response::Event::Info(_) => "info",
            pb::parse_epub_response::Event::Chapter(_) => "chapter",
            pb::parse_epub_response::Event::Resource(_) => "resource",
            pb::parse_epub_response::Event::Status(_) => "status",
            pb::parse_epub_response::Event::Document(_) => "document",
            pb::parse_epub_response::Event::Navigation(_) => "navigation",
            pb::parse_epub_response::Event::MediaOverlay(_) => "media_overlay",
        })
        .collect()
}
