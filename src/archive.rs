// SPDX-License-Identifier: Apache-2.0

//! Opening a ZIP held in memory and inflating entries under a budget.
//!
//! This is where the zip-bomb policy from `docs/design.md` lives. Three rules,
//! each covering a hole the others leave:
//!
//! 1. **Entry count**, checked against the central directory before anything
//!    is inflated. Cheap, and it stops the archive whose whole payload is a
//!    million zero-byte names.
//! 2. **Total inflated bytes**, a running budget across every entry the call
//!    extracts. This is the heap ceiling.
//! 3. **Per-entry ratio**, inflated over stored. The total alone lets an
//!    attacker sit just under it and still buy a thousandfold amplification
//!    with a small upload; the ratio prices that back.
//!
//! Rules 2 and 3 are enforced twice: once against the sizes the central
//! directory declares, which is free and rejects the honest bomb before a byte
//! is inflated, and once against what actually came out of the decompressor,
//! which is what catches a header that lies. Only the second is load-bearing;
//! the first exists so the common case costs nothing.
//!
//! Nothing here touches the filesystem. `zip`'s `extract` family is never
//! called, and the crate is built without the features that would let it
//! decode anything but store and deflate.

use std::io::{Cursor, Read};

use tonic::Status;
use zip::result::ZipError;
use zip::{CompressionMethod, ZipArchive};

use crate::limits::Effective;

/// An archive over borrowed bytes. Nothing is copied to open it.
pub type MemoryArchive<'a> = ZipArchive<Cursor<&'a [u8]>>;

/// Inflated bytes read in one pass before the budget is re-checked.
const READ_CHUNK: usize = 64 * 1024;

/// The running decompression budget for one call.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Inflated bytes still available.
    remaining: u64,
    /// Inflated bytes spent so far.
    consumed: u64,
    /// Entries inflated so far.
    entries: u32,
}

impl Budget {
    /// Open a budget for `limit` total inflated bytes.
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self {
            remaining: limit,
            consumed: 0,
            entries: 0,
        }
    }

    /// Inflated bytes spent so far.
    #[must_use]
    pub const fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Entries inflated so far.
    #[must_use]
    pub const fn entries(&self) -> u32 {
        self.entries
    }
}

/// Translate a `zip` error into the status the fleet contract calls for.
///
/// The split is about who has to act. `UnsupportedArchive` and an unsupported
/// compression method mean this build cannot read that archive, which is
/// UNIMPLEMENTED and stays true however the caller retries;
/// `InvalidArchive` means the bytes are broken, which is theirs to fix.
/// Encryption arrives as `UnsupportedArchive("Password required…")` and is
/// deliberately in the first group: a DRM'd or obfuscated EPUB is a format
/// this service does not implement, not a malformed one.
#[must_use]
pub fn zip_status(error: &ZipError) -> Status {
    match error {
        ZipError::UnsupportedArchive(detail) => Status::unimplemented(format!(
            "this archive needs a feature this build lacks: {detail}"
        )),
        ZipError::CompressionMethodNotSupported(id) => Status::unimplemented(format!(
            "entry uses compression method {id}; only store and deflate are supported"
        )),
        ZipError::InvalidPassword => {
            Status::unimplemented("the archive is encrypted; DRM is not supported")
        }
        ZipError::FileNotFound => {
            Status::invalid_argument("the archive is missing an entry it declares")
        }
        ZipError::InvalidArchive(detail) => {
            Status::invalid_argument(format!("not a readable ZIP archive: {detail}"))
        }
        ZipError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            Status::invalid_argument("the archive is truncated")
        }
        ZipError::Io(io) => Status::internal(format!("reading the archive failed: {io}")),
        // `ZipError` is `#[non_exhaustive]`; a variant added upstream is a
        // failure to read the input, not a bug here.
        other => Status::invalid_argument(format!("the archive could not be read: {other}")),
    }
}

/// Open an in-memory archive.
///
/// # Errors
///
/// `INVALID_ARGUMENT` when the bytes are not a ZIP or are truncated,
/// `RESOURCE_EXHAUSTED` when the archive declares more entries than the call
/// allows.
pub fn open<'a>(bytes: &'a [u8], limits: &Effective) -> Result<MemoryArchive<'a>, Status> {
    if bytes.is_empty() {
        return Err(Status::invalid_argument(
            "the upload was empty; send the EPUB as one or more `chunk` frames",
        ));
    }
    let archive = ZipArchive::new(Cursor::new(bytes)).map_err(|e| zip_status(&e))?;

    let entries = u32::try_from(archive.len()).unwrap_or(u32::MAX);
    if entries > limits.max_entries {
        return Err(Status::resource_exhausted(format!(
            "the archive declares {entries} entries, over the {} allowed",
            limits.max_entries
        )));
    }
    Ok(archive)
}

/// What the central directory says about one entry, gathered without
/// inflating it.
#[derive(Clone, Debug)]
pub struct EntryInfo {
    /// Position in the central directory. Emission order follows this, which
    /// is what makes a resource arrive "when its entry is hit".
    pub index: usize,
    /// The name as stored, normalized by [`crate::href::check_entry_name`].
    pub name: String,
    /// Inflated size as declared. May be a lie; treated as a hint only.
    pub declared_size: u64,
    /// Stored size as declared.
    pub compressed_size: u64,
}

/// Read the central directory and check every entry name and encoding.
///
/// This runs before any event is emitted, so a hostile name or an entry this
/// build cannot decode fails the call cleanly instead of truncating a stream
/// that has already started. Directory entries are dropped: they carry no
/// content and their names would collide with real files after normalization.
///
/// # Errors
///
/// `INVALID_ARGUMENT` for a name that escapes the archive root or is otherwise
/// unusable, `UNIMPLEMENTED` for an encrypted entry or a compression method
/// outside store and deflate.
pub fn scan(archive: &mut MemoryArchive<'_>) -> Result<Vec<EntryInfo>, Status> {
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        // `by_index_raw` reads the header without building a decompressor, so
        // this pass costs a seek per entry and no inflation.
        let entry = archive.by_index_raw(index).map_err(|e| zip_status(&e))?;

        if entry.encrypted() {
            return Err(Status::unimplemented(format!(
                "entry {:?} is encrypted; DRM and entry obfuscation are not supported",
                entry.name()
            )));
        }
        match entry.compression() {
            CompressionMethod::Stored | CompressionMethod::Deflated => {}
            other => {
                return Err(Status::unimplemented(format!(
                    "entry {:?} uses compression method {other:?}; only store and deflate are \
                     supported",
                    entry.name()
                )));
            }
        }

        let name = entry.name().to_owned();
        let declared_size = entry.size();
        let compressed_size = entry.compressed_size();
        drop(entry);

        let Some(normalized) = crate::href::check_entry_name(&name)
            .map_err(|e| Status::invalid_argument(format!("archive entry {name:?}: {e}")))?
        else {
            continue; // A directory entry.
        };

        entries.push(EntryInfo {
            index,
            name: normalized,
            declared_size,
            compressed_size,
        });
    }
    Ok(entries)
}

/// Inflate one entry under the budget.
///
/// # Errors
///
/// `RESOURCE_EXHAUSTED` when the entry would take the call over its total
/// inflated budget or exceeds the per-entry compression ratio; otherwise
/// whatever [`zip_status`] makes of the failure.
pub fn read_entry(
    archive: &mut MemoryArchive<'_>,
    entry: &EntryInfo,
    limits: &Effective,
    budget: &mut Budget,
) -> Result<Vec<u8>, Status> {
    // The free checks first, against what the archive says about itself.
    check_budget(entry.declared_size, budget.remaining, &entry.name)?;
    check_ratio(
        entry.declared_size,
        entry.compressed_size,
        limits,
        &entry.name,
    )?;

    let mut file = archive.by_index(entry.index).map_err(|e| zip_status(&e))?;

    let ceiling = usize::try_from(budget.remaining).unwrap_or(usize::MAX);
    let hint = usize::try_from(entry.declared_size.min(budget.remaining)).unwrap_or(0);
    let mut out = Vec::with_capacity(hint);
    let mut chunk = vec![0u8; READ_CHUNK];

    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|e| zip_status(&ZipError::Io(e)))?;
        if read == 0 {
            break;
        }
        // Checked *before* the copy, so the allocation never overshoots the
        // budget even by one chunk. This is the check that catches a central
        // directory understating the entry.
        if out.len() + read > ceiling {
            return Err(exhausted(&entry.name, budget.remaining));
        }
        out.extend_from_slice(&chunk[..read]);
    }
    drop(file);

    let actual = out.len() as u64;
    check_ratio(actual, entry.compressed_size, limits, &entry.name)?;

    budget.remaining -= actual;
    budget.consumed += actual;
    budget.entries += 1;
    Ok(out)
}

/// The status a budget overrun produces.
fn exhausted(name: &str, remaining: u64) -> Status {
    Status::resource_exhausted(format!(
        "inflating {name:?} would take this call past its decompressed-size cap; \
         {remaining} bytes of budget were left. Raise max_uncompressed_mib if the book is \
         genuinely this large."
    ))
}

/// Reject an entry that cannot fit in what is left of the budget.
fn check_budget(size: u64, remaining: u64, name: &str) -> Result<(), Status> {
    if size > remaining {
        return Err(exhausted(name, remaining));
    }
    Ok(())
}

/// Reject an entry that inflates further than the ratio allows.
///
/// Skipped entirely below [`Effective::compression_ratio_floor_bytes`]: a
/// small file with a huge ratio is a well-compressed small file, and applying
/// the rule to it would reject ordinary books.
fn check_ratio(inflated: u64, stored: u64, limits: &Effective, name: &str) -> Result<(), Status> {
    if inflated < limits.compression_ratio_floor_bytes || stored == 0 {
        return Ok(());
    }
    let ratio = inflated / stored;
    if ratio > u64::from(limits.max_compression_ratio) {
        return Err(Status::resource_exhausted(format!(
            "entry {name:?} inflates {ratio}x, over the {}x limit ({stored} stored bytes \
             becoming {inflated}); this is the shape of a decompression bomb",
            limits.max_compression_ratio
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ratio_rule_ignores_small_entries() {
        let limits = Effective::default();
        // 4 KiB from 8 bytes is 512x and completely harmless.
        assert!(check_ratio(4096, 8, &limits, "content.opf").is_ok());
    }

    #[test]
    fn the_ratio_rule_catches_a_large_amplification() {
        let limits = Effective::default();
        let status = check_ratio(64 * 1024 * 1024, 1024, &limits, "bomb.xhtml")
            .expect_err("64 MiB from 1 KiB is a bomb");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn a_stored_entry_never_trips_the_ratio_rule() {
        let limits = Effective::default();
        assert!(check_ratio(64 * 1024 * 1024, 64 * 1024 * 1024, &limits, "big.png").is_ok());
    }

    #[test]
    fn opening_empty_bytes_is_a_caller_error() {
        let status = open(&[], &Effective::default()).expect_err("no archive in zero bytes");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn opening_junk_is_a_caller_error() {
        let status =
            open(b"this is not a zip file at all", &Effective::default()).expect_err("not a zip");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
