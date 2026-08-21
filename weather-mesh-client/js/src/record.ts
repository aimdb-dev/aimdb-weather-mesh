/**
 * A typed view of one record, shaped for `useSyncExternalStore`.
 *
 * @module
 */

import type { WasmDb } from "./wasm.js";

/**
 * One record on one slot.
 *
 * The pair of `subscribe` and `getSnapshot` is exactly React's
 * `useSyncExternalStore` contract, and both are bound methods so they can be
 * passed directly without wrapping:
 *
 * ```ts
 * const t = useSyncExternalStore(handle.subscribe, handle.getSnapshot);
 * ```
 *
 * No React is imported here, and none should be. The same pair drives a Svelte
 * store or a Vue ref; shipping a hook would pick a framework the mesh has no
 * business picking.
 *
 * ## Why the snapshot is cached
 *
 * `WasmDb.get()` deserializes out of wasm on every call, so it returns a *fresh
 * object* each time. `useSyncExternalStore` compares snapshots by identity and
 * re-renders when they differ — a `getSnapshot` that allocates on every call
 * re-renders forever. So this handle keeps the last value and replaces it only
 * when a subscription push arrives, which makes identity stable between
 * changes.
 *
 * ## One underlying subscription
 *
 * However many listeners attach, the handle opens at most one wasm
 * subscription and fans out from it. That keeps the cost of ten components
 * reading the same slot the same as one, and it means the cache is fed even
 * while a listener is between renders.
 */
export class RecordHandle<T> {
    readonly key: string;

    #db: WasmDb;
    #listeners = new Set<() => void>();
    #unsubscribe: (() => void) | null = null;
    #snapshot: T | undefined;
    #primed = false;
    #closed = false;

    constructor(db: WasmDb, key: string) {
        this.#db = db;
        this.key = key;
    }

    /**
     * Register a listener; returns its own unsubscribe.
     *
     * Opening the wasm subscription is deferred to the first listener, so a
     * handle nobody reads costs nothing.
     */
    subscribe = (onStoreChange: () => void): (() => void) => {
        this.#assertOpen();
        this.#listeners.add(onStoreChange);

        if (this.#unsubscribe === null) {
            this.#unsubscribe = this.#db.subscribe(this.key, (value) => {
                this.#snapshot = value as T;
                this.#primed = true;
                for (const listener of this.#listeners) listener();
            });

            // Re-read after opening. A value produced between the render that
            // called getSnapshot and this subscription would otherwise be
            // missed: the push went nowhere, and the cache still holds the
            // older read. Same shape as the graph-start gate the station
            // handle uses on the Rust side, for the same reason.
            this.#prime();
        }

        return () => {
            this.#listeners.delete(onStoreChange);
            if (this.#listeners.size === 0) this.#detach();
        };
    };

    /**
     * The latest value, or `undefined` if the slot has published nothing yet.
     *
     * Safe to call during render: it reads the cache, and only falls through to
     * wasm once, before any subscription exists.
     */
    getSnapshot = (): T | undefined => {
        if (this.#closed) return undefined;
        if (!this.#primed) this.#prime();
        return this.#snapshot;
    };

    /** Whether this record has ever carried a value. */
    get hasValue(): boolean {
        return this.getSnapshot() !== undefined;
    }

    #prime(): void {
        const value = this.#db.get(this.key);
        this.#snapshot = (value ?? undefined) as T | undefined;
        this.#primed = true;
    }

    #detach(): void {
        this.#unsubscribe?.();
        this.#unsubscribe = null;
        // Deliberately keep the cached snapshot: a component unmounting and
        // remounting should not flash empty while the first push arrives.
        // `#primed` stays true so the remount does not re-read a value the
        // subscription is about to deliver anyway.
    }

    /** @internal — called by the mesh on close. */
    _close(): void {
        this.#detach();
        this.#listeners.clear();
        this.#snapshot = undefined;
        this.#primed = false;
        this.#closed = true;
    }

    #assertOpen(): void {
        if (this.#closed) {
            throw new Error(
                `Record ${this.key} is closed: the mesh connection it belongs to was closed.`,
            );
        }
    }
}
