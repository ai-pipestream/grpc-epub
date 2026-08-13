// SPDX-License-Identifier: Apache-2.0

//! Turning the strings an EPUB contains into archive paths that cannot point
//! outside the archive.
//!
//! Two kinds of string arrive here and both are attacker-controlled:
//!
//! - **ZIP entry names**, taken verbatim from the central directory.
//! - **OPF and container hrefs**, which are IRI references relative to the
//!   document that contains them, and therefore percent-encoded and full of
//!   `..`.
//!
//! Nothing in this service writes to disk, so `../../etc/passwd` cannot
//! overwrite anything here. It is still refused, for two reasons: the paths go
//! out on the wire as `Chapter.href` and `Resource.href`, where a client that
//! *does* write files would inherit the traversal, and an entry name that
//! escapes the archive root has no honest reading — a real EPUB never has one.

/// Longest path this module will accept, in bytes.
///
/// Bounds the work a crafted name can cause in normalization and keeps a
/// pathological name out of the response. No real EPUB comes close.
const MAX_PATH_BYTES: usize = 1024;

/// What an href turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A path inside the archive, normalized and root-relative, using `/`.
    Entry(String),
    /// An absolute URI: an EPUB 3 remote resource, or a `data:` payload.
    /// Nothing is fetched, ever; the caller records it and moves on.
    Remote(String),
}

/// Why a path was refused. Every variant maps to `INVALID_ARGUMENT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The path was empty once the fragment was removed.
    Empty,
    /// The path was longer than [`MAX_PATH_BYTES`].
    TooLong,
    /// The path began with `/`, so it is not relative to anything.
    Absolute,
    /// The path escaped the archive root through `..`.
    Traversal,
    /// The path contained a byte no archive path may contain: a NUL, or a
    /// backslash, which some extractors treat as a separator and some do not.
    IllegalByte(char),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("path is empty"),
            Self::TooLong => write!(f, "path is longer than {MAX_PATH_BYTES} bytes"),
            Self::Absolute => f.write_str("path is absolute; archive paths are root-relative"),
            Self::Traversal => f.write_str("path escapes the archive root"),
            Self::IllegalByte(c) => write!(f, "path contains an illegal character {c:?}"),
        }
    }
}

/// Decode `%XX` escapes.
///
/// OPF hrefs are IRI references, so a space is `%20` while the ZIP entry name
/// holds a real space. An invalid escape is left alone rather than rejected:
/// producers that never encoded anything also never escaped their literal
/// `%`, and a lookup fallback (see [`crate::extract`]) tries the raw form too.
#[must_use]
pub fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The directory part of an archive path, with no trailing slash.
///
/// `"OEBPS/content.opf"` is `"OEBPS"`; `"content.opf"` is `""`.
#[must_use]
pub fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

/// True when `href` is an absolute URI rather than a path inside the archive.
///
/// Deliberately conservative: a scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-"
/// / "." )` followed by `:`, and a bare Windows drive letter (`c:foo`) is one
/// character, so single-character schemes are treated as paths. Neither form
/// is ever opened; this only decides which error the caller gets.
fn is_absolute_uri(href: &str) -> bool {
    let Some(colon) = href.find(':') else {
        return false;
    };
    if colon < 2 {
        return false;
    }
    let scheme = &href[..colon];
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
}

/// Resolve an OPF or container href against the directory holding it.
///
/// `base_dir` is a normalized archive directory with no trailing slash, as
/// returned by [`parent_dir`]. The fragment is dropped: `chap1.xhtml#part2`
/// and `chap1.xhtml` name the same entry.
///
/// # Errors
///
/// Returns [`PathError`] when the href is empty, over-long, absolute, escapes
/// the archive root, or contains a NUL or a backslash.
pub fn resolve(base_dir: &str, href: &str) -> Result<Target, PathError> {
    if href.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if is_absolute_uri(href) {
        return Ok(Target::Remote(href.to_owned()));
    }

    // Drop the fragment before decoding: a `%23` inside a path is a literal
    // `#` in the entry name and must survive, while a real `#` separator must
    // not.
    let without_fragment = href.split('#').next().unwrap_or_default();
    let decoded = percent_decode(without_fragment);

    normalize(base_dir, &decoded).map(Target::Entry)
}

/// Normalize a decoded, relative path against `base_dir`.
///
/// # Errors
///
/// Returns [`PathError`] on the same conditions as [`resolve`].
pub fn normalize(base_dir: &str, path: &str) -> Result<String, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() + base_dir.len() + 1 > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }
    for c in path.chars() {
        if c == '\0' || c == '\\' {
            return Err(PathError::IllegalByte(c));
        }
    }

    let mut segments: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };

    for segment in path.split('/') {
        match segment {
            // `a//b` and `a/./b` both mean `a/b`. A trailing empty segment
            // (`a/`) names a directory and is dropped the same way; the result
            // then simply matches no file entry.
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(PathError::Traversal);
                }
            }
            other => segments.push(other),
        }
    }

    if segments.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(segments.join("/"))
}

/// Check a ZIP entry name and return it in normalized form.
///
/// Entry names are already root-relative by definition, so this is
/// [`normalize`] with an empty base plus the directory-entry case: an archive
/// may legitimately store `OEBPS/` as a zero-length directory entry, which
/// normalizes to `OEBPS` and would then collide with a file of that name.
/// Directory entries are not files and never carry content, so callers skip
/// them; this reports them rather than inventing a path.
///
/// # Errors
///
/// Returns [`PathError`] when the name is unusable or escapes the root.
pub fn check_entry_name(name: &str) -> Result<Option<String>, PathError> {
    if name.ends_with('/') {
        // A directory entry. Still checked for traversal, still not a file.
        normalize("", name)?;
        return Ok(None);
    }
    normalize("", name).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrefs_resolve_against_the_opf_directory() {
        assert_eq!(
            resolve("OEBPS", "text/chap1.xhtml"),
            Ok(Target::Entry("OEBPS/text/chap1.xhtml".to_owned()))
        );
        assert_eq!(
            resolve("OEBPS/text", "../images/cover.png"),
            Ok(Target::Entry("OEBPS/images/cover.png".to_owned()))
        );
        assert_eq!(
            resolve("", "content.opf"),
            Ok(Target::Entry("content.opf".to_owned()))
        );
    }

    #[test]
    fn fragments_and_percent_escapes_are_handled() {
        assert_eq!(
            resolve("OEBPS", "chap%201.xhtml#heading"),
            Ok(Target::Entry("OEBPS/chap 1.xhtml".to_owned()))
        );
        // A `%23` is a literal `#` in the entry name, not a fragment marker.
        assert_eq!(
            resolve("OEBPS", "odd%23name.xhtml"),
            Ok(Target::Entry("OEBPS/odd#name.xhtml".to_owned()))
        );
    }

    #[test]
    fn traversal_out_of_the_archive_is_refused() {
        assert_eq!(
            resolve("OEBPS", "../../etc/passwd"),
            Err(PathError::Traversal)
        );
        assert_eq!(resolve("", "../secret"), Err(PathError::Traversal));
        assert_eq!(check_entry_name("../evil.xhtml"), Err(PathError::Traversal));
        assert_eq!(
            check_entry_name("a/b/../../../evil.xhtml"),
            Err(PathError::Traversal)
        );
        // Percent-encoded traversal is decoded before the check, not after.
        assert_eq!(
            resolve("OEBPS", "%2e%2e/%2e%2e/etc/passwd"),
            Err(PathError::Traversal)
        );
    }

    #[test]
    fn absolute_and_illegal_paths_are_refused() {
        assert_eq!(resolve("OEBPS", "/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(check_entry_name("/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(
            check_entry_name("OEBPS\\..\\evil.xhtml"),
            Err(PathError::IllegalByte('\\'))
        );
        assert_eq!(
            check_entry_name("OEBPS/nul\0.xhtml"),
            Err(PathError::IllegalByte('\0'))
        );
    }

    #[test]
    fn absolute_uris_are_remote_not_paths() {
        assert_eq!(
            resolve("OEBPS", "https://example.com/x.png"),
            Ok(Target::Remote("https://example.com/x.png".to_owned()))
        );
        assert!(matches!(
            resolve("OEBPS", "data:image/png;base64,AAAA"),
            Ok(Target::Remote(_))
        ));
        // A single-letter prefix is a path, not a scheme.
        assert_eq!(
            resolve("", "c:weird.xhtml"),
            Ok(Target::Entry("c:weird.xhtml".to_owned()))
        );
    }

    #[test]
    fn directory_entries_are_reported_rather_than_named() {
        assert_eq!(check_entry_name("OEBPS/"), Ok(None));
        assert_eq!(
            check_entry_name("OEBPS/a.xhtml"),
            Ok(Some("OEBPS/a.xhtml".to_owned()))
        );
    }

    #[test]
    fn parent_dir_strips_the_last_segment() {
        assert_eq!(parent_dir("OEBPS/content.opf"), "OEBPS");
        assert_eq!(parent_dir("content.opf"), "");
        assert_eq!(parent_dir("a/b/c.xhtml"), "a/b");
    }
}
