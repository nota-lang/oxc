//! Nota wasm compiler backend — `wasm-bindgen` over the three `oxc::nota` entries.
//!
//! The Part-4 playground imports the `pkg/` `wasm-pack` produces from this crate and calls:
//!
//! ```ts
//! import init, { compile, compileWithMappings, compileVirtual } from "@nota-lang/nota-wasm";
//! await init();                                  // load + instantiate the .wasm
//! const { code } = compile(src);                 // build path (JS)            → { code }
//! const { code, mappings } = compileWithMappings(src); // build + H1 mappings → { code, mappings }
//! const { code, mappings } = compileVirtual(src);      // H2 .tsx + H1        → { code, mappings }
//! ```
//!
//! Each binding wraps the corresponding `oxc::nota` function. On a Nota parse error the entry returns
//! `Err(Vec<OxcDiagnostic>)`; we render the diagnostics to a single string and throw it as a
//! `JsError` (the playground catches it and shows the message). Success values are plain JS objects
//! produced by `serde-wasm-bindgen` from the `camelCase` mirror structs below.
//!
//! ## Why mirror structs (not `#[derive(Serialize)]` on `oxc::nota`'s types)
//!
//! `oxc::nota`'s `CodeMapping` / `MappingCapabilities` deliberately carry **no** `serde` derive — the
//! published `oxc` crate adds no `serde` dependency (see `crates/oxc/examples/nota_compile.rs`, which
//! hand-rolls the `--virtual` JSON for exactly this reason). To avoid forcing `serde` into `oxc`, this
//! crate defines its own `#[derive(Serialize)]` mirrors and a cheap `From` conversion. The JS shape is
//! identical to the `--virtual` binary's (contract §9): `{ sourceOffsets, generatedOffsets, lengths,
//! generatedLengths, data: { completion, format, navigation, semantic, structure, verification } }`.

use std::fmt::Write as _;

use oxc::allocator::Allocator;
use oxc::diagnostics::OxcDiagnostic;
use oxc::nota::{
    self, CodeMapping as OxcCodeMapping, MappingCapabilities as OxcMappingCapabilities,
};
use oxc::parser::Parser;
use oxc::span::SourceType;
use serde::Serialize;
use wasm_bindgen::prelude::*;

// ===================================================================================================
// TypeScript surface for the playground. wasm-bindgen types our `JsValue` returns as `any`; this
// `typescript_custom_section` appends real named interfaces to the generated `.d.ts` so the
// playground can annotate results (e.g. `compile(src) as NotaCompileResult`). Kept byte-for-byte in
// sync with the `#[derive(Serialize)]` mirrors below + contract §9.
// ===================================================================================================
#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &'static str = r#"
/** The six Volar `CodeInformation` capability flags for a mapped range (contract §4 H1). */
export interface NotaMappingCapabilities {
  completion: boolean;
  format: boolean;
  navigation: boolean;
  semantic: boolean;
  structure: boolean;
  verification: boolean;
}

/** One Volar `CodeMapping` — parallel source⇄generated offset arrays + capability flags (contract §9). */
export interface NotaCodeMapping {
  sourceOffsets: number[];
  generatedOffsets: number[];
  lengths: number[];
  /** `null` when the generated length equals the source length for every segment. */
  generatedLengths: number[] | null;
  data: NotaMappingCapabilities;
}

/** Result of `compile`: the emitted JS module source. */
export interface NotaCompileResult {
  code: string;
}

/** Result of `compileWithMappings` / `compileVirtual`: emitted code + Volar CodeMappings (H1). */
export interface NotaMappedResult {
  code: string;
  mappings: NotaCodeMapping[];
}

/** Result of `parseAst`: the post-parse Nota AST as an ESTree JSON string (with `start`/`end`). */
export interface NotaParseAstResult {
  ast: string;
}
"#;

// ===================================================================================================
// Serializable mirrors of the `oxc::nota` result shapes (camelCase for the JS playground).
// ===================================================================================================

/// `{ code }` — the build-path result ([`nota::compile`]).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompileResult {
    /// The emitted JS module source.
    code: String,
}

/// `{ ast }` — the post-parse Nota AST as an ESTree JSON string (document mode, parser stage only).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ParseAstResult {
    /// `Program::to_estree_js_json(true)` — JSON with per-node `type` + `start`/`end`.
    ast: String,
}

/// `{ code, mappings }` — the H1/H2 result ([`nota::compile_with_mappings`] / [`nota::compile_virtual`]).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MappedResult {
    /// The emitted module source (JS for `compileWithMappings`, virtual `.tsx` for `compileVirtual`).
    code: String,
    /// The Volar `CodeMapping`s (H1).
    mappings: Vec<CodeMapping>,
}

/// Mirror of [`oxc::nota::CodeMapping`] (contract §9 `--virtual` JSON shape).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeMapping {
    source_offsets: Vec<u32>,
    generated_offsets: Vec<u32>,
    lengths: Vec<u32>,
    /// `None` ⇒ generated length equals source length for every segment (serializes to `null`).
    generated_lengths: Option<Vec<u32>>,
    data: MappingCapabilities,
}

/// Mirror of [`oxc::nota::MappingCapabilities`] (the six Volar `CodeInformation` flags).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MappingCapabilities {
    completion: bool,
    format: bool,
    navigation: bool,
    semantic: bool,
    structure: bool,
    verification: bool,
}

impl From<OxcMappingCapabilities> for MappingCapabilities {
    fn from(c: OxcMappingCapabilities) -> Self {
        Self {
            completion: c.completion,
            format: c.format,
            navigation: c.navigation,
            semantic: c.semantic,
            structure: c.structure,
            verification: c.verification,
        }
    }
}

impl From<&OxcCodeMapping> for CodeMapping {
    fn from(m: &OxcCodeMapping) -> Self {
        Self {
            source_offsets: m.source_offsets.clone(),
            generated_offsets: m.generated_offsets.clone(),
            lengths: m.lengths.clone(),
            generated_lengths: m.generated_lengths.clone(),
            data: m.data.into(),
        }
    }
}

fn map_mappings(mappings: &[OxcCodeMapping]) -> Vec<CodeMapping> {
    mappings.iter().map(CodeMapping::from).collect()
}

// ===================================================================================================
// Error rendering: `Vec<OxcDiagnostic>` → a single thrown `JsError`.
// ===================================================================================================

/// Render collected Nota diagnostics into one human-readable string for the thrown `JsError`. One
/// diagnostic per line (the playground shows this to the author).
fn diagnostics_to_error(errors: &[OxcDiagnostic]) -> JsError {
    let mut message = String::new();
    for (i, error) in errors.iter().enumerate() {
        if i > 0 {
            message.push('\n');
        }
        // `OxcDiagnostic`'s `Display` is the concise message (the `Debug` form adds the report frame,
        // which is noisy without source context); concatenate the messages.
        let _ = write!(message, "{error}");
    }
    if message.is_empty() {
        message.push_str("nota: compilation failed");
    }
    JsError::new(&message)
}

/// Serialize a `#[derive(Serialize)]` value into a JS value, mapping any serializer error to a
/// `JsError` (serialization of these plain structs cannot realistically fail, but stay total).
fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsError::new(&e.to_string()))
}

// ===================================================================================================
// The three exported entries (the playground's JS API).
// ===================================================================================================

/// Compile a `.nota` source string to a JS module. Returns `{ code }`.
///
/// JS: `compile(source: string): { code: string }` — throws on a Nota parse error.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen]
pub fn compile(source: &str) -> Result<JsValue, JsError> {
    match nota::compile(source, None) {
        // No `source_map_path`: the playground renders the `code`; a flat sourcemap is not needed.
        Ok(compiled) => to_js(&CompileResult { code: compiled.code }),
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Parse a `.nota` source and return its **post-parse Nota AST** as ESTree JSON. Returns `{ ast }`,
/// where `ast` is a JSON string the playground `JSON.parse`s and renders as a collapsible tree.
///
/// This is the parser stage only — no lowering, no codegen — so it is the faithful Nota tree
/// (`NotaDocument` / `NotaHeading` / `NotaElement` / …) the reader builds before lowering to
/// hyperscript. Serialized via `oxc_ast`'s ESTree serializer (every Nota node `#[generate_derive]`s
/// `ESTree`); `ranges = true` so each node carries `start`/`end` offsets, letting the tree slice a
/// one-line source preview from the editor text.
///
/// JS: `parseAst(source: string): { ast: string }` — throws on a Nota parse error.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen(js_name = parseAst)]
pub fn parse_ast(source: &str) -> Result<JsValue, JsError> {
    // One arena for the parse; the `Program` borrows from it, so serialize before it drops.
    // `tsx` is the canonical Nota parse mode (embedded TS admitted), matching the compile entries.
    let allocator = Allocator::default();
    match Parser::new(&allocator, source, SourceType::tsx()).parse_nota_document() {
        Ok(program) => to_js(&ParseAstResult { ast: program.to_estree_js_json(true) }),
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Compile a `.nota` source to JS **plus** structured Volar [`CodeMapping`]s (H1).
/// Returns `{ code, mappings }`.
///
/// JS: `compileWithMappings(source: string): { code: string, mappings: CodeMapping[] }`.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen(js_name = compileWithMappings)]
pub fn compile_with_mappings(source: &str) -> Result<JsValue, JsError> {
    match nota::compile_with_mappings(source, None) {
        Ok(compiled) => {
            to_js(&MappedResult { code: compiled.code, mappings: map_mappings(&compiled.mappings) })
        }
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Compile a `.nota` source to the type-preserving **virtual `.tsx`** emit + CodeMappings (H2 + H1).
/// Returns `{ code, mappings }` (contract §9 — the language-server / playground virtual view).
///
/// JS: `compileVirtual(source: string): { code: string, mappings: CodeMapping[] }`.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen(js_name = compileVirtual)]
pub fn compile_virtual(source: &str) -> Result<JsValue, JsError> {
    match nota::compile_virtual(source) {
        Ok(compiled) => {
            to_js(&MappedResult { code: compiled.code, mappings: map_mappings(&compiled.mappings) })
        }
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Wire the panic hook on module load so a Rust panic surfaces as a readable `console.error` in the
/// browser (wasm-bindgen calls `start` automatically after instantiation).
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}
