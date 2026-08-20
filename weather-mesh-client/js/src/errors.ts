/**
 * Errors this package throws.
 *
 * Every one of them names what to do next, because the recovery path for a
 * protocol break is UX rather than a log line.
 *
 * @module
 */

/** Base class, so a consumer can catch everything from this package at once. */
export class MeshError extends Error {
    constructor(message: string, options?: { cause?: unknown }) {
        super(message, options);
        this.name = new.target.name;
    }
}

/**
 * Thrown when `connectWeatherMesh()` is called somewhere without a DOM.
 *
 * Importing this module is side-effect free, so an SSR bundle can import it
 * safely; only connecting needs a browser. Guard the call, or move it into an
 * effect.
 */
export class BrowserOnlyError extends MeshError {
    constructor() {
        super(
            "connectWeatherMesh() needs a browser: the wasm module uses WebSocket and " +
                "the DOM. Importing this package server-side is safe — call connect from " +
                "a client component or an effect instead.",
        );
    }
}

/**
 * The hub could not be reached.
 *
 * ## Why this is not always `ProtocolMismatchError`
 *
 * A hub speaking a different AimX major refuses the WebSocket upgrade with
 * HTTP 426. A browser's WebSocket API does not expose the status code or body
 * of a failed upgrade — the page sees an opaque error event — so from here, a
 * refused upgrade and an unreachable host are indistinguishable. `clientSpeaks`
 * is reported so the message is still actionable, and `probableProtocolMismatch`
 * says the hub answered but rejected the connection.
 */
export class MeshConnectionError extends MeshError {
    readonly url: string;
    readonly clientSpeaks: string;
    readonly probableProtocolMismatch: boolean;

    constructor(args: {
        url: string;
        clientSpeaks: string;
        probableProtocolMismatch?: boolean;
        cause?: unknown;
    }) {
        super(
            `Could not connect to the weather mesh at ${args.url}. ` +
                `This client speaks AimX ${args.clientSpeaks}. ` +
                (args.probableProtocolMismatch
                    ? "The hub refused the connection, which usually means it speaks a " +
                      "different AimX major — check which version of " +
                      "@aimdb/weather-mesh-client matches it."
                    : "Check that the hub is reachable and the URL is correct."),
            { cause: args.cause },
        );
        this.url = args.url;
        this.clientSpeaks = args.clientSpeaks;
        this.probableProtocolMismatch = args.probableProtocolMismatch ?? false;
    }
}

/**
 * The hub's AimX major differs from this client's, and the hub said so.
 *
 * AimX majors are hard cuts — the hub accepts exactly one and refuses the rest,
 * with no N/N−1 window — so the recovery is to install the matching package
 * version, which the message names.
 *
 * Only thrown when the hub's version is actually known. Today nothing in the
 * browser can learn it (see {@link MeshConnectionError}), so this type exists
 * ahead of its trigger deliberately: npm versions are immutable, and adding a
 * new error class later would be a breaking change to code that catches on
 * type. See `next-steps.md` for the hub-side endpoint that will populate it.
 */
export class ProtocolMismatchError extends MeshConnectionError {
    readonly hubSpeaks: string;

    constructor(args: { url: string; clientSpeaks: string; hubSpeaks: string; cause?: unknown }) {
        super({ ...args, probableProtocolMismatch: true });
        this.hubSpeaks = args.hubSpeaks;
        this.message =
            `The weather mesh at ${args.url} speaks AimX ${args.hubSpeaks}, ` +
            `this client speaks ${args.clientSpeaks}. ` +
            "Install the @aimdb/weather-mesh-client major that matches the hub.";
    }
}

/**
 * `station(n)` asked for a slot the hub was not serving at connect time.
 *
 * The local database is built from the connect-time discovery reply, so it
 * holds no record for such a slot and a handle over it could never produce a
 * value — the underlying `WasmDb` throws `Unknown record key` on first use.
 * Handing a handle back anyway would make "the hub does not serve this slot"
 * look exactly like "no station has published yet".
 *
 * Slots the hub serves but nobody publishes to are *not* this error: the hub
 * registers its whole pool at startup, so a station that joins later lands in
 * a record `station(n)` already watches.
 */
export class SlotNotServedError extends MeshError {
    readonly slot: number;

    constructor(slot: number) {
        super(
            `This hub was not serving slot ${slot} when this client connected. ` +
                "stations() lists every slot it does serve; if the hub has been " +
                "reconfigured since, reconnect to pick up its new pool.",
        );
        this.slot = slot;
    }
}

/**
 * `stations({ live: true })` was asked to filter to slots that have published,
 * and the hub reported no per-record counts to filter on.
 *
 * Returning every configured slot instead would be a quiet lie — the hub
 * registers its whole pool at startup whether or not any station has joined.
 */
export class LivenessUnavailableError extends MeshError {
    constructor() {
        super(
            "This hub does not report per-record produced counts, so live stations " +
                "cannot be distinguished from configured-but-empty slots. Use " +
                "stations() to list every slot the hub serves, or run a hub built " +
                "with observability enabled.",
        );
    }
}
