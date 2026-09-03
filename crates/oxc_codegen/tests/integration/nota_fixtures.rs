//! Shared-fixture consumption: the main repo's `integration/*.nota` fixtures, compiled end-to-end.
//!
//! Each fixture goes through parse → lower → codegen with zero diagnostics, the emit-validity
//! invariant, and a few targeted Solid-JSX markers asserted. The fixtures live in the MAIN Nota
//! repo — this oxc fork is its git submodule at `<nota>/oxc` — so they are read at RUNTIME
//! relative to `CARGO_MANIFEST_DIR` (= `<nota>/oxc/crates/oxc_codegen`, hence
//! `../../../integration/…`). In a standalone oxc checkout (the fork's own CI) the files are
//! absent and each test SKIPS gracefully instead of failing.

use std::path::PathBuf;

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::nota::assert_valid_js;

/// `<nota>/integration/<name>`, computed from this crate's manifest dir.
fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../integration").join(name)
}

/// Compile one fixture document: parse (zero diagnostics) → lower (zero diagnostics) → codegen →
/// the validity invariant. Returns `None` (after an eprintln) when the fixture is absent — a
/// standalone oxc checkout without the parent repo.
#[expect(clippy::print_stderr)] // the graceful-skip notice must be visible in test output
fn compile_fixture(name: &str) -> Option<String> {
    let path = fixture_path(name);
    let Ok(source) = std::fs::read_to_string(&path) else {
        eprintln!("SKIP: shared fixture not found at {} (standalone oxc checkout)", path.display());
        return None;
    };
    let allocator = Allocator::default();
    let mut program = Parser::new(&allocator, &source, SourceType::nota())
        .parse_nota_document()
        .into_result()
        .unwrap_or_else(|errors| panic!("{name}: parse diagnostics: {errors:?}"));
    let ret = oxc_transformer::NotaLowering::new(&allocator, &source, false)
        .lower_document_program(&mut program);
    assert!(ret.diagnostics.is_empty(), "{name}: lowering diagnostics: {:?}", ret.diagnostics);
    let js = Codegen::new().build(&program).code;
    assert_valid_js(&js);
    Some(js)
}

#[test]
fn nota_fixture_mega_compiles_clean() {
    // The feature mega-test: control flow, dynamic tags, raw spans, doc-state, components.
    let Some(js) = compile_fixture("mega.nota") else { return };
    for marker in [
        "<Show when=",
        "<For each=",
        "<Dynamic component=",
        "<Tex display",
        "<Heading rank=",
        "<UlLi",
    ] {
        assert!(js.contains(marker), "mega.nota emit lacks {marker}: {js}");
    }
    assert!(!js.contains("import "), "the reader emits no imports: {js}");
}

#[test]
fn nota_fixture_prose_sugars_compiles_clean() {
    // The 2026-08 prose sugars: strikethrough, thematic break, attrs groups (hoisted + marker),
    // comments-as-trivia.
    let Some(js) = compile_fixture("prose-sugars.nota") else { return };
    for marker in [
        r#"<s>{"struck"}</s>"#,
        "<hr />",
        r#"<Attrs class="note" />"#, // flow-position marker
        r#"<Heading rank={1} id="sugars" class="demo">"#, // hoisted heading attrs
        r#"<UlLi class="hot">"#,     // hoisted item attrs
    ] {
        assert!(js.contains(marker), "prose-sugars.nota emit lacks {marker}: {js}");
    }
    assert!(!js.contains("import "), "the reader emits no imports: {js}");
}
