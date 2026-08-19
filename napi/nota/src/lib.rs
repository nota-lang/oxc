use std::fmt::Write as _;

use oxc::allocator::Allocator;
use oxc::diagnostics::OxcDiagnostic;
use oxc::nota::{self, CodeMapping, MappingCapabilities};
use oxc::parser::{NotaHighlightKind, Parser};
use oxc::span::SourceType;
use serde::Serialize;
use tsify::Tsify;
use wasm_bindgen::prelude::*;

/// The six Volar `CodeInformation` capability flags for a mapped range.
#[derive(Serialize, Tsify)]
#[tsify(missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaMappingCapabilities {
    pub completion: bool,
    pub format: bool,
    pub navigation: bool,
    pub semantic: bool,
    pub structure: bool,
    pub verification: bool,
}

/// One Volar `CodeMapping` — parallel source⇄generated offset arrays + capability flags (the
/// `--virtual` JSON shape).
#[derive(Serialize, Tsify)]
#[tsify(missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaCodeMapping {
    pub source_offsets: Vec<u32>,
    pub generated_offsets: Vec<u32>,
    pub lengths: Vec<u32>,
    /// `null` when the generated length equals the source length for every segment.
    pub generated_lengths: Option<Vec<u32>>,
    pub data: NotaMappingCapabilities,
}

/// Result of `compile`: the emitted JS module source + its free names.
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaCompileResult {
    /// The emitted JS module source.
    pub code: String,
    /// The module's free (value-position, root-unresolved) names, sorted — the runtime surface plus
    /// the ambient-prelude refs plus any unbound user names. The `@nota-lang/compiler` shim
    /// intersects this with its ambient-name set to synthesize the prelude import.
    pub free_names: Vec<String>,
}

/// Result of `compileWithMappings`: emitted code + Volar CodeMappings.
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaMappedResult {
    /// The emitted JS module source, types preserved (`compileWithMappings` only — `compileVirtual`
    /// returns its own [`NotaVirtualResult`], which additionally carries recovered `errors`).
    pub code: String,
    /// The Volar `CodeMapping`s.
    pub mappings: Vec<NotaCodeMapping>,
}

/// One recovered Nota syntax/lowering diagnostic (byte-spanned into the `.nota`).
#[derive(Serialize, Tsify)]
#[tsify(missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaError {
    pub message: String,
    pub start: u32,
    pub len: u32,
}

/// Result of `compileVirtual`: the type-preserving virtual `.tsx` + CodeMappings **plus** any
/// recovered diagnostics.
///
/// The virtual path uses EOF error-recovery, so it never throws on malformed markup — the syntax
/// problems come back in `errors` for the language server to surface.
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaVirtualResult {
    pub code: String,
    pub mappings: Vec<NotaCodeMapping>,
    pub errors: Vec<NotaError>,
}

/// Result of `parseAst`: the post-parse Nota AST as an ESTree JSON string (with `start`/`end`).
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaParseAstResult {
    /// `Program::to_estree_js_json(true)` — JSON with per-node `type` + `start`/`end`.
    pub ast: String,
}

impl NotaError {
    /// Extract `{message, start, len}` from an `OxcDiagnostic` — the first label's byte span, or
    /// `(0, 0)` when the diagnostic carries no label. Mirrors the binary's `--virtual` error shape.
    fn from_diagnostic(error: &OxcDiagnostic) -> Self {
        let (start, len) = error
            .labels
            .as_ref()
            .and_then(|labels| labels.first())
            .map_or((0u32, 0u32), |label| (label.offset() as u32, label.len() as u32));
        Self { message: error.to_string(), start, len }
    }
}

impl From<MappingCapabilities> for NotaMappingCapabilities {
    fn from(c: MappingCapabilities) -> Self {
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

impl From<&CodeMapping> for NotaCodeMapping {
    fn from(m: &CodeMapping) -> Self {
        Self {
            source_offsets: m.source_offsets.clone(),
            generated_offsets: m.generated_offsets.clone(),
            lengths: m.lengths.clone(),
            generated_lengths: m.generated_lengths.clone(),
            data: m.data.into(),
        }
    }
}

fn map_mappings(mappings: &[CodeMapping]) -> Vec<NotaCodeMapping> {
    mappings.iter().map(NotaCodeMapping::from).collect()
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

// ===================================================================================================
// The exported entries (the playground's JS API): three compile paths, the AST view, and the
// highlight spans.
//
// Sourcemap channel: the native `compile`/`compile_with_mappings` take a `source_map_path` and can
// return a `SourceMap` (`oxc::nota::NotaCompiled::map` / `NotaCompiledWithMappings`); neither wasm
// wrapper below exposes it — `source_map_path` is always passed `None`, and `NotaCompileResult`/
// `NotaMappedResult` carry no `map` field. This is a **recorded scope decision, not an oversight**:
// today's wasm consumers (playground, language server) work off `code` + Volar `CodeMapping`s and
// don't need a standalone sourcemap. Revisit if a consumer needs one.
// ===================================================================================================

/// Compile a `.nota` source string to a JS module. Returns `{ code, freeNames }`.
///
/// JS: `compile(source: string): { code: string, freeNames: string[] }` — throws on a Nota parse
/// error.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen]
pub fn compile(source: String) -> Result<NotaCompileResult, JsError> {
    match nota::compile(&source, None) {
        // No `source_map_path`: the playground renders the `code`; a flat sourcemap is not needed.
        Ok(compiled) => {
            Ok(NotaCompileResult { code: compiled.code, free_names: compiled.free_names })
        }
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Parse a `.nota` source and return its **post-parse Nota AST** as ESTree JSON. Returns `{ ast }`,
/// where `ast` is a JSON string the playground `JSON.parse`s and renders as a collapsible tree.
///
/// This is the parser stage only — no lowering, no codegen — so it is the faithful Nota tree
/// (`NotaDocument` / `NotaHeading` / `NotaElement` / …) the reader builds before lowering to
/// Solid JSX. Serialized via `oxc_ast`'s ESTree serializer (every Nota node `#[generate_derive]`s
/// `ESTree`); `ranges = true` so each node carries `start`/`end` offsets, letting the tree slice a
/// one-line source preview from the editor text.
///
/// JS: `parseAst(source: string): { ast: string }` — throws on a Nota parse error.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen(js_name = parseAst)]
pub fn parse_ast(source: &str) -> Result<NotaParseAstResult, JsError> {
    // One arena for the parse; the `Program` borrows from it, so serialize before it drops.
    // `nota` is the canonical Nota parse mode (embedded TS admitted), matching the compile entries.
    let allocator = Allocator::default();
    match Parser::new(&allocator, source, SourceType::nota()).parse_nota_document() {
        Ok(program) => Ok(NotaParseAstResult { ast: program.to_estree_js_json(true) }),
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
pub fn compile_with_mappings(source: &str) -> Result<NotaMappedResult, JsError> {
    // No `source_map_path`; `NotaMappedResult` carries no `map` field either — the sourcemap
    // channel is deliberately not exposed over wasm yet (see the section note above).
    match nota::compile_with_mappings(source, None) {
        Ok(compiled) => {
            Ok(NotaMappedResult { code: compiled.code, mappings: map_mappings(&compiled.mappings) })
        }
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Compile a `.nota` source to the type-preserving **virtual `.tsx`** emit + CodeMappings + any
/// recovered diagnostics. Returns `{ code, mappings, errors }` — the language-server / playground
/// virtual view (NOTA_READER.md §Compiler entries).
///
/// JS: `compileVirtual(source: string): { code: string, mappings: CodeMapping[], errors: {
/// message: string, start: number, len: number }[] }`.
///
#[wasm_bindgen(js_name = compileVirtual)]
pub fn compile_virtual(source: &str) -> NotaVirtualResult {
    let compiled = nota::compile_virtual(source);
    NotaVirtualResult {
        code: compiled.code,
        mappings: map_mappings(&compiled.mappings),
        errors: compiled.errors.iter().map(NotaError::from_diagnostic).collect(),
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
        NotaHighlightKind::Comment => "comment",
        NotaHighlightKind::EmphasisStrike => "emphasis-strike",
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

/// The reader's **emit surface**, grouped by binding module — the introspectable truth
/// `@nota-lang/compiler` derives its name lists from (no hand-copied TS mirror). The groups are
/// the `oxc_transformer` `*_EMIT_NAMES` constants verbatim; `reserved` is `Doc` + all groups (the
/// binding-collision set); `flowTags` is the emit's `<Reforest>`-interior policy list.
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaEmitSurface {
    /// `@nota-lang/core` structural names (`NotaDoc`, `Reforest`, …).
    pub structural: Vec<String>,
    /// `solid-js` names the lowering emits (`For`, `Show`).
    pub solid: Vec<String>,
    /// `solid-js/web` names the lowering emits (`Dynamic`).
    pub solid_web: Vec<String>,
    /// Ambient-prelude names the lowering emits free (`Tex`, `Heading`, …).
    pub prelude: Vec<String>,
    /// Every reserved emit name (`Doc` + all groups) — user bindings of these are diagnosed.
    pub reserved: Vec<String>,
    /// Host tags whose interior the emit wraps in `<Reforest>`.
    pub flow_tags: Vec<String>,
}

/// The emit surface — see [`NotaEmitSurface`].
///
/// JS: `emitSurface(): NotaEmitSurface`.
#[wasm_bindgen(js_name = emitSurface)]
pub fn emit_surface() -> NotaEmitSurface {
    use oxc::transformer::{
        FLOW_TAGS, PRELUDE_EMIT_NAMES, SOLID_EMIT_NAMES, SOLID_WEB_EMIT_NAMES,
        STRUCTURAL_EMIT_NAMES, reserved_emit_names,
    };
    let own = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect();
    NotaEmitSurface {
        structural: own(STRUCTURAL_EMIT_NAMES),
        solid: own(SOLID_EMIT_NAMES),
        solid_web: own(SOLID_WEB_EMIT_NAMES),
        prelude: own(PRELUDE_EMIT_NAMES),
        reserved: own(&reserved_emit_names()),
        flow_tags: own(FLOW_TAGS),
    }
}

/// The reader's line-classifier regex patterns (the `regex` crate originals, JS-compatible) —
/// the truth that editor line-tier transliterations (the LSP's delegated-line walk, emacs
/// font-lock) consume or are checked against.
#[derive(Serialize, Tsify)]
#[tsify(into_wasm_abi, missing_as_null)]
#[serde(rename_all = "camelCase")]
pub struct NotaLineClassifiers {
    pub percent_line: String,
    pub fence_line: String,
    pub fence_close_line: String,
    pub empty_statement: String,
    pub heading: String,
    pub list_marker: String,
    pub prop_line: String,
}

/// The line classifiers — see [`NotaLineClassifiers`].
///
/// JS: `lineClassifiers(): NotaLineClassifiers`.
#[wasm_bindgen(js_name = lineClassifiers)]
pub fn line_classifiers() -> NotaLineClassifiers {
    let sources = oxc::parser::line_classifier_sources();
    NotaLineClassifiers {
        percent_line: sources.percent_line.to_string(),
        fence_line: sources.fence_line.to_string(),
        fence_close_line: sources.fence_close_line.to_string(),
        empty_statement: sources.empty_statement.to_string(),
        heading: sources.heading.to_string(),
        list_marker: sources.list_marker.to_string(),
        prop_line: sources.prop_line.to_string(),
    }
}

/// Wire the panic hook on module load so a Rust panic surfaces as a readable `console.error` in the
/// browser (wasm-bindgen calls `start` automatically after instantiation).
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}
