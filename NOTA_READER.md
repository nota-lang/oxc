# Nota reader — implementation notes (lives with the code)

Internal design notes for the Nota reader built into this oxc fork (branch `nota`). The
cross-team spec is `/Users/will/Code/nota/design/contract.md`; this file is the **Part-1
implementation memory** — read it before extending the reader. Updated per phase.

## Status: Phases A, B, C complete ✓

- **A** (spike): `@p{Hello}` → `h("p", {}, ["Hello"])`, round-tripped through `oxc_codegen`.
- **B** (element core): host/component/dynamic tags, `[props]` (string/expr/shorthand/spread/
  markup-valued, multiple groups union), recursive bodies, `@{}` fragment, `@name`/`@(expr)`
  interpolation. Embedded JS (prop values, `@(expr)` heads) delegates to `parse_expr`.
- **C** (document mode + whitespace): file → `export default function Doc(){…; return
  decode(Fragment(...))}`; the Scribble whitespace algorithm (one `"\n"` per interior newline,
  §7 para-break); colon/block sugar; `%`/`%%%` statements with top-level routing
  (import/export + F1 hoist+export, other `%`→Doc prelude, **no IIFE**), nested-`%`→IIFE,
  `await`→`async`; component-body markup wrapped in `decode(...)`.

44 codegen fixtures + 76 parser unit tests + 3 `oxc::nota::compile` tests green. The full
canonical golden (contract §2) is blocked on Phase D (`@for`); the F1 *component definition*
already lowers byte-exact to stage-3 (see capstone `canonical_golden_component_matches_stage3`).

### New fork sites added in B/C (beyond the spike's 3)

- **`Kind::MarkupText`** (`lexer/kind.rs`): the promised dedicated body-text kind (replaced the
  spike's `Kind::Str` shortcut). One enum line + one `to_str` arm — BUT it bumped the variant
  count, so **`crates/oxc_estree_tokens/src/raw_transfer/estree_kind.rs` `KINDS_LEN` assert had to
  go 169→170** (a hand-maintained guard, not generated; `MarkupText` is appended near the end so
  the 0–11 discriminants `to_kind` relies on are unaffected). This is the ONE extra coupling the
  "zero new Kind variants" spike note didn't foresee.
- **Lexer offset-resume** (`lexer/mod.rs` `seek_and_lex`/`seek_and_lex_markup`; `lexer/source.rs`
  `set_offset`; `cursor.rs` `nota_seek_to`/`nota_seek_markup`): the seam for parsing `%`/`%%%`
  statement JS at a known offset (keeps `%`-body spans byte-exact) and for resuming markup right
  after a peeked delimiter. `MARKUP_TEXT_END_TABLE` now ALSO stops at `\n` (line boundaries → the
  parser detects line-start `%`/sugar and the Scribble per-line algorithm).
- **`cursor.rs` `expect_markup_text` / `byte_at`**: the JSX-`expect_jsx_child` analog (re-lex the
  next token as markup body text after a closing delimiter), and a raw-source byte peek (the
  element-vs-interpolation switch: `@name{`/`[`/`:` element vs `@name ` interpolation, without the
  JS lexer skipping significant whitespace).
- **`oxc` umbrella crate `nota` module** (`crates/oxc/src/nota.rs`, behind the `codegen` feature):
  the `compile(source, source_map_path) -> Result<NotaCompiled{code, map}, Vec<Diagnostic>>` entry
  — the only place with *both* the reader and `oxc_codegen` (codegen only dev-deps the parser).

## The three fork sites (the shallow-fork seam)

1. **Markup lexer scan-method** · `crates/oxc_parser/src/lexer/markup.rs` (NEW), registered in
   `lexer/mod.rs`. `next_markup_text()` models `next_jsx_child`/`read_jsx_child`: `byte_search!`
   + a `SafeByteMatchTable` (`MARKUP_TEXT_END_TABLE` = `}` | `@` | `{`) scans a maximal literal
   run, leaves source positioned **at** the terminator (unconsumed), returns the run via
   `finish_re_lex`. `cursor.rs` adds `advance_for_markup_text()` (mirrors
   `advance_for_jsx_child`: saves `prev_token_end`, sets `self.token = self.lexer.next_markup_text()`).
   - **Spike shortcut:** the text run is returned as `Kind::Str`. **Phase B/C must promote to a
     dedicated `Kind::MarkupText`** (one `kind.rs` enum line + one `to_str` arm — the match is
     exhaustive, no `_`) once bodies interleave text / `@` / `{}` segments. `Str` won't scale.
   - Body text is taken via `self.token_source(&token)` — the **raw source slice**, NOT
     `cur_string()`. Escape/whitespace processing is owned by the Nota layer (Phase C), and this
     keeps embedded spans byte-identical (feeds the §1.6 span-fidelity invariant + H1 CodeMappings).

2. **`@`-hook in the expression parser** · `crates/oxc_parser/src/js/expression.rs:240`, in
   `parse_primary_expression`:
   ```rust
   Kind::At if self.nota_markup => self.parse_nota_element(),
   Kind::At => self.parse_decorated_expression(),   // unchanged decorator path
   ```

3. **The `parse_nota` module** · `crates/oxc_parser/src/js/nota.rs` (NEW), registered in `js/mod.rs`.
   `parse_nota_element()` parses `@ tag { text }` → `build_h_call()`. `parse_nota_expression()`
   is the `Result`-returning wrapper mirroring `ParserImpl::parse_expression`.

**Public entry** · `crates/oxc_parser/src/lib.rs`: field `nota_markup: bool` on `ParserImpl`
(init `false`) + public `Parser::parse_nota_expression() -> Result<Expression, Vec<OxcDiagnostic>>`.

## The `@` disambiguation rule (LOAD-BEARING — do not break)

A single parser-owned bool **`ParserImpl.nota_markup`** (D3: markup state in the parser, not the
lexer). `nota_markup == true` ⇒ `@` in expression position is **always** Nota markup, never a
decorator. Default `false` ⇒ all existing JS/TS unchanged.

- **`Context` (the bitflag set) is SATURATED — `Context: u8`, all 8 bits used.** Do NOT add markup
  states as `Context` flags; widening to `u16` is a pervasive perf-sensitive change. Use parser
  fields / a small `markup_state` struct for further sub-states (in-verbatim, in-math, at-line-start).
- **Consequence (contract delta):** JS/TS **decorators are unavailable inside `.nota` files (v1).**
  Sound because decorators only appear in class/statement position, never in a Nota expression
  context, and per notation.md an `@`-form is an expression everywhere inside Nota.
- The flag is **set once** and only *read* when recursing into embedded JS — `@` inside embedded JS
  (prop values, `@(expr)` heads, `%` bodies) is still Nota markup. Embedded JS is parsed by
  delegating to oxc's `parse_expr`/`parse_statement` with the flag left ON; it only changes the `@`
  arm, so all other JS parsing is byte-identical.

**Phase-C document-mode entry** should be `parse_nota_document()`: set `nota_markup = true` once,
parse the whole file as markup, emit `export default function Doc() { return decode(Fragment(...)); }`.
For an expression-mode entry that must reject trailing garbage, add `expect(Kind::Eof)` (the spike's
`parse_nota_expression` ignores trailing input — fine for document mode which loops siblings).

## AST-build recipe (`build_h_call`) — Phase B extends each argument

`use oxc_ast::{NONE, ast::*}`; all nodes arena-allocated via `self.ast` (`AstBuilder`):
```rust
let callee   = ast.expression_identifier(Span::empty(span.start), "h");
let tag_lit  = ast.expression_string_literal(tag_span, tag_name, None);     // "p" (host → string)
let props    = ast.expression_object(Span::empty(tag_span.end), ast.vec()); // {}
let text_lit = ast.expression_string_literal(text_span, text_value, None);  // "Hello"
let mut elements = ast.vec(); elements.push(ArrayExpressionElement::from(text_lit));
let children = ast.expression_array(text_span, elements);                   // ["Hello"]
let mut args = ast.vec_with_capacity(3);
args.push(Argument::from(tag_lit)); args.push(Argument::from(props)); args.push(Argument::from(children));
ast.expression_call(span, callee, NONE, args, false)                       // h(tag, props, children)
```
Reusable facts:
- `Expression → Argument` and `Expression → ArrayExpressionElement` via `From::from`
  (`inherit_variants!`). `NONE` = the no-type-arguments sentinel.
- `&'a str: Into<Str<'a>>` — string-literal values take a raw `&str` directly.
- **Tag dispatch (Phase B):** host (lowercase) → `expression_string_literal`; component (Capitalized)
  → `expression_identifier(tag_span, name)`; dynamic `@(expr)` → the IIFE form (contract §3).
  Host-vs-component is literally "is arg0 a string or an identifier".
- **Props (Phase B):** fill the `props` `ObjectExpression` with `ObjectPropertyKind`s
  (string→attr, expr→`{…}`, bare→shorthand, `...x`→spread, markup-valued→nested `build_h_call`).
- **Nested children (Phase B):** push more `ArrayExpressionElement`s — a recursive `build_h_call`
  for `@em{world}`, or string runs from the Phase-C whitespace pass.

## Testing layout

- Parser AST-shape unit tests: `crates/oxc_parser/src/js/nota.rs` `#[cfg(test)]`.
- End-to-end (parse → codegen → assert string + validity invariant): `crates/oxc_codegen/tests/
  integration/nota.rs`. **Lives in oxc_codegen, not oxc_parser**, because `oxc_codegen`
  dev-depends on `oxc_parser` (putting codegen in the parser's dev-deps = a dependency cycle).
- Validity invariant: every emitted JS string re-parses cleanly under the **stock** oxc parser.

## Phase B–F risks / sequencing (from the spike)

- Promote `Kind::Str` → `Kind::MarkupText` before multi-segment bodies (Phase B).
- Whitespace (Phase C) is the fiddly pole: the lexer returns one text *segment* (stops at
  terminator); the parser accumulates segments around `@`/`{` boundaries and applies the Scribble
  algorithm (strip common indent, per-line trim, drop newline after `{`/before `}`, interior
  `"\n"` children, empty/whitespace-only → dropped so `@p{}` → `h("p",{},[])`).
- The re-lex seam (parser sets `self.token = self.lexer.next_markup_text()`) is proven; same seam
  serves `[props]`→JS, `@(expr)`→JS, `%`/`%%%`→statements (the latter by NOT re-lexing — delegate
  to stock parse with `nota_markup` left on).
