/**
 * Loading the wasm module, and the one entry point that uses it.
 *
 * @module
 */

import { BrowserOnlyError } from "./errors.js";
import { createMesh, type ConnectOptions, type WeatherMesh } from "./mesh.js";
import type { WeatherMeshWasm } from "./wasm.js";

/**
 * Initialisation is deferred and memoised.
 *
 * Importing this package must have no side effects — an SSR bundle imports it
 * and must not touch `WebSocket` or `document` — so the wasm binary is fetched
 * on the first connect, and shared by every connect after that.
 */
let loading: Promise<WeatherMeshWasm> | null = null;

async function loadWasm(): Promise<WeatherMeshWasm> {
    // The generated glue resolves its own `.wasm` beside itself via
    // `import.meta.url`, which is what makes this work under a bundler and as
    // plain ESM alike.
    const mod = await import("../pkg/weather_mesh_client.js");
    await mod.default();

    return {
        createWeatherDb: mod.createWeatherDb,
        // `discover` is a static on the generated class; the facade wants a
        // plain function so a fake module stays a plain object.
        discover: (url: string) => mod.WasmDb.discover(url),
        temperatureKey: mod.temperatureKey,
        humidityKey: mod.humidityKey,
        dewPointKey: mod.dewPointKey,
        slotFromKey: mod.slotFromKey,
        aimxProtocolVersion: mod.aimxProtocolVersion,
    } as WeatherMeshWasm;
}

/**
 * Connect to the weather mesh.
 *
 * With no arguments it targets the public mesh, which is read-only and needs no
 * credentials — the README example runs verbatim. Pass a `url` for a
 * self-hosted hub.
 *
 * ```ts
 * const mesh = await connectWeatherMesh();
 * for (const station of mesh.stations()) {
 *   station.temperature.subscribe(() => {
 *     console.log(station.slot, station.temperature.getSnapshot()?.celsius);
 *   });
 * }
 * ```
 *
 * Owns wasm init, the discovery pass, record configuration and the bridge — the
 * six-step ceremony a consumer used to perform by hand. Call {@link
 * WeatherMesh.close} when finished.
 *
 * @throws {BrowserOnlyError} when called without a DOM.
 * @throws {MeshConnectionError} when the hub cannot be reached, which in a
 * browser also covers a hub refusing this client's AimX major.
 */
export async function connectWeatherMesh(options: ConnectOptions = {}): Promise<WeatherMesh> {
    if (typeof window === "undefined" || typeof WebSocket === "undefined") {
        throw new BrowserOnlyError();
    }
    loading ??= loadWasm();
    return createMesh(await loading, options);
}
