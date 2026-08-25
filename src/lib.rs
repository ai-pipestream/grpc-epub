// SPDX-License-Identifier: Apache-2.0

//! A gRPC server that unpacks EPUB archives in memory and streams the spine
//! back chapter by chapter.
//!
//! Design rules:
//!
//! - **Thin packager.** This process owns the ZIP, `META-INF/container.xml`,
//!   the OPF package document, and the spine. It does not own HTML. Chapter
//!   XHTML goes out as bytes for the HTML collector to read, and nothing here
//!   parses, sanitizes, or rewrites it.
//! - **Nothing touches disk.** The upload lives in one `Vec<u8>`, entries are
//!   inflated into memory one at a time, and the container runs read-only.
//!   The usual EPUB reader unpacks to a temp directory; that is the practice
//!   this service exists to avoid.
//! - **The stream is the product.** `info` is emitted before a single chapter
//!   is inflated and each `chapter` is emitted as its entry comes off the
//!   archive, so a reader can paint chapter 1 while chapter 12 is still
//!   compressed. `status` is a trailer of counts, never the payload. See
//!   [`extract`] for why the upload is still buffered and the output is not.
//! - **The Document is a projection, not the product.** With
//!   `ParseOptions.emit_document` set, [`document_fold`] folds the same events
//!   into one `ai.pipestream.document.v1.Document` and the server sends it
//!   immediately before the trailer. It is the book's skeleton — spine order,
//!   OPF metadata, the table of contents, image pointers — and never a second
//!   copy of the bytes.
//! - **Typed slots over strings.** Everything the Document schema types, this
//!   service fills as that type: a book's dates reach `DocumentMeta` as
//!   instants ([`datetime`] reads them), its language and subjects as the
//!   fields the query layer understands. The open-vocabulary remainder of
//!   Dublin Core is what `extra` and `custom_fields` are for, and nothing that
//!   has a typed home is written to them as a string as well.
//! - **Navigation is metadata, not content.** [`nav`] reads the EPUB 3
//!   navigation document and the EPUB 2 NCX, and [`smil`] reads media-overlay
//!   cues. Both are lists of links and timings that the book states about
//!   itself, which is why reading them does not breach the thin-packager rule
//!   the way parsing a chapter would.
//! - **Hostile input is the normal case.** [`archive`] holds the zip-bomb
//!   policy, [`href`] holds the path-traversal policy, and [`opf`] holds the
//!   external-entity policy. Each is tested against an attack the format can
//!   actually express.

pub mod archive;
pub mod datetime;
pub mod document_fold;
pub mod extract;
pub mod href;
pub mod limits;
pub mod metrics;
pub mod nav;
pub mod opf;
pub mod proto;
pub mod service;
pub mod smil;

pub use document_fold::DocumentFold;
pub use limits::Limits;
pub use metrics::Metrics;
pub use service::EpubGrpc;
