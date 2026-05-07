use argunix_store::BuilderRecord;

/// Per-connection auth bookkeeping.
///
/// On every accepted connection the handler starts in `Unauthenticated`.
/// Whichever auth method succeeds (token via password, or pubkey against
/// the `builders` row) flips it to the corresponding state. PR #5 reads
/// this state when the builder opens its `control` channel:
///
/// - `FreshEnrollment { pubkey }` ⇒ expect a `hello` message; on receipt,
///   `BuilderStore::upsert(NewBuilder { name, pubkey, capabilities })`.
/// - `Established(record)` ⇒ verify the `hello.name` matches `record.name`;
///   refresh capabilities via the same upsert (idempotent on `name`).
#[derive(Debug, Clone)]
pub enum AuthState {
    Unauthenticated,
    /// Token auth succeeded. The pubkey the builder presented during the
    /// SSH handshake is captured here so we can persist it in the
    /// builders row when `hello` arrives.
    FreshEnrollment {
        pubkey: argunix_domain::BuilderPubkey,
    },
    /// Pubkey matched an active row in `builders`. We carry the row so
    /// later channels can read the cached capabilities and the row id.
    Established(BuilderRecord),
}

impl Default for AuthState {
    fn default() -> Self {
        Self::Unauthenticated
    }
}
