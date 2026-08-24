# Probes

Standalone experiments that settle a design question the spike itself cannot
ask, because they need aimdb's API directly rather than through the station
crates.

Each is its own cargo workspace (`[workspace]` in its manifest) so it does not
join the mesh workspace, and reaches the aimdb crates by the same relative path
everything else here does. Run one with `cargo run` from its directory; a
sibling aimdb checkout must exist at `../../../../aimdb`.

- `cr6-consumer-shape` — what shape the consumer half of an FFI binding can
  take, given that `SyncConsumer` is `Send + !Sync` with `&mut self` receivers
  after issue #200. Findings in `../review.md` §4.
