# Nota reader — implementation notes (lives with the code)

Internal design notes for the Nota reader built into this oxc fork (branch `nota`). The
cross-team spec is `/Users/will/Code/nota/design/contract.md`; this file is the **Part-1
implementation memory** — read it before extending the reader. Updated per phase.

## Status: Phases A, B, C, D, E, F complete ✓ — **the reader is feature-complete.**

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
- **F** (verbatim / code / math + general escapes): the final reader phase.
  - **General `\` escape**: `\<c>` → literal `<c>` with the `\` **dropped** (`\@ \{ \} \| \$ \* \_ \:
    \[ \] \`` `` and `\\`); a lone trailing `\` is literal. Hooked as a `\` byte-peek arm in all three
    markup collectors (`collect_markup`, `collect_markup_range`, `collect_block_body_range`); `\` added
    to `MARKUP_TEXT_END_TABLE`. (This *fixes* the D/E `\*`-kept-the-`\` behavior — `\*` now emits `*`.)
  - **Verbatim `@head|{ … }|`**: a raw body (sigils off, braces literal, ends at `}|`); the armed
    escape `|@` re-enters Nota (`parse_nota_form`) to produce a **sibling** element child. Raw runs →
    `String.raw\`…\`` children: `@code|{@foo{x}}|` → `h("code", {}, [String.raw\`@foo{x}\`])`. A single
    `\n` right after `|{` / before `}|` is dropped (the Scribble brace rule); otherwise fully raw.
  - **Code**: inline `` `…` `` → `h(CodeInline, {}, [String.raw\`…\`])`; fenced ```` ```lang⏎…⏎``` ````
    → `h(CodeBlock, { lang? }, [String.raw\`…\`])`. Fully raw (no interp). The fence length is the
    opening backtick-run length; a shorter run inside is literal; a `≥3` run whose opener line is bare
    (modulo a lang tag) is a block, else inline.
  - **Math**: `$…$` → `h(Math, {}, [String.raw\`…\`])`; `$$…$$` → `h(Math, { display: true }, […])`.
    Raw LaTeX, but `@name`/`@(expr)` interpolate a **string value** as a `${…}` **substitution** in the
    one `String.raw` template (`$a_@i$` → `String.raw\`a_${i}\``); `\$`/`\@` are literal but **KEEP the
    backslash** (it's LaTeX's own escape). `@name` is scanned over the **raw source** (not the JS
    lexer) so the closing `$` isn't swallowed (`$` is a JS identifier-continue byte).

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

**120 nota codegen fixtures + 76 parser-lib tests + 3 `oxc::nota::compile` tests green** (the codegen
`integration` target is 229 total). Validity invariant holds across all fixtures (emitted JS re-parses
under stock oxc — including every `String.raw` template). Parser conformance (`cargo coverage --
parser`) unchanged vs baseline: test262 100%, babel 99.37%/98.26%, typescript 99.86%/59.22%, misc 100%
(the `semantic_babel` stack-overflow at the tail is pre-existing, unrelated to the parser, and
identical before/after).

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

### New sites added in F (all in `js/nota.rs` + the lexer end-table; NO new fork sites)

F, like D/E, added **zero** new fork seams. It is all in `js/nota.rs` + four bytes added to
`MARKUP_TEXT_END_TABLE` (`\` `` ` `` `$` `|`) so a text run *stops at* the new sigils for the parser's
byte-peek to classify. Raw spans are scanned **over the raw source** in the parser (the D/E
line-construct pattern — `byte_at`/byte slices, never lexing), so the lexer stays dumb. The NOTA_READER
Phase-F note suggested a dedicated lexer scan-method; in the end the existing raw-source-scan idiom was
the cleaner fit (the closers `}|`/fence/`$$` are multi-byte and context-dependent, ill-suited to a
`byte_search!` static table), and it keeps the fork at the same three sites.

- **The `String.raw` builder** (the one AST shape D/E never built) — `build_string_raw` /
  `build_string_raw_interp` build `String.raw\`…\`` as a `TaggedTemplateExpression` (`expression_tagged_
  template`) whose callee is the `String.raw` static-member and whose `TemplateLiteral` quasis carry
  `cooked: None` + a **raw** `Str` (so `\` and `{}` survive — that is the point of `String.raw`).
  **Crucial codegen detail:** we DO NOT use `template_element`'s `escape_raw: true` — it doubles every
  `\` in the printed source, which would make `String.raw\`\sum\`` print `\\sum` and yield the *wrong*
  runtime string. We pass `escape_raw: false` and `raw_quasi` pre-escapes ONLY the two template-syntax
  breakers — a backtick (closes the template) and a `${` (opens a substitution) — with a leading `\`.
  Those two cannot round-trip *exactly* through `String.raw` (JS has no raw escape for a bare backtick;
  the `\` leaks at runtime), but they are degenerate in verbatim/code/math and the escape keeps the
  emitted JS **valid** (the §1.6 validity invariant — every `String.raw` re-parses under stock oxc).
- **`build_raw_element`** wraps a code/math child in `h(<Name>, props, [child])` where `<Name>` is the
  ambient identifier `CodeInline`/`CodeBlock`/`Math` (referenced like a component tag — **no import
  emitted**; the shim prepends the bindings).
- **Verbatim** (`parse_verbatim_element` → `collect_verbatim_body`): triggered by the head-switch arm
  `byte_at(head.end)==b'|' && byte_at(head.end+1)==b'{'`. Scans raw bytes from past `|{`: a `}|` closes;
  a `|@` flushes the raw run, `nota_seek_to(@)`, `parse_nota_form(false)` parses **one** form as a
  sibling child, then the raw scan resumes at `prev_token_end`. Children alternate `String.raw\`run\``
  and Nota elements. Unterminated → `nota_unterminated_verbatim` diagnostic.
- **Code** (`parse_code_span` via `parse_code_or_literal`): a backtick run; `≥3` whose opener line is
  bare (modulo a lang tag, no backticks) → `parse_fenced_code` (closes at a `≥fence_len` run at a line
  start; the code body drops the `\n` before the close fence; **resume is right after the backtick
  run**, NOT the rest of the line, so a trailing `}` that closes an enclosing `@d{…}` body is left for
  the collector). Else inline: close = next `≥fence_len` run (shorter runs literal). No close → the
  backticks are literal text.
- **Math** (`parse_math_span` via `parse_math_or_literal`): `$`/`$$`; scans raw LaTeX skipping `\<c>`
  (escaped, kept) and splitting at `@` into `${…}` substitutions. `@(expr)` delegates to `parse_expr`
  (parens bound it); `@name` is scanned over the **raw source** stopping at `$` — letting the JS lexer
  read `@i$` would eat the closing `$` (a JS identifier-continue byte). No close → the `$` is literal.
- **Emphasis × raw spans** — `find_emphasis_close` now skips a raw span (`skip_raw_span_for_emphasis`
  on `` ` ``/`$`/`|{`) so a `*`/`_` *inside* code/math/verbatim cannot mis-close the emphasis (e.g.
  `*a \`b * c\` d*`). This is the F analog of its existing `{}`-balance / `\`-skip.
- **Diagnostics**: `nota_unterminated_verbatim` (`diagnostics.rs`).
- **Known gap (deferred, not Phase-F core):** the head-adjacent `@foo\:` form (notation.md §Colon —
  `@foo` interpolates, then a literal `:`) is NOT handled: the JS lexer eats the `\` right after a
  bare-identifier head (lexes `foo\:` and chokes on the bad Unicode escape) *before* the head is
  classified, and the `NotaHead.colon_escaped` field that was plumbed for it is still unwired.
  Body-position `\:` works (`@p{a\: b}` → `"a: b"`); only the head-adjacent case is open.

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

## Phase F (verbatim / code / math + general escapes) — DONE ✓ (retrospective)

Phase F lowered `|{…}|` raw bodies, fenced code ```` ```lang…``` ````, inline code `` `…` ``, and
`$…$`/`$$…$$` math to `String.raw` templates, plus the general `\` escape (contract §3 last rows;
notation.md §Verbatim/§Math/§Code). The implementation lives entirely in `js/nota.rs` (the
`Phase F` impl block) + four bytes in `MARKUP_TEXT_END_TABLE`. Key decisions / deviations from the
original guidance, recorded for future readers:

- **Escapes first, exactly as guided.** A `\` byte-peek arm in all three collectors consumes `\<c>` →
  literal `<c>` (`\` dropped); `\` is in the end-table. This *replaced* the D/E `\*`-kept-the-`\`
  behavior. (The old `is_escaped` / `find_emphasis_close` `\`-skip are now belt-and-suspenders: the `\`
  arm consumes `\*` before `*` is ever peeked as an emphasis terminator.)
- **Deviation: raw spans are scanned in the PARSER, not a new lexer scan-method.** The guidance
  proposed a `next_markup_text` lexer sibling with a raw end-table. In practice the closers (`}|`, a
  `≥fence_len` backtick run at a line start, `$`/`$$`) are multi-byte and context-dependent — a poor
  fit for `byte_search!`'s *static* `SafeByteMatchTable`. The established D/E raw-source-scan idiom
  (`byte_at` + byte slices over `source_text`, the parser owning semantics, the lexer staying dumb)
  was the cleaner fit and kept the fork at the **same three sites**. The only lexer touch is the
  four new end-table bytes so a text run *stops at* the sigils for the byte-peek to classify.
- **`String.raw` builder caveat (the load-bearing gotcha).** `template_element(escape_raw: true)`
  doubles every `\` in the *printed* source — fatal for `String.raw`, whose printed body must equal the
  runtime string (so a LaTeX/code `\` prints as one `\`). We use `escape_raw: false` + `raw_quasi`
  pre-escapes ONLY a backtick and a `${` (the template-syntax breakers). Those two cannot round-trip
  exactly through `String.raw` (the `\` leaks at runtime) but are degenerate in raw content; the escape
  keeps the emitted JS valid (the §1.6 validity invariant — covered by `assert_valid_js` on every
  fixture, incl. a literal-backtick verbatim and a `${`-in-LaTeX math test).
- **`|@` re-arm** = flush raw run, `nota_seek_to(@)`, `parse_nota_form(false)` for one **sibling**
  child, resume the raw scan at `prev_token_end`. **Math `@`-interp** = a `${…}` **substitution** in
  the single `String.raw` template (a different shape from verbatim's sibling children); `@name` is
  read over the raw source so the closing `$` (a JS ident-continue byte) isn't swallowed.
- **Ambient prelude bindings** `CodeInline`/`CodeBlock`/`Math` referenced as bare identifiers — no
  import emitted (the shim prepends them), exactly like a component tag.
- **Raw spans are pushed as `BodyItem::Child`** (pre-lowered `h(...)`), so `apply_whitespace`/Scribble
  never touch their content — as guided.
- **Emphasis now skips raw spans** in `find_emphasis_close` (`skip_raw_span_for_emphasis`), so a marker
  inside code/math/verbatim can't mis-close the emphasis.
- **One deferred gap** (out of Phase-F scope): head-adjacent `@foo\:` (the JS lexer eats the `\` after
  a bare-ident head before classification). See the "New sites added in F" §. Body-position `\:` works.

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
  - **Phase-F status for H1:** the math `@(expr)`/`@name` interpolations are spliced oxc nodes that
    *do* carry source spans (the substitution exprs in `build_string_raw_interp`), and `parse_math_interp`
    builds the `@name` identifier with a real `Span::new(name_start, j)` — so they are H1-mappable like
    any `@(expr)`. But the **`String.raw` scaffolding is generated-only** (the `String.raw` member, the
    `h(CodeInline|…)` wrapper, the `TaggedTemplateExpression`/`TemplateLiteral` nodes all use synthetic
    spans) and stays unmapped. One nuance H1 must respect: a raw quasi's source ≠ its emitted text when
    `raw_quasi` injected an escaping `\` before a backtick/`${` — those quasis are not byte-identical to
    source, so they cannot be 1:1 mapped (mark unmapped); the common (un-escaped) raw quasi *is*
    byte-identical to its `[from,to)` source slice and could be mapped if a raw-content hover is ever
    wanted (low priority — raw spans are opaque to TS).
- **H2 — type-preserving virtual emit.** "Same parse, two codegen tails" (contract §4 H2). The reader
  is already codegen-agnostic (it builds an oxc `Program`/`Expression`; the build emit vs the virtual
  `.tsx` emit differ only in the codegen call + TS-stripping). Embedded TS in `[props]`/`%`/`@(expr)`
  is parsed by oxc's TS-aware `parse_expr`/`parse_statement` already, so the types are *in the AST*;
  H2 just needs the virtual tail to NOT strip them and to print `.tsx`. No reader change expected.

## Phase B–F sequencing notes (history, for context)

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
- **F lesson — the same raw-source-scan idiom scaled to raw spans; no new lexer mode was needed.** The
  D/E "decide over raw source, the lexer stays dumb" pattern extended directly to verbatim/code/math:
  each is a self-contained raw scan (`collect_verbatim_body`/`parse_code_span`/`parse_math_span`) that
  finds its own multi-byte closer and re-seeks the lexer only on resume. The four new end-table bytes
  exist *only* to make a text run stop at the sigils so the byte-peek classifies; they never change
  normal JS/TS lexing (the markup-text lexer path is unreachable unless `nota_markup` is on), so parser
  conformance is byte-identical to baseline.
- **F lesson — `String.raw` codegen is the trap.** `escape_raw: true` (the lexer's own raw escaping)
  doubles `\`, which is correct for an *un*tagged template but **wrong for `String.raw`** (whose printed
  body equals its runtime value). Use `escape_raw: false` and escape only the backtick / `${` template-
  syntax breakers by hand. Verify with the validity invariant (re-parse under stock oxc) — it catches
  a malformed raw template immediately.
