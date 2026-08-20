import { describe, expect, it } from "vitest";

import { LivenessUnavailableError, MeshConnectionError, SlotNotServedError } from "../src/errors.js";
import { createMesh } from "../src/mesh.js";
import { FakeWasmDb, fakeWasm, poolRows } from "./fake-wasm.js";

describe("createMesh", () => {
    it("configures one record per discovered mesh key and builds once", async () => {
        const { wasm, db } = fakeWasm({ rows: poolRows(3) });
        await createMesh(wasm);

        expect(db.built).toBe(true);
        expect(db.configured).toHaveLength(9); // 3 slots × 3 records
        expect(db.configured[0]).toEqual({
            key: "station.0.temperature",
            options: { schemaType: "temperature", buffer: "SingleLatest" },
        });
    });

    /**
     * A hub may serve records this build carries no schema for. Configuring one
     * throws at build time, so they are filtered — and so is anything outside
     * the mesh's own namespace.
     */
    it("skips records this build cannot dispatch", async () => {
        const rows = [
            ...poolRows(1),
            { record_key: "station.0.pressure", name: "pressure", schema_type: "pressure", writable: false },
            { record_key: "internal.metrics", name: "metrics", schema_type: "temperature", writable: false },
            { record_key: "station.1.temperature", name: "temperature", writable: false }, // no schema_type
        ];
        const { wasm, db } = fakeWasm({ rows });
        const mesh = await createMesh(wasm);

        expect(db.configured.map((c) => c.key)).toEqual([
            "station.0.temperature",
            "station.0.humidity",
            "station.0.dew_point",
        ]);
        // Slot 1's only row was skipped, so the database holds nothing for it.
        // A station handle over it would throw `Unknown record key` on first
        // use, so the mesh does not offer one.
        expect(mesh.stations().map((s) => s.slot)).toEqual([0]);
        expect(() => mesh.station(1)).toThrow(SlotNotServedError);
    });

    /**
     * The browser cannot tell a dead host from a hub refusing this client's
     * AimX major, so the error must name both possibilities — and must not
     * claim the mismatch is probable, because nothing established that the
     * hub answered at all.
     */
    it("wraps a failed discovery as a connection error naming the client version", async () => {
        const { wasm } = fakeWasm({ rows: [], discoverError: new Error("socket closed") });

        await expect(createMesh(wasm, { url: "wss://example.invalid/ws" })).rejects.toSatisfy(
            (e: unknown) =>
                e instanceof MeshConnectionError &&
                e.clientSpeaks === "3.0" &&
                !e.probableProtocolMismatch &&
                e.message.includes("wss://example.invalid/ws") &&
                e.message.includes("AimX 3.0") &&
                e.message.includes("unreachable") &&
                e.message.includes("different AimX major"),
        );
    });
});

describe("WeatherMesh.stations", () => {
    it("lists every slot the hub serves, in slot order", async () => {
        const { wasm } = fakeWasm({ rows: poolRows(4) });
        const mesh = await createMesh(wasm);

        expect(mesh.stations().map((s) => s.slot)).toEqual([0, 1, 2, 3]);
    });

    /**
     * The hub registers its whole configured pool at startup — 64 slots by
     * default — whether or not any station has joined. So an unfiltered list is
     * "slots this hub serves", and `live` is the one that answers "who is
     * actually publishing".
     */
    it("filters to slots that have published when asked", async () => {
        const { wasm } = fakeWasm({
            rows: poolRows(5, (slot) => (slot === 1 || slot === 4 ? 12 : 0)),
        });
        const mesh = await createMesh(wasm);

        expect(mesh.stations()).toHaveLength(5);
        expect(mesh.stations({ live: true }).map((s) => s.slot)).toEqual([1, 4]);
    });

    /**
     * Without produced counts the honest answer is "cannot tell". Returning the
     * whole pool would report 64 stations on an empty mesh.
     */
    it("refuses to guess liveness when the hub reports no counts", async () => {
        const { wasm } = fakeWasm({ rows: poolRows(3) });
        const mesh = await createMesh(wasm);

        expect(() => mesh.stations({ live: true })).toThrow(LivenessUnavailableError);
    });

    /** Dew point is produced by the hub for every slot, joined or not. */
    it("reads liveness off temperature, not off the hub-derived dew point", async () => {
        const rows = poolRows(2).map((row) =>
            row.record_key === "station.0.dew_point" ? { ...row, produced_count: 99 } : row,
        );
        rows.find((r) => r.record_key === "station.1.temperature")!.produced_count = 7;
        rows.find((r) => r.record_key === "station.0.temperature")!.produced_count = 0;

        const { wasm } = fakeWasm({ rows });
        const mesh = await createMesh(wasm);

        expect(mesh.stations({ live: true }).map((s) => s.slot)).toEqual([1]);
    });
});

describe("WeatherMesh.station", () => {
    it("addresses a slot directly, with typed records and no key strings", async () => {
        const db = new FakeWasmDb();
        const { wasm } = fakeWasm({ rows: poolRows(2), db });
        const mesh = await createMesh(wasm);

        const station = mesh.station(1);
        station.temperature.subscribe(() => {});
        db.push("station.1.temperature", { schema_version: 2, celsius: 24.5, timestamp: 1 });

        expect(station.temperature.getSnapshot()?.celsius).toBe(24.5);
    });

    /** A station joining after connect publishes into a pool record the hub
     * already serves, so the handle exists — and is empty — before the
     * station does. */
    it("hands back an empty handle for a pool slot no station has joined", async () => {
        const db = new FakeWasmDb();
        const { wasm } = fakeWasm({ rows: poolRows(2), db });
        const mesh = await createMesh(wasm);

        const late = mesh.station(1);
        expect(late.temperature.getSnapshot()).toBeUndefined();

        late.humidity.subscribe(() => {});
        db.push("station.1.humidity", { percent: 55, timestamp: 1 });
        expect(late.humidity.getSnapshot()?.percent).toBe(55);
    });

    /**
     * A slot outside the hub's pool has no record in the built database, so a
     * handle could never produce a value — `WasmDb` would throw `Unknown
     * record key` at first use. Refusing here, with the slot named, beats an
     * opaque wasm error later or a handle that is silently forever-empty.
     */
    it("throws for a slot the hub does not serve", async () => {
        const { wasm } = fakeWasm({ rows: poolRows(2) });
        const mesh = await createMesh(wasm);

        expect(() => mesh.station(41)).toThrow(SlotNotServedError);
        expect(() => mesh.station(41)).toThrow(/slot 41/);
    });

    it("returns the same handle for the same slot", async () => {
        const { wasm } = fakeWasm({ rows: poolRows(2) });
        const mesh = await createMesh(wasm);
        expect(mesh.station(1)).toBe(mesh.station(1));
    });
});

describe("WeatherMesh.close", () => {
    it("disconnects the bridge before freeing the database", async () => {
        const db = new FakeWasmDb();
        const { wasm } = fakeWasm({ rows: poolRows(2), db });
        const mesh = await createMesh(wasm);

        const station = mesh.station(0);
        station.temperature.subscribe(() => {});

        mesh.close();

        expect(db.unsubscribeCalls).toBe(1);
        expect(db.freed).toBe(true);
        expect(() => mesh.stations()).toThrow();
    });

    it("is idempotent", async () => {
        const { wasm } = fakeWasm({ rows: poolRows(1) });
        const mesh = await createMesh(wasm);
        mesh.close();
        expect(() => mesh.close()).not.toThrow();
    });
});
