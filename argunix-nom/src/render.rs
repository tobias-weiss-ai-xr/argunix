//! Rendering [`NomEvent`]s back to the prefixed plain text stored in a
//! finished job's `.log.zst`.

use std::borrow::Cow;

use crate::event::NomEvent;

/// One line of stored-log text for an event, or `None` for events that
/// only drive the live view (`ActStart` / `ActStop` / `Progress`).
///
/// A `Line` gets a `name> ` prefix — the `nix-output-monitor` style —
/// so a reader of the flat stored log can tell which derivation each
/// line came from even when builds ran interleaved in parallel. ANSI
/// escape sequences are stripped from `Line.text` here: the stored log
/// is a plain-text artifact (grep, cat, download), so a build tool's
/// colour codes (`\x1b[…m`, …) would just be visual noise. The live
/// view receives the raw `text` over SSE and renders SGR colours
/// itself; this stripping only affects what hits disk.
///
/// `Message` (nix's own diagnostics) and `Raw` pass through verbatim —
/// argunix synthesises both and never embeds escape sequences.
pub fn render_storage_line(ev: &NomEvent) -> Option<String> {
    match ev {
        NomEvent::Line { label, text, .. } => Some(format!("{label}> {}", strip_ansi(text))),
        NomEvent::Message { text, .. } => Some(text.clone()),
        NomEvent::Raw { text } => Some(text.clone()),
        NomEvent::ActStart { .. } | NomEvent::ActStop { .. } | NomEvent::Progress { .. } => None,
    }
}

/// Drop ANSI escape sequences from `s`. Handles CSI (`\x1b[…<final>`),
/// OSC (`\x1b]…BEL` / `…\x1b\\`), and single-character escapes
/// (`\x1b<one byte>`) — enough to clean up colour codes from pytest /
/// cargo / gcc and the cursor-movement / line-clear sequences they
/// sometimes emit. Returns the input unchanged when no escape is
/// present (`Cow::Borrowed`), so the common no-ANSI case allocates
/// nothing.
///
/// UTF-8 safe: we never copy a partial multi-byte sequence. ESC
/// (`0x1B`) and every byte we walk while *inside* an escape sequence
/// (CSI / OSC parameter and final bytes, OSC terminators `BEL` /
/// `ESC \`) are all `< 0x80`, which can never appear inside a UTF-8
/// continuation. Everything between escapes is copied as a contiguous
/// byte range straight out of the input, preserving any multi-byte
/// codepoints verbatim.
fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    // `kept_start` is the start of the current run of non-escape
    // bytes; we flush it as a single `push_str` whenever we hit ESC.
    let mut kept_start = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if kept_start < i {
            // The kept run is a substring of `s` we never split inside
            // a multi-byte sequence (see the SAFETY comment on the
            // function), so the from_utf8 always succeeds.
            out.push_str(std::str::from_utf8(&bytes[kept_start..i]).expect("kept run is utf8"));
        }
        // Trailing lone ESC: drop and stop.
        if i + 1 >= bytes.len() {
            kept_start = bytes.len();
            break;
        }
        match bytes[i + 1] {
            // CSI: `ESC [ <params...> <final byte in 0x40..=0x7E>`.
            b'[' => {
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&c) {
                        break;
                    }
                }
            }
            // OSC: `ESC ] ... BEL` or `ESC ] ... ESC \`.
            b']' => {
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == 0x07 {
                        i += 1;
                        break;
                    }
                    if c == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // Anything else: a one-byte escape (`ESC c`); drop both.
            _ => {
                i += 2;
            }
        }
        kept_start = i;
    }
    if kept_start < bytes.len() {
        out.push_str(std::str::from_utf8(&bytes[kept_start..]).expect("kept run is utf8"));
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ActivityKind;

    #[test]
    fn line_gets_a_derivation_prefix() {
        let ev = NomEvent::Line {
            activity: 7,
            label: "hello-2.12.1".into(),
            text: "compiling".into(),
        };
        assert_eq!(
            render_storage_line(&ev).as_deref(),
            Some("hello-2.12.1> compiling"),
        );
    }

    #[test]
    fn raw_and_message_pass_through() {
        assert_eq!(
            render_storage_line(&NomEvent::Raw {
                text: "plain".into()
            })
            .as_deref(),
            Some("plain"),
        );
        assert_eq!(
            render_storage_line(&NomEvent::Message {
                level: 0,
                text: "error: oops".into(),
            })
            .as_deref(),
            Some("error: oops"),
        );
    }

    #[test]
    fn activity_events_render_nothing() {
        assert!(render_storage_line(&NomEvent::ActStop { id: 1 }).is_none());
        assert!(
            render_storage_line(&NomEvent::Progress {
                done: 1,
                expected: 2,
                running: 1,
                failed: 0,
            })
            .is_none()
        );
        assert!(
            render_storage_line(&NomEvent::ActStart {
                id: 1,
                parent: 0,
                act: ActivityKind::Build,
                label: "x".into(),
            })
            .is_none()
        );
    }

    #[test]
    fn strip_ansi_drops_csi_colour_codes() {
        let line = NomEvent::Line {
            activity: 1,
            label: "pytest".into(),
            text: "\x1b[32m.\x1b[0m\x1b[32m.\x1b[0m".into(),
        };
        // Same `pytest> ..` regardless of how many SGR codes were
        // wrapped around the dots.
        assert_eq!(render_storage_line(&line).as_deref(), Some("pytest> .."),);
    }

    #[test]
    fn strip_ansi_preserves_non_escape_text_in_order() {
        let line = NomEvent::Line {
            activity: 1,
            label: "build".into(),
            text: "before \x1b[31mred\x1b[0m after".into(),
        };
        assert_eq!(
            render_storage_line(&line).as_deref(),
            Some("build> before red after"),
        );
    }

    #[test]
    fn strip_ansi_drops_cursor_and_line_clear_sequences() {
        // Pytest's "carriage-return + clear-to-end" trick:
        // `\x1b[K` (EL — erase in line) and `\x1b[2J` (ED — erase
        // display) and `\x1b[H` (CUP — cursor home) all need to go.
        let line = NomEvent::Line {
            activity: 1,
            label: "p".into(),
            text: "row1\x1b[Krow2\x1b[2Jrow3\x1b[Hrow4".into(),
        };
        assert_eq!(
            render_storage_line(&line).as_deref(),
            Some("p> row1row2row3row4"),
        );
    }

    #[test]
    fn strip_ansi_drops_osc_titles() {
        // OSC: `ESC ] 0 ; …title… BEL` (`\x07`) — used by some tools.
        let line = NomEvent::Line {
            activity: 1,
            label: "p".into(),
            text: "before\x1b]0;a title\x07after".into(),
        };
        assert_eq!(
            render_storage_line(&line).as_deref(),
            Some("p> beforeafter"),
        );
    }

    #[test]
    fn strip_ansi_drops_incomplete_trailing_escape() {
        // A `\x1b[` with no terminator at end of string must not
        // propagate into the stored log. (Argues that we don't carry
        // partial state across lines either: nix emits whole lines.)
        let line = NomEvent::Line {
            activity: 1,
            label: "p".into(),
            text: "tail\x1b[".into(),
        };
        assert_eq!(render_storage_line(&line).as_deref(), Some("p> tail"));
    }

    #[test]
    fn strip_ansi_preserves_utf8_multi_byte_runs() {
        // Multi-byte UTF-8 (`é`, `→`, `🦀`) wrapped in SGR codes must
        // survive intact — we copy non-escape runs as byte ranges, so
        // multi-byte sequences cross the strip untouched.
        let line = NomEvent::Line {
            activity: 1,
            label: "build".into(),
            text: "\x1b[31mh\u{00e9}llo \u{2192} \u{1f980}\x1b[0m end".into(),
        };
        assert_eq!(
            render_storage_line(&line).as_deref(),
            Some("build> h\u{00e9}llo \u{2192} \u{1f980} end"),
        );
    }

    #[test]
    fn strip_ansi_passes_through_when_no_escape_present() {
        // Happy path: no allocation beyond the existing `format!`.
        let line = NomEvent::Line {
            activity: 1,
            label: "p".into(),
            text: "clean text".into(),
        };
        assert_eq!(render_storage_line(&line).as_deref(), Some("p> clean text"),);
        // Direct call: should return Borrowed on the no-escape path.
        assert!(matches!(strip_ansi("clean"), Cow::Borrowed(_)));
        assert!(matches!(strip_ansi("with\x1b[0m"), Cow::Owned(_)));
    }
}
