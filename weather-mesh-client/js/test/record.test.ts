import { describe, expect, it, vi } from "vitest";

import { RecordHandle } from "../src/record.js";
import type { TemperatureV2 } from "../src/generated/TemperatureV2.js";
import { FakeWasmDb } from "./fake-wasm.js";

const KEY = "station.17.temperature";

function reading(celsius: number): TemperatureV2 {
    return { schema_version: 2, celsius, timestamp: 1_754_044_800_000 };
}

/**
 * A db with KEY configured, as it always is by the time a `RecordHandle`
 * exists — the mesh only builds handles over keys it configured, because the
 * real `WasmDb` throws for any other key (and so does the fake).
 */
function fakeDb(): FakeWasmDb {
    const db = new FakeWasmDb();
    db.configureRecord(KEY, { schemaType: "temperature", buffer: "SingleLatest" });
    return db;
}

describe("RecordHandle", () => {
    /**
     * The one that earns its keep. `useSyncExternalStore` compares snapshots by
     * identity and re-renders when they differ, so a `getSnapshot` that returns
     * what `WasmDb.get()` hands back — a fresh object every call — renders in an
     * infinite loop. React would not report a bug; the tab would just get hot.
     */
    it("returns an identical snapshot until a value actually arrives", () => {
        const db = fakeDb();
        db.set(KEY, reading(21.5));
        const handle = new RecordHandle<TemperatureV2>(db, KEY);

        const first = handle.getSnapshot();
        expect(first).toEqual(reading(21.5));
        expect(handle.getSnapshot()).toBe(first);
        expect(handle.getSnapshot()).toBe(first);

        handle.subscribe(() => {});
        db.push(KEY, reading(22.5));

        const second = handle.getSnapshot();
        expect(second).not.toBe(first);
        expect(second?.celsius).toBe(22.5);
        expect(handle.getSnapshot()).toBe(second);
    });

    /**
     * React reads `getSnapshot` during render and subscribes in an effect. A
     * value landing in that gap is pushed to nobody, so without a re-read after
     * subscribing the cache would still hold the render-time value and React's
     * post-subscribe check would see no change.
     */
    it("re-reads after subscribing, so a value arriving mid-gap is not lost", () => {
        const db = fakeDb();
        const handle = new RecordHandle<TemperatureV2>(db, KEY);

        expect(handle.getSnapshot()).toBeUndefined(); // render

        db.set(KEY, reading(19.0)); // arrives before the effect runs
        handle.subscribe(() => {});

        expect(handle.getSnapshot()?.celsius).toBe(19.0);
    });

    it("opens one wasm subscription however many listeners attach", () => {
        const db = fakeDb();
        const handle = new RecordHandle<TemperatureV2>(db, KEY);

        const a = vi.fn();
        const b = vi.fn();
        const dropA = handle.subscribe(a);
        const dropB = handle.subscribe(b);
        expect(db.subscribeCalls).toBe(1);

        db.push(KEY, reading(20));
        expect(a).toHaveBeenCalledTimes(1);
        expect(b).toHaveBeenCalledTimes(1);

        dropA();
        expect(db.unsubscribeCalls).toBe(0); // one listener still attached
        dropB();
        expect(db.unsubscribeCalls).toBe(1); // last one out closes it
    });

    it("stops notifying a listener that unsubscribed", () => {
        const db = fakeDb();
        const handle = new RecordHandle<TemperatureV2>(db, KEY);

        const stale = vi.fn();
        const live = vi.fn();
        const drop = handle.subscribe(stale);
        handle.subscribe(live);

        drop();
        db.push(KEY, reading(23));

        expect(stale).not.toHaveBeenCalled();
        expect(live).toHaveBeenCalledTimes(1);
    });

    /**
     * A remount should not flash empty while waiting for the next push, so the
     * cached value survives the last unsubscribe.
     */
    it("keeps its value across a full unsubscribe and resubscribe", () => {
        const db = fakeDb();
        const handle = new RecordHandle<TemperatureV2>(db, KEY);

        const drop = handle.subscribe(() => {});
        db.push(KEY, reading(18.25));
        drop();

        expect(handle.getSnapshot()?.celsius).toBe(18.25);
        handle.subscribe(() => {});
        expect(handle.getSnapshot()?.celsius).toBe(18.25);
        expect(db.subscribeCalls).toBe(2);
    });

    it("reports a slot that has published nothing as valueless", () => {
        const handle = new RecordHandle<TemperatureV2>(fakeDb(), KEY);
        expect(handle.getSnapshot()).toBeUndefined();
        expect(handle.hasValue).toBe(false);
    });

    /** Both are passed straight to `useSyncExternalStore`, unwrapped. */
    it("has callable methods that survive being detached from the handle", () => {
        const db = fakeDb();
        db.set(KEY, reading(21));
        const handle = new RecordHandle<TemperatureV2>(db, KEY);

        const { subscribe, getSnapshot } = handle;
        expect(getSnapshot()?.celsius).toBe(21);
        expect(() => subscribe(() => {})()).not.toThrow();
    });
});
