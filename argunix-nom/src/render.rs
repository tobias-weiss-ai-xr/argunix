//! Rendering [`NomEvent`]s back to the prefixed plain text stored in a
//! finished job's `.log.zst`.

use crate::event::NomEvent;

/// One line of stored-log text for an event, or `None` for events that
/// only drive the live view (`ActStart` / `ActStop` / `Progress`).
///
/// A `Line` gets a `name> ` prefix — the `nix-output-monitor` style —
/// so a reader of the flat stored log can tell which derivation each
/// line came from even when builds ran interleaved in parallel.
/// `Message` (nix's own diagnostics) and `Raw` pass through verbatim.
pub fn render_storage_line(ev: &NomEvent) -> Option<String> {
    match ev {
        NomEvent::Line { label, text, .. } => Some(format!("{label}> {text}")),
        NomEvent::Message { text, .. } => Some(text.clone()),
        NomEvent::Raw { text } => Some(text.clone()),
        NomEvent::ActStart { .. } | NomEvent::ActStop { .. } | NomEvent::Progress { .. } => None,
    }
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
}
