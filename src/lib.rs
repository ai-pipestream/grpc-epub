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
//!   Docling's EPUB backend unpacks to a temp directory; that is the practice
//!   this service exists to avoid.
//! - **The stream is the product.** `info` is emitted before a single chapter
//!   is inflated and each `chapter` is emitted as its entry comes off the
//!   archive, so a reader can paint chapter 1 while chapter 12 is still
//!   compressed. `status` is a trailer of counts, never the payload. See
//!   [`extract`] for why the upload is still buffered and the output is not.
//! - **Hostile input is the normal case.** [`archive`] holds the zip-bomb
//!   policy, [`href`] holds the path-traversal policy, and [`opf`] holds the
//!   external-entity policy. Each is tested against an attack the format can
//!   actually express.

pub mod archive;
pub mod extract;
pub mod href;
pub mod limits;
pub mod metrics;
pub mod opf;
pub mod proto;
pub mod service;

pub use limits::Limits;
pub use metrics::Metrics;
pub use service::EpubGrpc;
