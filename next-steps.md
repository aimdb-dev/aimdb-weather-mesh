# Next steps — `aimdb-wasm-adapter` changes the browser client needs

Written while implementing `weather-mesh-client` (design 008 §4). Everything
below is a change in the **`aimdb` repository**, not this one. The client works
without any of it — each item is either a limitation the client currently works
around, or a claim in `aimdb` that has stopped being true.

Each item states what was observed, where, and what the client does today, so
none of this has to be rediscovered.

---

## 1. A hub's protocol version is unknowable from a browser

**Blocks:** `ProtocolMismatchError` carrying a real `hubSpeaks`.
**Severity:** the recovery path design 008 §8 promises does not currently work.

`version_compatible` (`aimdb-core/src/remote/protocol.rs:53`) accepts exactly
one AimX major, and the WebSocket connector refuses the rest at the HTTP upgrade
with 426 (`aimdb-websocket-connector/src/server/http.rs:154`). §8 makes
`ProtocolMismatchError { hubSpeaks, clientSpeaks }` the recovery UX for that hard
cut — the error names the package version to install.

A browser cannot produce it. The WebSocket API surfaces a failed upgrade as an
opaque `error` event with no status code and no body, so from JavaScript a hub
refusing 3.0 and a hub that is simply down are indistinguishable.

**Today the client throws `MeshConnectionError`** with `clientSpeaks` and a
message naming both possibilities — unreachable host and refused upgrade —
because the browser cannot tell them apart. `probableProtocolMismatch` stays
`false` until something can actually establish that the hub answered; like
`ProtocolMismatchError` — defined and exported, deliberately unused — it exists
ahead of its trigger because npm versions are immutable, and changing an error's
shape or meaning later would break anyone catching on type.

**Options, cheapest first.**

1. **Serve the version over plain HTTP** beside the WebSocket route —
   `GET /version` returning `{"aimx": "3.0"}`, readable with `fetch` before
   dialing. Roughly ten lines in the websocket connector's HTTP handler, and it
   makes the check work for every browser client, not just this one.
2. **Accept the upgrade and negotiate in-band**, refusing on the first frame
   with a structured error. Changes the wire and the gate; not worth it for this
   alone.
3. **Include the version in the 426 body.** Does not help — the browser cannot
   read the body either.

Option 1 is the recommendation. When it lands, the client's `createMesh` probes
it before `discover` and throws `ProtocolMismatchError` with both versions
populated; nothing else in the facade changes.

---

## 2. The bridge's inbound path does not run migrations

**Blocks:** nothing today. **Severity:** a silent assumption worth making loud.

`produce_from_json` (`aimdb-wasm-adapter/src/ws_bridge.rs:937`) deserializes
with `serde_json::from_value::<T>`, not through `Linkable::from_bytes`. So the
`Migratable` chain never runs in the browser: a `schema_version: 1` temperature
payload arriving over AimX would fail to decode and log a `console.warn` rather
than upgrade to v2.

This is invisible in the current topology because the hub normalizes on ingest —
`TemperatureV2::from_bytes` migrates at the MQTT boundary
(`weather-contracts/src/temperature.rs:235-238`), so everything served over AimX
is already v2. But it means **the browser's version tolerance belongs to the
hub, not to the contracts crate it compiles in**, and the day anything bridges
raw station payloads to a browser it breaks quietly.

Two ways to close it, and the choice is a design call rather than a bug fix:

- **Deserialize through `Linkable` where the type implements it.** Correct, and
  costs a trait bound the registry does not currently require — `Streamable` does
  not imply `Linkable`, so `SchemaOps` would need a second, optional closure.
- **Document it and leave it.** Defensible: the AimX plane is a normalized plane
  by design, and migration is an ingest concern.

Either way, `SchemaRegistry`'s docs should say which it is. The client's
`create_weather_db` carries a note pointing here.

---

## 3. `record.list` cannot distinguish a live station from an empty slot

**Blocks:** honest `stations()` semantics. **Severity:** shapes the public API.

Design 008 §4.4 annotates `mesh.stations()` as *"discovery: which slots are
publishing"*. `record.list` does not answer that. The hub registers its whole
configured pool at startup — `for slot in 0..slots` at
`weather-hub/src/main.rs:74`, defaulting to 64 — so a discovery reply on an
entirely empty mesh returns 64 slots' worth of records.

`RecordMetadata` (`aimdb-core/src/remote/metadata.rs`) carries `produced_count`,
which does answer it — but it is `#[cfg(feature = "observability")]`, so whether
a browser can tell a live station from an empty slot depends on how the hub was
built.

**Today the client splits the two meanings**: `stations()` returns every slot the
hub serves, and `stations({ live: true })` filters on `produced_count`, throwing
`LivenessUnavailableError` when the hub reports none. Returning the pool
unfiltered would have been a quiet lie.

Worth deciding upstream: is a liveness signal part of AimX's record metadata, or
an observability extra? If the former, a small always-present field
(`has_produced: bool`, or `last_produced_ms`) would let every client answer the
question without a feature flag. If the latter, the mesh's operations notes
should say that the public hub runs with observability on, because the browser
API's usefulness depends on it.

Related, and smaller: `RecordMetadata.entity` documents itself as the field
clients should use "instead of parsing keys", but it is the key's *last* segment
— `temperature` for `station.17.temperature`, not `17`. For any hierarchical key
scheme the interesting identifier is not the leaf. The client parses the key
through an exported rule instead; the docstring should not promise more than the
field delivers.

---

## 4. The adapter's React layer cannot be imported, and its README says it can

**Blocks:** nothing. **Severity:** a live trap for the next reader.

`aimdb-wasm-adapter/src/react/useAimDb.tsx` is 300 lines of `AimDbProvider`,
`useRecord`, `useSetRecord` and `useBridge`, specified in design 025 §6.5
("P7: React Hooks"). Two things are wrong with it as shipped:

- It does `await import("../pkg/aimdb_wasm_adapter")` — a **relative path into
  the adapter's own build output**. That resolves only when compiled from inside
  the adapter's source tree, so it cannot be consumed from an installed package.
- `aimdb-wasm-adapter/README.md:99` advertises
  `import { AimDbProvider, useRecord } from '@aimdb/wasm/react'`. That package
  name does not exist; the published one is `@aimdb/aimdb-wasm-adapter`, and
  `wasm-pack` packs only `pkg/`, so the `.tsx` is not in the artifact at all.

The mesh client deliberately does not use it: its hooks are string-keyed
(`useRecord<T>('sensors.temperature.vienna')`), which is exactly the layer the
mesh facade exists to hide, and putting the client's React ergonomics in an
`aimdb` crate would put the fastest-moving surface behind the slowest release
process.

**Recommendation: relabel rather than delete.** The file contains four things
worth keeping as a reference — the `cancelled` guard against StrictMode double
mounts, refs-instead-of-state for the cleanup closure (its own comment records
the stale-closure bug that motivated it), `bridge.disconnect()` before
`db.free()`, and the not-ready fallback. All four are reproduced in the mesh
client's facade. Move it to `examples/`, fix the README's import line, and say
it is a pattern to copy rather than an API to install.

If a second AimDB browser app ever wants the raw string-key plane with React, a
shared layer earns its keep — but as its own `@aimdb/react` package, so the wasm
artifact stays a pure wasm-pack output and the React opinion versions
separately.

---

## 5. The published npm package name describes something that does not exist

**Blocks:** a second AimDB browser application. **Severity:** already published.

Design 008 §2 objects to `Makefile:132` renaming the wasm-pack output to
`@aimdb/aimdb-wasm-adapter` with a `sed`, on the grounds that the artifact is
never a generic adapter — it is always the fusion of the adapter with one mesh's
contracts, and a second browser app would collide with the name.

That is no longer a Makefile line to delete: **`@aimdb/aimdb-wasm-adapter` is
live on npm at 0.1.1**, published 2026-03-10. The `@aimdb` scope is claimed
(which resolves half of decision 8), and the name will keep resolving to a
package whose contents are one specific application's fusion.

Nothing forces action before the mesh's first tag —
`@aimdb/weather-mesh-client` is free and unaffected. But the published package
should be deprecated or documented for what it is before a third party installs
it expecting a reusable adapter.

---

## 6. Smaller notes

- **`WasmDb::discover` returns `JsValue`.** The client declares the row shape by
  hand in `js/src/wasm.ts`. A `#[wasm_bindgen]` struct, or a ts-rs export of
  `RecordMetadata`, would make it a checked type instead of a transcription.
  Same class of drift the mesh just removed for record keys.
- **`SchemaRegistry::register<T>` keys by `T::NAME` and silently overwrites.**
  `TemperatureV1` and `TemperatureV2` both declare `NAME = "temperature"`, so
  registering both would drop one, and the symptom is a browser rendering
  nothing with no error anywhere. The rule the client follows — register the
  newest type per name, exactly once — belongs in the registry's own docs, and a
  `debug_assert` on duplicate names would make it loud.
- **`aimdb-wasm-adapter` does not build on the host.** Its bridge holds
  `web_sys` closures across await points, so the futures are `!Send` and a host
  build fails with 14 errors. That is correct behaviour for a wasm-only crate,
  but it is not declared: consumers discover it by compiling. A
  `compile_error!` on non-wasm targets, or a note in the README, would turn a
  wall of `E0277` into one line.

---

## What this repository is waiting on

None of the above blocks `weather-mesh-client`. What does block *publishing* it
is unchanged and lives elsewhere: `aimdb-wasm-adapter` is at 0.3.0 locally and
0.2.0 on crates.io, so the npm artifact cannot be built from a registry
resolution until the `aimdb` release train is cut. The npm package itself needs
no crates.io release — a wasm blob carries no Cargo metadata — so this is a
reproducibility gate rather than a hard one.

One decision is owned by this repository and should be settled before the first
publish: `DEFAULT_MESH_URL` in `weather-mesh-client/js/src/mesh.ts` is currently
`wss://mesh.aimdb.dev/ws`, a placeholder. Decision 12 makes the zero-argument
connect target a DNS alias the deployment commits to keeping. An npm version is
immutable, so the wrong value there is permanent for that version.
