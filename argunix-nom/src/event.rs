//! Wire types for [`crate::NomParser`] output — the structured build
//! events the daemon stores and streams to the web UI. Every variant
//! is JSON-serialisable (a `kind` tag) so the web layer can forward
//! them over SSE without re-parsing.

use serde::{Deserialize, Serialize};

/// What kind of work a nix activity represents — the subset of nix's
/// internal `ActivityType` argunix surfaces in the live view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// Realising a derivation (`actBuild`).
    Build,
    /// Substituting a path from a binary cache (`actSubstitute`).
    Substitute,
    /// A file transfer / download (`actFileTransfer`).
    Download,
    /// Copying a store path (`actCopyPath`).
    CopyPath,
}

/// One structured event distilled from nix's `internal-json` log
/// stream.
///
/// `Line` / `Raw` / `Message` carry text that renders into the stored
/// log (see [`crate::render_storage_line`]); `ActStart` / `ActStop` /
/// `Progress` drive the live "what's building" view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NomEvent {
    /// A line of build output, attributed to the activity that
    /// produced it; `label` is that activity's short derivation name.
    Line {
        activity: u64,
        label: String,
        text: String,
    },
    /// An activity (build / copy / download / substitute) started.
    ActStart {
        id: u64,
        parent: u64,
        act: ActivityKind,
        label: String,
    },
    /// An activity finished.
    ActStop { id: u64 },
    /// Aggregate build progress — nix's `actBuilds` counter.
    Progress {
        done: u64,
        expected: u64,
        running: u64,
        failed: u64,
    },
    /// A free-standing nix diagnostic (error / warning / notice) not
    /// tied to a single derivation.
    Message { level: u8, text: String },
    /// A line that was not `internal-json` — passed through verbatim.
    /// Covers an archived `nix-store --read-log` dump, a builder that
    /// somehow lacked the flag, and argunix's own injected notices.
    Raw { text: String },
}
