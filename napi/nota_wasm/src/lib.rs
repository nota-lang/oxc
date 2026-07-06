//! Nota wasm compiler backend — `wasm-bindgen` over the `oxc::nota` entries.
//!
//! The Part-4 playground imports the `pkg/` `wasm-pack` produces from this crate and calls:
//!
//! ```ts
//! import init, { compile, compileWithMappings, compileVirtual, parseAst,
//!                highlight, highlightKindNames } from "@nota-lang/nota-wasm";
//! await init();                                  // load + instantiate the .wasm
//! const { code } = compile(src);                 // build path (JS)            → { code }
//! const { code, mappings } = compileWithMappings(src); // build + Volar CodeMappings → { code, mappings }
//! const { code, mappings, errors } = compileVirtual(src); // virtual .tsx + mappings + recovered diagnostics
//! const { ast } = parseAst(src);                 // post-parse Nota AST (ESTree JSON string)
//! const spans = highlight(src);                  // [start, end, kind] u32 triples (editor spans)
//! const names = highlightKindNames();            // kind discriminant → kebab-case name
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
//! identical to the `--virtual` binary's (NOTA_READER.md §Compiler entries): `{ sourceOffsets, generatedOffsets, lengths,
//! generatedLengths, data: { completion, format, navigation, semantic, structure, verification } }`.

use std::fmt::Write as _;

use oxc::allocator::Allocator;
use oxc::diagnostics::OxcDiagnostic;
use oxc::nota::{
    self, CodeMapping as OxcCodeMapping, MappingCapabilities as OxcMappingCapabilities,
};
use oxc::parser::{NotaHighlightKind, Parser};
use oxc::span::SourceType;
use serde::Serialize;
use wasm_bindgen::prelude::*;

// ===================================================================================================
// TypeScript surface for the playground. wasm-bindgen types our `JsValue` returns as `any`; this
// `typescript_custom_section` appends real named interfaces to the generated `.d.ts` so the
// playground can annotate results (e.g. `compile(src) as NotaCompileResult`). Kept byte-for-byte in
// sync with the `#[derive(Serialize)]` mirrors below + the `--virtual` JSON shape
// (NOTA_READER.md §Compiler entries).
// ===================================================================================================
#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &'static str = r#"
/** The six Volar `CodeInformation` capability flags for a mapped range. */
export interface NotaMappingCapabilities {
  completion: boolean;
  format: boolean;
  navigation: boolean;
  semantic: boolean;
  structure: boolean;
  verification: boolean;
}

/** One Volar `CodeMapping` — parallel source⇄generated offset arrays + capability flags (the `--virtual` JSON shape). */
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

/** Result of `compileWithMappings`: emitted code + Volar CodeMappings. */
export interface NotaMappedResult {
  code: string;
  mappings: NotaCodeMapping[];
}

/** One recovered Nota syntax/lowering diagnostic (byte-spanned into the `.nota`). */
export interface NotaError {
  message: string;
  start: number;
  len: number;
}

/**
 * Result of `compileVirtual`: the type-preserving virtual `.tsx` + CodeMappings **plus** any
 * recovered diagnostics. The virtual path uses EOF error-recovery, so it never throws on malformed
 * markup — the syntax problems come back in `errors` for the language server to surface.
 */
export interface NotaVirtualResult {
  code: string;
  mappings: NotaCodeMapping[];
  errors: NotaError[];
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

/// `{ code, mappings }` — the mapped result ([`nota::compile_with_mappings`] / [`nota::compile_virtual`]).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MappedResult {
    /// The emitted module source (JS for `compileWithMappings`, virtual `.tsx` for `compileVirtual`).
    code: String,
    /// The Volar `CodeMapping`s.
    mappings: Vec<CodeMapping>,
}

/// `{ code, mappings, errors }` — the recovered virtual result ([`nota::compile_virtual`]).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VirtualResult {
    code: String,
    mappings: Vec<CodeMapping>,
    errors: Vec<NotaErrorJs>,
}

/// Mirror of a recovered diagnostic as the `{ message, start, len }` JSON the shim expects.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotaErrorJs {
    message: String,
    start: u32,
    len: u32,
}

impl NotaErrorJs {
    /// Extract `{message, start, len}` from an `OxcDiagnostic` — the first label's byte span, or
    /// `(0, 0)` when the diagnostic carries no label. Mirrors the binary's `--virtual` error shape.
    fn from_diagnostic(error: &OxcDiagnostic) -> Self {
        let (start, len) = error
            .labels
            .as_ref()
            .and_then(|labels| labels.first())
            .map_or((0u32, 0u32), |label| {
                (label.offset() as u32, label.len() as u32)
            });
        Self { message: error.to_string(), start, len }
    }
}

/// Mirror of [`oxc::nota::CodeMapping`] (the `--virtual` JSON shape — NOTA_READER.md §Compiler entries).
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
// The exported entries (the playground's JS API): three compile paths, the AST view, and the
// highlight spans.
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
    // `nota` is the canonical Nota parse mode (embedded TS admitted), matching the compile entries.
    let allocator = Allocator::default();
    match Parser::new(&allocator, source, SourceType::nota()).parse_nota_document() {
        Ok(program) => to_js(&ParseAstResult { ast: program.to_estree_js_json(true) }),
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Compile a `.nota` source to JS **plus** structured Volar [`CodeMapping`]s.
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

/// Compile a `.nota` source to the type-preserving **virtual `.tsx`** emit + CodeMappings.
/// Returns `{ code, mappings }` — the language-server / playground virtual view
/// (NOTA_READER.md §Compiler entries).
///
/// JS: `compileVirtual(source: string): { code: string, mappings: CodeMapping[] }`.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen(js_name = compileVirtual)]
pub fn compile_virtual(source: &str) -> Result<JsValue, JsError> {
    match nota::compile_virtual(source) {
        Ok(compiled) => to_js(&VirtualResult {
            code: compiled.code,
            mappings: map_mappings(&compiled.mappings),
            errors: compiled.errors.iter().map(NotaErrorJs::from_diagnostic).collect(),
        }),
        // Practically unreachable on the recovery path; kept total.
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Reader-faithful syntax highlighting: classified spans for the whole `.nota` source, flattened
/// to `[start, end, kind]` triples (byte offsets; `kind` indexes [`highlight_kind_names`]).
///
/// The editor-tooling view of the parse (`Parser::parse_nota_highlights`, a parser-stage entry
/// like `parseAst`'s — it never reaches the lowering, so it is not part of `oxc::nota`'s compile
/// seam). This crate owns the editor-facing encoding: the flat triples and the kind→name table.
///
/// JS: `highlight(source: string): Uint32Array` — throws on a Nota parse error (the editor keeps
/// its last-good spans while a document is mid-edit).
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen]
pub fn highlight(source: &str) -> Result<Vec<u32>, JsError> {
    let allocator = Allocator::default();
    // `nota` is the canonical Nota parse mode, matching the compile entries and `parseAst`.
    match Parser::new(&allocator, source, SourceType::nota()).parse_nota_highlights() {
        Ok(spans) => {
            let mut flat = Vec::with_capacity(spans.len() * 3);
            for span in spans {
                flat.push(span.start);
                flat.push(span.end);
                flat.push(u32::from(span.kind as u8));
            }
            Ok(flat)
        }
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// The stable kebab-case, CSS-class-ready name of a highlight kind. Lives here (not in the
/// reader) because naming is an editor-surface concern; the match is exhaustive, so a new reader
/// kind fails this crate's build until it is named.
fn highlight_kind_name(kind: NotaHighlightKind) -> &'static str {
    match kind {
        NotaHighlightKind::Sigil => "sigil",
        NotaHighlightKind::TagHost => "tag-host",
        NotaHighlightKind::TagComponent => "tag-component",
        NotaHighlightKind::PropName => "prop-name",
        NotaHighlightKind::Interpolation => "interpolation",
        NotaHighlightKind::ControlKeyword => "control-keyword",
        NotaHighlightKind::HeadingMarker => "heading-marker",
        NotaHighlightKind::Heading => "heading",
        NotaHighlightKind::ListMarker => "list-marker",
        NotaHighlightKind::EmphasisStrong => "emphasis-strong",
        NotaHighlightKind::EmphasisEm => "emphasis-em",
        NotaHighlightKind::MathDelim => "math-delim",
        NotaHighlightKind::Math => "math",
        NotaHighlightKind::CodeDelim => "code-delim",
        NotaHighlightKind::CodeLang => "code-lang",
        NotaHighlightKind::Code => "code",
        NotaHighlightKind::Verbatim => "verbatim",
        NotaHighlightKind::Escape => "escape",
        NotaHighlightKind::JsKeyword => "js-keyword",
        NotaHighlightKind::JsString => "js-string",
        NotaHighlightKind::JsNumber => "js-number",
        NotaHighlightKind::JsComment => "js-comment",
        NotaHighlightKind::JsOperator => "js-operator",
        NotaHighlightKind::StyleText => "style-text",
    }
}

/// The name of every highlight kind, in discriminant order — index a triple's `kind` into this
/// to get its CSS-class-ready name (e.g. `0` → `"sigil"`, `1` → `"tag-host"`).
///
/// JS: `highlightKindNames(): string[]`.
#[wasm_bindgen(js_name = highlightKindNames)]
pub fn highlight_kind_names() -> Vec<String> {
    NotaHighlightKind::ALL.iter().map(|kind| highlight_kind_name(*kind).to_string()).collect()
}

/// Wire the panic hook on module load so a Rust panic surfaces as a readable `console.error` in the
/// browser (wasm-bindgen calls `start` automatically after instantiation).
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}
