// SPDX-License-Identifier: Apache-2.0

//! Media overlays: the SMIL documents that align a book's text with a
//! recording of someone reading it.
//!
//! # Why this is worth parsing here
//!
//! An EPUB 3 media overlay is a small SMIL document of the shape
//!
//! ```text
//! <par id="p1">
//!   <text src="chap1.xhtml#s1"/>
//!   <audio src="../audio/ch1.mp3" clipBegin="0:00:00.000" clipEnd="0:00:12.500"/>
//! </par>
//! ```
//!
//! and that is the whole of it: a flat or shallowly nested list of `<par>`
//! elements, each pairing one fragment of text with one span of audio. The
//! alignment was authored by hand and shipped in the book, so reading it needs
//! no speech recognition and no model; the only thing standing between the
//! archive and a cue-level timeline is one XML parse of a file measured in
//! kilobytes.
//!
//! # Clock values
//!
//! SMIL clock values come in three spellings and books use all of them:
//! `0:00:12.500` (full), `12.5s` (a timecount with a unit), and a bare `12.5`
//! (seconds). [`parse_clock`] accepts every form the specification defines and
//! reports `None` for anything else rather than guessing a number that would
//! then be indistinguishable from a real one.
//!
//! # Failure policy
//!
//! The same as navigation: nothing here fails a call. A SMIL document that
//! does not parse, or whose cues point outside the archive, yields the cues it
//! managed and the caller warns.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::href::{self, Target};

/// Largest number of cues read from one overlay.
///
/// A cue per sentence over a chapter runs to hundreds; tens of thousands is
/// not a chapter. The cues end up in one response message, so the bound is
/// what keeps a crafted overlay from becoming the payload.
const MAX_CUES: usize = 20_000;

/// One `<par>`: a span of narration and the text it reads aloud.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cue {
    /// Archive path and fragment of the narrated text, resolved from `<text
    /// src>`. Empty when the reference was unusable.
    pub text_href: String,
    /// Archive path of the audio resource, resolved from `<audio src>`. Empty
    /// when the `<par>` carried no audio, which is legal and means the text is
    /// shown without narration.
    pub audio_href: String,
    /// Offset into `audio_href` where the cue starts, in seconds.
    pub start_time: f64,
    /// Offset into `audio_href` where the cue ends, in seconds. Zero when the
    /// overlay declared no `clipEnd`, which means "to the end of the file".
    pub end_time: f64,
    /// The `<par>` element's `id`, empty when it had none.
    pub identifier: String,
}

/// One SMIL document's worth of cues.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlay {
    /// Archive path of the SMIL document.
    pub source_href: String,
    /// Every cue, in document order.
    pub cues: Vec<Cue>,
}

impl Overlay {
    /// Whether the parse found nothing worth emitting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// How long the narration runs, in milliseconds.
    ///
    /// The end of the last cue that declares one, not the sum of the cue
    /// durations: cues are contiguous spans of one recording, so summing them
    /// would double-count nothing and under-count every gap.
    #[must_use]
    pub fn duration_ms(&self) -> f64 {
        self.cues.iter().map(|cue| cue.end_time).fold(0.0, f64::max) * 1000.0
    }
}

/// Parse a SMIL clock value into seconds.
///
/// The three forms the SMIL specification defines, all of which appear in real
/// books:
///
/// - **Full**: `[hours:]minutes:seconds[.fraction]`, as `0:00:12.5` or
///   `03:20.5`.
/// - **Timecount**: a number with a unit, as `12.5s`, `200ms`, `3min`, `1h`.
/// - **Bare**: a number, which the specification reads as seconds.
///
/// Returns `None` for anything else. A clock value that cannot be read is not
/// worth guessing at: a wrong offset is indistinguishable from a right one
/// once it is on the wire, while a missing cue is visibly missing.
#[must_use]
pub fn parse_clock(raw: &str) -> Option<f64> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }

    if value.contains(':') {
        let mut seconds = 0.0f64;
        let mut parts = 0usize;
        for part in value.split(':') {
            let part: f64 = part.trim().parse().ok()?;
            if part < 0.0 {
                return None;
            }
            seconds = seconds * 60.0 + part;
            parts += 1;
        }
        // `h:m:s` and `m:s` are the only shapes; four fields is not a clock.
        return (2..=3).contains(&parts).then_some(seconds);
    }

    // Longest unit first, so `ms` is never read as `m` with a trailing `s`.
    for (unit, scale) in [("ms", 0.001), ("min", 60.0), ("h", 3600.0), ("s", 1.0)] {
        if let Some(number) = value.strip_suffix(unit) {
            let number: f64 = number.trim().parse().ok()?;
            return (number >= 0.0).then_some(number * scale);
        }
    }

    let number: f64 = value.parse().ok()?;
    (number >= 0.0 && number.is_finite()).then_some(number)
}

/// Read one attribute by local name, resolving the predefined entities.
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

/// Resolve a SMIL `src`, keeping any fragment.
///
/// The fragment on a `<text src>` is the whole point: it names the sentence
/// the cue highlights. An unusable reference yields an empty string rather
/// than dropping the cue, since the timing is still true even when the target
/// cannot be addressed.
fn resolve_src(base_dir: &str, raw: &str) -> String {
    let (path, fragment) = match raw.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (raw, None),
    };
    if path.is_empty() {
        return String::new();
    }
    let Ok(Target::Entry(resolved)) = href::resolve(base_dir, path) else {
        return String::new();
    };
    match fragment {
        Some(fragment) if !fragment.is_empty() => format!("{resolved}#{fragment}"),
        _ => resolved,
    }
}

/// Parse a SMIL media overlay.
///
/// `source_href` is the document's own archive path, used to resolve its
/// relative `src` references and to report where the cues came from.
#[must_use]
pub fn parse_overlay(bytes: &[u8], source_href: &str) -> Overlay {
    let base_dir = href::parent_dir(source_href).to_owned();
    let mut reader = Reader::from_reader(bytes);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.expand_empty_elements = false;

    let mut buf = Vec::new();
    let mut overlay = Overlay {
        source_href: source_href.to_owned(),
        cues: Vec::new(),
    };
    let mut current: Option<Cue> = None;

    // A malformed overlay costs the cues past the break and nothing else.
    while let Ok(event) = reader.read_event_into(&mut buf) {
        match event {
            // `<text>` and `<audio>` are empty elements and `<par>` is not, so
            // both start forms have to reach the same arms.
            Event::Start(start) | Event::Empty(start) => {
                match start.local_name().as_ref() {
                    "par" => {
                        // A `<par>` inside a `<par>` is not a shape the format
                        // defines; finish the outer one rather than lose it.
                        if let Some(cue) = current.take() {
                            push(&mut overlay.cues, cue);
                        }
                        current = Some(Cue {
                            identifier: attribute(&start, "id"),
                            ..Cue::default()
                        });
                    }
                    "text" => {
                        if let Some(cue) = current.as_mut() {
                            cue.text_href = resolve_src(&base_dir, &attribute(&start, "src"));
                        }
                    }
                    "audio" => {
                        if let Some(cue) = current.as_mut() {
                            cue.audio_href = resolve_src(&base_dir, &attribute(&start, "src"));
                            // An absent or unreadable clip bound means "from
                            // the start" and "to the end", which is what the
                            // zero default already says.
                            cue.start_time =
                                parse_clock(&attribute(&start, "clipBegin")).unwrap_or(0.0);
                            cue.end_time =
                                parse_clock(&attribute(&start, "clipEnd")).unwrap_or(0.0);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(end) => {
                if end.local_name().as_ref() == "par"
                    && let Some(cue) = current.take()
                {
                    push(&mut overlay.cues, cue);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        if overlay.cues.len() >= MAX_CUES {
            break;
        }
        buf.clear();
    }

    if let Some(cue) = current.take() {
        push(&mut overlay.cues, cue);
    }
    overlay
}

/// Keep a cue if it says anything.
///
/// A `<par>` with no target and no timing is an empty element wearing a cue's
/// name. A `<par>` whose references escaped the archive still carries a true
/// timing, and dropping it would understate how much narration the book holds,
/// so a non-zero `clipEnd` is enough on its own.
fn push(cues: &mut Vec<Cue>, cue: Cue) {
    if cue.text_href.is_empty() && cue.audio_href.is_empty() && cue.end_time == 0.0 {
        return;
    }
    cues.push(cue);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMIL: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<smil xmlns="http://www.w3.org/ns/SMIL" xmlns:epub="http://www.idpf.org/2007/ops" version="3.0">
  <body>
    <seq id="s1" epub:textref="chap1.xhtml">
      <par id="p1">
        <text src="chap1.xhtml#sentence1"/>
        <audio src="../audio/ch1.mp3" clipBegin="0:00:00.000" clipEnd="0:00:12.500"/>
      </par>
      <par id="p2">
        <text src="chap1.xhtml#sentence2"/>
        <audio src="../audio/ch1.mp3" clipBegin="12.5s" clipEnd="20s"/>
      </par>
    </seq>
  </body>
</smil>"#;

    #[test]
    fn an_overlay_yields_one_cue_per_par_in_document_order() {
        let overlay = parse_overlay(SMIL, "OEBPS/overlays/ch1.smil");
        assert_eq!(overlay.source_href, "OEBPS/overlays/ch1.smil");
        assert_eq!(overlay.cues.len(), 2);

        let first = &overlay.cues[0];
        assert_eq!(first.identifier, "p1");
        assert_eq!(
            first.text_href, "OEBPS/overlays/chap1.xhtml#sentence1",
            "src is relative to the SMIL document, and the fragment is the cue"
        );
        assert_eq!(
            first.audio_href, "OEBPS/audio/ch1.mp3",
            "`..` resolves against the overlay's own directory"
        );
        assert!((first.start_time - 0.0).abs() < f64::EPSILON);
        assert!((first.end_time - 12.5).abs() < f64::EPSILON);

        let second = &overlay.cues[1];
        assert!((second.start_time - 12.5).abs() < f64::EPSILON);
        assert!((second.end_time - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_narration_length_is_where_the_last_cue_ends() {
        let overlay = parse_overlay(SMIL, "OEBPS/overlays/ch1.smil");
        assert!((overlay.duration_ms() - 20_000.0).abs() < f64::EPSILON);
        assert!((Overlay::default().duration_ms() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn every_clock_spelling_the_specification_defines_is_read() {
        // Full clock values.
        assert_eq!(parse_clock("0:00:12.5"), Some(12.5));
        assert_eq!(parse_clock("00:00:12.500"), Some(12.5));
        assert_eq!(parse_clock("1:02:03"), Some(3723.0));
        assert_eq!(parse_clock("03:20.5"), Some(200.5));
        // Timecounts.
        assert_eq!(parse_clock("12.5s"), Some(12.5));
        assert_eq!(parse_clock("200ms"), Some(0.2));
        assert_eq!(parse_clock("3min"), Some(180.0));
        assert_eq!(parse_clock("1h"), Some(3600.0));
        // Bare numbers are seconds.
        assert_eq!(parse_clock("12.5"), Some(12.5));
        assert_eq!(parse_clock("  7 "), Some(7.0));
    }

    #[test]
    fn a_clock_value_that_cannot_be_read_is_none_rather_than_a_guess() {
        assert_eq!(parse_clock(""), None);
        assert_eq!(parse_clock("soon"), None);
        assert_eq!(parse_clock("-5"), None);
        assert_eq!(parse_clock("1:2:3:4"), None, "four fields is not a clock");
        assert_eq!(parse_clock("12:xx"), None);
        assert_eq!(parse_clock("inf"), None);
        // `ms` must not be read as `m` plus a stray `s`.
        assert_eq!(parse_clock("60ms"), Some(0.06));
    }

    #[test]
    fn a_par_with_no_timings_still_carries_its_alignment() {
        let no_audio = br#"<smil><body><par id="p1">
            <text src="chap1.xhtml#s1"/>
        </par></body></smil>"#;
        let overlay = parse_overlay(no_audio, "OEBPS/ch1.smil");
        assert_eq!(overlay.cues.len(), 1);
        assert_eq!(overlay.cues[0].text_href, "OEBPS/chap1.xhtml#s1");
        assert_eq!(overlay.cues[0].audio_href, "");
    }

    #[test]
    fn a_par_that_says_nothing_is_not_a_cue() {
        let hollow = br#"<smil><body><par id="p1"/><par id="p2">
            <text src="chap1.xhtml#s1"/></par></body></smil>"#;
        let overlay = parse_overlay(hollow, "OEBPS/ch1.smil");
        assert_eq!(overlay.cues.len(), 1);
        assert_eq!(overlay.cues[0].identifier, "p2");
    }

    #[test]
    fn a_broken_overlay_costs_the_cues_it_could_not_reach_and_no_more() {
        let truncated = br#"<smil><body><par id="p1">
            <text src="chap1.xhtml#s1"/>
            <audio src="a.mp3" clipBegin="0s" clipEnd="1s"/>
          </par>
          <par id="p2"><text src="chap1.xhtml"#;
        let overlay = parse_overlay(truncated, "OEBPS/ch1.smil");
        assert_eq!(overlay.cues.len(), 1);
        assert_eq!(overlay.cues[0].identifier, "p1");

        assert!(parse_overlay(b"not xml", "OEBPS/ch1.smil").is_empty());
    }

    #[test]
    fn a_cue_pointing_outside_the_archive_keeps_its_timing_and_loses_its_target() {
        let hostile = br#"<smil><body><par id="p1">
            <text src="../../etc/passwd#x"/>
            <audio src="https://example.com/a.mp3" clipBegin="0s" clipEnd="2s"/>
        </par></body></smil>"#;
        let overlay = parse_overlay(hostile, "OEBPS/ch1.smil");
        assert_eq!(overlay.cues.len(), 1, "the timing is still true");
        assert_eq!(overlay.cues[0].text_href, "");
        assert_eq!(overlay.cues[0].audio_href, "");
        assert!((overlay.cues[0].end_time - 2.0).abs() < f64::EPSILON);
    }
}
