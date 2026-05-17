//! Rotating header slogans.
//!
//! Edit `slogans.txt` to change them — one slogan per line, blank lines and
//! `#` comments ignored. The list is embedded at compile time and parsed
//! once; a fresh slogan is picked at random on every full-page render (see
//! the `slogan()` helper in `ui.rs` and its use in `base.html`).

use rand::seq::SliceRandom;
use std::sync::OnceLock;

/// Fallback used only if `slogans.txt` ends up empty.
const FALLBACK: &str = "Your CI is too good to be locked in a forge.";

/// Slogan pool, parsed once from the embedded `slogans.txt`.
fn pool() -> &'static [&'static str] {
    static POOL: OnceLock<Vec<&'static str>> = OnceLock::new();
    POOL.get_or_init(|| {
        include_str!("slogans.txt")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    })
}

/// A random slogan, re-picked on every call.
pub fn pick() -> &'static str {
    pool()
        .choose(&mut rand::thread_rng())
        .copied()
        .unwrap_or(FALLBACK)
}
