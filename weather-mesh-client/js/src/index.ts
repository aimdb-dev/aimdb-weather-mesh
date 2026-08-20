/**
 * `@aimdb/weather-mesh-client` — the browser client for the AimDB weather mesh.
 *
 * ```ts
 * import { connectWeatherMesh } from "@aimdb/weather-mesh-client";
 *
 * const mesh = await connectWeatherMesh();
 * const vienna = mesh.station(17);
 * vienna.temperature.subscribe(() => {
 *   console.log(vienna.temperature.getSnapshot()?.celsius);
 * });
 * ```
 *
 * @module
 */

export { connectWeatherMesh } from "./connect.js";
export {
    DEFAULT_MESH_URL,
    StationHandle,
    WeatherMesh,
    type ConnectOptions,
    type StationsOptions,
} from "./mesh.js";
export { RecordHandle } from "./record.js";

export {
    BrowserOnlyError,
    LivenessUnavailableError,
    MeshConnectionError,
    MeshError,
    ProtocolMismatchError,
} from "./errors.js";

// The contract types, generated from the Rust definitions by ts-rs. They are
// the same shapes the wasm module validates against, so they cannot drift from
// what the mesh actually carries.
export type { DewPointV1 } from "./generated/DewPointV1.js";
export type { HumidityV1 } from "./generated/HumidityV1.js";
export type { TemperatureV2 } from "./generated/TemperatureV2.js";

// The raw plane, kept public beneath the facade. A power user composes keys
// from the same rule the facade applies instead of writing strings by hand.
export type {
    BridgeOptions,
    BufferSpec,
    MeshRecord,
    RecordMetadata,
    RecordOptions,
    WasmDb,
    WsBridge,
} from "./wasm.js";
