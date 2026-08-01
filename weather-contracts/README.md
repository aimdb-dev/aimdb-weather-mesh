# weather-contracts

The data contracts of the weather mesh: the `Temperature`, `Humidity` and
`DewPoint` schemas that every station encodes and the hub decodes. The Rust type
*is* the wire format — a payload that does not match is rejected at the hub.

Records are addressed by `StringKey` at the hub and at the stations, so this
crate carries no key constants; it defines the schemas only. It is `no_std`
compatible (with `alloc`) for use on MCU nodes.

## Schemas

| Type | Schema name | Version | JSON payload |
|---|---|---|---|
| `Temperature` (= `TemperatureV2`) | `temperature` | 2 | `{"schema_version":2,"celsius":21.4,"timestamp":1754044800000}` |
| `TemperatureV1` (legacy) | `temperature` | 1 | `{"schema_version":1,"temp":70.5,"timestamp":1754044800000,"unit":"F"}` |
| `Humidity` | `humidity` | 1 | `{"percent":58.2,"timestamp":1754044800000}` |
| `DewPoint` | `dew_point` | 1 | `{"celsius":13.2,"timestamp":1754044800000}` |

`timestamp` is Unix time in **milliseconds**, taken when the reading was made.
Encoding and decoding go through `Linkable::to_bytes` / `from_bytes`, which use
`serde_json`.

### Implemented traits

| Type | `Streamable` | `Observable` | `Settable` | `Linkable` | `Simulatable` |
|---|---|---|---|---|---|
| `Temperature` | ✅ | `celsius`, `°C` | ✅ `f32` | ✅ (needs `linkable` **and** `migratable`) | ✅ random walk |
| `TemperatureV1` | — | — | — | ✅ (`linkable`) | — |
| `Humidity` | ✅ | `percent`, `%` | ✅ `f32` | ✅ (`linkable`) | ✅ random walk |
| `DewPoint` | ✅ | `celsius`, `°C` | — | ✅ (`linkable`) | — |

`Observable` is what makes a record's signal gauge (last / min / max / mean)
appear in `aimdb record list` and `record get`. `DewPoint` is not `Settable`
because it is derived, never sensed: the hub produces it with a `transform_join`
over the temperature and humidity records of the same slot, using the Magnus
approximation `T_dp ≈ T − (100 − RH) / 5` — accurate to about ±1 °C for
RH > 50 %, and computable in plain `f32` arithmetic without `libm`.

## Schema evolution

`Temperature` carries a **version-aware payload** so stations and hubs can be
updated independently. `from_bytes` reads `schema_version` and migrates older
payloads to the current schema before the record ever sees them:

| Payload version | Result |
|---|---|
| 1 | Migrated to v2. `unit` decides the conversion: `"F"` → `(t − 32) × 5/9`, `"K"` → `t − 273.15`, `"C"` or anything else → taken as Celsius. |
| 2 | Parsed directly. |
| missing `schema_version` | `MigrationError::MissingVersion`. |
| > 2 | `MigrationError::VersionTooNew` — update the receiver. |

Downgrading works in the same way: `migrate_to_version(1)` emits a v1 payload
with `unit = "C"`, for a consumer that still expects the old schema.

`Humidity` and `DewPoint` are unversioned v1 schemas with no migration chain;
their payloads carry no `schema_version` field.

`TemperatureV1` is kept in the crate so the migration path stays covered by CI
rather than being asserted by hand.

## Cargo features

| Feature | Default | Effect |
|---|---|---|
| `std` | ✅ | Builds against `std`. Disable for `no_std` targets; `alloc` is required either way. |
| `linkable` | | `Linkable` (JSON `to_bytes` / `from_bytes`). Required for records that link to a connector topic. Pulls in `serde_json`. |
| `migratable` | | The `Temperature` migration chain. **`Temperature`'s `Linkable` impl needs both `linkable` and `migratable`** — with `linkable` alone, only `TemperatureV1`, `Humidity` and `DewPoint` are linkable. |
| `simulatable` | | Random-walk generators for `Temperature` and `Humidity`. Pulls in `rand` without default features; the caller supplies the RNG. |

A node that publishes or consumes over MQTT therefore wants:

```toml
weather-contracts = { path = "../weather-contracts", features = ["linkable", "migratable"] }
```

and an MCU node adds `default-features = false`.

## Adding a schema

1. Add a module under `src/` with the struct, its `SchemaType` (`NAME`, and
   `VERSION` if it is not 1) and the traits it needs — at minimum `Streamable`,
   plus `Linkable` behind the `linkable` feature for anything that crosses a
   connector.
2. Re-export it from [`src/lib.rs`](src/lib.rs).
3. Register it on a record key at the [hub](../weather-hub) and at the
   [stations](../weather-station-openmeteo) that produce it.

Changing an existing schema in a way that breaks older payloads means a new
version struct plus a `MigrationStep`, wired into the `migration_chain!` macro —
`src/temperature.rs` is the worked example.
