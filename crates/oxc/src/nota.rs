//! Nota compiler entry — the `nota source → { code, map }` seam.
//!
//! This is the single callable the Wave-3 compiler shim (`@nota-lang/compiler`, the wasm/napi
//! wrapper) builds on, and the entry used for the cross-stream integration loop (contract §2). It
//! lives in the `oxc` umbrella crate because that is the only place with *both* the Nota reader
//! (`oxc_parser`, document mode) and `oxc_codegen` available (`oxc_codegen` only dev-depends on
//! `oxc_parser`, so the combined entry cannot live in either of them — see `NOTA_READER.md`).
//!
//! The runtime import (`import { h, decode, Fragment, inlineComponent, blockComponent } from
//! "@nota-lang/runtime"`) is *not* emitted here; the shim/integrator prepends it (contract §1).

use std::path::PathBuf;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// The result of compiling a `.nota` source string.
pub struct NotaCompiled {
    /// The emitted JS module source (document mode: `export default function Doc() { … }`).
    pub code: String,
    /// The source map, if `source_map_path` was provided.
    pub map: Option<oxc_sourcemap::SourceMap>,
}

/// Compile a `.nota` source string to a JS module (+ optional source map).
///
/// Parses the whole file in Nota *document mode* (markup at the top level → `Doc`) and runs
/// `oxc_codegen`. On a parse error, returns the collected diagnostics (`Err`); the reader is a pure
/// function `String → (JS, map, diagnostics)` (impl.md §1.6).
///
/// `source_map_path` controls whether a source map is generated (it names the source in the map);
/// pass `None` to skip map generation (faster).
///
/// # Errors
/// If the source is not well-formed Nota.
pub fn compile(
    source_text: &str,
    source_map_path: Option<PathBuf>,
) -> Result<NotaCompiled, Vec<OxcDiagnostic>> {
    let allocator = Allocator::default();
    let program =
        Parser::new(&allocator, source_text, SourceType::default()).parse_nota_document()?;

    let options = CodegenOptions { source_map_path, ..CodegenOptions::default() };
    let CodegenReturn { code, map, .. } = Codegen::new().with_options(options).build(&program);

    Ok(NotaCompiled { code, map })
}

#[cfg(test)]
mod tests {
    use super::compile;

    #[test]
    fn compile_basic_document() {
        let out = compile("@h1{Hello}\n", None).expect("compiles");
        assert!(out.code.contains("export default function Doc()"), "{}", out.code);
        assert!(out.code.contains(r#"h("h1", {}, ["Hello"])"#), "{}", out.code);
        assert!(out.map.is_none());
    }

    #[test]
    fn compile_emits_source_map_when_requested() {
        let out = compile("@p{hi}\n", Some("doc.nota".into())).expect("compiles");
        assert!(out.map.is_some(), "source map present");
    }

    #[test]
    fn compile_reports_errors() {
        match compile("@p{unterminated", None) {
            Ok(_) => panic!("expected a diagnostic for an unterminated body"),
            Err(errors) => assert!(!errors.is_empty()),
        }
    }
}
