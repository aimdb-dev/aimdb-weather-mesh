/**
 * The wasm-pack output is generated, not committed, so a typecheck that runs
 * before `make wasm` has nothing to resolve. This wildcard stands in for it.
 *
 * A real `pkg/weather_mesh_client.d.ts` wins over an ambient wildcard, so once
 * wasm-pack has run the generated types are what `connect.ts` is checked
 * against — this only covers the gap.
 */
declare module "*/weather_mesh_client.js" {
    const mod: any;
    export = mod;
}
