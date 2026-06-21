# Nota reader — implementation notes (lives with the code)

Internal design notes for the Nota reader built into this oxc fork (branch `nota`). The
cross-team spec is `/Users/will/Code/nota/design/contract.md`; this file is the **Part-1
implementation memory** — read it before extending the reader. Updated per phase.

## Status: Phases A, B, C, D, E complete ✓

- **A** (spike): `@p{Hello}` → `h("p", {}, ["Hello"])`, round-tripped through `oxc_codegen`.
- **B** (element core): host/component/dynamic tags, `[props]` (string/expr/shorthand/spread/
  markup-valued, multiple groups union), recursive bodies, `@{}` fragment, `@name`/`@(expr)`
  interpolation. Embedded JS (prop values, `@(expr)` heads) delegates to `parse_expr`.
- **C** (document mode + whitespace): file → `export default function Doc(){…; return
  decode(Fragment(...))}`; the Scribble whitespace algorithm (one `"\n"` per interior newline,
  §7 para-break); colon/block sugar; `%`/`%%%` statements with top-level routing
  (import/export + F1 hoist+export, other `%`→Doc prelude, **no IIFE**), nested-`%`→IIFE,
  `await`→`async`; component-body markup wrapped in `decode(...)`.
- **D** (control flow): `@if (c){a}` → `c ? Fragment(...a) : null`; `else`/`else if` (contextual,
  raw-source-peeked, blank-line breaks it, `\else` literal) → nested ternary; `@for (bind of iter)
  {body}` → **`iter.map((bind, _i) => Fragment({ key: _i }, ...body))`** (contract §4 E5 — reader
  injects `_i` as the wrapping-Fragment key). All are expressions; they nest in markup + code.
- **E** (markup sugar): emphasis `*…*`→`h("strong",…)` / `_…_`→`h("em",…)` (Typst word-boundary:
  marker iff NOT intra-word; `\*`/`\_` suppress; unbalanced → literal); headings `#{1,6}·`→
  `h("h{n}",…)`; lists `-·`/`+·`/`N.·`→`h("ulli"|"olli",…)` per line (runtime `struct` coalesces
  runs), with block-sugar continuation + deeper-marker nesting. The reader emits **flat per-line/
  per-span sentinels**; paragraph/list/section grouping is the runtime's job (contract §7).

**THE full canonical golden (contract §2) now lowers byte-exact to stage-3** (capstone
`canonical_golden_matches_stage3`), incl. the keyed `Fragment({ key: _i }, …)`, the
`["a","b"].map((x, _i) => …)`, and the `-`→`h("ulli",…)` sentinel. Emit (modulo formatting):
```js
export let Colorized = inlineComponent((children) => {
  let [color, setColor] = useState("red");
  return decode(h("span", { onClick: () => setColor("green"), style: { color } }, [children]));
}, "Colorized");
export default function Doc() {
  return decode(Fragment(["a", "b"].map((x, _i) => Fragment({ key: _i }, h("ulli", {}, [h(Colorized, {}, [x])])))));
}
```

**84 codegen fixtures + 76 parser-lib tests + 3 `oxc::nota::compile` tests green.** Validity
invariant holds across all fixtures (emitted JS re-parses under stock oxc). Parser conformance
(`cargo coverage -- parser`) unchanged vs baseline: test262 100%, babel 99.37%/98.26%, typescript
99.86%/59.22%, misc 100% (the `semantic_babel` stack-overflow at the tail is pre-existing,
unrelated to the parser, and identical before/after).

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

### New sites added in D/E (all in `js/nota.rs` + the lexer end-table; NO new fork sites)

D and E added **zero** new fork seams — they reuse B/C's machinery (the `@`-hook, `MARKUP_TEXT_END_TABLE`
re-lex, `nota_seek_markup`/`byte_at` raw peeks). All new code is in `js/nota.rs` + two bytes in the
end table. The shallow fork stays ≈ the same three sites.

- **`@for`/`@if` interception** — `parse_nota_form`, immediately after `bump_any()` consumes `@` and
  *before* the bare-identifier head path (because `for`/`if` lex as keyword tokens that
  `is_identifier_name` would accept as interpolation idents). Trigger = the `Kind::If`/`Kind::For`
  token **followed by `(`** (`control_head_has_paren` peeks past whitespace over the raw source —
  whitespace after `@for`/`@if` is insignificant, and `@for`/`@if` in JS-token mode skip ws to `(`
  for free). A bare `@if`/`@for` with no `(` falls through to the (degenerate) interpolation path.
  - `@if` → `parse_nota_if`: `parse_paren_expression()` for the cond, `parse_branch_fragment` for the
    branch (`Fragment(...children)`), then `parse_else_continuation`. **`else` is matched over the RAW
    SOURCE** (`peek_else` from one-past-`}`), NOT via the lexer token — so it is robust to `in_body`
    lexer mode AND to `\else` (which is not a clean JS token). `peek_else` skips ws, returns `None` on
    a blank line (≥2 newlines) or a leading `\` (escaped), else matches `else`/`else if`+`{`. `else if`
    re-seeks to the `if` and **recurses** (the recursion owns the resume → only the leaf branch
    resumes); `else {…}` parses the final branch. Output = a (nested) `ConditionalExpression`.
  - `@for` → `parse_nota_for`: `expect(LParen)` · `parse_binding_pattern()` (any pattern) · `expect(Of)`
    (else `nota_for_expects_of` diagnostic — C-style `for` has no `@`-form) · `parse_assignment_expression_or_higher()`
    for iter · `expect_closing(RParen)` · `parse_control_branch` for the body. `build_for_map` →
    `iter.map((bind, _i) => Fragment({ key: _i }, ...body))` (the `_i` is `FOR_KEY_PARAM`; the keyed
    Fragment uses `build_keyed_fragment`, the leading-props `Fragment(props?, …)` form of contract §1).
  - **`parse_control_branch`** = `parse_body` minus the post-close resume (leaves `}` current, returns
    `end`); the whole if/for chain resumes **once** via `resume_after_control(end, in_body)`
    (`nota_seek_markup` if a body child, else `nota_seek_to`).
- **Emphasis `*`/`_`** — added to `MARKUP_TEXT_END_TABLE` (the lexer now stops a text run at them).
  Handled in `collect_markup`'s byte-peek `Some(b'*'|b'_')` arm (and the same arm in
  `collect_markup_range`/`collect_block_body_range`): `is_emphasis_marker(off)` applies the **Typst
  `in_word` rule** — a marker iff NOT (`is_wordy(char_before)` && `is_wordy(char_after)`) and not
  `\`-escaped (`is_escaped`, odd backslash run). `is_wordy` = `char::is_alphanumeric` minus CJK
  (range-approximated; no `unicode-script` dep). An opening marker → `parse_emphasis`, which finds
  the close over the RAW SOURCE (`find_emphasis_close`: next non-in-word same-marker at brace-depth 0,
  bounded by a blank line / the enclosing `}` / EOF; `{}` balanced, `\` skipped) and collects
  `[open+1, close)` via `collect_markup_range` (nests `@`-forms + nested emphasis). **No matching
  close ⇒ literal** (Typst). `char_before`/`char_at` decode full UTF-8 scalars.
- **Headings `#` / lists `-`/`+`/`N.`** — hook the **line-start** machinery: the `collect_markup` `\n`
  arm (at brace depth 0, after the statement check), plus the document/element-body **start** (offset
  0 in `parse_document_body`; the `collect_block_body_range` start for list-item bodies). `try_heading`
  = 1–6 `#` at the line's first char + a space → `h("h{n}", {}, [rest-of-line])` (body via
  `collect_markup_range`, so it nests `*emph*`/`@forms`). `list_marker_at` classifies `-·`/`+·`/`N.·`
  (bullet/number/explicit-number) returning indent + body-column; `parse_list` walks a run of
  same/deeper markers, each item's body extent = rest-of-line + lines indented past the marker
  (`list_item_extent`, the block-sugar rule), collected by `collect_block_body_range` so a **deeper
  marker nests** as `ulli`/`olli` children inside the parent item (the runtime `struct` coalesces the
  inner run into the nested `<ul>`/`<ol>`). The reader does NOT group sibling list runs — it emits one
  `ulli`/`olli` per line and the runtime coalesces (contract §7). `\#`/`\-`/`\+` at line start are
  already safe (the `\` is the first char, so the marker scanners don't fire).
- **Diagnostics** (`diagnostics.rs`, new Nota section): `nota_for_expects_of`,
  `nota_control_expects_body`. Contextual-`else` misuse surfaces as no-continuation (the literal
  `else` text then fails the downstream scope/JS check) rather than a reader error, per the spec
  (`else` is *contextual* — literal elsewhere).

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

## Phase F blockers (verbatim / code / math — the NEXT wave)

Phase F is `|{…}|` raw bodies, fenced code ```` ```lang…``` ````, inline code `` `…` ``, and `$…$`/
`$$…$$` math — all lowering to `String.raw` templates (contract §3 last rows). Where each hooks, and
the gotchas D/E leave:

- **Backslash escapes are the FIRST thing F must finish.** D/E only handle the escapes they need:
  `\else` (`peek_else`) and `\*`/`\_` (`is_emphasis_marker` via `is_escaped`), and `\#`/`\-`/`\+` are
  incidentally safe (the `\` is the line's first char). But the **general** rule — "a backslash
  escapes any character and is literal elsewhere", with the `\` *stripped* from the output — is NOT
  implemented: `\*` currently emits `\*` (marker suppressed, backslash kept), `\@`/`\{`/`\}`/`\|`/`\$`
  aren't handled at all. F must add a backslash arm to the markup-text collectors (`collect_markup`,
  `collect_markup_range`, `collect_block_body_range` — they share the byte-peek shape) that consumes
  `\<c>` → literal `<c>`. **Add `\` to `MARKUP_TEXT_END_TABLE`** so a run stops at it (like `*`/`_`).
- **`is_escaped(off)`** (raw-source odd-backslash scan) is in place and reusable for every F escape
  decision; `find_emphasis_close` already skips `\`-escaped chars, so emphasis won't mis-close inside
  raw spans once those spans are recognized.
- **Raw spans (`|{…}|`, fences, `$…$`) should lex wholly** (impl.md §1.3 Typst lesson: "Lex raw spans
  in the lexer"). The cleanest seam is a new lexer scan-method (the `next_markup_text` sibling) that
  the parser invokes when it peeks the opening sigil — mirror `next_markup_text`'s `byte_search!` +
  end-table, but with the raw-span terminator (`}|`, the closing fence, the closing `$`/`$$`). Then
  build `h(CodeInline|CodeBlock|Math, …, [String.raw\`…\`])`. The `String.raw` template-literal AST is
  the one builder D/E didn't touch — `ast.template_literal` / `ast.expression_tagged_template` with a
  `String.raw` callee, and `cooked: None` raw quasis so `\` and `{}` survive.
- **`|@` armed escape** (re-enter Nota inside a raw body to produce element children) and **`@`
  interpolation inside `$…$`** (→ `${…}` in the `String.raw`) reuse the existing `parse_nota_form`
  recursion + the re-lex seam; the raw-span lexer must yield control back at `|@`/`@`.
- **Math/code are ambient prelude bindings** (`CodeInline`/`CodeBlock`/`Math`) — the reader just
  references the identifiers (like a component tag); no import is emitted (the shim prepends them).
- The whitespace pass (`apply_whitespace`) must NOT touch raw-span content; collect raw spans as a
  pre-built `BodyItem::Child` (already-lowered `h(...)`), never as `BodyItem::Text`, so Scribble skips
  them (the same way emphasis/heading/list elements are pushed as `Child` today).

## Blockers for H1/H2 (Part 5 — CodeMappings + virtual emit; the LSP's deps)

These are the cross-cutting compiler-feedback requirements Part 5 places back on the reader (contract
§4 H1/H2). NOT yet started; the spans needed already exist.

- **H1 — Volar `CodeMappings`.** The reader already keeps embedded-JS spans byte-exact (the §1.6
  span-fidelity invariant: `@(expr)`/`[props]`/`%`-body nodes carry their *source* spans because they
  are spliced, not reformatted). H1 is **exposing** that as per-range `(sourceOffset, generatedOffset,
  length, capabilities)` tuples, not new analysis. **The gap:** generated boilerplate currently uses
  `Span::empty(start)` / synthetic spans (every `build_h`/`build_fragment`/`build_decode`/`build_for_map`
  node), and codegen owns the generated offsets — so H1 needs a codegen pass (or a post-walk) that
  pairs each *source-spanned* node with its emitted offset and marks boilerplate unmapped. Component-
  identifier tags (`h(Aside, …)`) and `@(expr)` heads are the navigation/hover ranges; the keyed
  `Fragment({key:_i},…)` / `.map((x,_i)=>…)` wrappers D/E synthesize are **generated-only** (unmapped).
- **H2 — type-preserving virtual emit.** "Same parse, two codegen tails" (contract §4 H2). The reader
  is already codegen-agnostic (it builds an oxc `Program`/`Expression`; the build emit vs the virtual
  `.tsx` emit differ only in the codegen call + TS-stripping). Embedded TS in `[props]`/`%`/`@(expr)`
  is parsed by oxc's TS-aware `parse_expr`/`parse_statement` already, so the types are *in the AST*;
  H2 just needs the virtual tail to NOT strip them and to print `.tsx`. No reader change expected.

## Phase B–E sequencing notes (history, for context)

- Promote `Kind::Str` → `Kind::MarkupText` before multi-segment bodies (Phase B). ✓
- Whitespace (Phase C) is the fiddly pole: the lexer returns one text *segment* (stops at
  terminator); the parser accumulates segments around `@`/`{` boundaries and applies the Scribble
  algorithm (strip common indent, per-line trim, drop newline after `{`/before `}`, interior
  `"\n"` children, empty/whitespace-only → dropped so `@p{}` → `h("p",{},[])`). ✓
- The re-lex seam (parser sets `self.token = self.lexer.next_markup_text()`) is proven; same seam
  serves `[props]`→JS, `@(expr)`→JS, `%`/`%%%`→statements (the latter by NOT re-lexing — delegate
  to stock parse with `nota_markup` left on), and (D/E) emphasis/heading/list bodies via the
  raw-source-range collectors (`collect_markup_range`/`collect_block_body_range`). ✓
- **D/E lesson — line-start vs inline sugar live at different hooks.** Block sugar (`#`/`-`/`+`/`N.`,
  `%`) keys off the **`\n` arm** of `collect_markup` (+ the body/document start); inline sugar
  (`*`/`_`, `@`, `{}`) keys off the **byte-peek after a markup-text run** (the lexer end-table). Both
  decide marker-vs-literal over the **raw source** (`byte_at`/`char_before`/`is_escaped`), never by
  lexing — the lexer stays dumb (D3: semantics in the parser).
- **D/E lesson — control-flow `else` + emphasis close are matched over RAW SOURCE, not tokens.** This
  is what makes them robust to the `in_body` lexer mode and to escapes (`\else`, `\*`) that aren't
  clean JS tokens. Re-seek (`nota_seek_to`/`nota_seek_markup`) into the right mode only *after* the
  raw-source decision. The chain/span resumes **once** at its end (`resume_after_control`).
