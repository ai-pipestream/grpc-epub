// SPDX-License-Identifier: Apache-2.0

//! The caps that make it safe to point this server at a file someone else
//! made.
//!
//! There are two layers. [`Limits`] is what the process was started with, from
//! defaults or the environment, and it is a ceiling. A caller's
//! [`ParseOptions`](crate::proto::v1::ParseOptions) is resolved against it by
//! [`Limits::resolve`], which takes the *lower* of the two for every field. A
//! request can therefore ask for less headroom than the server allows and
//! never for more, so an operator's sizing decision cannot be argued away from
//! the wire.

use crate::proto::v1 as pb;

/// One mebibyte, the unit every size option on the wire is expressed in.
pub const MIB: u64 = 1024 * 1024;

/// Default ceiling on the compressed upload.
pub const DEFAULT_MAX_DOCUMENT_MIB: u32 = 256;

/// Default ceiling on total inflated bytes for one call.
///
/// Twice the upload ceiling rather than equal to it, because an EPUB is mostly
/// XHTML and a 2:1 overall expansion is ordinary rather than suspicious. The
/// per-entry ratio is what catches a bomb; this is what caps the heap.
pub const DEFAULT_MAX_UNCOMPRESSED_MIB: u32 = 512;

/// Default ceiling on the archive's entry count.
///
/// A book with ten thousand files in it is possible (one XHTML file per verse
/// is a real pattern) and a hundred thousand is an attack on the central
/// directory parse rather than a book.
pub const DEFAULT_MAX_ENTRIES: u32 = 10_000;

/// Default ceiling on one entry's inflated-to-stored ratio.
///
/// Deflate's theoretical best is 1032:1. Prose runs 3:1 to 5:1 and a
/// pathological but legitimate file (a large embedded SVG, a generated
/// stylesheet) can reach the low hundreds, so this sits above the plausible
/// range and well below the achievable one.
pub const DEFAULT_MAX_COMPRESSION_RATIO: u32 = 200;

/// Inflated size an entry must exceed before the ratio rule applies to it.
///
/// Without a floor the rule is useless: a 200-byte `mimetype` stored as 40
/// bytes is a 5:1 ratio, and a 4 KiB OPF of repetitive XML routinely beats
/// 100:1 while being incapable of hurting anyone. A bomb has to be *large*
/// after inflation to be a bomb, so the rule only looks at entries that are.
pub const DEFAULT_RATIO_FLOOR_BYTES: u64 = MIB;

/// Default ceiling on a single inbound `chunk` frame.
///
/// Not a document limit: an upload is any number of chunks. This bounds the
/// transient per-call buffer and, with it, how much one hostile length prefix
/// can make the transport allocate.
pub const DEFAULT_MAX_CHUNK_BYTES: u64 = 16 * MIB;

/// Default ceiling on concurrent inflating calls.
///
/// The bound exists to cap heap, not to shed load: each in-flight call can
/// hold its upload plus one inflated entry, so eight of them at the default
/// caps is the memory ceiling of the process. Calls past the bound wait.
pub const DEFAULT_MAX_CONCURRENT_PARSES: usize = 8;

/// Ceilings the process enforces, in bytes rather than the wire's MiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest compressed upload accepted.
    pub max_document_bytes: u64,
    /// Largest total inflated size one call may produce.
    pub max_uncompressed_bytes: u64,
    /// Largest number of entries the archive may declare.
    pub max_entries: u32,
    /// Largest inflated-to-stored ratio one entry may have.
    pub max_compression_ratio: u32,
    /// Inflated size above which the ratio rule applies.
    pub compression_ratio_floor_bytes: u64,
    /// Largest single inbound `chunk` frame.
    pub max_chunk_bytes: u64,
    /// Largest number of calls that may inflate at once.
    pub max_concurrent_parses: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_document_bytes: u64::from(DEFAULT_MAX_DOCUMENT_MIB) * MIB,
            max_uncompressed_bytes: u64::from(DEFAULT_MAX_UNCOMPRESSED_MIB) * MIB,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_compression_ratio: DEFAULT_MAX_COMPRESSION_RATIO,
            compression_ratio_floor_bytes: DEFAULT_RATIO_FLOOR_BYTES,
            max_chunk_bytes: DEFAULT_MAX_CHUNK_BYTES,
            max_concurrent_parses: DEFAULT_MAX_CONCURRENT_PARSES,
        }
    }
}

/// Read a `u64` environment variable, falling back to `default`.
///
/// A value that does not parse is ignored rather than fatal, and a zero is
/// treated as "not set": no limit here has a meaningful zero, and silently
/// running with an unbounded cap because of a typo is the failure mode worth
/// designing out.
fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(0) | None => default,
        Some(value) => value,
    }
}

impl Limits {
    /// Build the process limits from `GRPC_EPUB_*` environment variables,
    /// falling back to the defaults above.
    ///
    /// See the README for the full list.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            max_document_bytes: env_u64(
                "GRPC_EPUB_MAX_DOCUMENT_MIB",
                u64::from(DEFAULT_MAX_DOCUMENT_MIB),
            ) * MIB,
            max_uncompressed_bytes: env_u64(
                "GRPC_EPUB_MAX_UNCOMPRESSED_MIB",
                u64::from(DEFAULT_MAX_UNCOMPRESSED_MIB),
            ) * MIB,
            max_entries: u32::try_from(env_u64(
                "GRPC_EPUB_MAX_ENTRIES",
                u64::from(DEFAULT_MAX_ENTRIES),
            ))
            .unwrap_or(u32::MAX),
            max_compression_ratio: u32::try_from(env_u64(
                "GRPC_EPUB_MAX_COMPRESSION_RATIO",
                u64::from(DEFAULT_MAX_COMPRESSION_RATIO),
            ))
            .unwrap_or(u32::MAX),
            compression_ratio_floor_bytes: env_u64(
                "GRPC_EPUB_COMPRESSION_RATIO_FLOOR_BYTES",
                DEFAULT_RATIO_FLOOR_BYTES,
            ),
            max_chunk_bytes: env_u64("GRPC_EPUB_MAX_CHUNK_BYTES", DEFAULT_MAX_CHUNK_BYTES),
            max_concurrent_parses: usize::try_from(env_u64(
                "GRPC_EPUB_MAX_CONCURRENT_PARSES",
                defaults.max_concurrent_parses as u64,
            ))
            .unwrap_or(defaults.max_concurrent_parses),
        }
    }

    /// Resolve a caller's options against these ceilings.
    ///
    /// Every numeric field takes the minimum of what the caller asked for and
    /// what the server allows, with zero meaning "no preference".
    #[must_use]
    pub fn resolve(&self, options: &pb::ParseOptions) -> Effective {
        /// Lower `ceiling` to `requested` when the caller named a smaller one.
        fn narrow(requested: u32, ceiling: u64, unit: u64) -> u64 {
            match requested {
                0 => ceiling,
                n => ceiling.min(u64::from(n).saturating_mul(unit)),
            }
        }

        Effective {
            max_document_bytes: narrow(options.max_document_mib, self.max_document_bytes, MIB),
            max_uncompressed_bytes: narrow(
                options.max_uncompressed_mib,
                self.max_uncompressed_bytes,
                MIB,
            ),
            max_entries: match options.max_entries {
                0 => self.max_entries,
                n => self.max_entries.min(n),
            },
            max_compression_ratio: match options.max_compression_ratio {
                0 => self.max_compression_ratio,
                n => self.max_compression_ratio.min(n),
            },
            compression_ratio_floor_bytes: self.compression_ratio_floor_bytes,
            include_images: options.include_images.unwrap_or(true),
            include_stylesheets: options.include_stylesheets.unwrap_or(false),
            include_all_resources: options.include_all_resources.unwrap_or(false),
            emit_document: options.emit_document,
            parse_navigation: options.parse_navigation.unwrap_or(true),
            parse_media_overlays: options.parse_media_overlays.unwrap_or(false),
        }
    }

    /// Render these limits for `GetServiceInfo`.
    #[must_use]
    pub fn to_proto(self) -> pb::ServerLimits {
        /// Bytes back to the MiB the wire speaks, rounding down but never to
        /// zero: a sub-MiB ceiling is unusual but reporting it as "unlimited"
        /// would be a lie.
        fn mib(bytes: u64) -> u32 {
            u32::try_from((bytes / MIB).max(1)).unwrap_or(u32::MAX)
        }

        pb::ServerLimits {
            max_document_mib: mib(self.max_document_bytes),
            max_uncompressed_mib: mib(self.max_uncompressed_bytes),
            max_entries: self.max_entries,
            max_compression_ratio: self.max_compression_ratio,
            compression_ratio_floor_bytes: self.compression_ratio_floor_bytes,
            max_chunk_bytes: self.max_chunk_bytes,
            max_concurrent_parses: u32::try_from(self.max_concurrent_parses).unwrap_or(u32::MAX),
        }
    }
}

/// One call's resolved settings: the caller's options already clamped to the
/// server's ceilings, so nothing downstream has to remember to clamp again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effective {
    /// Largest compressed upload this call may deliver.
    pub max_document_bytes: u64,
    /// Largest total inflated size this call may produce.
    pub max_uncompressed_bytes: u64,
    /// Largest number of entries this call's archive may declare.
    pub max_entries: u32,
    /// Largest inflated-to-stored ratio one entry may have.
    pub max_compression_ratio: u32,
    /// Inflated size above which the ratio rule applies.
    pub compression_ratio_floor_bytes: u64,
    /// Whether image resources are emitted.
    pub include_images: bool,
    /// Whether stylesheet resources are emitted.
    pub include_stylesheets: bool,
    /// Whether every manifest resource is emitted regardless of kind.
    pub include_all_resources: bool,
    /// Whether the events are also folded into a `document` event, sent
    /// immediately before `status`. Not a limit: it is the one option that
    /// adds to the stream rather than trimming it, and it lives here because
    /// this is what one call's settings are.
    pub emit_document: bool,
    /// Whether the navigation document or NCX is parsed into a `navigation`
    /// event and into the Document's outline. On unless refused: the bytes are
    /// inflated either way, so the only new cost is one small XML parse.
    pub parse_navigation: bool,
    /// Whether each SMIL media overlay is parsed into a `media_overlay` event.
    /// Off unless asked for: overlays are one file per chapter and none of
    /// them is inflated otherwise.
    pub parse_media_overlays: bool,
}

impl Default for Effective {
    fn default() -> Self {
        Limits::default().resolve(&pb::ParseOptions::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_means_the_server_default() {
        let limits = Limits::default();
        let effective = limits.resolve(&pb::ParseOptions::default());
        assert_eq!(effective.max_document_bytes, limits.max_document_bytes);
        assert_eq!(effective.max_entries, limits.max_entries);
        assert!(effective.include_images, "images are on unless refused");
        assert!(
            !effective.include_stylesheets,
            "css is off unless asked for"
        );
        assert!(
            effective.parse_navigation,
            "the table of contents is read unless refused"
        );
        assert!(
            !effective.parse_media_overlays,
            "overlays cost one inflate per chapter, so they are opt-in"
        );
    }

    #[test]
    fn a_request_can_lower_a_limit_but_never_raise_it() {
        let limits = Limits::default();

        let lowered = limits.resolve(&pb::ParseOptions {
            max_uncompressed_mib: 1,
            max_entries: 5,
            ..Default::default()
        });
        assert_eq!(lowered.max_uncompressed_bytes, MIB);
        assert_eq!(lowered.max_entries, 5);

        let raised = limits.resolve(&pb::ParseOptions {
            max_uncompressed_mib: u32::MAX,
            max_entries: u32::MAX,
            max_compression_ratio: u32::MAX,
            max_document_mib: u32::MAX,
            ..Default::default()
        });
        assert_eq!(raised.max_uncompressed_bytes, limits.max_uncompressed_bytes);
        assert_eq!(raised.max_entries, limits.max_entries);
        assert_eq!(raised.max_compression_ratio, limits.max_compression_ratio);
        assert_eq!(raised.max_document_bytes, limits.max_document_bytes);
    }

    #[test]
    fn include_images_distinguishes_absent_from_false() {
        let limits = Limits::default();
        assert!(limits.resolve(&pb::ParseOptions::default()).include_images);
        assert!(
            !limits
                .resolve(&pb::ParseOptions {
                    include_images: Some(false),
                    ..Default::default()
                })
                .include_images
        );
    }
}
