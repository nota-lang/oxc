# Nota wasm reader

`oxc::nota` compiled with `wasm-bindgen`. The crate lives under `napi/` for workspace-layout
reasons; it does not use napi-rs. `@nota-lang/compiler` vendors the generated bundler target.

The public reader operations are:

```ts
compile(source: string): NotaOutput; // strict, TypeScript-stripped build emit
analyze(source: string): NotaOutput; // recovered editor analysis
highlightKindNames(): string[];
emitSurface(): NotaEmitSurface;
lineClassifiers(): NotaLineClassifiers;
```

Both compilation paths return the same shape:

```ts
interface NotaOutput {
  code: string;
  freeNames: string[];
  mappings: CodeMapping[];
  errors: NotaDiagnostic[];
  ast: string | null;
  highlights: number[]; // flat [start, end, kind] triples
}
```

`analyze` parses once and derives its type-preserving TSX, mappings, recovered diagnostics, AST,
free names, and highlights from that parse. `compile` throws a `JsError` on invalid input and leaves
the editor-only fields empty.

Build from the oxc workspace root:

```sh
just nota-build
```

This writes the bundler glue and wasm to `target/js`. The main repository copies those artifacts
into `packages/compiler/src/generated` during the compiler build.
