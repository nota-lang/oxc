//! Nota reader end-to-end fixtures: `.nota` source → Nota parse → codegen JS string.
//!
//! These are the golden/snapshot tests for the Nota reader (impl.md §1.6 layer 1). Two emit modes:
//! * **expression mode** (`nota_expr`) — elides the `Doc` wrapper and injected imports, matching
//!   notation.md / contract §3 (`@p{Hello}` → `h("p", {}, ["Hello"])`); the bulk of fixtures.
//! * **document mode** (`nota_doc`) — the full module incl. `export default function Doc()`,
//!   hoisted `import`/`export`, `decode(...)` wrap, F1, `await`→`async` (contract §2/§C).
//!
//! Every fixture also asserts the *validity invariant* (impl.md §1.6 layer 4): the emitted JS
//! re-parses cleanly under the STOCK oxc parser.

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Reformat a JS string by re-parsing and re-printing it through `Codegen`, so two strings that
/// differ only in formatting (e.g. the codegen wraps a >2-element array across lines) compare equal.
/// This is how the fixtures honor the contract's "modulo formatting" clause.
#[track_caller]
fn reformat(js: &str) -> String {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, js, SourceType::default().with_module(true)).parse();
    assert!(ret.errors.is_empty(), "reformat: not valid JS: {js:?}\n{:?}", ret.errors);
    Codegen::new().build(&ret.program).code
}

/// Assert that the Nota-emitted JS equals `expected` modulo formatting.
#[track_caller]
fn assert_js_eq(emitted: &str, expected: &str) {
    assert_eq!(
        reformat(emitted),
        reformat(expected),
        "\nemitted:  {emitted}\nexpected: {expected}"
    );
}

/// Compile a single Nota expression to a JS string (expression mode).
#[track_caller]
fn nota_expr_raw(source: &str) -> String {
    let allocator = Allocator::default();
    let expr = Parser::new(&allocator, source, SourceType::default())
        .parse_nota_expression()
        .unwrap_or_else(|errors| panic!("Nota parse failed for {source:?}: {errors:?}"));
    let mut codegen = Codegen::new();
    codegen.print_expression(&expr);
    let js = codegen.into_source_text();
    assert_valid_js(&js);
    js
}

/// Compile a Nota expression and assert it equals `expected` (modulo formatting + validity).
#[track_caller]
fn nota_expr(source: &str, expected: &str) {
    let js = nota_expr_raw(source);
    assert_js_eq(&js, expected);
}

/// Compile a whole `.nota` file to a JS module string (document mode).
#[track_caller]
fn nota_doc(source: &str) -> String {
    let allocator = Allocator::default();
    let program = Parser::new(&allocator, source, SourceType::default())
        .parse_nota_document()
        .unwrap_or_else(|errors| panic!("Nota document parse failed for {source:?}: {errors:?}"));
    let js = Codegen::new().build(&program).code;
    assert_valid_js(&js);
    js
}

/// Assert that compiling `source` fails with at least one diagnostic (impl.md §1.6 layer 3).
#[track_caller]
fn nota_expr_err(source: &str) {
    let allocator = Allocator::default();
    let result = Parser::new(&allocator, source, SourceType::default()).parse_nota_expression();
    assert!(result.is_err(), "expected a diagnostic for {source:?}, but parse succeeded");
}

/// Assert the emitted JS re-parses cleanly under the stock oxc parser (the validity invariant).
#[track_caller]
fn assert_valid_js(js: &str) {
    let allocator = Allocator::default();
    // Module source type so `import`/`export` in document-mode output is accepted.
    let ret = Parser::new(&allocator, js, SourceType::default().with_module(true)).parse();
    assert!(!ret.panicked, "stock oxc panicked re-parsing emitted JS: {js:?}");
    assert!(ret.errors.is_empty(), "emitted JS did not re-parse cleanly: {js:?}\n{:?}", ret.errors);
}

// ===============================================================================================
// Phase A / B — expression mode (contract §3)
// ===============================================================================================

#[test]
fn host_element_text() {
    nota_expr("@p{Hello}", r#"h("p", {}, ["Hello"])"#);
    nota_expr("@em{hi}", r#"h("em", {}, ["hi"])"#);
}

#[test]
fn component_element() {
    // Capitalized tag → identifier (component); `@Unknown{}` is a downstream TS scope error, not ours.
    nota_expr("@Aside{hi}", r#"h(Aside, {}, ["hi"])"#);
    nota_expr("@Unknown{}", r#"h(Unknown, {}, [])"#);
}

#[test]
fn empty_body_is_no_children() {
    // The whitespace pass drops empty/whitespace-only text → `[]` (contract §0 note, §3).
    nota_expr("@p{}", r#"h("p", {}, [])"#);
    nota_expr("@input{}", r#"h("input", {}, [])"#);
}

#[test]
fn nested_children() {
    nota_expr("@p{Hello @em{world}}", r#"h("p", {}, ["Hello ", h("em", {}, ["world"])])"#);
}

#[test]
fn fragment() {
    nota_expr("@{one @b{two}}", r#"Fragment("one ", h("b", {}, ["two"]))"#);
    nota_expr("@{x}", r#"Fragment("x")"#);
}

#[test]
fn interpolation() {
    // Bare identifier and `@(expr)` interpolation inside a body.
    nota_expr("@p{@name}", r#"h("p", {}, [name])"#);
    nota_expr("@p{@(user.posts[0])}", r#"h("p", {}, [user.posts[0]])"#);
    nota_expr("@p{@(a + b)}", r#"h("p", {}, [a + b])"#);
}

#[test]
fn interpolation_in_text() {
    // Surrounding spaces are kept by the preceding/following text segments.
    nota_expr("@p{x @name y}", r#"h("p", {}, ["x ", name, " y"])"#);
}

#[test]
fn props_string_and_expr() {
    nota_expr(r#"@a[href:"/x"]{go}"#, r#"h("a", { href: "/x" }, ["go"])"#);
    nota_expr("@a[href:url]{go}", r#"h("a", { href: url }, ["go"])"#);
}

#[test]
fn props_shorthand_and_spread() {
    // Empty body → no children.
    nota_expr("@input[disabled, ...rest]{}", r#"h("input", { disabled, ...rest }, [])"#);
}

#[test]
fn props_markup_valued() {
    nota_expr("@fig[cap:@em{hi}]{x}", r#"h("fig", { cap: h("em", {}, ["hi"]) }, ["x"])"#);
}

#[test]
fn props_multiple_groups_accumulate() {
    // Multiple `[…]` groups union into one props object.
    nota_expr("@a[href:url][title:t]{go}", r#"h("a", { href: url, title: t }, ["go"])"#);
}

#[test]
fn self_closing_with_props_no_body() {
    nota_expr("@hr[class:c]", r#"h("hr", { class: c }, [])"#);
}

#[test]
fn dynamic_tag_iife() {
    nota_expr(
        "@(getTag()){hi}",
        r#"(() => { const _Tag = getTag(); return h(_Tag, {}, ["hi"]); })()"#,
    );
}

#[test]
fn dynamic_tag_direct() {
    // A head already valid as a tag (Capitalized ident / member expr) emits directly.
    nota_expr("@(Box){hi}", r#"h(Box, {}, ["hi"])"#);
    nota_expr("@(ui.Card){hi}", r#"h(ui.Card, {}, ["hi"])"#);
}

#[test]
fn dynamic_tag_with_props() {
    nota_expr(
        "@(comps[k])[x:1]{hi}",
        r#"(() => { const _Tag = comps[k]; return h(_Tag, { x: 1 }, ["hi"]); })()"#,
    );
}

#[test]
fn balanced_braces_are_literal() {
    // Scribble: `@foo{f{o}o}` → `(foo "f{o}o")` — balanced braces are literal body text.
    nota_expr("@code{f{o}o}", r#"h("code", {}, ["f{o}o"])"#);
}

// ===============================================================================================
// Phase C — whitespace table (notation.md §Whitespace; ·=space ⏎=newline). One `"\n"` per
// interior newline, never coalesced (contract §7 paragraph-break marker).
// ===============================================================================================

#[test]
fn ws_single_line_spaces_kept() {
    // `@foo{·bar·}` → ⟦ "·bar·" ⟧
    nota_expr("@foo{ bar }", r#"h("foo", {}, [" bar "])"#);
}

#[test]
fn ws_leading_trailing_newline_dropped() {
    // `@foo{⏎··bar⏎}` → ⟦ "bar" ⟧  (drop newline after `{` / before `}`; strip common indent)
    nota_expr("@foo{\n  bar\n}", r#"h("foo", {}, ["bar"])"#);
}

#[test]
fn ws_interior_indent_and_newlines() {
    // `@foo{⏎··begin⏎····x⏎··end}` → ⟦ "begin", "⏎", "··x", "⏎", "end" ⟧
    nota_expr(
        "@foo{\n  begin\n    x\n  end}",
        r#"h("foo", {}, ["begin", "\n", "  ", "x", "\n", "end"])"#,
    );
}

#[test]
fn ws_blank_line_is_two_newlines() {
    // CRITICAL (contract §7): a blank source line → ≥2 adjacent "\n" (the para-break marker).
    // `@foo{⏎··bar⏎⏎··baz⏎}` → ⟦ "bar", "⏎", "⏎", "baz" ⟧
    nota_expr("@foo{\n  bar\n\n  baz\n}", r#"h("foo", {}, ["bar", "\n", "\n", "baz"])"#);
}

#[test]
fn ws_leading_and_trailing_blank_lines() {
    // `@foo{⏎⏎··bar⏎⏎}` → ⟦ "⏎", "bar", "⏎" ⟧
    nota_expr("@foo{\n\n  bar\n\n}", r#"h("foo", {}, ["\n", "bar", "\n"])"#);
}

#[test]
fn ws_body_only_newlines() {
    // `@foo{⏎}` → ⟦ "⏎" ⟧ (only-newlines body preserved, not dropped to []).
    nota_expr("@foo{\n}", r#"h("foo", {}, ["\n"])"#);
    // Three newlines → three "\n".
    nota_expr("@foo{\n\n\n}", r#"h("foo", {}, ["\n", "\n", "\n"])"#);
}

#[test]
fn ws_common_indent_strip_keeps_leftover() {
    // `@foo{bar⏎·······baz⏎·····bbb}` → ⟦ "bar","⏎","··","baz","⏎","bbb" ⟧
    nota_expr(
        "@foo{bar\n       baz\n     bbb}",
        r#"h("foo", {}, ["bar", "\n", "  ", "baz", "\n", "bbb"])"#,
    );
}

#[test]
fn ws_element_on_following_line() {
    // `@foo{bar @baz{3}⏎·····blah}` → ⟦ "bar ", h("baz",{},["3"]), "⏎", "blah" ⟧
    nota_expr(
        "@foo{bar @baz{3}\n     blah}",
        r#"h("foo", {}, ["bar ", h("baz", {}, ["3"]), "\n", "blah"])"#,
    );
}

#[test]
fn ws_nested_element_independent() {
    // Each element body's whitespace is computed independently (Scribble `@text{...}` case).
    nota_expr(
        "@text{Some @b{bold\n  text}, and\n  more text.}",
        r#"h("text", {}, ["Some ", h("b", {}, ["bold", "\n", "text"]), ", and", "\n", "more text."])"#,
    );
}

// ===============================================================================================
// Phase C — document mode, decode wrap, statements/hoisting/F1, await→async, colon sugar
// ===============================================================================================

#[test]
fn doc_basic() {
    // A file → `export default function Doc() { return decode(Fragment(...)); }`.
    let js = nota_doc("@h1{Hello}\n@p{Welcome to @em{Nota}.}\n");
    assert_js_eq(
        &js,
        r#"export default function Doc() {
  return decode(Fragment(h("h1", {}, ["Hello"]), "\n", h("p", {}, ["Welcome to ", h("em", {}, ["Nota"]), "."])));
}"#,
    );
}

#[test]
fn doc_import_hoisted() {
    let js = nota_doc("% import { load } from \"./posts\"\n@h1{Posts}\n");
    assert!(js.starts_with("import { load } from \"./posts\";"), "import hoisted: {js}");
    assert!(js.contains("export default function Doc()"), "{js}");
}

#[test]
fn doc_top_level_statement_prepended_no_iife() {
    // Top-level `%` (non import/export, non-F1) prepends into Doc's body (contract R5, no IIFE).
    let js = nota_doc("% const n = 3\n@p{@n}\n");
    assert!(js.contains("const n = 3;"), "prelude const present: {js}");
    assert!(!js.contains("=> {"), "no IIFE for top-level %: {js}");
}

#[test]
fn doc_await_makes_doc_async() {
    let js = nota_doc("% const posts = await load()\n@h1{Posts}\n");
    assert!(js.contains("export default async function Doc()"), "Doc is async: {js}");
}

#[test]
fn doc_fence_statements() {
    let js = nota_doc("%%%\nconst a = 1;\nconst b = 2;\n%%%\n@p{@a@b}\n");
    assert!(js.contains("const a = 1;"), "{js}");
    assert!(js.contains("const b = 2;"), "{js}");
}

#[test]
fn doc_f1_component_hoist_export_name() {
    // F1: `%const X = inlineComponent(...)` → hoist to module scope, export, pass "X" as 2nd arg.
    let js = nota_doc("%const Card = inlineComponent((children) => @span{@children})\n@Card{hi}\n");
    assert!(js.contains("export let") || js.contains("export const"), "F1 exported: {js}");
    assert!(js.contains(r#", "Card")"#), "F1 name passed as 2nd arg: {js}");
}

#[test]
fn colon_sugar_inline() {
    // `@foo: hello world` → `@foo{hello world}`.
    nota_expr("@foo: hello world", r#"h("foo", {}, ["hello world"])"#);
}

// ===============================================================================================
// Diagnostics (impl.md §1.6 layer 3)
// ===============================================================================================

#[test]
fn err_unterminated_body() {
    nota_expr_err("@p{hello");
}

#[test]
fn err_unterminated_props() {
    nota_expr_err("@a[href:url{x}");
}

#[test]
fn err_bad_head() {
    // `@` with no valid head (EOF) is a reader error.
    nota_expr_err("@");
    // `@` immediately followed by punctuation that is neither `(` nor an identifier.
    nota_expr_err("@%{x}");
}

#[test]
fn err_unterminated_dynamic_head() {
    nota_expr_err("@(getTag(){x}");
}

#[test]
fn unknown_component_is_not_a_reader_error() {
    // `@Unknown{}` is valid to the reader (→ `h(Unknown, …)`); the missing binding is a downstream
    // TS scope error, NOT a reader diagnostic (contract / notation.md).
    nota_expr("@Unknown{x}", r#"h(Unknown, {}, ["x"])"#);
}

// ===============================================================================================
// THE canonical golden (contract §2): stage-1 `.nota` → must equal stage-3 (modulo formatting).
// ===============================================================================================

/// The contract §2 stage-1 source.
const CANONICAL_NOTA: &str = r#"%let Colorized = inlineComponent((children) => {
  let [color, setColor] = useState("red");
  return @span[onClick: () => setColor("green")][style: {color}]{@children};
})

@for (x of ["a", "b"]) {
  - @Colorized{@x}
}
"#;

/// Compile a `.nota` document without the validity assertion (used where unlowered Phase-D `@for`
/// is still present, which is not yet valid JS).
#[track_caller]
fn nota_doc_no_validity(source: &str) -> String {
    let allocator = Allocator::default();
    let program = Parser::new(&allocator, source, SourceType::default())
        .parse_nota_document()
        .expect("document parses");
    Codegen::new().build(&program).code
}

#[test]
fn canonical_golden_component_matches_stage3() {
    // The FULL canonical golden needs Phase D (`@for`); within Phase B/C scope, the F1 component
    // definition must lower EXACTLY to contract §2 stage-3 (F1 hoist+export+name, `decode` wrap,
    // `@children` → the bound param). We assert the component definition prefix matches stage-3.
    let js = nota_doc_no_validity(CANONICAL_NOTA);
    let expected_component = r#"export let Colorized = inlineComponent((children) => {
  let [color, setColor] = useState("red");
  return decode(h("span", { onClick: () => setColor("green"), style: { color } }, [children]));
}, "Colorized");"#;
    // Compare the component definition (everything up to `export default`), modulo formatting.
    let component_emitted = js.split("export default").next().unwrap();
    assert_js_eq(component_emitted, expected_component);

    // The document scaffolding is present: `export default function Doc()` + `decode(Fragment(...))`.
    assert!(js.contains("export default function Doc()"), "{js}");
    assert!(js.contains("decode(Fragment("), "{js}");
}

#[test]
fn canonical_golden_minus_phase_d_is_valid() {
    // A Phase-D-free analog of the canonical golden (a literal markup body instead of `@for`),
    // exercising the WHOLE document pipeline end-to-end with the validity invariant intact.
    let src = r#"%let Colorized = inlineComponent((children) => {
  return @span[style: {color}]{@children};
})

@Colorized{a}
"#;
    let js = nota_doc(src); // asserts validity (re-parses under stock oxc)
    assert!(js.contains(r#"inlineComponent((children) => {"#), "{js}");
    assert!(js.contains(r#"return decode(h("span", { style: { color } }, [children]));"#), "{js}");
    assert!(js.contains(r#", "Colorized")"#), "F1 name: {js}");
    assert!(js.contains(r#"h(Colorized, {}, ["a"])"#), "component use: {js}");
}

#[test]
fn doc_percent_literal_midline() {
    // `50%` mid-line is literal text, not a statement.
    let js = nota_doc("@p{Tax is 50% today}\n");
    assert!(js.contains(r#""Tax is 50% today""#), "{js}");
}

#[test]
fn doc_backslash_percent_line_start_not_statement() {
    // `\%` at line start is NOT a statement line (first non-ws is `\`, not `%`).
    let js = nota_doc_no_validity("\\% literal\n");
    // It is treated as markup text (a `%` statement would have hoisted/prepended a JS statement).
    assert!(!js.contains("export let"), "should not be hoisted as F1: {js}");
    assert!(js.contains("Fragment("), "{js}");
}

#[test]
fn nested_percent_statement_wraps_rest_in_iife() {
    // notation.md §Statements: `%` nested in an element body wraps the remaining siblings in an IIFE.
    let js = nota_expr_raw("@aside{\n  Intro.\n  % const n = count()\n  @p{@n items}\n}");
    // The IIFE: `(() => { const n = count(); return Fragment(...); })()`
    assert!(js.contains("const n = count();"), "{js}");
    assert!(js.contains("=> {"), "IIFE present: {js}");
    assert!(js.contains("return Fragment("), "IIFE returns a Fragment: {js}");
    // "Intro." is a sibling BEFORE the `%`, so it stays outside the IIFE.
    assert!(js.contains(r#""Intro.""#), "{js}");
}

#[test]
fn nested_percent_await_makes_iife_async() {
    let js = nota_expr_raw("@aside{\n  % const x = await f()\n  @p{@x}\n}");
    assert!(js.contains("async () =>") || js.contains("async ()=>"), "async IIFE: {js}");
}

#[test]
fn doc_paragraph_break_is_double_newline() {
    // §7 end-to-end: a blank line between two top-level paragraphs surfaces as ≥2 adjacent "\n"
    // (the runtime's paragraph-break marker `/\n[^\S\n]*\n/`), never coalesced.
    let js = nota_doc("@p{one}\n\n@p{two}\n");
    // Between the two `h("p", …)` there must be at least two "\n" string children.
    assert!(js.contains(r#""\n", "\n""#), "expected adjacent newlines for the para break: {js}");
}
