// SPDX-License-Identifier: Apache-2.0

//! Generated protobuf code for the `ai.pipestream.epub.v1` package and the
//! vendored `ai.pipestream.document.v1` schema it projects into.
//!
//! The files under `src/gen` are produced by `buf generate` (see
//! `buf.gen.yaml`; never edit them by hand). Regenerate after any change under
//! `proto/`:
//!
//! ```sh
//! buf lint
//! buf generate
//! buf build -o src/gen/file_descriptor_set.binpb
//! ```
//!
//! The modules below mirror the protobuf package path exactly, because prost
//! writes cross-package references as relative Rust paths: the epub stubs
//! reach the Document as `super::super::super::document::v1::Document`, which
//! only resolves if `epub::v1` and `document::v1` are siblings under
//! `ai::pipestream`. [`v1`] and [`document_v1`] are the short names the rest
//! of the crate uses.

/// The protobuf package tree, nested to match the `.proto` package paths.
#[allow(clippy::all, clippy::pedantic, clippy::nursery, missing_docs)]
pub mod ai {
    /// The `ai.pipestream` namespace.
    pub mod pipestream {
        /// The `ai.pipestream.document` namespace.
        pub mod document {
            /// Messages and enums for `ai.pipestream.document.v1`: the
            /// canonical Document schema, vendored byte-identical from
            /// gRParse. Nothing here is edited in this repo.
            pub mod v1 {
                include!("gen/ai/pipestream/document/v1/ai.pipestream.document.v1.rs");
            }
        }
        /// The `ai.pipestream.epub` namespace.
        pub mod epub {
            /// Messages, enums, client, and server for
            /// `ai.pipestream.epub.v1`.
            ///
            /// Wire-level documentation lives in the `.proto` files (buf
            /// enforces a comment on every item there); the generated Rust
            /// carries it over where prost supports it.
            pub mod v1 {
                // The prost output already ends with an `include!` of the
                // tonic half, pulling in the client and server modules.
                include!("gen/ai/pipestream/epub/v1/ai.pipestream.epub.v1.rs");
            }
        }
    }
}

/// The `ai.pipestream.epub.v1` package: this service's own wire contract.
pub use ai::pipestream::epub::v1;

/// The `ai.pipestream.document.v1` package: the Document plane this collector
/// projects into when `ParseOptions.emit_document` is set.
pub use ai::pipestream::document::v1 as document_v1;

/// Serialized `FileDescriptorSet` for `proto/ai/pipestream/epub/v1`, backing
/// gRPC server reflection.
///
/// Codegen runs through `buf generate` rather than a build script, so there is
/// no build-time descriptor set to reuse; this is the `buf build` output
/// checked in next to the generated Rust.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("gen/file_descriptor_set.binpb");
