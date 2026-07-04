# Nota reader — architecture notes (lives with the code)

The Nota *reader* built into this oxc fork (branch `nota`): parser, AST, lowering, and compiler
entries. The cross-team spec is `design/contract.md` (authoritative), with surface syntax in
`design/notation.md`. This file describes the **current architecture** — history lives in git.

## Pipeline & file map

```
.nota source
  → oxc_parser         document/expression entry → faithful Nota AST
  → oxc_transformer    NotaLowering: Nota AST → hyperscript (h/Fragment/decode) Program
  → oxc_codegen        JS text (+ sourcemap, + opt-in offset log)
  → crates/oxc/src/nota.rs   the compile entries + Volar CodeMapping join
```
(The parse-stage views — `parseAst`'s document parse and the highlight spans — branch off after
`oxc_parser`; the wasm bindings consume them directly as `Parser` entries.)

| Piece | File |
|---|---|
| Markup lexing (typed child tokens + pure scans) | `crates/oxc_parser/src/lexer/nota.rs` |
| Parser (markup → Nota AST) | `crates/oxc_parser/src/nota/mod.rs` |
| Highlight pass (AST walk + embedded-JS re-lex → spans) | `crates/oxc_parser/src/nota/highlight.rs` |
| Nota AST nodes (`Expression::NotaMarkup` umbrella) | `crates/oxc_ast/src/ast/nota.rs` |
| Lowering pass (AST → hyperscript, `%` routing, F1) | `crates/oxc_transformer/src/nota/{mod,lower,build}.rs` |
| Scribble whitespace algorithm (pure + unit tests) | `crates/oxc_transformer/src/nota/scribble.rs` |
| Volar mapping marks | `crates/oxc_transformer/src/nota/mapping.rs` |
| Compile entries + CodeMapping join (+ H1/H2 tests) | `crates/oxc/src/nota.rs` |
| Dev tools | `crates/oxc/examples/nota_compile.rs`, `nota_inspect.rs` |
| wasm bindings (playground) | `napi/nota_wasm/src/lib.rs` |
| E2E fixtures (parse→lower→codegen, exact-emit) | `crates/oxc_codegen/tests/integration/nota.rs` |

**Parse-then-lower** (the shape oxc uses for JSX): the parser leaves every `@`-form in place as
`Expression::NotaMarkup` and a whole file as one `NotaMarkupKind::Document` statement. The eight
`@`-forms live in the `NotaForm` sub-enum, inherited (via `inherit_variants!`, the
`Statement`/`Declaration` pattern) by every position that can hold a form — `NotaMarkupKind`,
`NotaChild`, `NotaPropValue`, `NotaVerbatimPart` — so `parse_nota_form` returns one `NotaForm`
that converts by zero-cost `From`, and the lowering has a single `lower_form` dispatch;
`NotaLowering` (a `VisitMut` + document rebuild) produces the emitted module. This supersedes
`design/implementation.md` D1/D2 (parse-time lowering, zero new AST nodes) — the faithful AST buys
the playground's `parseAst` ESTree view, testable stages, and the groundwork for a `.nota`
formatter, at the cost of the generated-code churn D2 warned about (paid once; regenerate with
`just ast`, which panics at the end on a missing `oxfmt` — exit 101 is expected; verify with
`cargo build -p oxc_ast`).

## The fork seam (kept deliberately narrow)

1. **Lexer** — `lexer/nota.rs`, `pub mod` in `lexer/mod.rs`. `next_nota_child` (the
   `next_jsx_child` analog) returns one markup child per call: a maximal `Kind::MarkupText` run or
   a single *consumed* sigil as a typed token (`@` `{` `}` `\n` `*` `_` `\` `` ` `` `$` `|`; new
   kinds in `lexer/kind.rs`). `next_nota_head` lexes an `@`-head with Nota identifier rules — a
   `\` *terminates* the head (so `@foo\:` works) instead of starting a JS `\u` escape. Plus
   offset-seek entries (`seek_and_lex{,_markup,_nota_head}`, `Source::set_offset`) and a temporary
   source-end clamp (`set_end_offset`) for bounding statement parses.
2. **Parser hook** — `js/expression.rs`: `Kind::At if self.nota_markup => parse_nota_form(...)`.
   The `nota_markup` bool on `ParserImpl` is the *entire* `@`-vs-decorator disambiguation
   (decorators are unavailable inside `.nota`, contract R7). Do NOT try to move markup state into
   `Context` — its `u8` is bit-saturated.
3. **The `nota` parser module** — `nota/mod.rs` + entries in `lib.rs`
   (`parse_nota_expression` / `parse_nota_document`), cursor seams in `cursor.rs`
   (`advance_for_nota_child`, `nota_seek_to/markup/head`), diagnostics in `diagnostics.rs`.

Codegen has two additions: an opt-in offset log riding the existing `add_source_mapping` hooks
(for the CodeMapping join), and `unreachable!` arms for `NotaMarkup` (always lowered first).
`oxc_formatter` has stub `FormatWrite` impls for the Nota nodes (same reason).

## How parsing works (the invariants)

- **The parser drives the lexer between JS and markup modes per call** — there is no persistent
  lexer mode field (unlike Typst): oxc's lexer returns plain tokens, and each pull site chooses
  `bump_any` (JS), `advance_for_nota_child` (markup), or a `nota_seek_*` re-entry at a raw offset.
  After any multi-byte extent is consumed by a scan, the parser re-seeks explicitly.
- **`collect_markup` is the one body loop**, dispatching on typed tokens; `BodyMode`
  (`Body`/`Document`/`Bounded`) is *Axis 1* — the **collection semantics** of a markup body: does a
  depth-0 `}` close the body, do `%` statement lines fire, is collection clipped to a range.
  Balanced `{…}` inside a body is literal text (Scribble `@foo{f{o}o}`); brace depth is a parser
  counter over the typed `LCurly`/`RCurly` tokens.
- **A parser-owned `NotaRegion` stack is *Axis 2*** — the host an inner `@`-form's *tail* resumes
  into, orthogonal to `BodyMode`: `Markup(BodyMode)` (resume by markup-lex — the only region markup
  children may push into), `Js` (an expression-position form or a `k: @form` prop value — resume by
  JS-lex), or `Raw` (the tail after a `|@` armed form in any raw span — *parked*). `resume_at` is the one
  exit primitive every construct returns through, a three-way dispatch on the top region;
  `nota_park` sets a zero-width `Undetermined` token and reads no source (the enclosing raw scan
  owns the following bytes and re-seeks itself from `prev_token_end`), and `Undetermined` makes an
  accidental token inspection after a park loud. A markup-child push under a `Js`/`Raw` top is a
  routing bug and panics.
- **Line-start constructs chain**: the `\n` arm consumes a *run* of `%`/`%%%` statements, list
  runs, then a heading — each resumes at a line start that may open the next. **A body/range start
  is a line start too** (contract R9): `collect_markup`'s entry runs the same hook, so the
  document opener, `@{- item}`, `@foo: - item`, and `*- item*` all arm — with first-line extents
  clipped at the enclosing body's depth-0 `}` (`brace_clip_on_line`, string/`@`-form-aware) or the
  bounded range's end. Literal braces in prose never re-enter `collect_markup`, so `a {- b} c`
  stays text.
- **Raw spans share ONE content model (contract R13)** — verbatim `|{…}|`, inline/block code, and
  inline/fence math are all *raw runs interleaved with `|@`-armed `@`-forms*. Extents are **pure
  pre-scans** first (`lex_code_span` / `lex_math_span` / `verbatim_boundary`); then a **second
  bounded scan** (`armed_boundary`) walks the fixed extent for `|@`, each of which re-enters Nota via
  `parse_nota_form_in(Raw)` — its tail parks, and the scan resumes from `prev_token_end`. A bare `@`
  is literal (no direct interpolation). Dollar spans mirror backtick spans (the `≥N`-run close, the
  fence with a whitespace-only opener tail), diverging only in the TeX escape (the dollar close scan
  skips `\<c>`). Because the extent is fixed first, an armed form's parse is clamped to it: the head
  lexer stops at the close (else `$`, an identifier byte, would be eaten — `|@energy$`), and the pure
  scans see a source view clamped to the extent (`nota_scan_source`), so a nested close past the
  extent is unreachable — a form that swallows the close overruns and is a fatal diagnostic
  (`nota_armed_form_overruns_span`), never a panic. There is no escape for a literal `|@`.
- **Multi-byte extents are measured over the raw source** by the pure scans in `lexer/nota.rs`
  (emphasis close, raw-span extents, list/colon block extents, `else`
  continuation): the closers are multi-byte and context-dependent — a poor fit for token lexing —
  and raw-source matching is robust to lexer mode and to escapes (`\else`, `\*`) that are not
  clean JS tokens. Line-start classifiers (`%`/fence/heading/list/`|`-prop lines) are `lazy-regex`
  patterns over the line slice; the extent walkers step a shared `Scan` byte cursor whose
  embedded-JS skips (`skip_js_string`/`skip_balanced`/`skip_at_form`) make an `@`-form's
  `(…)`/`[…]` groups opaque — a bracket or `*` inside `"…"` cannot unbalance them.
- **Embedded JS is parsed by oxc itself** (`parse_expr` / `parse_statement_list_item` /
  `parse_binding_pattern`) with `SourceType::is_nota()` set, so `@`-forms nest inside embedded JS.
  **The JS lexer's one-token lookahead never reads bytes past a region boundary it does not own**: a
  `[props]` group's `]` is validated *without advancing* and the continuation chosen by a raw byte
  peek at its end, a `|@`-armed form's exit inside a raw span is parked, and a raw span's runs stay
  the raw scan's — each once mis-lexed a trailing `\`-run as a JS escape. The lone exception is a `%`/`%%%`
  statement region, whose boundary is discoverable only *after* the JS parse: it is **bounded** by
  temporarily clamping the lexer's source end (`with_source_end_bound`) to `statement_bound` — the
  next line-leading `%` or the first **blank line** (contract R8: ASI applies as at end of input) —
  / the closing fence; otherwise the JS lexer reads the delimiter as `%` (modulo) or `%%%` as three
  operators. Within the bound a `%` line is a JS statement *list* (`% a(); b();`), transitioning to
  markup at end-of-line; stale lexer diagnostics from the trailing one-token lookahead (markup bytes
  JS can't lex) are **erased** when the region parses clean.
- **Every text child is a real source slice** — including single-byte sigils that turned out
  literal — so `NotaText` spans are always true source positions (sourcemaps / Volar / ESTree).
- **`@`-head commit protocol**: the head's boundary token (bare ident or the `)` of `@(expr)`) is
  left as lookahead until `commit_head` classifies the glued trigger (`{` `[` `:` `|{` or none)
  and consumes it in the lexer mode that trigger implies. This is the single
  whitespace-sensitive byte peek at the head→body boundary (`@foo{` element vs `@foo ` interp).
- **The glued `:` is *positional* (contract R9)**: `@head:` sugars only where the form is a
  markup-body child (the top `NotaRegion` is `Markup`, never a `Js` island or a `Raw` scan) **and**
  its `@` sits at a line start modulo whitespace — walking back over spaces/tabs reaches offset 0, a
  `\n`, or the top markup frame's body start (`Markup { start }`; a body's own start is a line start,
  so `@a: @b: c` chains and `@p{  @a: b}` fires). One `colon_trigger_live` is computed at
  `parse_nota_form` entry and threaded into *both* trigger consumers — `commit_head` and the
  hyphen-extension check — so a dead colon uniformly demotes to interpolation (the `:` stays literal)
  and never extends a hyphenated head (`t @my-foo:` → `@my` + `-foo:`). A colon body nested in a
  bounded frame (emphasis / heading / list-item / colon) additionally clips at that frame's end, so
  `*@a: bar* rest` yields `strong[a["bar"]]` + `" rest"` rather than swallowing the tail.

## Lowering (the emit surface)

`NotaLowering` owns everything from Nota AST to the contract §1–§3 emit: the Scribble whitespace
algorithm (`scribble.rs`, pure, unit-tested against the reference reader — one `"\n"` child per
interior newline, never coalesced: a blank line = two adjacent `"\n"`, the runtime's
paragraph-break marker, contract §7); document assembly (`export default function Doc()`, `%`
routing: `import`/`export` + F1 component bindings hoist to module scope, other statements prepend
into `Doc`; a `%` nested in an element body scopes the remaining siblings into an IIFE);
`@for` → `iter.map((bind, _i) => Fragment({ key: _i }, ...))` with a collision-checked fresh `_i`;
reserved-name collision diagnostics (`Doc`/`h`/`Fragment`/`decode`/`inlineComponent`/
`blockComponent`).

Semantic pins (deliberate, tested):
- **`Doc` and the nested-`%` IIFE are always synchronous** — no `await`-driven auto-`async`
  (supersedes notation.md §Statements; top-level `await` emits non-parsing JS by design).
- **`String.raw` emit**: never use codegen's `escape_raw` (it doubles `\`, wrong for `String.raw`,
  whose printed body must equal the runtime string). Content containing a template breaker (a
  backtick or `${`) falls back to a **cooked string literal** — a `\`-escape inside `String.raw`
  would leak into the runtime value.
- The reader does **not** emit the `@nota-lang/runtime` import (the shim/integrator prepends it);
  `CodeInline`/`CodeBlock`/`Math` are ambient prelude identifiers.

## Compiler entries (`crates/oxc/src/nota.rs`)

One internal pipeline (`compile_internal`: tsx parse → lower → optional TS strip → codegen), three
wrappers:
- `compile(src, map_path?)` — build path; **strips embedded TS** via `oxc_transformer`'s TS pass.
- `compile_with_mappings(src, map_path?)` — build + Volar `CodeMapping`s (types preserved —
  stripping would shift offsets).
- `compile_virtual(src)` — the type-preserving virtual `.tsx` for the language server (lenient on
  collision diagnostics so the editor degrades gracefully).

The CodeMapping join: lowering records source-span *marks* (embedded JS = full caps, component
identifiers = navigation/hover); codegen's offset log records where each source-spanned node was
emitted; the join keeps innermost leaves, then **byte-exact-filters** (source slice == generated
slice) — the load-bearing safety net that drops reformatted composites and host-tag
reinterpretations. Every surviving segment round-trips byte-for-byte.

## Highlighting (`oxc_parser/src/nota/highlight.rs`)

`Parser::parse_nota_highlights(src)` is the **reader-faithful syntax highlighter** — a
parser-stage view (like the document parse behind `parseAst`), consumed directly by the wasm
bindings rather than through `oxc::nota` (it never reaches the lowering, so it is not part of the
compile seam): parse in document mode, walk the Nota AST (`oxc_ast_visit::Visit`) emitting
structural spans (sigils, tag names, prop names, markers, raw runs, escapes), and re-lex the
embedded-JS extents with the crate's own lexer for token classes — holes punched where
`Expression::NotaMarkup` re-enters the JS. Output: `NotaHighlightSpan` (`start`/`end`/
`NotaHighlightKind`) sorted start-asc/end-desc (outer under-layers before contained overlays;
clients paint in list order). The wasm crate ships it as `highlight()` (flat `[start, end, kind]`
`Uint32Array` triples) + `highlightKindNames()`; the playground's CM6 editor paints these
(`packages/playground/src/nota-mode.ts`), replacing the TextMate-grammar path, which structurally
cannot track markup⇄JS mutual nesting. Kind discriminants are a stable wire format — append,
never renumber (`NotaHighlightKind::ALL` is test-guarded). Known approximation: regex literals in
embedded JS re-lex as `/` operators (no parser context in the pump).

## Testing

| What | Where | Run |
|---|---|---|
| E2E fixtures (exact emit + validity invariant) | `oxc_codegen/tests/integration/nota.rs` | `cargo test -p oxc_codegen --test integration nota` |
| Lexer scan units (boundaries, classifiers, string-aware skips) + highlight spans | `oxc_parser` lib (`lexer/nota.rs`, `nota/highlight.rs`) | `cargo test -p oxc_parser --lib nota` |
| Scribble whitespace + mapping marks | `oxc_transformer` lib | `cargo test -p oxc_transformer --lib nota` |
| Compile entries + H1/H2 mappings | `crates/oxc/src/nota.rs` | `cargo test -p oxc --features codegen nota` |
| AST plumbing smoke | `oxc_ast` lib | `cargo test -p oxc_ast --lib nota` |

The **validity invariant** (every fixture's emit re-parses under stock oxc) runs inside the
codegen integration tests. Parser conformance (`cargo coverage -- parser`) must stay byte-identical
to upstream — the markup lexer path is unreachable unless `nota_markup` is set. The
`fuzz_findings*` modules in the integration tests hold `#[ignore]`d specs for the still-open
product calls (see `TODO.md` at the repo root); the pipeline inspector for new probes is
`cargo run -q -p oxc --example nota_inspect --features codegen -- --inline '<src>'` (debug build
only — release `panic=abort` defeats its per-stage isolation).

## Known gaps

- Deferred product calls (each has an `#[ignore]`d spec): `@else`, `await` inside `@for`,
  `@for (const x of …)`, a `%%` run, `@p[:]`, `@br{children}` void elements.
- Quoted prop keys (`["data-x": v]`) parse but the AST does not record the quoting (the lowering
  re-derives it from identifier validity).
- Error recovery is fatal-heavy: most malformed markup stops the parse with one diagnostic.
  A Typst-style errors-as-nodes model is the natural upgrade if IDE tolerance is ever needed.
