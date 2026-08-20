/**
 * A stand-in for the wasm module, so the facade is testable without a browser.
 *
 * It reimplements nothing that matters: the key rule is asked of the real
 * exports in the Rust test suite, and mirrored here only so a test can name a
 * slot. What this fake actually models is the *behaviour the facade has to cope
 * with* — chiefly that `get()` returns a fresh object every call, which is the
 * reason `RecordHandle` caches at all.
 */

import type { RecordMetadata, RecordOptions, WasmDb, WeatherMeshWasm, WsBridge } from "../src/wasm.js";

export class FakeWasmDb implements WasmDb {
    values = new Map<string, unknown>();
    configured: Array<{ key: string; options: RecordOptions }> = [];
    subscribers = new Map<string, Set<(value: unknown) => void>>();
    built = false;
    freed = false;

    /** Every subscribe/unsubscribe, so a test can assert on the fan-out. */
    subscribeCalls = 0;
    unsubscribeCalls = 0;

    constructor(private readonly schemas = ["temperature", "humidity", "dew_point"]) {}

    knownSchemas(): string[] {
        return [...this.schemas];
    }

    configureRecord(recordKey: string, options: RecordOptions): void {
        this.configured.push({ key: recordKey, options });
    }

    async build(): Promise<void> {
        this.built = true;
    }

    connectBridge(): WsBridge {
        return new FakeBridge();
    }

    subscribe(recordKey: string, callback: (value: unknown) => void): () => void {
        this.subscribeCalls += 1;
        let set = this.subscribers.get(recordKey);
        if (!set) {
            set = new Set();
            this.subscribers.set(recordKey, set);
        }
        set.add(callback);
        return () => {
            this.unsubscribeCalls += 1;
            set.delete(callback);
        };
    }

    /**
     * A *fresh object every call*, exactly as deserializing out of wasm does.
     * This is the behaviour that makes a naive `getSnapshot` re-render forever.
     */
    get(recordKey: string): unknown {
        const value = this.values.get(recordKey);
        return value === undefined ? undefined : structuredClone(value);
    }

    set(recordKey: string, value: unknown): void {
        this.values.set(recordKey, value);
    }

    isBuilt(): boolean {
        return this.built;
    }

    free(): void {
        this.freed = true;
    }

    /** Simulate a value arriving from the hub. */
    push(recordKey: string, value: unknown): void {
        this.values.set(recordKey, value);
        for (const cb of this.subscribers.get(recordKey) ?? []) cb(structuredClone(value));
    }
}

export class FakeBridge implements WsBridge {
    disconnected = false;
    freed = false;
    private state = "connected";
    private listeners: Array<(status: string) => void> = [];

    status(): string {
        return this.state;
    }
    onStatusChange(callback: (status: string) => void): void {
        this.listeners.push(callback);
    }
    onGap(): void {}
    droppedUpdates(): number {
        return 0;
    }
    async listTopics(): Promise<unknown> {
        return [];
    }
    async query(): Promise<unknown> {
        return [];
    }
    disconnect(): void {
        this.disconnected = true;
        this.state = "disconnected";
        for (const l of this.listeners) l(this.state);
    }
    free(): void {
        this.freed = true;
    }
}

export function fakeWasm(args: {
    rows: RecordMetadata[];
    db?: FakeWasmDb;
    discoverError?: unknown;
}): { wasm: WeatherMeshWasm; db: FakeWasmDb } {
    const db = args.db ?? new FakeWasmDb();
    const wasm: WeatherMeshWasm = {
        createWeatherDb: () => db,
        discover: async () => {
            if (args.discoverError !== undefined) throw args.discoverError;
            return args.rows;
        },
        temperatureKey: (slot) => `station.${slot}.temperature`,
        humidityKey: (slot) => `station.${slot}.humidity`,
        dewPointKey: (slot) => `station.${slot}.dew_point`,
        slotFromKey: (key) => {
            const m = /^station\.(\d+)\.(temperature|humidity|dew_point)$/.exec(key);
            return m ? Number(m[1]) : undefined;
        },
        aimxProtocolVersion: () => "3.0",
    };
    return { wasm, db };
}

/** A `record.list` reply for a hub serving `slots` slots. */
export function poolRows(slots: number, producedCount?: (slot: number) => number): RecordMetadata[] {
    const rows: RecordMetadata[] = [];
    for (let slot = 0; slot < slots; slot++) {
        for (const [suffix, schema] of [
            ["temperature", "temperature"],
            ["humidity", "humidity"],
            ["dew_point", "dew_point"],
        ] as const) {
            const row: RecordMetadata = {
                record_key: `station.${slot}.${suffix}`,
                name: schema,
                schema_type: schema,
                entity: suffix,
                writable: false,
            };
            if (producedCount && suffix === "temperature") {
                row.produced_count = producedCount(slot);
            }
            rows.push(row);
        }
    }
    return rows;
}
