// SPDX-License-Identifier: Apache-2.0

//! Generated protobuf code for the `ai.pipestream.epub.v1` package.
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

/// Messages, enums, client, and server for the `ai.pipestream.epub.v1`
/// protobuf package.
///
/// Wire-level documentation lives in the `.proto` files (buf enforces a
/// comment on every item there); the generated Rust carries it over where
/// prost supports it.
#[allow(clippy::all, clippy::pedantic, clippy::nursery, missing_docs)]
pub mod v1 {
    // The prost output already ends with an `include!` of the tonic half,
    // pulling in the client and server modules.
    include!("gen/ai/pipestream/epub/v1/ai.pipestream.epub.v1.rs");
}

/// Serialized `FileDescriptorSet` for `proto/ai/pipestream/epub/v1`, backing
/// gRPC server reflection.
///
/// Codegen runs through `buf generate` rather than a build script, so there is
/// no build-time descriptor set to reuse; this is the `buf build` output
/// checked in next to the generated Rust.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("gen/file_descriptor_set.binpb");
