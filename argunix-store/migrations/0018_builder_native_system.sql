-- The builder's native `system` (from `nix show-config`), distinct from the
-- emulated `extra-platforms` that also appear in `systems`. The scheduler
-- prefers native builders absolutely: an emulated (binfmt) builder is only
-- considered for a `<system>` when no native builder for it is connected.
--
-- Existing rows predate the distinction; default to '' ("unknown native").
-- Such a builder is treated as non-native for every system until its next
-- reconnect refreshes the snapshot from a native-system-aware agent.
ALTER TABLE builders ADD COLUMN native_system TEXT NOT NULL DEFAULT '';
