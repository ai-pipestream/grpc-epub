// SPDX-License-Identifier: Apache-2.0

//! Opening a ZIP held in memory and inflating entries under a budget.
//!
//! This is where the zip-bomb policy from `docs/design.md` lives. Three rules,
//! each covering a hole the others leave:
//!
//! 1. **Entry count**, checked before anything is inflated, and before the
//!    central directory is even parsed: the `zip` crate builds a record for
//!    every entry the directory lists before it says how many there are, at
//!    a dozen times the bytes each one takes in the archive. Cheap, and it
//!    stops the archive whose whole payload is a million zero-byte names.
//! 2. **Total inflated bytes**, a running budget across every entry the call
//!    extracts. This is the heap ceiling.
//! 3. **Per-entry ratio**, inflated over stored. The total alone lets an
//!    attacker sit just under it and still buy a thousandfold amplification
//!    with a small upload; the ratio prices that back.
//!
//! Rules 2 and 3 are enforced twice: once against the sizes the central
//! directory declares, which is free and rejects the honest bomb before a byte
//! is inflated, and again against what is coming out of the decompressor, on
//! every chunk as it arrives, which is what catches a header that lies. Only
//! the second is load-bearing; the first exists so the common case costs
//! nothing. Because the second runs while inflating rather than once the entry
//! is whole, an entry whose header understates it is stopped as soon as it
//! passes the ratio, not after it has filled the rest of the budget.
//!
//! The stored size a ratio is taken against is the declared one, but never
//! more than the bytes in front of the central directory: no entry can be
//! stored in more of the archive than there is, and believing a larger claim
//! would let any entry pass the ratio rule.
//!
//! Nothing here touches the filesystem. `zip`'s `extract` family is never
//! called, and the crate is built without the features that would let it
//! decode anything but store and deflate.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use tonic::Status;
use zip::result::ZipError;
use zip::{CompressionMethod, ZipArchive};

use crate::limits::Effective;

/// An archive over borrowed bytes. Nothing is copied to open it.
pub type MemoryArchive<'a> = ZipArchive<Cursor<&'a [u8]>>;

/// Inflated bytes read in one pass before the budget is re-checked.
const READ_CHUNK: usize = 64 * 1024;

/// The signature every central directory record starts with (APPNOTE 4.3.12).
const CENTRAL_RECORD_SIGNATURE: &[u8; 4] = b"PK\x01\x02";

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

    // `ZipArchive::new` parses every record the central directory declares,
    // and allocates for each, before the count below can be checked: a
    // 256 MiB upload of forged ZIP64 records costs it gigabytes. Each record
    // it parses starts with this signature, so the number of signatures in
    // the upload bounds what it will build, and counting them is one pass
    // over bytes already in memory. A conforming archive holds one per entry,
    // plus whatever its stored payloads happen to contain.
    let records = bytes
        .windows(CENTRAL_RECORD_SIGNATURE.len())
        .filter(|window| window == CENTRAL_RECORD_SIGNATURE)
        .count();
    if records > limits.max_entries as usize {
        return Err(Status::resource_exhausted(format!(
            "the archive holds {records} central directory records, over the {} entries allowed",
            limits.max_entries
        )));
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
    /// Stored size as declared, capped at the offset of the central
    /// directory: an entry cannot be stored in more bytes than precede it.
    pub compressed_size: u64,
}

/// The central directory, read and checked.
#[derive(Debug, Default)]
pub struct Scan {
    /// Every file entry with a usable name, in central-directory order.
    pub entries: Vec<EntryInfo>,
    /// Entries left out because their names cannot be archive paths: the name
    /// as stored, and why.
    pub unusable: Vec<(String, crate::href::PathError)>,
}

/// Read the central directory and check every entry name and encoding.
///
/// This runs before any event is emitted, so an entry this build cannot
/// decode fails the call cleanly instead of truncating a stream that has
/// already started. Directory entries are dropped: they carry no content and
/// their names would collide with real files after normalization.
///
/// An entry whose name cannot be an archive path (one that escapes the root,
/// is absolute, or holds a NUL) is left out and reported, not fatal. Nothing
/// can name it, so nothing in it is ever read or sent, which is all the
/// traversal policy needs; a stray `__MACOSX/../x` or `/mimetype` from a
/// careless zip tool does not have to cost the reader the whole book. If the
/// book needed that entry, the spine or the container check says so.
///
/// Two entries whose names normalize to the same path are fatal. The `zip`
/// crate keys entries by their raw names, so `OEBPS/ch1.xhtml` and
/// `OEBPS\ch1.xhtml` are two files to it and one path here, and whichever
/// the lookup kept would be the one served: a second `META-INF/encryption.xml`
/// could stand in for the one that declares DRM, or a chapter could carry
/// other bytes than a conforming reader shows. There is no honest reading of
/// such an archive, so there is no choice to make.
///
/// # Errors
///
/// `UNIMPLEMENTED` for an encrypted entry or a compression method outside
/// store and deflate, `INVALID_ARGUMENT` for two entries naming one path.
pub fn scan(archive: &mut MemoryArchive<'_>) -> Result<Scan, Status> {
    let mut scan = Scan {
        entries: Vec::with_capacity(archive.len()),
        unusable: Vec::new(),
    };
    let stored_ceiling = archive.central_directory_start();
    // Normalized path to the raw name that claimed it first.
    let mut claimed: HashMap<String, String> = HashMap::with_capacity(archive.len());
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
        let compressed_size = entry.compressed_size().min(stored_ceiling);
        drop(entry);

        let normalized = match crate::href::check_entry_name(&name) {
            Ok(Some(normalized)) => normalized,
            Ok(None) => continue, // A directory entry.
            Err(error) => {
                scan.unusable.push((name, error));
                continue;
            }
        };

        if let Some(first) = claimed.get(&normalized) {
            return Err(Status::invalid_argument(format!(
                "archive entries {first:?} and {name:?} both name {normalized:?}; an archive \
                 that holds two files at one path has no single reading"
            )));
        }
        claimed.insert(normalized.clone(), name);

        scan.entries.push(EntryInfo {
            index,
            name: normalized,
            declared_size,
            compressed_size,
        });
    }
    Ok(scan)
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
        // Both rules are checked against what the decompressor has produced
        // so far, and *before* the copy, so the allocation never overshoots
        // either of them even by one chunk. These are the checks that catch a
        // central directory understating the entry.
        let inflated = out.len() + read;
        if inflated > ceiling {
            return Err(exhausted(&entry.name, budget.remaining));
        }
        check_ratio(inflated as u64, entry.compressed_size, limits, &entry.name)?;
        out.extend_from_slice(&chunk[..read]);
    }
    drop(file);

    let actual = out.len() as u64;
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

    /// A ZIP64 archive whose central directory lists `count` empty entries,
    /// every one pointing at the same local header: about 50 bytes of upload
    /// per entry a reader has to build a record for.
    fn forged_zip64(count: u32) -> Vec<u8> {
        let mut out = Vec::new();
        // The one local header: an empty stored file named "a".
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.push(b'a');
        let directory = out.len() as u64;
        for index in 0..count {
            out.extend_from_slice(CENTRAL_RECORD_SIGNATURE);
            out.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&[0; 16]);
            out.extend_from_slice(&4u16.to_le_bytes()); // a distinct 4-byte name
            out.extend_from_slice(&[0; 12]);
            out.extend_from_slice(&0u32.to_le_bytes()); // local header at 0
            out.extend_from_slice(&index.to_le_bytes());
        }
        let size = out.len() as u64 - directory;
        let zip64_end = out.len() as u64;
        out.extend_from_slice(b"PK\x06\x06");
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&[45, 0, 45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&u64::from(count).to_le_bytes());
        out.extend_from_slice(&u64::from(count).to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&directory.to_le_bytes());
        out.extend_from_slice(b"PK\x06\x07");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&zip64_end.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        out.extend_from_slice(&[0xff; 8]);
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// The entry count is enforced before the central directory is parsed.
    ///
    /// Parsing it is what costs: the `zip` crate builds a record for every
    /// entry the directory lists before it reports how many there are, at a
    /// dozen times the bytes each takes in the archive, so a 256 MiB upload
    /// of these cost it gigabytes before the count was ever compared.
    #[test]
    fn a_forged_directory_is_refused_before_it_is_parsed() {
        let forged = forged_zip64(100_000);
        assert!(
            ZipArchive::new(Cursor::new(forged.as_slice())).is_ok(),
            "the forgery is a readable archive, so only the count stops it"
        );

        let status = open(&forged, &Effective::default()).expect_err("over the entry cap");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert!(
            status.message().contains("central directory records"),
            "refused by the signature count, before the crate parsed anything: {}",
            status.message()
        );
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
