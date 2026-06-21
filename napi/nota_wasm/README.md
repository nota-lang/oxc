# `nota_wasm` — the Nota wasm compiler backend

The Nota reader (`oxc::nota`) compiled to WebAssembly via [`wasm-bindgen`], so it runs **in-browser**
for the Part-4 playground (contract §9: *"A **wasm** backend (wasm-bindgen over the same three
entries) serves the browser playground (Part 4)"*).

It wraps the three `oxc::nota` entries and returns plain JS objects (via [`serde-wasm-bindgen`]).
This crate lives under `napi/` only because the workspace `members = [… "napi/*" …]` glob auto-includes
it; it does **not** use the `napi`/`napi-derive` stack the sibling `napi/*` crates use — it is a
`wasm-pack` crate.

## JS API (what the playground calls)

```ts
import init, { compile, compileWithMappings, compileVirtual } from "@nota-lang/nota-wasm";

await init();                          // load + instantiate the .wasm (default export; `target web`)

compile(source: string): { code: string };
//   the build path — emits the JS module (oxc::nota::compile).

compileWithMappings(source: string): { code: string; mappings: CodeMapping[] };
//   build + H1 Volar CodeMappings (oxc::nota::compile_with_mappings).

compileVirtual(source: string): { code: string; mappings: CodeMapping[] };
//   H2 type-preserving virtual `.tsx` emit + H1 CodeMappings (oxc::nota::compile_virtual).

// CodeMapping (contract §9 shape, camelCase):
//   { sourceOffsets: number[]; generatedOffsets: number[]; lengths: number[];
//     generatedLengths: number[] | null;
//     data: { completion, format, navigation, semantic, structure, verification: boolean } }
```

All three **throw** a `JsError` (a normal JS `Error`) on a Nota parse error; its `.message` is the
rendered diagnostics (one per line). Wrap calls in `try/catch` in the playground.

`init` is the default export (`__wbg_init`): in a browser/bundler it fetches `nota_wasm_bg.wasm` next
to the JS; in Node, read the `.wasm` bytes and pass them to the named `initSync(bytes)` export instead.

## Build

```sh
# from the oxc/ workspace root. Requires: rustup target add wasm32-unknown-unknown ; cargo install wasm-pack
wasm-pack build napi/nota_wasm --target web --out-dir pkg --out-name nota_wasm
```

Output: `napi/nota_wasm/pkg/` — the package the playground imports
(`nota_wasm.js`, `nota_wasm_bg.wasm`, `nota_wasm.d.ts`, `package.json`). The generated
`package.json` `name` is `nota_wasm`; the playground can `pnpm add`/alias it as `@nota-lang/nota-wasm`
(contract §5 scoped naming), or import the `pkg/` path directly.

Use `--target bundler` instead of `web` if the playground bundler (Vite) prefers the bundler glue.

### `wasm-opt` note

`[package.metadata.wasm-pack.profile.release] wasm-opt = false` is set in `Cargo.toml`: the Rust
toolchain here emits bulk-memory ops (`memory.fill`/`memory.copy`), and an older `wasm-opt` on `PATH`
rejects them without `--enable-bulk-memory-opt`, failing the optimize step (the `.wasm` itself compiles
+ bindgens fine). The `.wasm` ships unoptimized (~0.95 MB); re-enable with a newer `wasm-opt`
(`wasm-opt = ["-O", "--enable-bulk-memory"]`) or let Vite optimize it.

## Develop / verify (no wasm-pack needed for these)

```sh
cargo check -p nota_wasm                                  # native typecheck
cargo check -p nota_wasm --target wasm32-unknown-unknown  # the real target
cargo fmt -p nota_wasm -- --check
cargo clippy -p nota_wasm
```

[`wasm-bindgen`]: https://github.com/rustwasm/wasm-bindgen
[`serde-wasm-bindgen`]: https://github.com/cloudflare/serde-wasm-bindgen
