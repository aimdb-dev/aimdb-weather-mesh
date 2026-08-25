# Patches

Changes to the sibling aimdb checkout, produced while working through
`../REQUIREMENTS.md` one item at a time. Not applied to any aimdb branch — this
repository cannot push there.

**`ALL-core-changes.patch` is the one to apply.** The three topic patches below
are for reading: they were cut when each item was finished, so they overlap in
`handle.rs` and `error.rs` and do not stack cleanly.

| Patch | Covers | Review |
|---|---|---|
| `aimdb-sync-remove-busy-waits.patch` | CR-2, CR-3, CR-4 — the spin in `AimDbHandle::new`, the 10 ms poll in `detach_internal`, and the two `lock().unwrap()` sites that went with them | §1 |
| `aimdb-error-classification.patch` | CR-5 — `DbErrorKind`, `SyncError::kind()`, `#[non_exhaustive]` on both enums | §5 |
| `aimdb-sync-fork-safety.patch` | CR-1 — the fork generation, its `pthread_atfork` handler, and the producer/consumer guards. Fork-only files; the matching hunks in `handle.rs` and `error.rs` are in the cumulative patch | §7 |
| `ALL-core-changes.patch` | all of the above, against `aimdb-core` and `aimdb-sync` | — |

The weather-mesh half of CR-1 — `StationHandle::is_closed` answering honestly in
a forked child — is committed in this repository rather than carried here.

Verified together: `aimdb-sync` and `aimdb-core` are `cargo fmt` clean,
`cargo test -p weather-station --features sync` passes 16 / 2 ignored / 4, and
`make spike-cpp` is all-green.
