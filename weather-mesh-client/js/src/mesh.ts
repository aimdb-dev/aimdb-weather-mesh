/**
 * The mesh, as a consumer sees it: slots and readings, not keys and payloads.
 *
 * @module
 */

import { LivenessUnavailableError, MeshConnectionError } from "./errors.js";
import type { DewPointV1 } from "./generated/DewPointV1.js";
import type { HumidityV1 } from "./generated/HumidityV1.js";
import type { TemperatureV2 } from "./generated/TemperatureV2.js";
import { RecordHandle } from "./record.js";
import type { BufferSpec, RecordMetadata, WasmDb, WeatherMeshWasm, WsBridge } from "./wasm.js";

/**
 * The public mesh endpoint.
 *
 * A DNS alias the deployment commits to keeping, never a hub location — moving
 * the hub means re-pointing the alias, not publishing a package. Pass a URL to
 * {@link connectWeatherMesh} to target a self-hosted hub instead.
 *
 * NOTE: confirm this alias exists before the first publish. An npm version is
 * immutable, so a wrong default here is permanent for that version.
 */
export const DEFAULT_MESH_URL = "wss://mesh.aimdb.dev/ws";

/** Default buffer for mesh records: the browser wants the current reading. */
const DEFAULT_BUFFER: BufferSpec = "SingleLatest";

export interface ConnectOptions {
    /** Hub URL. Defaults to {@link DEFAULT_MESH_URL}. */
    url?: string;
    /** Reconnect automatically when the socket drops. Default `true`. */
    autoReconnect?: boolean;
    /** Replay the hub's current values on connect. Default `true`. */
    lateJoin?: boolean;
}

export interface StationsOptions {
    /**
     * Return only slots that have actually published.
     *
     * The hub registers its whole configured pool at startup — 64 slots by
     * default — so an unfiltered list is "slots this hub serves", not "stations
     * that exist". Filtering needs per-record produced counts, which a hub
     * reports only when built with observability; without them this throws
     * {@link LivenessUnavailableError} rather than quietly returning the pool.
     */
    live?: boolean;
}

/** One slot's readings. */
export class StationHandle {
    readonly slot: number;
    readonly temperature: RecordHandle<TemperatureV2>;
    readonly humidity: RecordHandle<HumidityV1>;
    readonly dewPoint: RecordHandle<DewPointV1>;

    /**
     * How many values the hub has seen on this slot's temperature record, when
     * the hub reports it. `undefined` means the hub reported no counts, not
     * that the slot is empty.
     */
    readonly producedCount: number | undefined;

    constructor(args: {
        slot: number;
        temperature: RecordHandle<TemperatureV2>;
        humidity: RecordHandle<HumidityV1>;
        dewPoint: RecordHandle<DewPointV1>;
        producedCount?: number;
    }) {
        this.slot = args.slot;
        this.temperature = args.temperature;
        this.humidity = args.humidity;
        this.dewPoint = args.dewPoint;
        this.producedCount = args.producedCount;
    }

    /** @internal */
    _close(): void {
        this.temperature._close();
        this.humidity._close();
        this.dewPoint._close();
    }
}

/**
 * A live, read-only view of the weather mesh.
 *
 * Read-only by design: stations are the write path (one writer per slot) and
 * the public bridge refuses writes anyway, so this API does not offer what the
 * mesh would reject. `set()` remains on the underlying {@link WasmDb} for
 * local-first use.
 */
export class WeatherMesh {
    readonly #db: WasmDb;
    readonly #bridge: WsBridge;
    readonly #stations = new Map<number, StationHandle>();
    readonly #wasm: WeatherMeshWasm;
    #closed = false;

    /** @internal — use {@link connectWeatherMesh}. */
    constructor(args: {
        wasm: WeatherMeshWasm;
        db: WasmDb;
        bridge: WsBridge;
        rows: RecordMetadata[];
    }) {
        this.#wasm = args.wasm;
        this.#db = args.db;
        this.#bridge = args.bridge;

        for (const slot of slotsIn(args.wasm, args.rows)) {
            this.#stations.set(slot.slot, this.#buildStation(slot.slot, slot.producedCount));
        }
    }

    /** The AimX version this client speaks. */
    get clientSpeaks(): string {
        return this.#wasm.aimxProtocolVersion();
    }

    /** Current transport status, straight from the bridge. */
    status(): string {
        return this.#bridge.status();
    }

    /** Called on every transport status change. */
    onStatusChange(callback: (status: string) => void): void {
        this.#bridge.onStatusChange(callback);
    }

    /**
     * How many updates the transport dropped because the page could not keep
     * up. Non-zero means what is rendered is behind the mesh.
     */
    droppedUpdates(): number {
        return this.#bridge.droppedUpdates();
    }

    /** Every slot discovered at connect time. See {@link StationsOptions}. */
    stations(options: StationsOptions = {}): StationHandle[] {
        this.#assertOpen();
        const all = [...this.#stations.values()].sort((a, b) => a.slot - b.slot);
        if (!options.live) return all;

        if (all.every((s) => s.producedCount === undefined)) {
            throw new LivenessUnavailableError();
        }
        return all.filter((s) => (s.producedCount ?? 0) > 0);
    }

    /**
     * A specific slot, by number.
     *
     * Works for a slot that was not in the discovery reply — a station that
     * joins later publishes into a record the hub already serves, and the
     * handle starts producing values when it does.
     */
    station(slot: number): StationHandle {
        this.#assertOpen();
        let handle = this.#stations.get(slot);
        if (handle === undefined) {
            handle = this.#buildStation(slot, undefined);
            this.#stations.set(slot, handle);
        }
        return handle;
    }

    /** The underlying database, for callers that want the raw record plane. */
    get db(): WasmDb {
        return this.#db;
    }

    /** Close the transport and release every handle. */
    close(): void {
        if (this.#closed) return;
        this.#closed = true;
        for (const station of this.#stations.values()) station._close();
        this.#stations.clear();
        this.#bridge.disconnect();
        // Bridge before database: the bridge holds a clone of the database
        // handle, and freeing underneath it is a use-after-free in wasm.
        this.#bridge.free();
        this.#db.free();
    }

    #buildStation(slot: number, producedCount: number | undefined): StationHandle {
        return new StationHandle({
            slot,
            producedCount,
            temperature: new RecordHandle<TemperatureV2>(this.#db, this.#wasm.temperatureKey(slot)),
            humidity: new RecordHandle<HumidityV1>(this.#db, this.#wasm.humidityKey(slot)),
            dewPoint: new RecordHandle<DewPointV1>(this.#db, this.#wasm.dewPointKey(slot)),
        });
    }

    #assertOpen(): void {
        if (this.#closed) throw new Error("This WeatherMesh is closed.");
    }
}

/** The slots present in a `record.list` reply, with their temperature counts. */
function slotsIn(
    wasm: WeatherMeshWasm,
    rows: RecordMetadata[],
): Array<{ slot: number; producedCount: number | undefined }> {
    const slots = new Map<number, number | undefined>();

    for (const row of rows) {
        const slot = wasm.slotFromKey(row.record_key);
        if (slot === undefined || slot === null) continue;

        // Liveness is read off temperature: every station publishes it, and dew
        // point is produced by the hub for every configured slot whether or not
        // a station ever joined — counting that would call an empty slot live.
        if (row.record_key === wasm.temperatureKey(slot)) {
            slots.set(slot, row.produced_count);
        } else if (!slots.has(slot)) {
            slots.set(slot, undefined);
        }
    }

    return [...slots].map(([slot, producedCount]) => ({ slot, producedCount }));
}

/**
 * Discover, configure and build a mesh view against an already-loaded wasm
 * module.
 *
 * {@link connectWeatherMesh} is this plus the wasm load. Split out so a test
 * can drive the whole facade with a fake module and no browser.
 *
 * @internal
 */
export async function createMesh(
    wasm: WeatherMeshWasm,
    options: ConnectOptions = {},
): Promise<WeatherMesh> {
    const url = options.url ?? DEFAULT_MESH_URL;
    const clientSpeaks = wasm.aimxProtocolVersion();

    let rows: RecordMetadata[];
    try {
        rows = await wasm.discover(url);
    } catch (cause) {
        // A hub speaking another AimX major refuses the upgrade with HTTP 426,
        // which a browser surfaces as an indistinguishable failure. Say so
        // rather than reporting a generic network error.
        throw new MeshConnectionError({
            url,
            clientSpeaks,
            probableProtocolMismatch: true,
            cause,
        });
    }

    const db = wasm.createWeatherDb();
    const known = new Set(db.knownSchemas());

    for (const row of rows) {
        if (wasm.slotFromKey(row.record_key) === undefined) continue;
        // A record whose schema this build does not carry cannot be dispatched;
        // configuring it would throw at build time.
        if (row.schema_type === undefined || !known.has(row.schema_type)) continue;
        db.configureRecord(row.record_key, {
            schemaType: row.schema_type,
            buffer: DEFAULT_BUFFER,
        });
    }

    await db.build();

    const bridge = db.connectBridge(url, {
        subscribeTopics: ["station.#"],
        autoReconnect: options.autoReconnect ?? true,
        lateJoin: options.lateJoin ?? true,
    });

    return new WeatherMesh({ wasm, db, bridge, rows });
}
