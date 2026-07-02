//! Nota reader end-to-end fixtures: `.nota` source → Nota parse → codegen JS string.
//!
//! These are the golden/snapshot tests for the Nota reader. Two emit modes:
//! * **expression mode** (`nota_expr`) — elides the `Doc` wrapper and injected imports
//!   (`@p{Hello}` → `h("p", {}, ["Hello"])`); the bulk of fixtures.
//! * **document mode** (`nota_doc`) — the full module incl. `export default function Doc()`,
//!   hoisted `import`/`export`, `decode(...)` wrap, inline components.
//!
//! Every fixture also asserts the *validity invariant*: the emitted JS re-parses cleanly under the
//! STOCK oxc parser.

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Reformat a JS string by re-parsing and re-printing it through `Codegen`, so two strings that
/// differ only in formatting (e.g. the codegen wraps a >2-element array across lines) compare equal.
/// This is how the fixtures compare "modulo formatting".
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
    let mut expr = Parser::new(&allocator, source, SourceType::default())
        .parse_nota_expression()
        .unwrap_or_else(|errors| panic!("Nota parse failed for {source:?}: {errors:?}"));
    oxc_transformer::NotaLowering::new(&allocator, source, false).lower_expression(&mut expr);
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
    let mut program = Parser::new(&allocator, source, SourceType::default())
        .parse_nota_document()
        .unwrap_or_else(|errors| panic!("Nota document parse failed for {source:?}: {errors:?}"));
    oxc_transformer::NotaLowering::new(&allocator, source, false)
        .lower_document_program(&mut program);
    let js = Codegen::new().build(&program).code;
    assert_valid_js(&js);
    js
}

/// Assert that compiling `source` fails with at least one diagnostic.
#[track_caller]
fn nota_expr_err(source: &str) {
    let allocator = Allocator::default();
    let result = Parser::new(&allocator, source, SourceType::default()).parse_nota_expression();
    assert!(result.is_err(), "expected a diagnostic for {source:?}, but parse succeeded");
}

/// Assert that compiling `source` in **document mode** fails with at least one diagnostic.
#[track_caller]
fn nota_doc_err(source: &str) {
    let allocator = Allocator::default();
    let result = Parser::new(&allocator, source, SourceType::default()).parse_nota_document();
    assert!(
        result.is_err(),
        "expected a document-mode diagnostic for {source:?}, but parse succeeded"
    );
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
// Expression mode
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
    nota_expr("@Unknown{}", r"h(Unknown, {}, [])");
}

#[test]
fn empty_body_is_no_children() {
    // The whitespace pass drops empty/whitespace-only text → `[]`.
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
fn head_boundary_dispatch() {
    // The head→body switch is whitespace-sensitive, and the head's boundary token (the `)` of a
    // dynamic head, or the bare identifier) is consumed uniformly regardless of which branch wins.
    // A trigger glued to the head opens an element; anything else (including a space) makes the head
    // an interpolation, with the following text resumed as markup.

    // `@(expr)` boundary token (`)`): glued text, a space, and end-of-input all close the head as an
    // interpolation — exercising the resume-markup-text and resume-JS paths after consuming `)`.
    nota_expr("@p{@(a + b)c}", r#"h("p", {}, [a + b, "c"])"#);
    nota_expr("@p{@(a + b) c}", r#"h("p", {}, [a + b, " c"])"#);
    nota_expr("@(a + b)", "a + b");

    // A `{` glued to a dynamic head opens an element; a space before any would-be trigger does not.
    nota_expr("@(Box){hi}", r#"h(Box, {}, ["hi"])"#);
    nota_expr("@p{@(Box) x}", r#"h("p", {}, [Box, " x"])"#);

    // Bare-identifier head: a glued `{` is an element trigger; a trailing space is not.
    nota_expr("@p{@foo bar}", r#"h("p", {}, [foo, " bar"])"#);
    nota_expr("@p{@foo{x}}", r#"h("p", {}, [h("foo", {}, ["x"])])"#);
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
    // `@code{f{o}o}` — balanced braces are literal body text, not a nested form.
    nota_expr("@code{f{o}o}", r#"h("code", {}, ["f{o}o"])"#);
}

// ===============================================================================================
// Whitespace table (·=space ⏎=newline). One `"\n"` per interior newline, never coalesced (so a
// blank line surfaces as the paragraph-break marker — two adjacent newlines).
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
    // Kept indentation (past the common strip) stays joined with its line's content (notation.md).
    nota_expr(
        "@foo{\n  begin\n    x\n  end}",
        r#"h("foo", {}, ["begin", "\n", "  x", "\n", "end"])"#,
    );
}

#[test]
fn ws_blank_line_is_two_newlines() {
    // CRITICAL: a blank source line → ≥2 adjacent "\n" (the para-break marker).
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
    // `@foo{bar⏎·······baz⏎·····bbb}` → ⟦ "bar","⏎","··baz","⏎","bbb" ⟧ (kept indent joined to content)
    nota_expr(
        "@foo{bar\n       baz\n     bbb}",
        r#"h("foo", {}, ["bar", "\n", "  baz", "\n", "bbb"])"#,
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
    // Each element body's whitespace is computed independently.
    nota_expr(
        "@text{Some @b{bold\n  text}, and\n  more text.}",
        r#"h("text", {}, ["Some ", h("b", {}, ["bold", "\n", "text"]), ", and", "\n", "more text."])"#,
    );
}

// ===============================================================================================
// Document mode: decode wrap, statements/hoisting/inline components, colon sugar
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
    // A top-level `%` statement (not import/export, not an inline component) prepends into Doc's
    // body, with no IIFE.
    let js = nota_doc("% const n = 3\n@p{@n}\n");
    assert!(js.contains("const n = 3;"), "prelude const present: {js}");
    assert!(!js.contains("=> {"), "no IIFE for top-level %: {js}");
}

#[test]
fn doc_fence_statements() {
    let js = nota_doc("%%%\nconst a = 1;\nconst b = 2;\n%%%\n@p{@a@b}\n");
    assert!(js.contains("const a = 1;"), "{js}");
    assert!(js.contains("const b = 2;"), "{js}");
}

#[test]
fn doc_f1_component_hoist_export_name() {
    // An inline component (`%const X = inlineComponent(...)`) is hoisted to module scope, exported,
    // and passed its name "X" as the 2nd arg.
    let js = nota_doc("%const Card = inlineComponent((children) => @span{@children})\n@Card{hi}\n");
    assert!(js.contains("export let") || js.contains("export const"), "component exported: {js}");
    assert!(js.contains(r#", "Card")"#), "component name passed as 2nd arg: {js}");
}

#[test]
fn colon_sugar_inline() {
    // `@foo: hello world` → `@foo{hello world}`.
    nota_expr("@foo: hello world", r#"h("foo", {}, ["hello world"])"#);
}

#[test]
fn colon_body_brace_handling() {
    // A literal `}` in a top-level colon body is content (no enclosing brace to close → no clip); an
    // escaped `\}` is literal too. But colon sugar nested in a braced body clips at the parent's `}`.
    assert!(
        nota_doc("@def: a } b\n").contains(r#"["a } b"]"#),
        "literal }} kept in a top-level colon body"
    );
    assert!(
        nota_doc("@def: a \\} b\n").contains(r#"["a } b"]"#),
        "escaped \\}} is a literal in a colon body"
    );
    assert!(
        nota_doc("@p{@a: b}\n").contains(r#"h("p", {}, [h("a", {}, ["b"])])"#),
        "colon sugar nested in a braced body clips at the parent brace"
    );
}

#[test]
fn colon_body_props_string_brace_does_not_clip() {
    // A `}` inside an `@`-form's props string on the colon line is embedded JS, not markup — the
    // extent scan must not clip the body there (it previously cut mid-string → a fatal lexer
    // error). The depth-0 `}` after it still closes the enclosing braced body.
    nota_expr(
        "@p{@a: @f[x: \"}\"] y}",
        r#"h("p", {}, [h("a", {}, [h("f", { x: "}" }, []), " y"])])"#,
    );
}

// ===============================================================================================
// Diagnostics
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
fn err_unterminated_fence() {
    // An unterminated `%%%` fence is rejected (correct). NOTE: the *message quality* is weak — it
    // surfaces as a generic "Unexpected token" at the next `@`, with no hint a fence is open (unlike
    // the precise "unterminated verbatim body"); a dedicated diagnostic would be better.
    nota_doc_err("%%%\nconst a = 1;\n@p{hi}\n");
}

#[test]
fn unknown_component_is_not_a_reader_error() {
    // `@Unknown{}` is valid to the reader (→ `h(Unknown, …)`); the missing binding is a downstream
    // TS scope error, NOT a reader diagnostic.
    nota_expr("@Unknown{x}", r#"h(Unknown, {}, ["x"])"#);
}

// ===============================================================================================
// Control flow (`@if` / `else` / `@for`). All are expressions.
// `@if (c){a}` → `c ? Fragment(...a) : null`; `@for (x of y){body}` → a keyed `.map`.
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
    assert!(js.contains("else text") || js.contains(r"\else text"), "else text literal: {js}");
}

#[test]
fn if_nested_in_for() {
    // Control flow nests: `@for` body contains an `@if`.
    nota_expr(
        "@for (x of xs) {@if (x) {@x}}",
        r"xs.map((x, _i) => Fragment({ key: _i }, x ? Fragment(x) : null))",
    );
}

#[test]
fn for_basic() {
    // `@for (x of y) {@li{@x}}` → `y.map((x, _i) => Fragment({ key: _i }, h("li", {}, [x])))`.
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
        r"items.map(({ id }, _i) => Fragment({ key: _i }, id))",
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
// Control-flow diagnostics
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
// Markup sugar. Emphasis (`*`/`_`), headings (`#`), lists (`-`/`+`/`N.`). Each lowers to an
// ordinary element; the runtime `struct` does the grouping.
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
    // `\*` suppresses the emphasis marker; the `\` is dropped so the literal `*` remains.
    let js = nota_expr_raw(r"@p{\*not bold\*}");
    assert!(!js.contains(r#"h("strong""#), "no strong: {js}");
    assert!(js.contains(r#""*not bold*""#), "literal stars, backslash dropped: {js}");
}

#[test]
fn escaped_hash_dash_at_line_start_not_construct() {
    // `\#`/`\-` at line start: the first char is `\`, not the marker, so no heading/list fires.
    let js = nota_doc("\\# not a heading\n\\- not a list\n");
    assert!(!js.contains(r#"h("h1""#), "no heading: {js}");
    assert!(!js.contains(r#"h("nota-ul-li""#), "no list: {js}");
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
    // `- a` → h("nota-ul-li", {}, ["a"]); the runtime struct coalesces runs into <ul>.
    let js = nota_doc("- a\n- b\n");
    assert!(js.contains(r#"h("nota-ul-li", {}, ["a"])"#), "{js}");
    assert!(js.contains(r#"h("nota-ul-li", {}, ["b"])"#), "{js}");
}

#[test]
fn list_number() {
    let js = nota_doc("+ first\n+ second\n");
    assert!(js.contains(r#"h("nota-ol-li", {}, ["first"])"#), "{js}");
    assert!(js.contains(r#"h("nota-ol-li", {}, ["second"])"#), "{js}");
}

#[test]
fn list_explicit_number_marker() {
    // `N.` is an alternate nota-ol-li marker; the written numbers are ignored.
    let js = nota_doc("1. one\n2. two\n");
    assert!(js.contains(r#"h("nota-ol-li", {}, ["one"])"#), "{js}");
    assert!(js.contains(r#"h("nota-ol-li", {}, ["two"])"#), "{js}");
}

#[test]
fn list_item_with_markup() {
    let js = nota_doc("- a *bold* item\n");
    // Compare modulo formatting (codegen wraps the >2-element array across lines).
    assert_js_eq(
        &js,
        r#"export default function Doc() {
  return decode(Fragment(h("nota-ul-li", {}, ["a ", h("strong", {}, ["bold"]), " item"])));
}"#,
    );
}

#[test]
fn list_nested() {
    // A deeper marker opens a nested list inside the parent item's children:
    //   - a
    //     - b
    //     - c
    // → h("nota-ul-li", {}, ["a", "\n", h("nota-ul-li",{},["b"]), h("nota-ul-li",{},["c"])]); the runtime `struct`
    // coalesces the inner `nota-ul-li` run into one nested `<ul>`, and `a`'s item carries it.
    let js = nota_doc("- a\n  - b\n  - c\n");
    assert_js_eq(
        &js,
        r#"export default function Doc() {
  return decode(Fragment(h("nota-ul-li", {}, ["a", "\n", h("nota-ul-li", {}, ["b"]), h("nota-ul-li", {}, ["c"])])));
}"#,
    );
}

#[test]
fn list_continuation_line() {
    // An item body continues on lines indented past its marker (block-sugar rule).
    let js = nota_doc("- first line\n  continued\n");
    assert!(js.contains("first line"), "{js}");
    assert!(js.contains("continued"), "{js}");
    // Both are children of the same nota-ul-li (no second nota-ul-li for "continued").
    assert_eq!(js.matches(r#"h("nota-ul-li""#).count(), 1, "one nota-ul-li only: {js}");
}

#[test]
fn dash_without_space_is_literal() {
    // `-5` (no space) is not a list marker.
    let js = nota_doc("-5 degrees\n");
    assert!(!js.contains(r#"h("nota-ul-li""#), "{js}");
}

#[test]
fn line_start_sugar_chains_after_a_construct() {
    // Line-start sugar (a heading / list) on the line right after another construct — a list run, a
    // `%%%` fence — is recognized, not read as literal text. The collector consumes a *run* of
    // line-start constructs (each resumes at a line start that may open the next), at the document
    // start and after a `\n` alike.
    assert!(
        nota_doc("- a\n- b\n# After\n").contains(r#"h("h1", {}, ["After"])"#),
        "heading after a list (at document start)"
    );
    assert!(
        nota_doc("intro\n- a\n# After\n").contains(r#"h("h1", {}, ["After"])"#),
        "heading after a list (after a paragraph)"
    );
    assert!(
        nota_doc("%%%\nconst x = 1;\n%%%\n# Title\n").contains(r#"h("h1", {}, ["Title"])"#),
        "heading after a %%% fence"
    );
    assert!(
        nota_doc("%%%\nconst x = 1;\n%%%\n- item\n").contains(r#"h("nota-ul-li", {}, ["item"])"#),
        "list after a %%% fence"
    );
}

// ===============================================================================================
// THE canonical golden: stage-1 `.nota` → must equal stage-3 (modulo formatting).
// ===============================================================================================

/// The canonical stage-1 source.
const CANONICAL_NOTA: &str = r#"%let Colorized = inlineComponent((children) => {
  let [color, setColor] = useState("red");
  return @span[onClick: () => setColor("green")][style: {color}]{@children};
})

@for (x of ["a", "b"]) {
  - @Colorized{@x}
}
"#;

/// Compile a `.nota` document without the validity assertion (used where an unlowered `@for` is
/// still present, which is not yet valid JS).
#[track_caller]
fn nota_doc_no_validity(source: &str) -> String {
    let allocator = Allocator::default();
    let mut program = Parser::new(&allocator, source, SourceType::default())
        .parse_nota_document()
        .expect("document parses");
    oxc_transformer::NotaLowering::new(&allocator, source, false)
        .lower_document_program(&mut program);
    Codegen::new().build(&program).code
}

/// THE canonical golden, stage-3: the `@for` is lowered to a *keyed* `.map`
/// (`(x, _i) => Fragment({ key: _i }, …)`), and the `-` list marker is lowered to the `"nota-ul-li"`
/// sentinel (the runtime `struct` later coalesces it).
const CANONICAL_STAGE3: &str = r#"export let Colorized = inlineComponent((children) => {
  let [color, setColor] = useState("red");
  return decode(h("span", { onClick: () => setColor("green"), style: { color } }, [children]));
}, "Colorized");

export default function Doc() {
  return decode(Fragment(["a", "b"].map((x, _i) => Fragment({ key: _i }, h("nota-ul-li", {}, [h(Colorized, {}, [x])])))));
}"#;

#[test]
fn canonical_golden_matches_stage3() {
    // THE capstone: stage-1 `.nota` compiles to a module equal (modulo formatting) to stage-3 —
    // incl. the inline component (hoist+export+name, `decode` wrap, `@children` → the bound param),
    // the keyed `Fragment({ key: _i }, …)`, the `["a", "b"].map((x, _i) => …)` loop lowering, and
    // the `-` → `h("nota-ul-li", …)` list sentinel. Also valid JS (re-parses under stock oxc — the
    // validity invariant), now that nothing is un-lowered.
    let js = nota_doc(CANONICAL_NOTA);
    assert_js_eq(&js, CANONICAL_STAGE3);
}

#[test]
fn canonical_golden_minus_phase_d_is_valid() {
    // A control-flow-free analog of the canonical golden (a literal markup body instead of `@for`),
    // exercising the WHOLE document pipeline end-to-end with the validity invariant intact.
    let src = r"%let Colorized = inlineComponent((children) => {
  return @span[style: {color}]{@children};
})

@Colorized{a}
";
    let js = nota_doc(src); // asserts validity (re-parses under stock oxc)
    assert!(js.contains(r"inlineComponent((children) => {"), "{js}");
    assert!(js.contains(r#"return decode(h("span", { style: { color } }, [children]));"#), "{js}");
    assert!(js.contains(r#", "Colorized")"#), "component name: {js}");
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
    assert!(!js.contains("export let"), "should not be hoisted as a component: {js}");
    assert!(js.contains("Fragment("), "{js}");
}

#[test]
fn nested_percent_statement_wraps_rest_in_iife() {
    // A `%` statement nested in an element body wraps the remaining siblings in an IIFE.
    let js = nota_expr_raw("@aside{\n  Intro.\n  % const n = count()\n  @p{@n items}\n}");
    // The IIFE: `(() => { const n = count(); return Fragment(...); })()`
    assert!(js.contains("const n = count();"), "{js}");
    assert!(js.contains("=> {"), "IIFE present: {js}");
    assert!(js.contains("return Fragment("), "IIFE returns a Fragment: {js}");
    // "Intro." is a sibling BEFORE the `%`, so it stays outside the IIFE. (A pre-existing 1-space
    // leftover indent now joins it as " Intro." since kept indent merges with its line's content.)
    assert!(js.contains("Intro."), "Intro. stays outside the IIFE: {js}");
}

#[test]
fn doc_paragraph_break_is_double_newline() {
    // End-to-end: a blank line between two top-level paragraphs surfaces as ≥2 adjacent "\n"
    // (the runtime's paragraph-break marker `/\n[^\S\n]*\n/`), never coalesced.
    let js = nota_doc("@p{one}\n\n@p{two}\n");
    // Between the two `h("p", …)` there must be at least two "\n" string children.
    assert!(js.contains(r#""\n", "\n""#), "expected adjacent newlines for the para break: {js}");
}

// ===============================================================================================
// Raw spans: verbatim (`|{ … }|`), code (`` `…` `` / fenced), math (`$…$` / `$$…$$`), and general
// backslash escapes. All raw spans lower to `String.raw` tagged templates.
// `CodeInline`/`CodeBlock`/`Math` are ambient prelude bindings.
// ===============================================================================================

// --- General backslash escape -----------------------------------------------------------------

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
    // `\*` is the literal `*` (NOT emphasis, and the backslash is dropped).
    // Two escaped markers → two literal stars, no `<strong>`.
    nota_expr(r"@p{\*hi\*}", r#"h("p", {}, ["*hi*"])"#);
    nota_expr(r"@p{a \_b\_ c}", r#"h("p", {}, ["a _b_ c"])"#);
}

#[test]
fn escape_backtick_and_at_in_prose() {
    // `\@`/`` \` `` keep their char literal, backslash dropped (so prose can mention `@foo` literally).
    nota_expr(r"@p{see \@foo}", r#"h("p", {}, ["see @foo"])"#);
    nota_expr(r"@p{a \` b}", r#"h("p", {}, ["a ` b"])"#);
}

// --- Verbatim `|{ … }|` ------------------------------------------------------------------------

#[test]
fn verbatim_raw_body() {
    // `@code|{@foo{x}}|` → `h("code", {}, [String.raw`@foo{x}`])`. Sigils off, braces literal —
    // `@foo{x}` is raw text, NOT a child.
    nota_expr(r"@code|{@foo{x}}|", r#"h("code", {}, [String.raw`@foo{x}`])"#);
}

#[test]
fn verbatim_keeps_backslash_and_braces() {
    // Raw: backslashes and braces survive verbatim into the `String.raw`.
    nota_expr(r"@code|{a\b {c} d}|", r#"h("code", {}, [String.raw`a\b {c} d`])"#);
}

#[test]
fn verbatim_armed_reentry() {
    // Multi-line verbatim: `|@` splits a raw run and a Nota child. The newline right after `|{` and
    // right before `}|` are dropped (the brace rule); the interior `\n` + 4-space indent survive raw.
    let src = "@code|{\ndef f(x):\n    return |@hl{x}\n}|";
    nota_expr(src, "h(\"code\", {}, [String.raw`def f(x):\n    return `, h(\"hl\", {}, [\"x\"])])");
}

#[test]
fn verbatim_component_tag() {
    // A verbatim body on a component tag.
    nota_expr(r"@Pre|{x@y}|", r"h(Pre, {}, [String.raw`x@y`])");
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

// --- Inline & fenced code ---------------------------------------------------------------------

#[test]
fn code_inline() {
    // `` `@x` `` → `h(CodeInline, {}, [String.raw`@x`])`. Fully raw (the `@` is literal).
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
    // ```` ```python⏎f(x)⏎``` ```` → `h(CodeBlock, { lang: "python" }, [String.raw`f(x)`])`.
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

// --- Math --------------------------------------------------------------------------------------

#[test]
fn math_inline_plain() {
    nota_expr(r"@p{$x^2$}", r#"h("p", {}, [h(Math, {}, [String.raw`x^2`])])"#);
}

#[test]
fn math_inline_interp() {
    // `$a_@i$` → `h(Math, {}, [String.raw`a_${i}`])`. `@i` interpolates a string value.
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
fn math_interp_with_template_breaker_falls_back_to_cooked_template() {
    // A backtick (or literal `${`) cannot ride through `String.raw` with substitutions — the `\`
    // escaping it would leak into the runtime string (`String.raw` does not process escapes).
    // The reader emits a plain (cooked) template instead, whose value reproduces the raw text.
    let js = nota_expr_raw("@p{$a`b_@i$}");
    assert!(!js.contains("String.raw"), "breaker content must not use String.raw: {js}");
    assert_js_eq(&js, r#"h("p", {}, [h(Math, {}, [`a\`b_${i}`])])"#);

    // `${` in display math (a single `$` is literal there): same fallback.
    let js = nota_expr_raw("@p{$$a${b}_@i$$}");
    assert!(!js.contains("String.raw"), "breaker content must not use String.raw: {js}");
    assert_js_eq(&js, r#"h("p", {}, [h(Math, { display: true }, [`a\${b}_${i}`])])"#);
}

#[test]
fn dollar_unterminated_is_literal() {
    nota_expr(r"@p{costs $5 today}", r#"h("p", {}, ["costs $5 today"])"#);
}

#[test]
fn escape_full_list() {
    // The whole backslash-escape list: `` \@ \{ \} \| \$ \* \_ \: \[ \] \` `` and `\\`.
    // Each → its literal char, backslash dropped.
    nota_expr(r"@p{\:}", r#"h("p", {}, [":"])"#);
    nota_expr(r"@p{\[}", r#"h("p", {}, ["["])"#);
    nota_expr(r"@p{\]}", r#"h("p", {}, ["]"])"#);
    nota_expr(r"@p{\_}", r#"h("p", {}, ["_"])"#);
}

#[test]
fn escape_colon_in_body_is_literal() {
    // A `\:` in body text is a literal colon (backslash dropped). (The head-adjacent `@foo\:` form
    // — making `@foo` interpolate before a literal `:` — is a known gap: the JS lexer eats the `\`
    // right after a bare-identifier head. That is separate from the general body escape exercised
    // here. Body-position `\:` works.)
    nota_expr(r"@p{a\: b}", r#"h("p", {}, ["a: b"])"#);
}

#[test]
fn verbatim_validity_with_backtick_in_raw() {
    // A literal backtick inside a verbatim raw body is preserved faithfully: `String.raw` cannot
    // carry a backtick, so the content falls back to a cooked string literal (no `String.raw`, no
    // leaked `\`). The emitted JS re-parses under stock oxc (validity invariant) and the runtime
    // value equals the source.
    let js = nota_expr_raw("@code|{a `b` c}|");
    assert!(
        !js.contains("String.raw"),
        "backtick content uses a cooked literal, not String.raw: {js}"
    );
    assert!(
        js.contains("`b`") && !js.contains(r"\`"),
        "backtick preserved, no leaked backslash: {js}"
    );
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
    // A fenced block at document level → `h(CodeBlock, { lang }, [String.raw`…`])`.
    let js = nota_doc("```python\nf(x)\n```\n");
    assert!(
        js.contains(r#"h(CodeBlock, { lang: "python" }, [String.raw`f(x)`])"#),
        "fenced block at doc level: {js}"
    );
}

#[test]
fn doc_inline_code_and_math() {
    let js = nota_doc("Use `f(x)` and $x^2$ here.\n");
    assert!(js.contains(r"h(CodeInline, {}, [String.raw`f(x)`])"), "{js}");
    assert!(js.contains(r"h(Math, {}, [String.raw`x^2`])"), "{js}");
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
    assert!(js.contains(r"h(CodeInline, {}, [String.raw`code`])"), "{js}");
    assert!(js.contains(r#"h("nota-ul-li""#), "{js}");
}

#[test]
fn unterminated_verbatim_is_an_error() {
    // An unterminated `|{` (no `}|`) is a diagnostic.
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
fn emphasis_close_skips_embedded_expression() {
    // A `*`/`_` *inside* an embedded `@(expr)` must NOT close the surrounding emphasis —
    // `find_emphasis_close` must step over the whole `@`-form (as its own doc comment claims), the
    // same way it steps over raw spans. Adversarial: spaces around the inner `*` make it a
    // marker-valid candidate that a naive scan (with no `@` arm) wrongly takes as the close, leaking
    // the rest of the line (` y) b*`) as literal text.
    nota_expr("@p{*a @(x * y) b*}", r#"h("p", {}, [h("strong", {}, ["a ", x * y, " b"])])"#);
    // Underscore emphasis with a `_` inside the embedded expression.
    nota_expr("@p{_a @(b_c) d_}", r#"h("p", {}, [h("em", {}, ["a ", b_c, " d"])])"#);
}

#[test]
fn verbatim_unicode_and_backtick_preserved() {
    // UTF-8 content survives; an embedded backtick is preserved FAITHFULLY. `String.raw` cannot carry
    // a backtick (it would leak a spurious `\`), so backtick content falls back to a cooked string
    // literal whose codegen escaping reproduces the source exactly.
    let js = nota_expr_raw("@code|{café `x` λ}|");
    assert!(js.contains("café") && js.contains("λ"), "unicode preserved: {js}");
    assert!(js.contains("`x`"), "backtick preserved literally: {js}");
    assert!(!js.contains(r"\`"), "no spurious backslash before a backtick: {js}");
}

#[test]
fn phase_f_mixed_document_end_to_end() {
    // A document mixing all the raw-span features: a heading, prose with inline code + math + an
    // escape, a fenced block, and a verbatim element — exercising the document-body collector + the
    // validity invariant together (the whole emitted module re-parses under stock oxc).
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
    assert!(js.contains(r"h(CodeInline, {}, [String.raw`id`])"), "inline code: {js}");
    assert!(js.contains(r"h(Math, {}, [String.raw`a_${i}`])"), "math interp: {js}");
    assert!(js.contains("cost is $5 and"), "escaped dollar → literal `$` in prose: {js}");
    assert!(js.contains(r#"h(CodeBlock, { lang: "rust" }"#), "fenced: {js}");
    assert!(js.contains(r#"h("figure", {}, [String.raw`verbatim @keep{raw}`])"#), "verbatim: {js}");
}

// ===============================================================================================
// Fuzzing findings (2026-06) — known reader/codegen bugs, as executable specs.
// ===============================================================================================
//
// Each `#[ignore]`d test asserts the **intended** behavior, so it FAILS today — that is the point:
// it is a red, runnable record of a real bug. Run them with
//     cargo test -p oxc_codegen --test integration -- --ignored
// They are `#[ignore]`d only so normal CI stays green; when a bug is fixed, delete its `#[ignore]`
// and the test turns green. Each test's comment states the severity, the repro, and the bug.
//
// Findings whose correct home is elsewhere live there instead: the paragraph-break bug is a runtime
// issue (`packages/runtime/tests/struct.test.ts` — a `test.fails` on `groupParas`), and the
// unterminated-`%%%`-fence rejection is a normal parser diagnostic (`err_unterminated_fence` above).
// So this module is exclusively the `#[ignore]`d, currently-failing reader/codegen bug specs.
mod fuzz_findings {
    use oxc_allocator::Allocator;
    use oxc_codegen::Codegen;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    use super::{nota_doc, nota_expr_raw};

    /// Emit document-mode JS **without** asserting the validity invariant — for the finding whose
    /// whole point is that the emit is *not* valid JavaScript.
    #[track_caller]
    fn emit_doc_unchecked(source: &str) -> String {
        let allocator = Allocator::default();
        let mut program = Parser::new(&allocator, source, SourceType::default())
            .parse_nota_document()
            .unwrap_or_else(|e| panic!("Nota parse failed for {source:?}: {e:?}"));
        oxc_transformer::NotaLowering::new(&allocator, source, false)
            .lower_document_program(&mut program);
        Codegen::new().build(&program).code
    }

    /// Does `js` re-parse cleanly under the STOCK oxc parser? (the validity invariant, as a bool).
    fn reparses(js: &str) -> bool {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, js, SourceType::default().with_module(true)).parse();
        !ret.panicked && ret.errors.is_empty()
    }

    /// Try to parse `source` in document mode; `true` iff it parses without diagnostics.
    fn doc_parses(source: &str) -> bool {
        let allocator = Allocator::default();
        Parser::new(&allocator, source, SourceType::default()).parse_nota_document().is_ok()
    }

    // --- [HIGH] Hyphenated/quoted prop keys emit valid JS (FIXED) --------------------------------
    // A key that is not a valid JS identifier (`data-x`, `aria-label`) must be a STRING-literal key:
    // `{ data-x: v }` parses as `data - x`. FIX: lower_props emits a quoted key for non-identifier
    // names → h("a", { "data-x": v }, ["y"]), which re-parses (validity invariant).
    #[test]
    fn hyphenated_prop_key_should_emit_valid_js() {
        let js = emit_doc_unchecked("@a[\"data-x\": v]{y}\n");
        assert!(reparses(&js), "emit should be valid JS (quoted key), but does not re-parse: {js}");
    }

    // --- 3. [HIGH] String.raw must not corrupt code/verbatim with a backtick or `${` (FIXED) ------
    // String.raw does NOT process a `\` escape, so escaping a backtick/`${` inside it leaks the `\`
    // into the runtime string. FIX: content with either breaker falls back to a cooked string literal
    // (build.rs `build_string_raw`), which reproduces the source exactly — no spurious backslash.
    #[test]
    fn string_raw_should_not_corrupt_backtick_in_code() {
        let js = nota_expr_raw("@code|{a `x` b}|");
        assert!(
            !js.contains(r"\`"),
            "should not backslash-escape backticks (corrupts runtime): {js}"
        );
    }

    // Same root cause for `${` (a template-substitution opener).
    #[test]
    fn string_raw_should_not_corrupt_dollar_brace_in_code() {
        let js = nota_expr_raw("@code|{a ${b} c}|");
        assert!(
            !js.contains(r"\${"),
            "should not backslash-escape a dollar-brace (corrupts runtime): {js}"
        );
    }

    // --- 4. [MEDIUM] CRLF mishandled -------------------------------------------------------------
    // FIX: `\r\n` is normalized in the Scribble pass (a `\r` before the split `\n` is dropped as part
    // of the line terminator), so no `\r` stays glued to text and a trailing `\r\n` after the closing
    // `}` no longer leaks a stray "\r" sibling.
    #[test]
    fn crlf_should_be_normalized() {
        let js = nota_doc("@p{line1\r\nline2}\r\n");
        assert!(!js.contains(r"\r"), "CRLF should be normalized, no stray carriage returns: {js}");
    }

    // --- 5. [MEDIUM] Emphasis honors the Typst open/close boundary rule (FIXED) ------------------
    // FIX: an opener must be followed by content (non-whitespace, not another same marker) and a
    // closer preceded by content, with no empty span (parser `can_open_emphasis`/`can_close_emphasis`
    // + the non-empty close guard). So empty (`**`/`****`), space-padded (`* foo *`), and run
    // (`***x***`) markers no longer produce empty/garbled spans.

    // `**`/`__`/`****` no longer emphasize *nothing* → no empty <strong>/<em>.
    #[test]
    fn empty_emphasis_markers_should_be_literal() {
        let js = nota_expr_raw("@p{** __ ****}");
        assert!(
            !js.contains(r#"h("strong", {}, [])"#) && !js.contains(r#"h("em", {}, [])"#),
            "empty markers should be literal text, not empty elements: {js}"
        );
    }

    // `***x***` no longer splits into empty <strong> pairs.
    #[test]
    fn triple_emphasis_markers_should_not_make_empty_strongs() {
        let js = nota_expr_raw("@p{***benefit***}");
        assert!(!js.contains(r#"h("strong", {}, [])"#), "no empty <strong> from ***x***: {js}");
    }

    // `* foo *` (space after the opening `*`) stays literal.
    #[test]
    fn space_padded_emphasis_should_be_literal() {
        let js = nota_expr_raw("@p{* foo *}");
        assert!(
            !js.contains("strong"),
            "space-padded markers should be literal (no <strong>): {js}"
        );
    }

    // --- 6. [MEDIUM] Colon/block sugar with a `| props` line dedents the body (FIXED) ------------
    // FIX: a `| props` line no longer throws off common-indent stripping. The body suffix after the
    // props now includes the preceding `\n` (collect_colon_body), so the whitespace pass treats its
    // first line as an indent line. notation.md golden: h("foo", { x: 1 }, ["hello"]).
    #[test]
    fn colon_sugar_props_line_should_not_break_dedent() {
        let js = nota_doc("@foo:\n  | x: 1\n  hello\n");
        assert!(js.contains(r#"["hello"]"#), "colon-sugar body should dedent to \"hello\": {js}");
    }

    // --- 7. [MEDIUM] Hyphenated (custom-element) tag names are host tags (FIXED) -----------------
    // FIX: a lowercase head extends over `-`-joined segments when an element trigger follows, so
    // `@my-widget{hi}` → h("my-widget", {}, ["hi"]). Interpolation is unaffected: `@my-foo bar` (no
    // trigger) stays `@my` interpolation + literal `-foo bar` (parser `scan_hyphenated_tag_tail`).
    #[test]
    fn hyphenated_tag_should_be_a_host_tag() {
        let js = nota_doc("@my-widget{hi}\n");
        assert!(
            js.contains(r#"h("my-widget", {}, ["hi"])"#),
            "custom-element tag should be a host tag: {js}"
        );
    }

    // --- 8. [LOW] Leading UTF-8 BOM is stripped (FIXED) ------------------------------------------
    // FIX: a leading UTF-8 BOM (U+FEFF) is skipped at the document start (parse_document_body), so it
    // is not collected as a text node; byte offsets after it are unchanged, so spans stay correct.
    #[test]
    fn leading_bom_should_be_stripped() {
        let js = nota_doc("\u{feff}@p{after bom}\n");
        assert!(!js.contains('\u{feff}'), "leading BOM should be stripped, not emitted: {js}");
    }

    // --- 9. [LOW-MED] Head-adjacent `@foo\:` parses (FIXED) ---------------------------------------
    // `@foo\:` (escape a literal colon right after a bare head, per notation.md) used to make the JS
    // lexer consume the `\` during head classification and choke with an unrelated "Invalid Unicode
    // escape sequence". FIX (parser `try_raw_escaped_head`): the `@ident\…` head is scanned over raw
    // bytes, so `@foo` interpolates and the markup collector renders `\:` as a literal `: hello`.
    #[test]
    fn head_adjacent_colon_escape_should_parse() {
        assert!(
            doc_parses("@foo\\: hello\n"),
            "`@foo\\:` should parse (literal colon per notation.md)"
        );
    }
}

// ===============================================================================================
// Fuzzing findings, round 2 (2026-06) — NON-ignored, currently-FAILING reader/codegen specs.
// ===============================================================================================
//
// Specs found by AI-driven spec-conformance fuzzing (the `nota_inspect` harness), each asserting the
// spec-correct behavior. The FIXED findings pass; the still-open ones are `#[ignore]`d with the
// blocker noted in the reason (like `fuzz_findings` above) so the suite stays green — un-ignore one
// and fix the reader to turn it green. The two purely-runtime findings (object/non-renderable child;
// paragraph break surviving inside a tight element) live in `packages/runtime/tests/serialize.test.ts`.
mod fuzz_findings_2 {
    use oxc_allocator::Allocator;
    use oxc_codegen::Codegen;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    use super::nota_expr_raw;

    /// Document-mode emit WITHOUT the validity assertion (for findings whose emit is invalid JS).
    #[track_caller]
    fn emit_doc_unchecked(source: &str) -> String {
        let allocator = Allocator::default();
        let mut program = Parser::new(&allocator, source, SourceType::default())
            .parse_nota_document()
            .unwrap_or_else(|e| panic!("Nota parse failed for {source:?}: {e:?}"));
        oxc_transformer::NotaLowering::new(&allocator, source, false)
            .lower_document_program(&mut program);
        Codegen::new().build(&program).code
    }

    /// Does `js` re-parse cleanly under the STOCK oxc parser? (the validity invariant, as a bool).
    fn reparses(js: &str) -> bool {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, js, SourceType::default().with_module(true)).parse();
        !ret.panicked && ret.errors.is_empty()
    }

    /// Parse `source` in document mode; `true` iff it parses without diagnostics. (Panics if the
    /// reader panics — itself a finding, which fails the test.)
    fn doc_parses(source: &str) -> bool {
        let allocator = Allocator::default();
        Parser::new(&allocator, source, SourceType::default()).parse_nota_document().is_ok()
    }

    /// Parse + lower a document; `true` iff lowering reported NO diagnostics. A reserved-name
    /// collision (a user `Doc` / runtime-import binding) or a duplicate `export default` → `false`.
    fn doc_lowers_clean(source: &str) -> bool {
        let allocator = Allocator::default();
        let mut program = Parser::new(&allocator, source, SourceType::default())
            .parse_nota_document()
            .unwrap_or_else(|e| panic!("Nota parse failed for {source:?}: {e:?}"));
        oxc_transformer::NotaLowering::new(&allocator, source, false)
            .lower_document_program(&mut program)
            .diagnostics
            .is_empty()
    }

    /// Parse `source` in document mode as **TS-aware** (`tsx`) — the canonical Nota parse (contract
    /// H2). `true` iff it parses without diagnostics.
    fn doc_parses_tsx(source: &str) -> bool {
        let allocator = Allocator::default();
        Parser::new(&allocator, source, SourceType::tsx()).parse_nota_document().is_ok()
    }

    /// Compile a Nota expression with the **TS-aware** (`tsx`) parse, asserting the emit re-parses as
    /// TSX. (The build path additionally strips the types — covered in `oxc::nota`; here we only
    /// pin that a generic call is parsed as a call, not as `f < Foo > x` comparison operators.)
    #[track_caller]
    fn nota_expr_tsx(source: &str) -> String {
        let allocator = Allocator::default();
        let mut expr = Parser::new(&allocator, source, SourceType::tsx())
            .parse_nota_expression()
            .unwrap_or_else(|e| panic!("Nota parse failed for {source:?}: {e:?}"));
        oxc_transformer::NotaLowering::new(&allocator, source, false).lower_expression(&mut expr);
        let mut codegen = Codegen::new();
        codegen.print_expression(&expr);
        let js = codegen.into_source_text();
        let reparse_allocator = Allocator::default();
        let reparse = Parser::new(&reparse_allocator, &js, SourceType::tsx()).parse();
        assert!(!reparse.panicked && reparse.errors.is_empty(), "emit not valid TSX: {js}");
        js
    }

    // ---- crashes (should be diagnostics, not panics) -------------------------------------------

    // [CRASH] `@` + a non-head char (`@@`, `@ `, `@1`, `@.`, `@-`, `@}`, trailing `@`) panics the
    // parser via `markup_to_child`'s `unreachable!("a document is never a body child")`.
    #[test]
    fn fuzz2_at_run_should_diagnose_not_panic() {
        assert!(!doc_parses("@@\n"), "`@@` should be a diagnostic, not a parser panic");
    }

    // ---- emitted JS that does not parse / run --------------------------------------------------

    // NOTE: top-level `await` (in a `%` statement, a prop value, an interpolation, or a `@for`
    // iterable) is intentionally NOT made valid by auto-`async`ifying `Doc`/the IIFE — the reader
    // emits synchronous functions, so such source produces JS that does not parse, by design.

    // [INVALID-JS] an empty/malformed prop group `@p[:]` emits broken JS instead of a diagnostic.
    #[test]
    #[ignore = "deferred: @p[:] support-vs-diagnose is a product call"]
    fn fuzz2_empty_prop_should_emit_valid_js() {
        let js = emit_doc_unchecked("@p[:]{x}\n");
        assert!(reparses(&js), "a malformed prop `[:]` must not emit invalid JS: {js}");
    }

    // [INVALID-JS] a user `% export default …` collides with the reader's `export default Doc`.
    #[test]
    fn fuzz2_export_default_should_not_collide_with_doc() {
        // The reader emits `export default function Doc`, so a user `% export default` is a second
        // default export. The collision is with a reader-injected name, so the parser can't catch it;
        // the lowering diagnoses it.
        assert!(
            !doc_lowers_clean("% export default 5\n@p{x}\n"),
            "a user `export default` must be diagnosed (it would be a second default export)"
        );
    }

    // ---- reader-injected name hygiene (collisions the oxc parser does NOT catch) ----------------

    // [INVALID-JS] reader's `_i` map index collides with a user loop var named `_i` → `(_i, _i) =>`
    // (a duplicate arrow parameter — a SyntaxError in a real engine; oxc's parser does not flag it).
    #[test]
    fn fuzz2_for_index_name_should_not_collide() {
        let js = emit_doc_unchecked("@for(_i of xs){@_i}\n");
        assert!(!js.contains("(_i, _i)"), "the reader's `_i` index collides with the user's: {js}");
    }

    // [INVALID-JS] a module-scope user `Doc` (here `%import Doc`) collides with `function Doc`.
    #[test]
    fn fuzz2_user_doc_binding_should_not_collide() {
        assert!(
            !doc_lowers_clean("%import Doc from \"./x\"\n@p{y}\n"),
            "a user `Doc` import must be diagnosed (it collides with the reader's `function Doc`)"
        );
    }

    // [RUNTIME-BREAK] a user binding `h` (or `Fragment`/`decode`/…) shadows the runtime import the
    // emitted markup calls, so `h(…)` invokes the user's value instead of the runtime function.
    #[test]
    fn fuzz2_user_binding_should_not_shadow_runtime_h() {
        assert!(
            !doc_lowers_clean("%let h = 1\n@p{x}\n"),
            "a user `h` binding must be diagnosed (it shadows the runtime import the markup calls)"
        );
    }

    // [INVALID-JS] a DESTRUCTURED user binding (`%const { h } = …`, `%const [Doc] = …`) shadows a
    // reserved emit name too — the reserved-name check walks the binding pattern, not just plain ids.
    #[test]
    fn fuzz2_destructured_binding_should_be_diagnosed() {
        assert!(
            !doc_lowers_clean("%const { h } = lib\n@p{x}\n"),
            "a destructured `h` must be diagnosed (it shadows the runtime import)"
        );
        assert!(
            !doc_lowers_clean("%const [Doc] = xs\n@p{x}\n"),
            "a destructured `Doc` must be diagnosed (it collides with `function Doc`)"
        );
        // A non-reserved destructured name is fine.
        assert!(
            doc_lowers_clean("%const { x } = lib\n@p{@(x)}\n"),
            "non-reserved destructure is OK"
        );
    }

    // [RUNTIME-BREAK] dynamic-tag `@(_Tag)` whose expr references `_Tag` → `const _Tag = _Tag` (TDZ).
    #[test]
    fn fuzz2_dynamic_tag_binding_should_not_self_reference() {
        let js = nota_expr_raw("@(_Tag){x}");
        assert!(
            !js.contains("const _Tag = _Tag"),
            "dynamic-tag `_Tag` self-references (TDZ): {js}"
        );
    }

    // ---- control-flow / keyword traps ----------------------------------------------------------

    // [DATA-LOSS] `@else` (with the `@` sigil) becomes an `<else>` element, not an else branch.
    #[test]
    #[ignore = "deferred: @else support-vs-diagnose is a product call"]
    fn fuzz2_at_else_should_be_a_branch_not_an_element() {
        let js = emit_doc_unchecked("@if(x){a}@else{b}\n");
        assert!(!js.contains("h(\"else\""), "`@else` is parsed as an <else> element: {js}");
    }

    // [REJECTS-VALID] `@for(const x of xs)` → "'const' is a reserved word"; `@for(let x …)` differs.
    #[test]
    #[ignore = "deferred: @for(const x) support-vs-diagnose product call"]
    fn fuzz2_for_binding_should_accept_declaration_keyword() {
        assert!(
            doc_parses("@for(const x of xs){@x}\n"),
            "@for should accept a `const`/`let` binding"
        );
    }

    // ---- TypeScript in the build path (contract H2: the canonical parse is TS-aware) ------------

    // The canonical Nota parse is TS-aware (`tsx`), so embedded TS (annotations, `type`/`interface`/
    // `enum`, `as`/`satisfies`/`!`) parses. (The build `compile` then strips the types — covered by
    // `oxc::nota::compile_accepts_and_strips_embedded_typescript`.)
    #[test]
    fn fuzz2_build_path_should_accept_embedded_ts() {
        assert!(
            doc_parses_tsx("% const n: number = 1\n@p{@(n)}\n"),
            "the TS-aware parse accepts embedded TS"
        );
    }

    // A generic call `f<Foo>(x)` must parse as a call, not as `f < Foo > x` (comparison operators).
    #[test]
    fn fuzz2_generic_call_should_not_parse_as_comparison() {
        let js = nota_expr_tsx("@(f<Foo>(x))");
        assert!(
            !js.contains("f < Foo"),
            "a generic call is mis-parsed as comparison operators: {js}"
        );
    }

    // ---- `%` / `%%%` statement & fence parsing -------------------------------------------------

    // [REJECTS-VALID] a `%%` line (a `%`-run that is neither 1 nor 3) hard-errors.
    #[test]
    #[ignore = "deferred: %% support-vs-diagnose is a product call"]
    fn fuzz2_double_percent_should_not_hard_error() {
        assert!(
            doc_parses("%% let x = 1\n@p{x}\n"),
            "a `%%` line should not be a hard parse error"
        );
    }

    // [REJECTS-VALID] a `%%%` fence whose body is a bare expression statement (`x`) errors with
    // "Unexpected token", though `x;` is valid JS (a `const x = 1;` body parses fine).
    #[test]
    fn fuzz2_fence_bare_expression_should_parse() {
        assert!(
            doc_parses("%%%\nx\n%%%\n"),
            "a %%% fence with a bare-expression body should parse"
        );
    }

    // [DATA-LOSS] markup on the line right after a `%%%` fence is dropped from the AST entirely.
    #[test]
    fn fuzz2_markup_after_fence_should_not_be_dropped() {
        let js = emit_doc_unchecked("%%%\nconst x = 1;\n%%%\n@p{hi}\n");
        assert!(js.contains("\"hi\""), "markup on the line after a %%% fence is dropped: {js}");
    }

    // [REJECTS-VALID] consecutive single-`%` statements without semicolons (2nd `%` lexed as modulo).
    #[test]
    fn fuzz2_consecutive_percent_statements_should_parse() {
        assert!(
            doc_parses("% let a = 1\n% let b = 2\n@p{x}\n"),
            "consecutive % statements should parse"
        );
    }

    // [REJECTS-VALID] a comment-only (or whitespace-only) `%` line.
    #[test]
    fn fuzz2_comment_only_percent_line_should_parse() {
        assert!(doc_parses("% // a comment\n@p{x}\n"), "a comment-only % line should parse");
    }

    // [REJECTS-VALID] `@foo\:` (escaped literal colon adjacent to a head) — notation.md §Colon.
    #[test]
    fn fuzz2_head_adjacent_colon_escape_should_parse() {
        assert!(doc_parses("@foo\\: hello\n"), "`@foo\\:` should parse (literal colon)");
        // The raw-escaped-head scan is Unicode-aware, so a Unicode head escapes too.
        assert!(doc_parses("@café\\: hello\n"), "`@café\\:` should parse (Unicode head)");
    }

    // ---- self-closing / lists / colon-block ----------------------------------------------------

    // [DATA-LOSS] a dedented nested-list item is duplicated (nested AND as a sibling).
    #[test]
    fn fuzz2_nested_list_dedent_should_not_duplicate() {
        let js = emit_doc_unchecked("- a\n  - b\n- c\n");
        assert_eq!(
            js.matches("[\"c\"]").count(),
            1,
            "a dedented nested-list item is duplicated: {js}"
        );
    }

    // [DATA-LOSS] text after a `@tag[props]` self-closing element drops its first word.
    #[test]
    fn fuzz2_self_closing_props_should_not_drop_following_text() {
        let js = emit_doc_unchecked("@img[src: \"a\"] and text\n");
        assert!(js.contains("and"), "text after a [props] self-closing element is dropped: {js}");
    }

    // [DATA-LOSS] line-start sugar (`#`, lists) is not recognized inside a colon-block body.
    #[test]
    fn fuzz2_colon_block_should_recognize_line_start_sugar() {
        let js = emit_doc_unchecked("@section:\n  # Title\n  body\n");
        assert!(
            js.contains("h(\"h1\""),
            "line-start sugar not recognized in a colon-block body: {js}"
        );
    }

    // [REJECTS-VALID] colon sugar `@head:` inside a braced body swallows the `}` → "Expected `}`".
    #[test]
    fn fuzz2_colon_sugar_in_braced_body_should_parse() {
        assert!(
            doc_parses("@p{@a: b}\n"),
            "colon sugar inside a braced body should parse, not error"
        );
    }

    // [DATA-LOSS] a void element with children silently drops them at render — diagnose it instead.
    #[test]
    #[ignore = "deferred: void-children support-vs-diagnose product call"]
    fn fuzz2_void_element_children_should_be_diagnosed() {
        assert!(!doc_parses("@br{hello}\n"), "a void element with children should be diagnosed");
    }

    // ---- whitespace / Scribble -----------------------------------------------------------------

    // [WHITESPACE] the document's first line keeps its indentation while later lines dedent.
    #[test]
    fn fuzz2_document_first_line_indent_should_be_stripped() {
        let js = emit_doc_unchecked("  a\n  b\n");
        assert!(!js.contains("\"  a\""), "the document's first line keeps its indentation: {js}");
    }

    // [WHITESPACE] trailing whitespace in a list-item body is not trimmed.
    #[test]
    fn fuzz2_list_item_trailing_whitespace_should_be_trimmed() {
        let js = emit_doc_unchecked("- a  \n- b\n");
        assert!(
            !js.contains("\"a  \""),
            "trailing whitespace in a list-item body is not trimmed: {js}"
        );
    }

    // [WHITESPACE] a blank line after a list item leaves a stray newline inside the item.
    #[test]
    fn fuzz2_blank_line_after_list_item_should_not_leave_newline() {
        let js = emit_doc_unchecked("- a\n- b\n\npara\n");
        assert!(
            !js.contains(r#"["b", "\n"]"#),
            "blank line after a list item leaves a stray \\n: {js}"
        );
    }

    // [WHITESPACE] an empty list item carries a stray newline body.
    #[test]
    fn fuzz2_empty_list_item_should_not_have_stray_newline() {
        let js = emit_doc_unchecked("- \n");
        assert!(!js.contains(r#"["\n"]"#), "an empty list item has a stray newline body: {js}");
    }

    // [WHITESPACE] a colon-sugar body keeps a trailing newline that a brace body correctly drops.
    #[test]
    fn fuzz2_colon_body_should_drop_trailing_newline() {
        let js = emit_doc_unchecked("@foo:\n");
        assert!(!js.contains(r#"["\n"]"#), "a colon-sugar body keeps the trailing newline: {js}");
    }

    // [WHITESPACE] a bare CR (not part of CRLF) is not normalized to a line break.
    #[test]
    fn fuzz2_bare_cr_should_be_normalized() {
        let js = emit_doc_unchecked("a\rb");
        assert!(!js.contains(r"\r"), "a bare CR is not normalized to a line break: {js}");
    }

    // [WHITESPACE] kept indentation (beyond the leftmost line) is split into its own text node.
    #[test]
    fn fuzz2_kept_indent_should_join_content() {
        let js = emit_doc_unchecked("@foo{\n  begin\n    x\n  end\n}");
        assert!(
            js.contains(r#""  x""#),
            "kept indentation is split from the content (spec joins it): {js}"
        );
    }

    // [WHITESPACE] a U+2028 line separator is not treated as a line break.
    #[test]
    fn fuzz2_unicode_line_separator_should_break() {
        let js = emit_doc_unchecked("a\u{2028}b");
        assert!(js.contains(r#""a", "\n", "b""#), "U+2028 is not treated as a line break: {js}");
    }

    // [WHITESPACE] a control-flow branch leaks the author's readability spaces (`{ a }` → " a ").
    #[test]
    fn fuzz2_control_flow_branch_should_trim_surrounding_space() {
        let js = nota_expr_raw("@if(x){ a }");
        assert!(
            !js.contains(r#"Fragment(" a ")"#),
            "a control-flow branch leaks surrounding spaces: {js}"
        );
    }

    // ---- sugar recognition / headings / fences -------------------------------------------------

    // [DATA-LOSS] markup on the line after a `%%%` fence (covered above); here: indented heading is
    // not recognized even though an indented LIST is.
    #[test]
    fn fuzz2_indented_heading_should_be_recognized() {
        let js = emit_doc_unchecked("  # H\n");
        assert!(
            js.contains("h(\"h1\""),
            "an indented heading is not recognized (indented lists are): {js}"
        );
    }

    // [DIVERGENCE] heading sugar requires a literal space and rejects a tab after `#`.
    #[test]
    fn fuzz2_tab_after_hash_should_be_a_heading() {
        let js = emit_doc_unchecked("#\tH\n");
        assert!(
            js.contains("h(\"h1\""),
            "heading sugar requires a literal space, rejects a tab: {js}"
        );
    }

    // [DIVERGENCE] a fenced-code info string uses the whole line as `lang`, not the first token.
    #[test]
    fn fuzz2_fence_lang_should_be_first_token() {
        let js = emit_doc_unchecked("```js extra words\ncode\n```\n");
        assert!(
            !js.contains("extra words"),
            "fenced-code lang should be the first token only: {js}"
        );
    }

    // ---- source-fidelity / component name ------------------------------------------------------

    // [FIDELITY] text/content nodes carry span 0..0 (tag-name strings get real spans) → source maps
    // and Volar mappings point all text content at source position 0.
    #[test]
    fn fuzz2_text_node_should_have_a_source_span() {
        use oxc_ast::ast::{Expression, NotaChild, NotaMarkupKind, Statement};
        let allocator = Allocator::default();
        let program = Parser::new(&allocator, "@p{Hello}", SourceType::default())
            .parse_nota_document()
            .expect("parses");
        let Some(Statement::ExpressionStatement(stmt)) = program.body.first() else {
            panic!("expected an expression statement")
        };
        let Expression::NotaMarkup(markup) = &stmt.expression else {
            panic!("expected Nota markup")
        };
        let NotaMarkupKind::Document(doc) = &markup.kind else { panic!("expected a document") };
        let NotaChild::Element(element) = doc.items.first().expect("one item") else {
            panic!("expected an element")
        };
        let text = element
            .children
            .iter()
            .find_map(|c| if let NotaChild::Text(t) = c { Some(t) } else { None })
            .expect("a text child");
        assert!(
            text.span.start != 0 || text.span.end != 0,
            "text node should carry its real source span, not 0..0: {:?}",
            text.span
        );
    }

    // [F1] the reader keeps a user-supplied component-name arg instead of overriding it with the
    // binding name, so the island manifest's `comp` can mismatch the exported registry key.
    #[test]
    fn fuzz2_component_name_should_use_binding_name() {
        let js = emit_doc_unchecked("%let C = inlineComponent((c) => @em{@c}, \"ZZZ\")\n\n@C{x}\n");
        assert!(
            !js.contains("\"ZZZ\""),
            "F1 should pass the binding name, not the user's name arg: {js}"
        );
    }
}
