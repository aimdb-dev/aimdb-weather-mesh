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
| `aimdb-mqtt-connector-tls-backend.patch` | CR-11 — `tokio-native-tls` / `tokio-rustls` / neither, and a `TlsConfiguration` that does not panic | §8 |
| `ALL-core-changes.patch` | all of the above, against `aimdb-core`, `aimdb-sync` and `aimdb-mqtt-connector` | — |

The weather-mesh halves are committed in this repository rather than carried
here: `StationHandle::is_closed` answering honestly in a forked child (CR-1),
and `weather-station`'s own `native-tls` / `rustls` features and pre-flight
backend (CR-11).

Verified together: `aimdb-core`, `aimdb-sync` and `aimdb-mqtt-connector` are
`cargo fmt` clean; `make test` passes across every feature combination it names,
including the two TLS ones added for CR-11; `make clippy` is clean on every host
step; and `make spike-cpp` is all-green. The wasm32 clippy step cannot run in
the container this was written in — that target is not installed — and is
unrelated to these changes.
