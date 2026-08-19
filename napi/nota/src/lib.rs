use std::fmt::Write as _;

use oxc::diagnostics::OxcDiagnostic;
use oxc::nota::{self, NotaOutput};
use oxc::parser::NotaHighlightKind;
use serde::Serialize;
use tsify::Tsify;
use wasm_bindgen::prelude::*;

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

/// Compile a `.nota` source string to stripped Solid JSX.
///
/// # Errors
/// Returns a `JsError` (thrown in JS) carrying the rendered diagnostics if `source` is not
/// well-formed Nota.
#[wasm_bindgen]
pub fn compile(source: &str) -> Result<NotaOutput, JsError> {
    match nota::compile(source, None) {
        Ok(compiled) => Ok(compiled),
        Err(errors) => Err(diagnostics_to_error(&errors)),
    }
}

/// Parse once and return virtual TSX, mappings, errors, AST, and highlights.
#[wasm_bindgen]
pub fn analyze(source: &str) -> NotaOutput {
    nota::analyze(source)
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
