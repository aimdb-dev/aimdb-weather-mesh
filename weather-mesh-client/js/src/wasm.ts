/**
 * The shape of the wasm-pack output, declared by hand.
 *
 * wasm-bindgen emits its own `.d.ts`, but everything crossing the boundary is
 * `any` or `unknown` there — it has no idea what `T` was on the Rust side. This
 * file is the one place that shape is written down, and the facade above it is
 * where `unknown` becomes a contract type. One cast, one file, reviewable.
 *
 * It also makes the facade testable without a browser: every type here is an
 * interface, so a test supplies a fake module and never loads wasm.
 *
 * @module
 */

import type { DewPointV1 } from "./generated/DewPointV1.js";
import type { HumidityV1 } from "./generated/HumidityV1.js";
import type { TemperatureV2 } from "./generated/TemperatureV2.js";

/** Every payload this mesh carries. */
export type MeshRecord = TemperatureV2 | HumidityV1 | DewPointV1;

/** Buffer selection for a record, as `configureRecord` expects it. */
export type BufferSpec = "SingleLatest" | { type: "SpmcRing"; capacity: number };

export interface RecordOptions {
    schemaType: string;
    buffer: BufferSpec;
}

export interface BridgeOptions {
    subscribeTopics?: string[];
    autoReconnect?: boolean;
    lateJoin?: boolean;
}

/**
 * One row of an AimX `record.list` reply.
 *
 * Only the fields the facade reads. `producedCount` is present only when the
 * serving hub was built with observability — see `stations({ live: true })`.
 */
export interface RecordMetadata {
    record_key: string;
    name: string;
    schema_type?: string;
    entity?: string;
    writable: boolean;
    produced_count?: number;
}

export interface WsBridge {
    status(): string;
    onStatusChange(callback: (status: string) => void): void;
    onGap(callback: (info: unknown) => void): void;
    droppedUpdates(): number;
    listTopics(): Promise<unknown>;
    query(pattern: string, options?: unknown): Promise<unknown>;
    disconnect(): void;
    free(): void;
}

export interface WasmDb {
    knownSchemas(): string[];
    configureRecord(recordKey: string, options: RecordOptions): void;
    build(): Promise<void>;
    connectBridge(url: string, options?: BridgeOptions): WsBridge;
    /** Returns its own unsubscribe function. */
    subscribe(recordKey: string, callback: (value: unknown) => void): () => void;
    get(recordKey: string): unknown;
    set(recordKey: string, value: unknown): void;
    isBuilt(): boolean;
    free(): void;
}

/**
 * The module `wasm-pack build --target web` produces, once initialised.
 *
 * `discover` is a static on the `WasmDb` class in the generated bindings; it is
 * surfaced here as a plain function so a fake module is trivial to write.
 */
export interface WeatherMeshWasm {
    createWeatherDb(): WasmDb;
    discover(url: string): Promise<RecordMetadata[]>;

    temperatureKey(slot: number): string;
    humidityKey(slot: number): string;
    dewPointKey(slot: number): string;
    slotFromKey(key: string): number | undefined;

    aimxProtocolVersion(): string;
}
