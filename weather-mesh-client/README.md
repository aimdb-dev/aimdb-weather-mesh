# weather-mesh-client

The browser client for the AimDB weather mesh, published to npm as
`@aimdb/weather-mesh-client`. Nothing here is published to crates.io — the
crate is a build input, and the artifact is the npm package.

```ts
import { connectWeatherMesh } from "@aimdb/weather-mesh-client";

const mesh = await connectWeatherMesh();       // the public mesh, no credentials
const vienna = mesh.station(17);

vienna.temperature.subscribe(() => {
  console.log(vienna.temperature.getSnapshot()?.celsius);  // typed, no cast
});
```

## Why this is a crate and not a package.json

`SchemaRegistry::register<T>()` builds a table of monomorphized closures keyed
by `T::NAME`. The dispatch table exists at Rust monomorphization time, not at
runtime, so **the contracts have to compile into the same wasm module as the
adapter** — there is no generic `@aimdb/wasm-adapter` that a separate contracts
package plugs into from JavaScript. The publishable unit is always "the browser
client for *this* mesh", and this crate is where the two are welded together.

`weather-contracts` cannot host the weld: it is `no_std` for MCU nodes, and
`wasm-bindgen` would break that exactly as `rumqttc` would.

## Layout

| | |
|---|---|
| `src/lib.rs` | The fusion: `createWeatherDb()`, plus the mesh's naming rule and protocol version, exported through `wasm-bindgen` |
| `js/src/` | The facade a consumer actually imports — `connectWeatherMesh`, `WeatherMesh`, `StationHandle`, `RecordHandle` |
| `js/src/generated/` | Contract types, emitted from the Rust definitions by ts-rs. Committed, and drift-checked in CI |
| `js/pkg/` | wasm-pack output. Generated, never committed |

The package is a sandwich: wasm-pack's output is an internal artifact, and the
TypeScript layer above it is what `package.json` points at. That split exists
because the wasm boundary types everything as `unknown` — wasm-bindgen has no
idea what `T` was — so something has to turn `unknown` into `TemperatureV2`.
Doing it in one reviewed TypeScript file beats doing it at every call site.

## The API is the mesh, not the adapter

Underneath sits the raw record plane: string keys, `unknown` payloads, and a
six-step create → discover → filter → configure → build → bridge ceremony. All
of that is sealed behind `connectWeatherMesh()`. What is left is slots and
readings.

Read-only by design. Stations are the write path — one writer per slot — and
the public bridge refuses writes anyway, so this API does not offer what the
mesh would reject. `set()` remains on the underlying `WasmDb`, which stays
exported for local-first use.

### Keys are a rule, not a list

`temperatureKey(17)` is exported, and so is its inverse `slotFromKey`. What is
compiled in is how a key is *spelled*; which slots exist is a property of a
running hub and is discovered at connect time. There is deliberately no
generated union of valid keys: npm versions are immutable, and an enumeration
baked into `3.0.0` is a claim about a deployment that the deployment is free to
falsify on its next restart.

The rule itself lives in `weather-contracts::keys`, which the hub and both
station templates also use. This crate re-exports it rather than restating it,
so TypeScript derives the mesh's spelling instead of transcribing it a fourth
time.

### Records fit `useSyncExternalStore`

Every record handle is a `subscribe` / `getSnapshot` pair, bound so it can be
passed directly:

```tsx
const temp = useSyncExternalStore(
  station.temperature.subscribe,
  station.temperature.getSnapshot,
);
```

No React is imported by this package and none should be — the same pair drives
a Svelte store or a Vue ref. The handle caches its snapshot, because `WasmDb.get()`
deserializes out of wasm on every call and returns a fresh object; handing that
to `useSyncExternalStore` unwrapped re-renders forever. It also opens at most
one wasm subscription per record however many listeners attach.

### `stations()` lists slots, `stations({ live: true })` lists stations

The hub registers its whole configured pool at startup — 64 slots by default —
whether or not a station ever joined, so a discovery reply describes what the
hub *serves*, not who is publishing. Filtering to real stations needs
per-record produced counts, which a hub reports only when built with
observability. Without them `stations({ live: true })` throws
`LivenessUnavailableError` rather than quietly returning 64 empty slots.

## Building

```bash
make wasm-check       # compile the fusion for wasm32 — what CI runs
make ts-bindings      # regenerate js/src/generated from the Rust contracts
make js               # typecheck and test the facade (no browser, no wasm)
make wasm             # full npm package (needs: cargo install wasm-pack)
```

The facade's tests drive it against a fake wasm module, so they need neither a
browser nor a wasm build — which is why they run in ordinary CI rather than
behind a browser matrix.

## Known gaps

Two behaviours here are shaped by limitations one layer down, and both are
tracked in [`next-steps.md`](../next-steps.md):

- **A protocol mismatch cannot be reported precisely.** A hub speaking a
  different AimX major refuses the WebSocket upgrade with HTTP 426, and a
  browser cannot read the status of a failed upgrade. `MeshConnectionError`
  carries `probableProtocolMismatch` and this client's version;
  `ProtocolMismatchError` exists for when the hub's version becomes knowable.
- **Inbound payloads are v2 only.** The bridge deserializes with plain serde
  rather than through `Linkable`, so the `Migratable` chain does not run in the
  browser. That is invisible today because the hub normalizes on ingest — the
  client's version tolerance is the hub's, not this crate's.
