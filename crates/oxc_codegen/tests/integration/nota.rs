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
// Phase D — control flow (`@if` / `else` / `@for`). All are expressions (contract §3 rows).
// `@if (c){a}` → `c ? Fragment(...a) : null`; `@for (x of y){body}` → keyed `.map` (contract §4 E5).
// ===============================================================================================

#[test]
fn if_single_branch() {
    // `@if (c) {a}` → `c ? Fragment("a") : null`.
    nota_expr("@if (c) {a}", r#"c ? Fragment("a") : null"#);
    // Whitespace after `@if` is insignificant (`@if(c)` ≡ `@if (c)`).
    nota_expr("@if(c){a}", r#"c ? Fragment("a") : null"#);
}

#[test]
fn if_else() {
    // `@if (c) {a} else {b}` → `c ? Fragment("a") : Fragment("b")`.
    nota_expr("@if (c) {a} else {b}", r#"c ? Fragment("a") : Fragment("b")"#);
}

#[test]
fn if_else_if() {
    // `@if (c) {a} else if (d) {b}` → nested ternary, `null` when no branch matches.
    nota_expr("@if (c) {a} else if (d) {b}", r#"c ? Fragment("a") : d ? Fragment("b") : null"#);
}

#[test]
fn if_else_if_else() {
    nota_expr(
        "@if (c) {a} else if (d) {b} else {e}",
        r#"c ? Fragment("a") : d ? Fragment("b") : Fragment("e")"#,
    );
}

#[test]
fn if_branch_with_markup_and_interp() {
    // Branch bodies nest markup and interpolation.
    nota_expr("@if (c) {Hi @em{@name}}", r#"c ? Fragment("Hi ", h("em", {}, [name])) : null"#);
}

#[test]
fn if_condition_is_arbitrary_expr() {
    nota_expr("@if (a && b.c) {x}", r#"a && b.c ? Fragment("x") : null"#);
}

#[test]
fn else_only_continues_as_next_token() {
    // A blank line between `}` and `else` breaks the continuation: the `else` is literal text in the
    // *following* sibling, so the `@if` has a `null` alternate. (Expression mode reads one form, so
    // here we assert the body-nested behavior via a fragment.)
    nota_expr(
        "@{@if (c) {a}\n\nelse text}",
        r#"Fragment(c ? Fragment("a") : null, "\n", "\n", "else text")"#,
    );
}

#[test]
fn else_adjacent_continues() {
    // No blank line ⇒ `else` continues even across a single newline.
    nota_expr("@if (c) {a}\nelse {b}", r#"c ? Fragment("a") : Fragment("b")"#);
}

#[test]
fn escaped_else_is_literal() {
    // `\else` right after the if-block forces a literal (not a continuation): `@if` keeps a `null`
    // alternate and the `\else` text surfaces in the surrounding body.
    let js = nota_expr_raw("@{@if (c) {a} \\else text}");
    assert!(js.contains(r#"Fragment("a") : null"#), "if has null alternate: {js}");
    assert!(js.contains("else text") || js.contains(r#"\else text"#), "else text literal: {js}");
}

#[test]
fn if_nested_in_for() {
    // Control flow nests: `@for` body contains an `@if`.
    nota_expr(
        "@for (x of xs) {@if (x) {@x}}",
        r#"xs.map((x, _i) => Fragment({ key: _i }, x ? Fragment(x) : null))"#,
    );
}

#[test]
fn for_basic() {
    // `@for (x of y) {@li{@x}}` → `y.map((x, _i) => Fragment({ key: _i }, h("li", {}, [x])))` (E5).
    nota_expr(
        "@for (x of y) {@li{@x}}",
        r#"y.map((x, _i) => Fragment({ key: _i }, h("li", {}, [x])))"#,
    );
}

#[test]
fn for_array_literal_iter() {
    nota_expr(
        r#"@for (x of ["a", "b"]) {@x}"#,
        r#"["a", "b"].map((x, _i) => Fragment({ key: _i }, x))"#,
    );
}

#[test]
fn for_destructuring_bind() {
    // `bind` is any binding pattern.
    nota_expr(
        "@for ([k, v] of pairs) {@k = @v}",
        r#"pairs.map(([k, v], _i) => Fragment({ key: _i }, k, " = ", v))"#,
    );
    nota_expr(
        "@for ({ id } of items) {@id}",
        r#"items.map(({ id }, _i) => Fragment({ key: _i }, id))"#,
    );
}

#[test]
fn for_multi_child_body() {
    // The body's whitespace pass yields multiple children, all spread after the key prop.
    nota_expr(
        "@for (x of xs) {@b{@x} done}",
        r#"xs.map((x, _i) => Fragment({ key: _i }, h("b", {}, [x]), " done"))"#,
    );
}

#[test]
fn control_flow_nested_in_markup_body() {
    // `@if`/`@for` are expressions, so they sit as children of an element body.
    nota_expr(
        "@ul{@for (x of xs) {@li{@x}}}",
        r#"h("ul", {}, [xs.map((x, _i) => Fragment({ key: _i }, h("li", {}, [x])))])"#,
    );
}

// ===============================================================================================
// Phase D diagnostics
// ===============================================================================================

#[test]
fn err_for_without_of() {
    // A C-style `for` has no `@`-form (write it in `%`); `@for` requires `of`.
    nota_expr_err("@for (let i = 0; i < n; i++) {x}");
}

#[test]
fn err_if_without_body() {
    nota_expr_err("@if (c) a");
}

#[test]
fn err_for_without_body() {
    nota_expr_err("@for (x of xs) x");
}

// ===============================================================================================
// Phase E — markup sugar. Emphasis (`*`/`_`), headings (`#`), lists (`-`/`+`/`N.`). Each lowers to
// an ordinary element; the runtime `struct` does the grouping (contract §3 / notation.md §sugar).
// ===============================================================================================

#[test]
fn emphasis_strong() {
    nota_expr("@p{*bold*}", r#"h("p", {}, [h("strong", {}, ["bold"])])"#);
}

#[test]
fn emphasis_em() {
    nota_expr("@p{_italic_}", r#"h("p", {}, [h("em", {}, ["italic"])])"#);
}

#[test]
fn emphasis_nested() {
    // `*a _b_ c*` → <strong>a <em>b</em> c</strong>.
    nota_expr(
        "@p{*a _b_ c*}",
        r#"h("p", {}, [h("strong", {}, ["a ", h("em", {}, ["b"]), " c"])])"#,
    );
}

#[test]
fn emphasis_intra_word_is_literal() {
    // Typst word-boundary rule: intra-word `_`/`*` are literal without escaping.
    nota_expr("@p{my_var_name}", r#"h("p", {}, ["my_var_name"])"#);
    nota_expr("@p{a*b*c}", r#"h("p", {}, ["a*b*c"])"#);
}

#[test]
fn emphasis_with_surrounding_text() {
    nota_expr("@p{say *hi* there}", r#"h("p", {}, ["say ", h("strong", {}, ["hi"]), " there"])"#);
}

#[test]
fn emphasis_contains_interpolation() {
    nota_expr("@p{*@name*}", r#"h("p", {}, [h("strong", {}, [name])])"#);
}

#[test]
fn emphasis_unbalanced_is_literal() {
    // No matching close before EOF/paragraph end ⇒ the marker is literal (Typst behavior).
    nota_expr("@p{a * b}", r#"h("p", {}, ["a * b"])"#);
}

#[test]
fn escaped_emphasis_marker_is_literal() {
    // `\*` suppresses the emphasis marker; Phase F drops the `\` so the literal `*` remains.
    let js = nota_expr_raw(r#"@p{\*not bold\*}"#);
    assert!(!js.contains(r#"h("strong""#), "no strong: {js}");
    assert!(js.contains(r#""*not bold*""#), "literal stars, backslash dropped: {js}");
}

#[test]
fn escaped_hash_dash_at_line_start_not_construct() {
    // `\#`/`\-` at line start: the first char is `\`, not the marker, so no heading/list fires.
    let js = nota_doc("\\# not a heading\n\\- not a list\n");
    assert!(!js.contains(r#"h("h1""#), "no heading: {js}");
    assert!(!js.contains(r#"h("ulli""#), "no list: {js}");
}

#[test]
fn emphasis_at_document_level() {
    // Sugar works at document level too (hooks the same markup machinery).
    let js = nota_doc("Some *bold* and _italic_ text.\n");
    assert!(js.contains(r#"h("strong", {}, ["bold"])"#), "{js}");
    assert!(js.contains(r#"h("em", {}, ["italic"])"#), "{js}");
}

// ----- Headings -----

#[test]
fn heading_h1() {
    let js = nota_doc("# Title\n");
    assert!(js.contains(r#"h("h1", {}, ["Title"])"#), "{js}");
}

#[test]
fn heading_levels() {
    let js = nota_doc("### Sub *bit*\n");
    // `### Sub *bit*` → h("h3", {}, ["Sub ", h("strong", {}, ["bit"])]).
    assert!(js.contains(r#"h("h3", {}, ["Sub ", h("strong", {}, ["bit"])])"#), "{js}");
}

#[test]
fn heading_all_six_levels() {
    let js = nota_doc("# a\n## b\n### c\n#### d\n##### e\n###### f\n");
    for (n, body) in [(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e"), (6, "f")] {
        assert!(js.contains(&format!(r#"h("h{n}", {{}}, ["{body}"])"#)), "h{n}: {js}");
    }
}

#[test]
fn heading_seven_hashes_is_not_heading() {
    // 7+ `#` is not a heading (1–6 only); it stays literal text.
    let js = nota_doc("####### too many\n");
    assert!(!js.contains(r#"h("h7""#), "no h7: {js}");
    assert!(!js.contains(r#"h("h"#), "no heading at all: {js}");
}

#[test]
fn hash_without_space_is_literal() {
    // `#tag` (no space after the run) is not a heading.
    let js = nota_doc("#tag here\n");
    assert!(!js.contains(r#"h("h1""#), "{js}");
}

// ----- Lists -----

#[test]
fn list_bullet() {
    // `- a` → h("ulli", {}, ["a"]); the runtime struct coalesces runs into <ul>.
    let js = nota_doc("- a\n- b\n");
    assert!(js.contains(r#"h("ulli", {}, ["a"])"#), "{js}");
    assert!(js.contains(r#"h("ulli", {}, ["b"])"#), "{js}");
}

#[test]
fn list_number() {
    let js = nota_doc("+ first\n+ second\n");
    assert!(js.contains(r#"h("olli", {}, ["first"])"#), "{js}");
    assert!(js.contains(r#"h("olli", {}, ["second"])"#), "{js}");
}

#[test]
fn list_explicit_number_marker() {
    // `N.` is an alternate olli marker; the written numbers are ignored.
    let js = nota_doc("1. one\n2. two\n");
    assert!(js.contains(r#"h("olli", {}, ["one"])"#), "{js}");
    assert!(js.contains(r#"h("olli", {}, ["two"])"#), "{js}");
}

#[test]
fn list_item_with_markup() {
    let js = nota_doc("- a *bold* item\n");
    // Compare modulo formatting (codegen wraps the >2-element array across lines).
    assert_js_eq(
        &js,
        r#"export default function Doc() {
  return decode(Fragment(h("ulli", {}, ["a ", h("strong", {}, ["bold"]), " item"])));
}"#,
    );
}

#[test]
fn list_nested() {
    // A deeper marker opens a nested list inside the parent item's children:
    //   - a
    //     - b
    //     - c
    // → h("ulli", {}, ["a", "\n", h("ulli",{},["b"]), h("ulli",{},["c"])]); the runtime `struct`
    // coalesces the inner `ulli` run into one nested `<ul>`, and `a`'s item carries it.
    let js = nota_doc("- a\n  - b\n  - c\n");
    assert_js_eq(
        &js,
        r#"export default function Doc() {
  return decode(Fragment(h("ulli", {}, ["a", "\n", h("ulli", {}, ["b"]), h("ulli", {}, ["c"])])));
}"#,
    );
}

#[test]
fn list_continuation_line() {
    // An item body continues on lines indented past its marker (block-sugar rule).
    let js = nota_doc("- first line\n  continued\n");
    assert!(js.contains("first line"), "{js}");
    assert!(js.contains("continued"), "{js}");
    // Both are children of the same ulli (no second ulli for "continued").
    assert_eq!(js.matches(r#"h("ulli""#).count(), 1, "one ulli only: {js}");
}

#[test]
fn dash_without_space_is_literal() {
    // `-5` (no space) is not a list marker.
    let js = nota_doc("-5 degrees\n");
    assert!(!js.contains(r#"h("ulli""#), "{js}");
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

/// THE canonical golden, stage-3 (contract §2), with the **Phase-D + E5** amendment applied: the
/// `@for` is lowered to a *keyed* `.map` (`(x, _i) => Fragment({ key: _i }, …)`), and the `-` list
/// marker is lowered to the `"ulli"` sentinel (Phase E — runtime `struct` later coalesces it).
const CANONICAL_STAGE3: &str = r#"export let Colorized = inlineComponent((children) => {
  let [color, setColor] = useState("red");
  return decode(h("span", { onClick: () => setColor("green"), style: { color } }, [children]));
}, "Colorized");

export default function Doc() {
  return decode(Fragment(["a", "b"].map((x, _i) => Fragment({ key: _i }, h("ulli", {}, [h(Colorized, {}, [x])])))));
}"#;

#[test]
fn canonical_golden_matches_stage3() {
    // THE capstone (contract §2): stage-1 `.nota` compiles to a module byte-equal (modulo
    // formatting) to stage-3 — incl. the F1 component (hoist+export+name, `decode` wrap, `@children`
    // → the bound param), the keyed `Fragment({ key: _i }, …)` (E5), the `["a", "b"].map((x, _i) =>
    // …)` (Phase D), and the `-` → `h("ulli", …)` list sentinel (Phase E). Also valid JS (re-parses
    // under stock oxc — the §1.6 validity invariant), now that nothing is un-lowered.
    let js = nota_doc(CANONICAL_NOTA);
    assert_js_eq(&js, CANONICAL_STAGE3);
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

// ===============================================================================================
// Phase F — verbatim (`|{ … }|`), code (`` `…` `` / fenced), math (`$…$` / `$$…$$`), general
// backslash escapes. All raw spans lower to `String.raw` tagged templates (contract §3 last rows;
// notation.md §Verbatim/§Math/§Code). `CodeInline`/`CodeBlock`/`Math` are ambient prelude bindings.
// ===============================================================================================

// --- General backslash escape (step 1) --------------------------------------------------------

#[test]
fn escape_general_chars_literal_backslash_dropped() {
    // `\<c>` → literal `<c>`, the backslash dropped. `@ { } * _ $ : [ ] | and \\` all escapable.
    nota_expr(r"@p{\@}", r#"h("p", {}, ["@"])"#);
    nota_expr(r"@p{\{}", r#"h("p", {}, ["{"])"#);
    nota_expr(r"@p{\}}", r#"h("p", {}, ["}"])"#);
    nota_expr(r"@p{\$}", r#"h("p", {}, ["$"])"#);
    nota_expr(r"@p{\|}", r#"h("p", {}, ["|"])"#);
    nota_expr(r"@p{a\\b}", r#"h("p", {}, ["a\\b"])"#); // `\\` → one literal backslash
}

#[test]
fn escape_star_keeps_literal_no_marker() {
    // `\*` is the literal `*` (NOT emphasis, and the backslash is dropped — the D/E `\*`-keeps-`\`
    // bug is fixed). Two escaped markers → two literal stars, no `<strong>`.
    nota_expr(r"@p{\*hi\*}", r#"h("p", {}, ["*hi*"])"#);
    nota_expr(r"@p{a \_b\_ c}", r#"h("p", {}, ["a _b_ c"])"#);
}

#[test]
fn escape_backtick_and_at_in_prose() {
    // `\@`/`` \` `` keep their char literal, backslash dropped (so prose can mention `@foo` literally).
    nota_expr(r"@p{see \@foo}", r#"h("p", {}, ["see @foo"])"#);
    nota_expr(r"@p{a \` b}", r#"h("p", {}, ["a ` b"])"#);
}

// --- Verbatim `|{ … }|` (step 2) --------------------------------------------------------------

#[test]
fn verbatim_raw_body() {
    // contract §3: `@code|{@foo{x}}|` → `h("code", {}, [String.raw`@foo{x}`])`. Sigils off, braces
    // literal — `@foo{x}` is raw text, NOT a child.
    nota_expr(r"@code|{@foo{x}}|", r#"h("code", {}, [String.raw`@foo{x}`])"#);
}

#[test]
fn verbatim_keeps_backslash_and_braces() {
    // Raw: backslashes and braces survive verbatim into the `String.raw`.
    nota_expr(r"@code|{a\b {c} d}|", r#"h("code", {}, [String.raw`a\b {c} d`])"#);
}

#[test]
fn verbatim_armed_reentry() {
    // notation.md §Verbatim multi-line: `|@` splits a raw run and a Nota child. The newline right
    // after `|{` and right before `}|` are dropped (the Scribble brace rule); the interior `\n` +
    // 4-space indent survive raw.
    let src = "@code|{\ndef f(x):\n    return |@hl{x}\n}|";
    nota_expr(src, "h(\"code\", {}, [String.raw`def f(x):\n    return `, h(\"hl\", {}, [\"x\"])])");
}

#[test]
fn verbatim_component_tag() {
    // A verbatim body on a component tag.
    nota_expr(r"@Pre|{x@y}|", r#"h(Pre, {}, [String.raw`x@y`])"#);
}

#[test]
fn verbatim_braces_literal_not_close() {
    // A bare `}` (not `}|`) is literal raw content; only `}|` closes.
    nota_expr(r"@code|{ {a} {b} }|", r#"h("code", {}, [String.raw` {a} {b} `])"#);
}

#[test]
fn verbatim_armed_interpolation() {
    // `|@name` re-arms an *interpolation* (not just elements) as a sibling child.
    nota_expr(r"@code|{a|@x b}|", r#"h("code", {}, [String.raw`a`, x, String.raw` b`])"#);
}

// --- Inline & fenced code (step 3) ------------------------------------------------------------

#[test]
fn code_inline() {
    // contract §3: `` `@x` `` → `h(CodeInline, {}, [String.raw`@x`])`. Fully raw (the `@` is literal).
    nota_expr("@p{`@x`}", r#"h("p", {}, [h(CodeInline, {}, [String.raw`@x`])])"#);
    nota_expr("@p{`a + b`}", r#"h("p", {}, [h(CodeInline, {}, [String.raw`a + b`])])"#);
}

#[test]
fn code_inline_keeps_backslash() {
    nota_expr(r"@p{`a\n`}", r#"h("p", {}, [h(CodeInline, {}, [String.raw`a\n`])])"#);
}

#[test]
fn code_inline_unterminated_is_literal() {
    // A backtick with no close is literal text.
    nota_expr("@p{a ` b}", r#"h("p", {}, ["a ` b"])"#);
}

#[test]
fn code_fenced_with_lang() {
    // contract §3: ```` ```python⏎f(x)⏎``` ```` → `h(CodeBlock, { lang: "python" }, [String.raw`f(x)`])`.
    let src = "@d{```python\nf(x)\n```}";
    nota_expr(src, r#"h("d", {}, [h(CodeBlock, { lang: "python" }, [String.raw`f(x)`])])"#);
}

#[test]
fn code_fenced_no_lang() {
    let src = "@d{```\nf(x)\n```}";
    nota_expr(src, r#"h("d", {}, [h(CodeBlock, {}, [String.raw`f(x)`])])"#);
}

#[test]
fn code_fenced_multiline_body() {
    let src = "@d{```\nline 1\nline 2\n```}";
    nota_expr(src, "h(\"d\", {}, [h(CodeBlock, {}, [String.raw`line 1\nline 2`])])");
}

// --- Math (step 4) ----------------------------------------------------------------------------

#[test]
fn math_inline_plain() {
    nota_expr(r"@p{$x^2$}", r#"h("p", {}, [h(Math, {}, [String.raw`x^2`])])"#);
}

#[test]
fn math_inline_interp() {
    // contract §3: `$a_@i$` → `h(Math, {}, [String.raw`a_${i}`])`. `@i` interpolates a string value.
    nota_expr(r"@p{$a_@i$}", r#"h("p", {}, [h(Math, {}, [String.raw`a_${i}`])])"#);
}

#[test]
fn math_keeps_latex_backslash() {
    // `\$`/`\@` keep the backslash (LaTeX's own escape); `\sum` survives.
    nota_expr(r"@p{$\sum x$}", r#"h("p", {}, [h(Math, {}, [String.raw`\sum x`])])"#);
    nota_expr(r"@p{$a \$ b$}", r#"h("p", {}, [h(Math, {}, [String.raw`a \$ b`])])"#);
    nota_expr(r"@p{$a \@ b$}", r#"h("p", {}, [h(Math, {}, [String.raw`a \@ b`])])"#);
}

#[test]
fn math_display_interp() {
    // `$$⏎\sum_@n x⏎$$` → `h(Math, { display: true }, [String.raw`\sum_${n} x`])`.
    let src = "@p{$$\n\\sum_@n x\n$$}";
    nota_expr(src, "h(\"p\", {}, [h(Math, { display: true }, [String.raw`\n\\sum_${n} x\n`])])");
}

#[test]
fn math_display_plain() {
    nota_expr(r"@p{$$x^2$$}", r#"h("p", {}, [h(Math, { display: true }, [String.raw`x^2`])])"#);
}

#[test]
fn math_interp_paren_expr() {
    nota_expr(r"@p{$a_@(i + 1)$}", r#"h("p", {}, [h(Math, {}, [String.raw`a_${i + 1}`])])"#);
}

#[test]
fn dollar_unterminated_is_literal() {
    nota_expr(r"@p{costs $5 today}", r#"h("p", {}, ["costs $5 today"])"#);
}

#[test]
fn escape_full_list() {
    // The whole backslash-escape list (notation.md §Verbatim): `` \@ \{ \} \| \$ \* \_ \: \[ \] \` ``
    // and `\\`. Each → its literal char, backslash dropped.
    nota_expr(r"@p{\:}", r#"h("p", {}, [":"])"#);
    nota_expr(r"@p{\[}", r#"h("p", {}, ["["])"#);
    nota_expr(r"@p{\]}", r#"h("p", {}, ["]"])"#);
    nota_expr(r"@p{\_}", r#"h("p", {}, ["_"])"#);
}

#[test]
fn escape_colon_in_body_is_literal() {
    // A `\:` in body text is a literal colon (backslash dropped). (The head-adjacent `@foo\:` form
    // from notation.md §Colon — making `@foo` interpolate before a literal `:` — is a deferred
    // Part-1 gap: the JS lexer eats the `\` right after a bare-identifier head; tracked separately
    // from Phase F, which owns the *general body* escape. Body-position `\:` works.)
    nota_expr(r"@p{a\: b}", r#"h("p", {}, ["a: b"])"#);
}

#[test]
fn verbatim_validity_with_backtick_in_raw() {
    // A literal backtick inside a verbatim raw body must not break the emitted template (validity
    // invariant): codegen escapes it as `` \` `` (the only safe encoding; the `\` leaks at runtime,
    // an accepted degeneracy). The point of this test is that the emitted JS re-parses under stock oxc.
    let js = nota_expr_raw("@code|{a `b` c}|");
    assert!(js.contains("String.raw"), "{js}");
    // (assert_valid_js already ran inside nota_expr_raw — the emitted JS is valid.)
}

#[test]
fn math_dollar_brace_validity() {
    // A literal `${` in LaTeX would open a template substitution; codegen escapes it (`\${`) to keep
    // the template valid JS. Validity invariant is the assertion (inside nota_expr_raw).
    let js = nota_expr_raw(r"@p{$a ${b}$}");
    assert!(js.contains("h(Math"), "{js}");
}

// --- Document-mode raw spans (the sugar machinery hooks the document body too) -----------------

#[test]
fn doc_fenced_code_block() {
    // notation.md §Code: a fenced block at document level → `h(CodeBlock, { lang }, [String.raw`…`])`.
    let js = nota_doc("```python\nf(x)\n```\n");
    assert!(
        js.contains(r#"h(CodeBlock, { lang: "python" }, [String.raw`f(x)`])"#),
        "fenced block at doc level: {js}"
    );
}

#[test]
fn doc_inline_code_and_math() {
    let js = nota_doc("Use `f(x)` and $x^2$ here.\n");
    assert!(js.contains(r#"h(CodeInline, {}, [String.raw`f(x)`])"#), "{js}");
    assert!(js.contains(r#"h(Math, {}, [String.raw`x^2`])"#), "{js}");
}

#[test]
fn doc_verbatim_block() {
    let js = nota_doc("@code|{@raw{stuff}}|\n");
    assert!(js.contains(r#"h("code", {}, [String.raw`@raw{stuff}`])"#), "{js}");
}

#[test]
fn doc_escape_line_start_percent_and_hash() {
    // `\%`/`\#` at line start: the `\` is dropped, the char is literal (no statement / no heading).
    let js = nota_doc("\\% not a statement\n\\# not a heading\n");
    assert!(!js.contains("export let") && !js.contains(r#"h("h1""#), "{js}");
    assert!(js.contains(r#""% not a statement""#), "literal %: {js}");
    assert!(js.contains(r##""# not a heading""##), "literal #: {js}");
}

#[test]
fn code_and_math_nest_in_emphasis() {
    // Raw spans nest inside emphasis bodies (the byte-peek arms are wired into all three collectors).
    nota_expr(
        "@p{*see `x`*}",
        r#"h("p", {}, [h("strong", {}, ["see ", h(CodeInline, {}, [String.raw`x`])])])"#,
    );
}

#[test]
fn verbatim_in_list_item() {
    // A verbatim/code span inside a list-item body (block-body collector).
    let js = nota_doc("- item with `code`\n");
    assert!(js.contains(r#"h(CodeInline, {}, [String.raw`code`])"#), "{js}");
    assert!(js.contains(r#"h("ulli""#), "{js}");
}

#[test]
fn unterminated_verbatim_is_an_error() {
    // impl.md §1.6 layer 3: an unterminated `|{` (no `}|`) is a diagnostic.
    nota_expr_err(r"@code|{ never closed");
}

#[test]
fn emphasis_close_skips_raw_spans() {
    // A `*`/`_` *inside* a raw span (code/math/verbatim) must NOT close the surrounding emphasis —
    // `find_emphasis_close` steps over raw spans. Adversarial: spaces around the inner `*` make it a
    // marker-valid candidate, which a naive scan would (wrongly) take as the close.
    nota_expr(
        "@p{*a `b * c` d*}",
        r#"h("p", {}, [h("strong", {}, ["a ", h(CodeInline, {}, [String.raw`b * c`]), " d"])])"#,
    );
    // Math `$…$` containing a `_` (LaTeX subscript) inside `_emph_`.
    nota_expr(
        "@p{_x $a_b$ y_}",
        r#"h("p", {}, [h("em", {}, ["x ", h(Math, {}, [String.raw`a_b`]), " y"])])"#,
    );
}

#[test]
fn verbatim_unicode_and_backtick_escape() {
    // UTF-8 content survives raw; an embedded backtick is escaped for template validity. The
    // assertion is the validity invariant (inside nota_expr_raw) plus the structural shape.
    let js = nota_expr_raw("@code|{café `x` λ}|");
    assert!(js.contains("café") && js.contains("λ"), "unicode preserved: {js}");
    assert!(js.contains(r"\`x\`"), "backtick escaped: {js}");
}

#[test]
fn phase_f_mixed_document_end_to_end() {
    // A document mixing all of Phase F: a heading, prose with inline code + math + an escape, a
    // fenced block, and a verbatim element — exercising the document-body collector + the validity
    // invariant together (the whole emitted module re-parses under stock oxc).
    let src = "\
# Demo

The fn `id` returns @em{x}; cost is \\$5 and $a_@i$.

```rust
fn id(x: i32) -> i32 { x }
```

@figure|{verbatim @keep{raw}}|
";
    let js = nota_doc(src);
    assert!(js.contains(r#"h("h1", {}, ["Demo"])"#), "heading: {js}");
    assert!(js.contains(r#"h(CodeInline, {}, [String.raw`id`])"#), "inline code: {js}");
    assert!(js.contains(r#"h(Math, {}, [String.raw`a_${i}`])"#), "math interp: {js}");
    assert!(js.contains("cost is $5 and"), "escaped dollar → literal `$` in prose: {js}");
    assert!(js.contains(r#"h(CodeBlock, { lang: "rust" }"#), "fenced: {js}");
    assert!(js.contains(r#"h("figure", {}, [String.raw`verbatim @keep{raw}`])"#), "verbatim: {js}");
}
