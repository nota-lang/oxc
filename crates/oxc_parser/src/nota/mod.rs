//! Nota `@`-markup → a faithful Nota AST (the *reader*).
//!
//! Nota is a document language whose `@`-markup ultimately lowers to hyperscript `h(...)` /
//! `Fragment(...)` / `decode(...)` calls — but that lowering is **deferred**. This module is the
//! *reader*: it parses `@`-markup into the faithful Nota AST nodes ([`NotaMarkup`] & friends, in
//! `oxc_ast::ast::nota`), leaving every `@`-form in place as `Expression::NotaMarkup` and a whole
//! `.nota` file as a single `NotaMarkupKind::Document` statement. The hyperscript lowering runs
//! *separately*, as an [`oxc_ast_visit::VisitMut`] pass in `oxc_transformer` (`nota::NotaLowering`) —
//! the deferred-pass analog of how `oxc_transformer` lowers JSX. All markup *lexing* state still
//! lives in the parser (the `nota_markup` flag + the markup-text re-lex seam).
//!
//! Layering (this file):
//! * **Element core**: host/component/dynamic tags, `[props]` (string attr, `{…}` expr value,
//!   shorthand, spread, markup-valued), bodies (recursive nesting), `@{…}` fragments,
//!   `@name`/`@(expr)` interpolation. Embedded JS (prop values, `@(expr)` heads) delegates to
//!   oxc's expression parser via the re-lex seam, and is stored verbatim in the Nota nodes.
//! * **Document mode + raw text**: a whole file → a `NotaDocument`. Body text is collected as raw
//!   `NotaText` runs (whitespace is *not* processed here — the Scribble whitespace algorithm runs
//!   later, in the lowering pass). Colon/block sugar is desugared to faithful nodes; `%`/`%%%`
//!   statements and `@`-markup are kept as faithful nodes for the lowering pass to route (module
//!   hoisting, `inlineComponent`/`blockComponent` bindings, `await`→`async`, and the `decode(...)`
//!   wrap all happen there, not here).
//!
//! The re-lex seam (`advance_for_nota_child`) mirrors JSX's `advance_for_jsx_child`: after a markup
//! delimiter we resume lexing in *markup-text* mode so significant whitespace is not skipped by the
//! JS lexer.

// Source offsets and substring lengths are cast to `u32` throughout: oxc's `Span` is `u32`-based
// (sources are bounded to 4 GiB), so these `as u32` casts cannot truncate in practice.
#![expect(
    clippy::cast_possible_truncation,
    reason = "source offsets/lengths fit in u32 (oxc's Span model)"
)]

use oxc_allocator::Vec as ArenaVec;
use oxc_ast::ast::*;
use oxc_diagnostics::OxcDiagnostic;
use oxc_span::{GetSpan, SourceType, Span};

use crate::{
    ParserConfig as Config, ParserImpl, diagnostics,
    error_handler::FatalError,
    lexer::Kind,
    lexer::nota::{
        CodeScan, byte_at, colon_block_extent, colon_prop_line_at, else_peek, escape_extent,
        find_emphasis_close, find_fence_close, heading_at, is_ident_start_at, is_statement_line,
        lex_code_span, line_content_end, line_indent_of, list_item_extent, list_marker_at,
        markup_trigger, next_line_start, next_percent_line_or_end, percent_line_is_empty,
        scan_hyphen_tail, span_of_slice, statement_kind,
    },
};

/// One piece of an element body, collected during the body-segment loop, *before* the Scribble
/// whitespace pass turns it into the final child expressions.
enum BodyItem<'a> {
    /// A literal text run (raw source slice; whitespace not yet processed).
    Text(&'a str),
    /// A nested markup child (element / fragment / interpolation / control flow / sugar / …).
    Child(NotaChild<'a>),
}

/// Demote a markup form ([`NotaMarkup`]) to a body child ([`NotaChild`]), reusing the boxed node
/// (no re-allocation). The document form never appears as a child.
fn markup_to_child(markup: NotaMarkup<'_>) -> NotaChild<'_> {
    match markup.kind {
        NotaMarkupKind::Element(e) => NotaChild::Element(e),
        NotaMarkupKind::Fragment(f) => NotaChild::Fragment(f),
        NotaMarkupKind::Interpolation(i) => NotaChild::Interpolation(i),
        NotaMarkupKind::If(n) => NotaChild::If(n),
        NotaMarkupKind::For(n) => NotaChild::For(n),
        NotaMarkupKind::Code(c) => NotaChild::Code(c),
        NotaMarkupKind::Math(m) => NotaChild::Math(m),
        NotaMarkupKind::Verbatim(v) => NotaChild::Verbatim(v),
        NotaMarkupKind::Document(_) => unreachable!("a document is never a body child"),
    }
}

/// How a markup-collection loop ([`ParserImpl::collect_markup`]) terminated.
enum MarkupClose {
    /// Closed by the body's `}` (depth 0); `end` is one byte past it.
    Curly { end: u32 },
    /// Reached end of file (the document body, or an unterminated element body).
    Eof,
}

/// Selects the three behaviours that differ between [`ParserImpl::collect_markup`]'s callers.
#[derive(Clone, Copy)]
enum BodyMode {
    /// An element / control-flow `{ … }` body: a depth-0 `}` closes it; `%`/`%%%` lines are statements.
    Body,
    /// The whole-file body: a depth-0 `}` is literal; `%`/`%%%` lines are statements; runs to EOF.
    Document,
    /// A bounded sub-range `[.., end)` (emphasis / colon-sugar / list-item / heading body): a `}` is
    /// always literal text and there are no `%`/`%%%` statement lines.
    Bounded { end: u32 },
}

impl BodyMode {
    /// The exclusive end offset of a [`BodyMode::Bounded`] range (`None` for the unbounded modes).
    fn bound(self) -> Option<u32> {
        match self {
            BodyMode::Bounded { end } => Some(end),
            BodyMode::Body | BodyMode::Document => None,
        }
    }

    /// Do line-start `%`/`%%%` statements fire in this mode? (They do not inside a bounded sub-range.)
    fn allows_statements(self) -> bool {
        !matches!(self, BodyMode::Bounded { .. })
    }
}

impl<'a, C: Config> ParserImpl<'a, C> {
    // ===========================================================================================
    // Entry points
    // ===========================================================================================

    /// Parse a whole source string as a single Nota *expression*.
    ///
    /// Mirrors [`ParserImpl::parse_expression`]: enables Nota markup mode (so `@` routes to
    /// markup, not decorators), primes the token stream, parses one `@`-form, and returns the
    /// lowered [`Expression`] or the collected diagnostics. Document mode is
    /// [`Self::parse_nota_document`].
    ///
    /// # Errors
    /// If the source is not a well-formed Nota expression.
    pub(crate) fn parse_nota_expression(mut self) -> Result<Expression<'a>, Vec<OxcDiagnostic>> {
        self.nota_markup = true;
        self.bump_any(); // prime `token` onto the first token
        let markup = self.parse_nota_form(false, false);
        let expr = Expression::NotaMarkup(self.ast.alloc(markup));
        self.finish_nota(expr)
    }

    /// Parse a whole `.nota` file in *document mode* → an oxc [`Program`] holding the **un-lowered**
    /// Nota document.
    ///
    /// The file is markup at the top level. The document is emitted as a single
    /// `Expression::NotaMarkup(NotaMarkupKind::Document(..))` statement; the separate lowering pass
    /// ([`NotaLowering`](oxc_ast::ast::NotaMarkup)) turns it into the runtime module:
    /// ```js
    /// <hoisted import/export + component bindings>
    /// export default function Doc() { <top-level % prelude>; return decode(Fragment(...siblings)); }
    /// ```
    ///
    /// # Errors
    /// If the file is not well-formed Nota.
    pub(crate) fn parse_nota_document(mut self) -> Result<Program<'a>, Vec<OxcDiagnostic>> {
        self.nota_markup = true;
        // Do NOT prime with a JS `bump_any` here: the file starts as markup (or a `%` line), and a
        // leading `\`/`%`/etc. would make the JS lexer choke. `parse_document_body` seeks the lexer
        // into the right mode (markup, or a statement) from offset 0 itself.
        let document = self.parse_document_body();
        let program = self.wrap_document_program(document);
        match self.finish_nota(()) {
            Ok(()) => Ok(program),
            Err(errors) => Err(errors),
        }
    }

    /// Wrap a parsed [`NotaDocument`] into a `Program` carrying it as a single
    /// `Expression::NotaMarkup(Document)` statement — the un-lowered reader output the lowering pass
    /// consumes.
    fn wrap_document_program(&self, document: NotaDocument<'a>) -> Program<'a> {
        let span = document.span;
        let markup = self.ast.nota_markup(span, NotaMarkupKind::Document(self.ast.alloc(document)));
        let expr = Expression::NotaMarkup(self.ast.alloc(markup));
        let stmt = self.ast.statement_expression(span, expr);
        self.ast.program(
            span,
            SourceType::default().with_module(true),
            self.source_text,
            self.ast.vec(),
            None,
            self.ast.vec(),
            self.ast.vec1(stmt),
        )
    }

    /// Shared finalize for the Nota entries: collect fatal/lexer/parser diagnostics.
    fn finish_nota<T>(mut self, value: T) -> Result<T, Vec<OxcDiagnostic>> {
        if let Some(FatalError { error, .. }) = self.fatal_error.take() {
            return Err(vec![error]);
        }
        self.check_unfinished_errors();
        let errors = self.lexer.errors.into_iter().chain(self.errors).collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(value)
    }

    // ===========================================================================================
    // Element / interpolation core
    // ===========================================================================================

    /// Parse one `@`-form: an element (`@tag…`/`@(expr)…`/`@{…}`) or an interpolation
    /// (`@name`/`@(expr)`). Entered with the current token at [`Kind::At`].
    ///
    /// `in_body`: `true` when this form is a *child of a markup body*, so its trailing context is
    /// re-lexed as markup text (the JSX `in_jsx_child` analog). `false` in JS expression position
    /// (top-level, prop values, `@(expr)` heads), where normal JS lexing resumes.
    /// `brace_significant`: `true` when a depth-0 `}` here closes an *enclosing* `{…}` element/control
    /// body (so `@head:` colon sugar must clip its body before it — `@p{@a: b}`), `false` when a `}`
    /// is literal text (the document/fragment top level, a `[props]`/expression position). Threaded to
    /// the colon-sugar extent only; the JS lexer / brace machinery handle every other form.
    pub(crate) fn parse_nota_form(
        &mut self,
        in_body: bool,
        brace_significant: bool,
    ) -> NotaMarkup<'a> {
        let span_start = self.start_span();

        // Consume `@` and lex the head. A head that opens with an identifier char (`@foo`, `@if`,
        // `@café`) is lexed with Nota identifier rules via `next_nota_head`: a trailing `\` (the Nota
        // escape) *terminates* the head — so `@foo\: x` is `@foo` followed by the literal `\:`
        // (notation.md §Colon) — instead of making the JS identifier reader choke on a bad `\u`
        // escape mid-head. Keyword heads still produce `Kind::If`/`Kind::For` (via `match_keyword`),
        // so the control-flow dispatch below is unchanged. Non-identifier heads (`@(expr)`, `@{…}`)
        // keep the JS-lexed path. (Mirrors Typst's per-mode identifier lexing.)
        debug_assert!(self.at(Kind::At), "parse_nota_form entered not at `@`");
        let after_at = self.cur_token().end();
        if is_ident_start_at(self.source_text, after_at) {
            self.nota_seek_head(after_at);
        } else {
            self.bump_any();
        }

        let kind = match self.cur_kind() {
            Kind::If => NotaMarkupKind::If({
                let n = self.parse_nota_if(span_start, in_body);
                self.ast.alloc(n)
            }),
            Kind::For => NotaMarkupKind::For({
                let n = self.parse_nota_for(span_start, in_body);
                self.ast.alloc(n)
            }),
            Kind::LCurly => NotaMarkupKind::Fragment({
                let f = self.parse_fragment(span_start, in_body);
                self.ast.alloc(f)
            }),
            _ => {
                let Some(head) = self.parse_nota_head() else {
                    // `@` not followed by a valid head (`@@`, `@ `, `@1`, `@.`, `@-`, EOF, …): record
                    // the unexpected-token diagnostic, but recover as an EMPTY FRAGMENT rather than
                    // the `unexpected()` dummy `Document` — a `Document` reaching `markup_to_child`
                    // (this `@`-form demoted to a body child) hits an `unreachable!` and panics.
                    self.set_unexpected();
                    let span = self.end_span(span_start);
                    let frag = self.ast.nota_fragment(span, self.ast.vec());
                    return self
                        .ast
                        .nota_markup(span, NotaMarkupKind::Fragment(self.ast.alloc(frag)));
                };
                // Cross the head→body boundary: classify the (whitespace-sensitive) trigger glued to
                // the head and consume the head's boundary token in the mode that trigger implies —
                // both inside `commit_head`. The parser then dispatches on the *typed* trigger, never
                // on raw bytes (the one byte peek lives inside `peek_markup_trigger`).
                match self.commit_head(&head, in_body) {
                    MarkupTrigger::Brace | MarkupTrigger::Bracket => NotaMarkupKind::Element({
                        let e = self.parse_element(span_start, head, in_body);
                        self.ast.alloc(e)
                    }),
                    MarkupTrigger::Colon => NotaMarkupKind::Element({
                        let e =
                            self.parse_colon_element(span_start, head, in_body, brace_significant);
                        self.ast.alloc(e)
                    }),
                    // `@code|{ … }|` — a *verbatim* body: `|{` opens a raw body that ends at `}|`
                    // (sigils off, braces literal; the armed escape `|@` re-enters Nota).
                    MarkupTrigger::Verbatim => NotaMarkupKind::Verbatim({
                        let v = self.parse_verbatim_element(span_start, head, in_body);
                        self.ast.alloc(v)
                    }),
                    // No trigger ⇒ interpolation: the head expression alone.
                    MarkupTrigger::None => NotaMarkupKind::Interpolation({
                        let i = self.finish_interpolation(head);
                        self.ast.alloc(i)
                    }),
                }
            }
        };

        let span = self.end_span(span_start);
        self.ast.nota_markup(span, kind)
    }

    /// The parsed head of an `@`-form: the tag/interpolation expression plus classification needed
    /// to decide element-vs-interpolation and host-vs-component-vs-dynamic.
    fn parse_nota_head(&mut self) -> Option<NotaHead<'a>> {
        if self.eat(Kind::LParen) {
            let expr = self.parse_expr();
            // `self.token` is now `)` (parse_expr stops there). Validate it, but do NOT consume it:
            // like the bare-ident head, the boundary token is left as one-token lookahead so
            // `commit_head` can classify the trigger glued to it (`peek_markup_trigger`) and then
            // consume it in the right lexer mode. `end` is the byte just past `)` — the switch point.
            self.expect_without_advance(Kind::RParen);
            let close_end = self.cur_token().end();
            Some(NotaHead { kind: HeadKind::Dynamic(expr), end: close_end })
        } else if self.cur_kind().is_identifier_name() {
            // Bare identifier head: host (lowercase) / component (Capitalized) / interpolation.
            // `is_identifier_name` also admits keyword-spelled tags (`@section`, `@title`, …).
            let token = self.cur_token();
            let name = self.token_source(&token);
            let span = token.span();
            // Custom-element / hyphenated host tag (`@my-widget`): a lowercase head may continue over
            // `-`-joined identifier segments — but ONLY when an element trigger ({/[/:/|{) follows the
            // full name. Otherwise the `-` is not part of an (interpolation) name (`@my-foo bar` stays
            // `@my` interpolation + literal `-foo bar`), so we keep just the leading identifier.
            if !is_component_name(name)
                && let Some(ext_end) = scan_hyphen_tail(self.source_text, span.end)
                && !matches!(markup_trigger(self.source_text, ext_end), MarkupTrigger::None)
            {
                let full = &self.source_text[span.start as usize..ext_end as usize];
                let span = Span::new(span.start, ext_end);
                return Some(NotaHead { kind: HeadKind::Named { name: full, span }, end: ext_end });
            }
            // Do NOT bump: the identifier is the head's boundary token, left as one-token lookahead
            // (see the dynamic-head branch). `commit_head` consumes it after classifying the trigger.
            Some(NotaHead { kind: HeadKind::Named { name, span }, end: span.end })
        } else {
            None
        }
    }

    /// Finish an `@`-form that turned out to be an *interpolation* (no `{`/`[`/`:`/`|{` trigger):
    /// `@name` → `name`; `@(expr)` → `expr`. The head's boundary token (the bare ident or the `)`)
    /// and the markup-text/JS resume were already handled by [`Self::commit_head`]; this only builds
    /// the spliced expression from the (already-captured) head.
    fn finish_interpolation(&self, head: NotaHead<'a>) -> NotaInterpolation<'a> {
        let expr = match head.kind {
            HeadKind::Named { name, span } => self.ast.expression_identifier(span, name),
            HeadKind::Dynamic(expr) => expr,
        };
        let span = expr.span();
        self.ast.nota_interpolation(span, expr)
    }

    /// Build the [`NotaTag`] for a parsed head: lowercase → host string name, Capitalized →
    /// component identifier, `@(expr)` → dynamic.
    fn head_to_tag(&self, head: NotaHead<'a>) -> NotaTag<'a> {
        match head.kind {
            HeadKind::Named { name, span } => {
                if is_component_name(name) {
                    NotaTag::Component(self.ast.alloc_identifier_reference(span, name))
                } else {
                    let host = self.ast.nota_host_name(span, name);
                    NotaTag::Host(self.ast.alloc(host))
                }
            }
            HeadKind::Dynamic(expr) => {
                let span = expr.span();
                let dyn_tag = self.ast.nota_dynamic_tag(span, expr);
                NotaTag::Dynamic(self.ast.alloc(dyn_tag))
            }
        }
    }

    /// Parse `@head { body }` and/or `@head [props] …`. `head.end` points at `{` or `[`.
    fn parse_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
    ) -> NotaElement<'a> {
        // `commit_head` already consumed the head's boundary token and left the `{`/`[` delimiter as
        // the current token.

        // Accumulate one or more `[props]` groups (faithfully, in source order).
        let mut props = self.ast.vec();
        while self.at(Kind::LBrack) {
            self.parse_props_group(&mut props);
        }

        // Body: `{ … }`, or self-closing (no body) → empty children.
        let (children, end) = if self.at(Kind::LCurly) {
            self.parse_body(in_body)
        } else {
            // Self-closing: consume trailing context as markup text if we are inside a body.
            let end = self.prev_token_end;
            if in_body {
                // Re-lex the trailing markup text starting right after the `]`. The current JS token
                // already consumed the first word past it, so `advance_for_nota_child` would drop it.
                self.nota_seek_markup(end);
            }
            (self.ast.vec(), end)
        };

        let span = Span::new(span_start, end);
        let tag = self.head_to_tag(head);
        self.ast.nota_element(span, tag, props, children, /* is_colon */ false)
    }

    /// `@head:` colon/block sugar. Handled in the document/colon module.
    fn parse_colon_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
        brace_significant: bool,
    ) -> NotaElement<'a> {
        self.parse_colon_body(span_start, head, in_body, brace_significant)
    }

    /// `@{ body }` → a fragment node. `@` already consumed.
    fn parse_fragment(&mut self, span_start: u32, in_body: bool) -> NotaFragment<'a> {
        let (children, end) = self.parse_body(in_body);
        let span = Span::new(span_start, end);
        self.ast.nota_fragment(span, children)
    }

    /// The element trigger glued to a head: which markup-significant byte(s) immediately follow it.
    /// `@p{…}`→`Brace`, `@p[…]`→`Bracket`, `@p:…`→`Colon`, `@code|{…}|`→`Verbatim`, else `None`
    /// (interpolation). This is the typed, whitespace-sensitive analog of Typst's `directly_at`: the
    /// single site that inspects raw bytes for the head→body decision (the byte after a head is not
    /// a JS token — a space is significant, and `|{` is not a JS token — so we peek rather than lex).
    /// Cross the head→body boundary: classify the trigger, then consume the head's boundary token
    /// (the bare identifier, or the `@(expr)` head's `)`) in the lexer mode that trigger implies, and
    /// return the trigger so the caller can dispatch on it. This is the *one* place the head's
    /// boundary token is consumed — uniform across named and dynamic heads — so callers never mix
    /// token and byte consumption at the seam.
    ///
    /// * `Brace`/`Bracket`/`Colon` — the trigger is a JS-lexable token glued to the head, so a single
    ///   `bump` consumes the boundary token and lexes the delimiter (`{`/`[`/`:`) as the next token.
    /// * `Verbatim` — `parse_verbatim_element` scans the body over raw source from `head.end`, so the
    ///   boundary token is left current (we must not let the JS lexer eat the `|`).
    /// * `None` (interpolation) — consume the boundary token, resuming markup text in a body (so
    ///   significant whitespace after the head is not skipped) or normal JS otherwise.
    fn commit_head(&mut self, head: &NotaHead<'a>, in_body: bool) -> MarkupTrigger {
        let trigger = markup_trigger(self.source_text, head.end);
        // An *extended* head — a hyphenated host tag (`@my-widget`) — runs past the lexer's current
        // boundary token, so `bump_any` (which consumes only that token) would mis-position; seek to
        // `head.end` instead. A plain head's boundary token ends exactly at `head.end` → bump.
        let extended = self.cur_token().end() != head.end;
        match trigger {
            MarkupTrigger::Brace | MarkupTrigger::Bracket | MarkupTrigger::Colon => {
                if extended {
                    self.nota_seek_to(head.end);
                } else {
                    self.bump_any();
                }
            }
            // Boundary token stays current; the verbatim body is scanned by absolute offset (head.end).
            MarkupTrigger::Verbatim => {}
            MarkupTrigger::None => {
                if in_body {
                    self.advance_for_nota_child();
                } else {
                    self.bump_any();
                }
            }
        }
        trigger
    }

    // ===========================================================================================
    // Body segment loop + Scribble whitespace
    // ===========================================================================================

    /// Parse a `{ … }` markup body into final child expressions + the end offset (past `}`).
    ///
    /// Entered with the current token at the body-open `{`. Collects raw text segments and nested
    /// `@`-forms (tracking balanced `{…}` as literal text — Scribble `@foo{f{o}o}` → `"f{o}o"`),
    /// then applies the Scribble whitespace algorithm. `in_body` governs how the body's *closing*
    /// `}` resumes lexing (markup text if this element is itself a body child).
    fn parse_body(&mut self, in_body: bool) -> (ArenaVec<'a, NotaChild<'a>>, u32) {
        let open = self.cur_token().span();
        debug_assert!(self.at(Kind::LCurly), "parse_body entered not at `{{`");
        self.advance_for_nota_child(); // switch the lexer into markup-body mode

        let mut items: Vec<BodyItem<'a>> = Vec::new();
        let close = self.collect_markup(&mut items, BodyMode::Body);
        match close {
            MarkupClose::Curly { end } => {
                // Body close `}`. Consume it, resuming markup text iff this element is a child.
                if in_body {
                    self.advance_for_nota_child();
                } else {
                    self.bump_any();
                }
                (self.body_items_to_children(items), end)
            }
            MarkupClose::Eof => {
                self.expect_markup_body_close(open);
                (self.body_items_to_children(items), self.prev_token_end)
            }
        }
    }

    /// Materialize collected body items into the faithful child list (raw — the Scribble whitespace
    /// pass runs at lowering time). Text runs become [`NotaText`] children carrying their real source
    /// span (so prose content — the bulk of a document — maps back to source for sourcemaps / Volar,
    /// per contract H1), via [`Self::span_of_slice`].
    fn body_items_to_children(&self, items: Vec<BodyItem<'a>>) -> ArenaVec<'a, NotaChild<'a>> {
        let mut out = self.ast.vec_with_capacity(items.len());
        for item in items {
            out.push(match item {
                BodyItem::Text(t) => {
                    let text = self.ast.nota_text(span_of_slice(self.source_text, t), t);
                    NotaChild::Text(self.ast.alloc(text))
                }
                BodyItem::Child(child) => child,
            });
        }
        out
    }

    /// Run `f` with the lexer's source end temporarily clamped to `bound` (so it lexes `Eof` there),
    /// restoring the prior end afterwards. Bounds a `%`/`%%%` statement's JS parse to its extent
    /// without the caller having to remember to restore the end (a forget-to-restore footgun).
    fn with_source_end_bound<R>(&mut self, bound: u32, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.lexer.nota_source_end();
        self.lexer.nota_set_source_end(bound);
        let result = f(self);
        self.lexer.nota_set_source_end(saved);
        result
    }

    /// Parse a run of consecutive `%`/`%%%` statement lines from `line_start`, pushing each parsed
    /// statement as a `NotaChild::Statement`. Returns the offset of the first non-statement line.
    /// Used for both document and element bodies (full-document deferral; lowering routes vs IIFEs).
    fn collect_statements(&mut self, line_start: u32, items: &mut Vec<BodyItem<'a>>) -> u32 {
        let mut at = line_start;
        while let Some((content, is_fence)) = statement_kind(self.source_text, at) {
            let end = if is_fence {
                self.collect_fence_statements(content, items)
            } else if percent_line_is_empty(self.source_text, content) {
                // A `%` line with no statement — empty (`%`), whitespace-only, or only a `//` line
                // comment — is a no-op: skip the line (it must NOT swallow the following markup as a
                // statement). `content` is on this line, so `next_line_start` below advances past it.
                content
            } else {
                // Bound the JS parse to this `%` statement's extent: the next line-leading `%` (a Nota
                // statement delimiter the JS lexer would otherwise mis-read as modulo — `1⏎% …`), or
                // source end. A multi-line statement (`%const X = inlineComponent((c) => {⏎ … ⏎})`) has
                // no intervening line-leading `%`, so the bound is source end and the parser stops
                // naturally at the next markup (`@`), exactly as before.
                let bound = next_percent_line_or_end(self.source_text, content);
                debug_assert!(self.source_text.is_char_boundary(bound as usize));
                let stmt = self.with_source_end_bound(bound, |p| {
                    p.nota_seek_to(content);
                    p.parse_statement_list_item(crate::context::StatementContext::StatementList)
                });
                let e = self.prev_token_end;
                self.push_statement(items, stmt);
                e
            };
            // A fence's resume offset is already the start of the line AFTER the closing fence; a
            // single-`%` statement's `end` is mid-line, so it advances to the next line. (Advancing a
            // fence again would drop the line right after it — markup or another statement.)
            at = if is_fence { end } else { next_line_start(self.source_text, end) };
            if !is_statement_line(self.source_text, at) {
                break;
            }
        }
        at
    }

    /// Parse the inner statements of a `%%%`…`%%%` fence (from `inner_start`), pushing each as a
    /// `NotaChild::Statement`. Returns the offset past the closing fence.
    fn collect_fence_statements(&mut self, inner_start: u32, items: &mut Vec<BodyItem<'a>>) -> u32 {
        let (inner_end, after_fence) = find_fence_close(self.source_text, inner_start);
        debug_assert!(self.source_text.is_char_boundary(inner_end as usize));
        // Bound the fence body's JS parse to `[inner_start, inner_end)` so the closing `%%%` is never
        // read as JS: a bare-expression body (`x`) then EOF → ASI → `x;`; without the bound `x⏎%%%`
        // mis-lexes as `x % % %` ("Unexpected token").
        self.with_source_end_bound(inner_end, |p| {
            p.nota_seek_to(inner_start);
            while p.prev_token_end < inner_end && !p.at(Kind::Eof) && !p.has_fatal_error() {
                if p.cur_token().start() >= inner_end {
                    break;
                }
                let stmt =
                    p.parse_statement_list_item(crate::context::StatementContext::StatementList);
                p.push_statement(items, stmt);
            }
        });
        after_fence
    }

    /// Push a parsed `%`/`%%%` statement as a `NotaChild::Statement` body item.
    fn push_statement(&self, items: &mut Vec<BodyItem<'a>>, stmt: Statement<'a>) {
        let span = stmt.span();
        let node = self.ast.nota_statement(span, stmt);
        items.push(BodyItem::Child(NotaChild::Statement(self.ast.alloc(node))));
    }

    /// The core markup-collection loop, shared by element bodies, the whole-file document body, and
    /// bounded sub-ranges (emphasis / colon-sugar / list-item / heading bodies, via
    /// [`Self::collect_markup_range`]). Collects [`BodyItem`]s (text runs / nested `@`-forms), tracking
    /// balanced `{…}` braces as literal text and emitting `\n` runs verbatim into the text buffer (the
    /// Scribble whitespace pass owns line handling). Entered with the current token already lexed as
    /// the first markup-text run.
    ///
    /// [`BodyMode`] selects the three behaviours that differ between callers: whether a depth-0 `}`
    /// closes the body or is literal text, whether line-start `%`/`%%%` statements fire, and whether
    /// collection is bounded to `[.., end)`. Returns how the body terminated (the document / bounded
    /// callers ignore it).
    fn collect_markup(&mut self, items: &mut Vec<BodyItem<'a>>, mode: BodyMode) -> MarkupClose {
        let mut depth = 0u32; // balanced-brace depth inside the body
        loop {
            if self.has_fatal_error() {
                return MarkupClose::Eof;
            }
            // A bounded range stops once the cursor reaches `end`: every sigil is its own token (start
            // = its offset), so this one check bounds the whole range.
            if let BodyMode::Bounded { end } = mode
                && self.cur_token().start() >= end
            {
                return MarkupClose::Eof;
            }
            // `next_nota_child` returns each markup sigil as a typed token (consumed), so this loop
            // dispatches on `cur_kind()` — never on raw bytes (the JSX/Typst model). A sigil's own
            // offset (`cur_token().start()`) is passed to the span helpers that re-scan and re-seek
            // (code/math/emphasis/escape span more than the one consumed sigil byte).
            match self.cur_kind() {
                Kind::MarkupText => {
                    let token = self.cur_token();
                    // In a bounded range, clip the run to `end` (and stop once it reaches it).
                    let (text, reached_end) = if let BodyMode::Bounded { end } = mode {
                        let clip = token.end().min(end);
                        (
                            &self.source_text[token.start() as usize..clip as usize],
                            token.end() >= end,
                        )
                    } else {
                        (self.token_source(&token), false)
                    };
                    if !text.is_empty() {
                        items.push(BodyItem::Text(text));
                    }
                    if reached_end {
                        return MarkupClose::Eof;
                    }
                    self.advance_for_nota_child();
                }
                Kind::NotaNewline => {
                    // Line boundary. Keep the `\n` as literal text, then consume any run of line-start
                    // constructs (`%`/`%%%` statements, `-`/`+`/`N.` lists, a heading) opening on the
                    // following lines, resuming markup text wherever that run ends.
                    items.push(BodyItem::Text("\n"));
                    let next_line = self.cur_token().end();
                    let resume = self.consume_line_start_constructs(next_line, depth, mode, items);
                    self.nota_seek_markup(resume);
                }
                Kind::LCurly => {
                    depth += 1;
                    items.push(BodyItem::Text("{"));
                    self.advance_for_nota_child();
                }
                Kind::RCurly if depth > 0 => {
                    depth -= 1;
                    items.push(BodyItem::Text("}"));
                    self.advance_for_nota_child();
                }
                // Body close: leave the `}` (RCurly) as the current token so the caller (`parse_body`)
                // can consume it, reporting its end (one past `}`). Document / bounded bodies treat a
                // depth-0 `}` as literal text instead.
                Kind::RCurly if matches!(mode, BodyMode::Body) => {
                    return MarkupClose::Curly { end: self.cur_token().end() };
                }
                Kind::RCurly => {
                    items.push(BodyItem::Text("}"));
                    self.advance_for_nota_child();
                }
                Kind::At => {
                    // A depth-0 `}` closes an element/control body, so a child `@`-form there has a
                    // significant brace (its colon sugar must clip before it); not so in document /
                    // bounded bodies, where `}` is literal.
                    let brace_significant = matches!(mode, BodyMode::Body);
                    let child = self.parse_nota_form(true, brace_significant);
                    items.push(BodyItem::Child(markup_to_child(child)));
                }
                Kind::Star | Kind::NotaUnderscore => {
                    // `next_nota_child` only emits these for a valid opener (the Typst word-boundary
                    // rule, now owned by the lexer), so recurse straight into the emphasis body;
                    // `parse_emphasis` still falls back to literal text when there is no matching close.
                    let m = if self.cur_kind() == Kind::Star { b'*' } else { b'_' };
                    self.parse_emphasis(m, self.cur_token().start(), items);
                }
                Kind::NotaBackslash => {
                    // General backslash escape: `\<c>` → literal `<c>`, `\` dropped.
                    let resume = self.push_escape(items, self.cur_token().start());
                    self.nota_seek_markup(resume);
                }
                Kind::NotaBacktick => {
                    // Code: inline `` `…` `` / fenced ```` ```…``` ````.
                    self.parse_code_or_literal(items, self.cur_token().start());
                }
                Kind::NotaDollar => {
                    // Math: `$…$` / `$$…$$`.
                    self.parse_math_or_literal(items, self.cur_token().start());
                }
                Kind::Pipe => {
                    // A bare `|` in a markup body is literal (the `|{`/`|@` forms are handled by the
                    // head switch / inside verbatim bodies, never here).
                    items.push(BodyItem::Text("|"));
                    self.advance_for_nota_child();
                }
                Kind::Eof => return MarkupClose::Eof,
                _ => {
                    // Any non-markup token here means lexing resumed in JS mode (after an `@`-form
                    // whose trailing context was not markup, e.g. in expression position). Re-enter
                    // markup from the current position.
                    self.advance_for_nota_child();
                }
            }
        }
    }

    /// Consume a run of line-start constructs starting at `at` (a line start): `%`/`%%%` statements
    /// (only when `mode` permits them), `-`/`+`/`N.` lists, then a trailing heading — pushing each as
    /// a child. Returns the offset to resume markup text from.
    ///
    /// `%` statements and lists each resume at a line start that may itself open another (a fence then
    /// a list, a list then a `%` line, a list then a heading), so the prefix loops until neither
    /// matches. `depth` gates lists/headings (they fire only at brace depth 0 — a balanced `{…}` is
    /// literal body text); a bounded `mode` clips recognition to within its `[.., end)`. Shared by
    /// [`Self::collect_markup`]'s `\n` arm and the document-body opener.
    fn consume_line_start_constructs(
        &mut self,
        mut at: u32,
        depth: u32,
        mode: BodyMode,
        items: &mut Vec<BodyItem<'a>>,
    ) -> u32 {
        loop {
            if mode.allows_statements() && is_statement_line(self.source_text, at) {
                at = self.collect_statements(at, items);
                continue;
            }
            if depth == 0
                && mode.bound().is_none_or(|end| at < end)
                && list_marker_at(self.source_text, at).is_some()
            {
                let (els, resume) = self.parse_list(at);
                for e in els {
                    items.push(BodyItem::Child(NotaChild::ListItem(self.ast.alloc(e))));
                }
                at = resume;
                continue;
            }
            break;
        }
        // A heading resumes at its trailing `\n` (h_end), so the caller's next `\n` iteration chains
        // into whatever follows it.
        if depth == 0
            && mode.bound().is_none_or(|end| at < end)
            && let Some((heading, h_end)) = self.try_heading(at)
        {
            items.push(BodyItem::Child(NotaChild::Heading(self.ast.alloc(heading))));
            return h_end;
        }
        at
    }

    // ===========================================================================================
    // Props
    // ===========================================================================================

    /// Parse one `[ k:v, bare, ...spread, k:@markup ]` group, pushing each into `props`.
    ///
    /// Hyperscript collapses the "string→attr vs expr→{…}" distinction into object properties:
    /// `[href:"/x"]`→`{href:"/x"}`, `[href:url]`→`{href:url}`, bare `disabled`→shorthand,
    /// `...rest`→spread, markup value `cap:@em{hi}`→`{cap: h("em",{},["hi"])}`. Multiple groups
    /// accumulate (union). Entered with the current token at `[`.
    fn parse_props_group(&mut self, props: &mut ArenaVec<'a, NotaProp<'a>>) {
        let open = self.cur_token().span();
        self.bump_any(); // consume `[`
        while !self.at(Kind::RBrack) && !self.at(Kind::Eof) && !self.has_fatal_error() {
            if self.at(Kind::Dot3) {
                // `...spread`
                let span_start = self.start_span();
                self.bump_any();
                let argument = self.parse_assignment_expression_or_higher();
                let span = self.end_span(span_start);
                let spread = self.ast.nota_spread_prop(span, argument);
                props.push(NotaProp::Spread(self.ast.alloc(spread)));
            } else {
                let prop = self.parse_prop_entry();
                props.push(prop);
            }
            if !self.eat(Kind::Comma) {
                break;
            }
        }
        self.expect_closing(Kind::RBrack, open);
    }

    /// Parse a single `key:value` or bare `key` property entry inside a `[…]` group.
    fn parse_prop_entry(&mut self) -> NotaProp<'a> {
        let span_start = self.start_span();
        let key_token = self.cur_token();
        let key_span = key_token.span();
        // Key name. (The bare-vs-quoted distinction — `["data-x": v]` — is not yet preserved in the
        // AST; fixtures use bare identifier keys. A quoted-key flag is a P5 fidelity follow-up.)
        let is_str_key = self.at(Kind::Str);
        let name: &'a str = if is_str_key {
            let s = self.cur_string();
            self.bump_any();
            s
        } else {
            let n = self.token_source(&key_token);
            self.bump_any();
            n
        };

        if self.eat(Kind::Colon) {
            // `key: value` — value may be embedded JS or markup (`@`-form).
            let value = if self.at(Kind::At) {
                let markup = self.parse_nota_form(false, false);
                NotaPropValue::Markup(self.ast.alloc(markup))
            } else {
                let expr = self.parse_assignment_expression_or_higher();
                let span = expr.span();
                let prop_expr = self.ast.nota_prop_expr(span, expr);
                NotaPropValue::Expression(self.ast.alloc(prop_expr))
            };
            let name_node = self.ast.nota_prop_name(key_span, name);
            let span = self.end_span(span_start);
            let field = self.ast.nota_field_prop(span, name_node, value);
            NotaProp::Field(self.ast.alloc(field))
        } else {
            // Bare key → shorthand `{ key }`. A string-literal key with no value is malformed.
            if is_str_key {
                let error = diagnostics::expect_token(
                    Kind::Colon.to_str(),
                    self.cur_kind().to_str(),
                    self.cur_token().span(),
                );
                return self.fatal_error(error);
            }
            let id = self.ast.identifier_reference(key_span, name);
            let span = self.end_span(span_start);
            let shorthand = self.ast.nota_shorthand_prop(span, id);
            NotaProp::Shorthand(self.ast.alloc(shorthand))
        }
    }

    // ===========================================================================================
    // Document mode + statements + colon sugar — implemented in the impl block further down.
    // ===========================================================================================

    // ===========================================================================================
    // Diagnostics
    // ===========================================================================================

    #[cold]
    fn expect_markup_body_close(&mut self, opening_span: Span) {
        let error = diagnostics::expect_closing(
            Kind::RCurly.to_str(),
            self.cur_kind().to_str(),
            self.cur_token().span(),
            opening_span,
        );
        self.set_fatal_error(error);
    }
}

// ===============================================================================================
// Control flow (`@if` / `else` / `@for`). All are expressions, so they nest in markup and embedded
// code alike. `@if` lowers to a (nested) ternary; `@for` lowers to
// `iter.map((bind, _i) => Fragment({ key: _i }, ...body))`. `else`/`else if` are contextual
// continuations (only as the next token after `}`, no blank line between).
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// `@if (cond) {branch}` with optional `else`/`else if` continuations → a (nested) ternary
    /// (`cond ? Fragment(...branch) : <alt-or-null>`). `@if` is keyless (single branch, no list
    /// reconciliation), so its branch `Fragment`s carry no `key`.
    fn parse_nota_if(&mut self, span_start: u32, in_body: bool) -> NotaIf<'a> {
        // current token: the `if` keyword. Parse the `(cond)` test (JS expression).
        assert!(self.eat(Kind::If));
        if !self.at(Kind::LParen) {
            let error = diagnostics::expect_token(
                Kind::LParen.to_str(),
                self.cur_kind().to_str(),
                self.cur_token().span(),
            );
            return self.fatal_error(error);
        }
        let cond = self.parse_paren_expression();
        // After `)` the next JS token is the branch-body `{` (whitespace skipped by the JS lexer).
        let cons = self.parse_branch_fragment(span_start);
        if self.has_fatal_error() {
            let span = self.end_span(span_start);
            return self.ast.nota_if(span, cond, cons, None);
        }
        // The branch `}` is the current token; its end is the continuation-scan origin.
        let close_end = self.cur_token().end();
        let alternate = self.parse_else_continuation(close_end, in_body);
        let span = Span::new(span_start, self.prev_token_end);
        self.ast.nota_if(span, cond, cons, alternate)
    }

    /// Parse whatever follows an `@if`/`else if` branch's `}`: an `else`/`else if` continuation, or
    /// nothing (→ `null`). Resumes the outer context (markup / JS) at the end of the whole chain.
    /// `close_end` is one byte past the just-parsed branch's `}`.
    fn parse_else_continuation(&mut self, close_end: u32, in_body: bool) -> Option<NotaElse<'a>> {
        match else_peek(self.source_text, close_end) {
            ElsePeek::None => {
                // No continuation: resume the outer context past the `}`.
                self.resume_after_control(close_end, in_body);
                None
            }
            ElsePeek::ElseIf { if_offset } => {
                // `else if (d) {…}` — re-seek to the `if` and recurse; the recursion owns the resume.
                self.nota_seek_to(if_offset);
                let span_start = self.cur_token().start();
                let nif = self.parse_nota_if(span_start, in_body);
                Some(NotaElse::ElseIf(self.ast.alloc(nif)))
            }
            ElsePeek::Else { brace_offset } => {
                // `else {b}` — re-seek to the `{` and parse the final branch, then resume.
                self.nota_seek_to(brace_offset);
                let span_start = self.cur_token().start();
                let alt = self.parse_branch_fragment(span_start);
                if self.has_fatal_error() {
                    return Some(NotaElse::Else(self.ast.alloc(alt)));
                }
                let else_end = self.cur_token().end();
                self.resume_after_control(else_end, in_body);
                Some(NotaElse::Else(self.ast.alloc(alt)))
            }
        }
    }

    /// `@for (bind of iter) {body}` → `iter.map((bind, _i) => Fragment({ key: _i }, ...body))`:
    /// the reader adds a fresh map-index param `_i` as the wrapping `Fragment`'s `key`. `bind` is
    /// any binding pattern.
    fn parse_nota_for(&mut self, span_start: u32, in_body: bool) -> NotaFor<'a> {
        // current token: the `for` keyword.
        self.bump_any(); // → `(`
        let open = self.cur_token().span();
        self.expect(Kind::LParen);
        let bind = self.parse_binding_pattern();
        if !self.at(Kind::Of) {
            // `@for` requires `of` (the comprehension form). C-style `for(;;)` has no `@`-form.
            let error = diagnostics::nota_for_expects_of(self.cur_token().span());
            return self.fatal_error(error);
        }
        self.bump_any(); // consume `of`
        let iter = self.parse_assignment_expression_or_higher();
        self.expect_closing(Kind::RParen, open);
        // Body `{ … }`: the next JS token is `{` (whitespace skipped).
        let (children, body_end) = self.parse_control_branch();
        let body = self.ast.nota_fragment(Span::new(span_start, body_end), children);
        if self.has_fatal_error() {
            let span = self.end_span(span_start);
            return self.ast.nota_for(span, bind, iter, body);
        }
        let span = Span::new(span_start, body_end);
        // Resume the outer context past the body's `}`.
        self.resume_after_control(body_end, in_body);
        self.ast.nota_for(span, bind, iter, body)
    }

    /// Parse an `@if`/`else if`/`else` branch body `{ … }` and wrap it in `Fragment(...children)`
    /// (no key — `@if` branches are not list children). `span_start` is the form's start.
    fn parse_branch_fragment(&mut self, span_start: u32) -> NotaFragment<'a> {
        let (children, end) = self.parse_control_branch();
        self.ast.nota_fragment(Span::new(span_start, end), children)
    }

    /// Parse a control-flow body `{ … }` into children + the end offset (one past `}`), leaving the
    /// `}` as the current token (so the caller can scan for a continuation / resume). Mirrors
    /// [`Self::parse_body`] but defers the post-close resume to the caller (the whole if/for chain
    /// resumes once, at its end).
    fn parse_control_branch(&mut self) -> (ArenaVec<'a, NotaChild<'a>>, u32) {
        if !self.at(Kind::LCurly) {
            // Malformed: `@if (c) <not `{`>`. Surface a clear diagnostic.
            let error = diagnostics::nota_control_expects_body(self.cur_token().span());
            self.set_fatal_error(error);
            return (self.ast.vec(), self.prev_token_end);
        }
        let open = self.cur_token().span();
        self.advance_for_nota_child(); // switch the lexer into markup-body mode

        let mut items: Vec<BodyItem<'a>> = Vec::new();
        match self.collect_markup(&mut items, BodyMode::Body) {
            // `collect_markup` lexed the close `}` as the current token; `end` is one past it.
            MarkupClose::Curly { end } => (self.body_items_to_children(items), end),
            MarkupClose::Eof => {
                self.expect_markup_body_close(open);
                (self.body_items_to_children(items), self.prev_token_end)
            }
        }
    }

    /// Resume the outer context after a control-flow form ends at `offset` (one past the final `}`):
    /// re-lex as markup text if this form is a body child, else as a normal JS token.
    fn resume_after_control(&mut self, offset: u32, in_body: bool) {
        if in_body {
            self.nota_seek_markup(offset);
        } else {
            self.nota_seek_to(offset);
        }
    }
}

/// A list marker found at a line start.
pub struct ListMarker {
    /// `true` for an ordered marker (`+` / `N.`); `false` for a bullet (`-`).
    pub ordered: bool,
    /// Indentation *depth* — the count of leading whitespace before the marker. This is what drives
    /// nesting: a marker deeper than the run's base nests inside the preceding item; one shallower
    /// ends the run. It is NOT a source offset (an earlier version conflated the two, so a dedented
    /// sibling at a *larger byte offset* than a nested marker was wrongly kept in the inner list).
    pub indent: u32,
    /// Byte offset of the marker's first char — the item's source start (for spans).
    pub offset: u32,
    /// Offset where the item body begins (just past the marker and its one separating space).
    pub body_col: u32,
}

/// The result of scanning for an `else`/`else if` contextual continuation after an `@if` branch.
pub enum ElsePeek {
    /// No continuation (the alternate is `null`).
    None,
    /// `else if (…) {…}` — resume parsing at the `if` keyword (`if_offset`).
    ElseIf { if_offset: u32 },
    /// `else {…}` — resume parsing at the body `{` (`brace_offset`).
    Else { brace_offset: u32 },
}

// ===============================================================================================
// Markup sugar (emphasis `*`/`_`, headings `#`, lists `-`/`+`/`N.`). Each lowers to an ordinary
// element; the runtime `decode`/`struct` does the grouping (the reader emits the flat
// per-line/per-span sentinels). Escaped `\* \_ \# \- \+` are the literal char.
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse an emphasis span opened by `marker` (`*`→`strong`, `_`→`em`) at raw offset `open`.
    ///
    /// Finds the matching close marker over the raw source ([`Self::find_emphasis_close`]); if one
    /// exists in this paragraph/brace scope, collects `[open+1, close)` as an (inline) markup body
    /// (nesting `@`-forms and nested emphasis) → `h(tag, {}, [...])` and resumes after `close`. With
    /// no matching close the marker is **literal** (Typst behavior) and we resume right after it.
    fn parse_emphasis(&mut self, marker: u8, open: u32, items: &mut Vec<BodyItem<'a>>) {
        if let Some(close) = find_emphasis_close(self.source_text, open, marker) {
            let mut body: Vec<BodyItem<'a>> = Vec::new();
            self.collect_markup_range(open + 1, close, &mut body);
            let children = self.body_items_to_children(body);
            let marker =
                if marker == b'*' { NotaEmphasisMarker::Strong } else { NotaEmphasisMarker::Em };
            let span = Span::new(open, close + 1);
            let element = self.ast.nota_emphasis(span, marker, children);
            items.push(BodyItem::Child(NotaChild::Emphasis(self.ast.alloc(element))));
            self.nota_seek_markup(close + 1);
        } else {
            self.push_literal_byte(items, marker);
            self.nota_seek_markup(open + 1);
        }
    }

    /// Push a single literal byte (an ASCII sigil that turned out to be non-significant) as text.
    fn push_literal_byte(&self, items: &mut Vec<BodyItem<'a>>, b: u8) {
        let s: &'a str = match b {
            b'*' => "*",
            b'_' => "_",
            b'#' => "#",
            b'-' => "-",
            b'+' => "+",
            _ => {
                // Fallback: allocate the single char.
                self.ast.allocator.alloc_str(std::str::from_utf8(&[b]).unwrap_or(""))
            }
        };
        items.push(BodyItem::Text(s));
    }

    // ------------------------------------------------------------------------------------------
    // Line constructs: headings (`#`) and lists (`-`/`+`/`N.`). Detected at a line start (the
    // `collect_markup` `\n` arm + the document/body start). Each emits a flat element; the runtime
    // `struct` coalesces list runs and owns section/paragraph grouping.
    // ------------------------------------------------------------------------------------------

    /// If the line at `line_start` opens with a heading marker (1–6 `#` then a space), parse it →
    /// `h("h{n}", {}, [rest-of-line])` and return `(element, end)` where `end` is the offset of the
    /// line's terminating `\n` (or EOF). Else `None` (the line is ordinary markup).
    fn try_heading(&mut self, line_start: u32) -> Option<(NotaHeading<'a>, u32)> {
        // The lexer detects the `#`-marker + extent; we collect the inline body and build the node.
        let (level, body_start, line_end) = heading_at(self.source_text, line_start)?;
        let mut items: Vec<BodyItem<'a>> = Vec::new();
        self.collect_markup_range(body_start, line_end, &mut items);
        let children = self.body_items_to_children(items);
        let span = Span::new(line_start, line_end);
        let element = self.ast.nota_heading(span, level, children);
        Some((element, line_end))
    }

    /// Parse a run of list items starting at `line_start` (the first line is known to be a list
    /// marker). Consecutive marker lines at the *same or deeper* indent form the run; a deeper
    /// marker nests inside the preceding item (its `struct`-coalesced inner list). Each item →
    /// `h("nota-ul-li"|"nota-ol-li", {}, [body])`. Returns `(elements, resume)` where `resume` is the offset
    /// where the run ended (a line that is neither a continuation nor a same-level marker).
    fn parse_list(&mut self, line_start: u32) -> (Vec<NotaListItem<'a>>, u32) {
        let base =
            list_marker_at(self.source_text, line_start).expect("parse_list: not a marker line");
        let base_indent = base.indent;
        let mut elements: Vec<NotaListItem<'a>> = Vec::new();
        let mut at = line_start;

        while let Some(marker) = list_marker_at(self.source_text, at) {
            if marker.indent < base_indent {
                break; // a shallower marker belongs to an enclosing list
            }
            if marker.indent > base_indent {
                // Deeper marker with no preceding same-level item to attach to (rare leading-deeper
                // case): treat as its own run at this indent.
            }
            // The item body extent: rest of the marker line + subsequent lines indented strictly
            // past the marker's *indent* (block-sugar rule). Deeper list markers within that extent
            // become nested `nota-ul-li`/`nota-ol-li` children via the recursive body collection.
            let line_end = line_content_end(self.source_text, at);
            let body_start = marker.body_col.min(line_end);
            let item_end = list_item_extent(self.source_text, line_end, marker.indent);

            let children = self.collect_list_item_body(body_start, item_end);
            let kind = if marker.ordered { NotaListKind::Ordered } else { NotaListKind::Unordered };
            let span = Span::new(marker.offset, item_end);
            elements.push(self.ast.nota_list_item(span, kind, children));

            at = item_end;
            // Skip a single trailing newline already consumed by the extent; continue if the next
            // line is another marker at >= base_indent.
            if list_marker_at(self.source_text, at).is_none() {
                break;
            }
        }
        (elements, at)
    }

    /// Collect a list item's body over `[start, end)`: the rest-of-marker-line content plus indented
    /// continuation lines, with nested list markers recursively lowered into `nota-ul-li`/`nota-ol-li`
    /// children. Shares the bounded markup collector [`Self::collect_markup_range`] with colon-sugar
    /// bodies (both want line-start list/heading sugar detected inside a `[start, end)` range).
    fn collect_list_item_body(&mut self, start: u32, end: u32) -> ArenaVec<'a, NotaChild<'a>> {
        let mut items: Vec<BodyItem<'a>> = Vec::new();
        self.collect_markup_range(start, end, &mut items);
        self.body_items_to_children(items)
    }
}

// ===============================================================================================
// Verbatim (`|{ … }|`), code (`` `…` `` / fenced), math (`$…$` / `$$…$$`), and the general
// backslash escape. Raw spans are scanned over the raw source (the line-construct pattern) and
// lowered to `String.raw` tagged-template literals so `\` and `{}` survive verbatim. Math
// `@`-interpolation becomes a `${…}` substitution in the one template; verbatim `|@` re-enters
// Nota as a *sibling* child. The raw spans are pushed as pre-lowered `BodyItem::Child`, so the
// Scribble whitespace pass (`apply_whitespace`) never touches their content.
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    // ------------------------------------------------------------------------------------------
    // General backslash escape. `\<c>` → literal `<c>` (the `\` dropped); a trailing lone `\` at
    // EOF is itself literal. Hooked from the markup collectors' byte-peek `\` arm.
    // ------------------------------------------------------------------------------------------

    /// Handle a `\` at raw offset `esc_off` (the run stopped there): push the escaped character as a
    /// literal text item (backslash dropped) and return the offset to resume markup text from. A
    /// lone trailing `\` (EOF after it) is pushed literally as `\`.
    fn push_escape(&self, items: &mut Vec<BodyItem<'a>>, esc_off: u32) -> u32 {
        let (lit, resume) = escape_extent(self.source_text, esc_off);
        items.push(BodyItem::Text(lit));
        resume
    }

    // ------------------------------------------------------------------------------------------
    // Verbatim `|{ … }|`. Raw body: ends at `}|`; the armed escape `|@` re-enters Nota to produce a
    // *sibling* element child. Lowers to `h("code", {}, [String.raw`…`, <child>, …])`.
    // ------------------------------------------------------------------------------------------

    /// Parse `@head|{ … }|` — a verbatim-body element. `head.end` points at the `|` of `|{`.
    fn parse_verbatim_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
    ) -> NotaVerbatim<'a> {
        // Body starts just past `|{`.
        let body_start = head.end + 2;
        let (parts, after) = self.collect_verbatim_body(body_start);
        let span = Span::new(span_start, after);
        let tag = self.head_to_tag(head);
        let element = self.ast.nota_verbatim(span, tag, parts);
        // Resume the outer context past the closing `}|`.
        if in_body {
            self.nota_seek_markup(after);
        } else {
            self.nota_seek_to(after);
        }
        element
    }

    /// Collect a verbatim body starting at `start` (just past `|{`): raw text runs become
    /// `String.raw` children; each `|@` re-arms one Nota `@`-form as a sibling child; the body ends
    /// at `}|`. Returns `(children, after)` where `after` is one past the closing `}|` (or EOF).
    fn collect_verbatim_body(&mut self, start: u32) -> (ArenaVec<'a, NotaVerbatimPart<'a>>, u32) {
        let bytes = self.source_text.as_bytes();
        let mut children = self.ast.vec();
        // Drop a single leading newline right after `|{` (the Scribble `{`-newline rule; otherwise
        // the body is raw — no indent strip, no trimming). `|{⏎def…` → the chunk starts at `def`.
        let mut start = start as usize;
        if bytes.get(start) == Some(&b'\n') {
            start += 1;
        }
        let mut run_start = start;
        let mut i = start;
        loop {
            if i >= bytes.len() {
                // Unterminated `|{` — surface a diagnostic; emit what we have.
                self.push_raw_run(&mut children, run_start, bytes.len());
                let span = Span::new(start as u32, bytes.len() as u32);
                self.set_fatal_error(diagnostics::nota_unterminated_verbatim(span));
                return (children, bytes.len() as u32);
            }
            // Close `}|`.
            if bytes[i] == b'}' && bytes.get(i + 1) == Some(&b'|') {
                // Drop a single trailing newline right before `}|` (the Scribble `}`-newline rule).
                let run_end = if i > run_start && bytes[i - 1] == b'\n' { i - 1 } else { i };
                self.push_raw_run(&mut children, run_start, run_end);
                return (children, i as u32 + 2);
            }
            // Armed escape `|@` — flush the raw run, then parse one `@`-form as a child.
            if bytes[i] == b'|' && bytes.get(i + 1) == Some(&b'@') {
                self.push_raw_run(&mut children, run_start, i);
                // Re-enter Nota at the `@` (one past the arming `|`); parse a single form.
                self.nota_seek_to(i as u32 + 1);
                debug_assert!(self.at(Kind::At), "verbatim `|@` not at `@`");
                let child = self.parse_nota_form(false, false);
                children.push(NotaVerbatimPart::Child(self.ast.alloc(child)));
                // `parse_nota_form` left the lexer just past the form; resume the raw scan there.
                i = self.prev_token_end as usize;
                run_start = i;
                continue;
            }
            i += 1;
        }
    }

    /// Push the raw slice `[from, to)` as a `String.raw\`…\`` child (skipped if empty).
    fn push_raw_run(
        &self,
        children: &mut ArenaVec<'a, NotaVerbatimPart<'a>>,
        from: usize,
        to: usize,
    ) {
        if to <= from {
            return;
        }
        let raw: &'a str = &self.source_text[from..to];
        let span = Span::new(from as u32, to as u32);
        let text = self.ast.nota_text(span, raw);
        children.push(NotaVerbatimPart::Raw(self.ast.alloc(text)));
    }

    // ------------------------------------------------------------------------------------------
    // Code — inline `` `…` `` and fenced ```` ```lang⏎…⏎``` ````. Fully raw, no
    // interpolation. The fence length is the opening backtick-run length; a shorter run inside is
    // literal. Inline (run on one line / 1–2 backticks) → `CodeInline`; a `≥3` run whose opener line
    // is otherwise blank (modulo a lang tag) → fenced `CodeBlock`.
    // ------------------------------------------------------------------------------------------

    /// Parse a code span at `tick_off`, or — if it has no valid close — emit the opening backtick run
    /// as literal text. Either way, re-seek markup text at the resume offset. The byte-peek arm
    /// driver for code in the markup collectors.
    fn parse_code_or_literal(&mut self, items: &mut Vec<BodyItem<'a>>, tick_off: u32) {
        // The lexer scans the (verbatim) code-span extent; we only build the AST node.
        match lex_code_span(self.source_text, tick_off) {
            CodeScan::Code { span, is_block, lang, content, resume } => {
                let language = lang.map(|l| self.ast.str(self.ast.allocator.alloc_str(l)));
                let element = self.ast.nota_code(span, language, content, is_block);
                items.push(BodyItem::Child(NotaChild::Code(self.ast.alloc(element))));
                self.nota_seek_markup(resume);
            }
            CodeScan::Literal { run, resume } => {
                // Not a valid opener: the backtick run is literal text.
                items.push(BodyItem::Text(run));
                self.nota_seek_markup(resume);
            }
        }
    }

    /// Parse a math span at `dollar_off`, or — if unterminated — emit the opening `$`-run as literal
    /// text. Re-seeks markup text at the resume offset.
    fn parse_math_or_literal(&mut self, items: &mut Vec<BodyItem<'a>>, dollar_off: u32) {
        if let Some(resume) = self.parse_math_span(items, dollar_off) {
            self.nota_seek_markup(resume);
        } else {
            let bytes = self.source_text.as_bytes();
            let mut i = dollar_off as usize;
            while i < bytes.len() && bytes[i] == b'$' {
                i += 1;
            }
            let lit: &'a str = &self.source_text[dollar_off as usize..i];
            items.push(BodyItem::Text(lit));
            self.nota_seek_markup(i as u32);
        }
    }

    // ------------------------------------------------------------------------------------------
    // Math — `$…$` (inline) and `$$…$$` (display). Raw LaTeX; `@name`/`@(expr)` interpolate
    // a *string value* as a `${…}` substitution in the one `String.raw` template; `\$`/`\@` are
    // literal but KEEP the backslash (it is LaTeX's own escape).
    // ------------------------------------------------------------------------------------------

    /// Parse a math span whose opening `$`-run starts at raw offset `dollar_off` (the run stopped
    /// there). One `$` → inline, `$$` → display. Pushes `h(Math, {display:true}?, [String.raw`…`])`
    /// into `items` and returns the resume offset, or `None` if unterminated-at-EOF without a close
    /// (then the `$` is literal — the caller emits it as text).
    fn parse_math_span(&mut self, items: &mut Vec<BodyItem<'a>>, dollar_off: u32) -> Option<u32> {
        let bytes = self.source_text.as_bytes();
        let display = bytes.get(dollar_off as usize + 1) == Some(&b'$');
        let delim_len = if display { 2 } else { 1 };
        let content_start = dollar_off as usize + delim_len;

        // Scan the raw LaTeX, splitting at `@`-interpolations, until the closing `$`/`$$`.
        let mut quasis: Vec<&'a str> = Vec::new();
        let mut exprs = self.ast.vec();
        let mut run_start = content_start;
        let mut i = content_start;
        let close = loop {
            if i >= bytes.len() {
                return None; // unterminated → the opening `$` is literal
            }
            match bytes[i] {
                b'\\' => {
                    // LaTeX escape: `\$`/`\@`/`\anything` — the backslash is KEPT (raw). Skip the
                    // escaped char so a `\$` does not close the span and a `\@` does not interpolate.
                    i += 2;
                }
                b'$' => {
                    if display {
                        if bytes.get(i + 1) == Some(&b'$') {
                            break i;
                        }
                        // A single `$` inside display math is literal LaTeX.
                        i += 1;
                    } else {
                        break i;
                    }
                }
                b'@' => {
                    // `@name`/`@(expr)` interpolation → a `${…}` substitution. Flush the raw chunk.
                    let chunk: &'a str = &self.source_text[run_start..i];
                    quasis.push(chunk);
                    let (expr, after) = self.parse_math_interp(i as u32);
                    exprs.push(expr);
                    i = after as usize;
                    run_start = i;
                }
                _ => i += 1,
            }
        };
        // Final raw chunk.
        let chunk: &'a str = &self.source_text[run_start..close];
        quasis.push(chunk);

        let after = close as u32 + delim_len as u32;
        let span = Span::new(dollar_off, after);
        // Build the alternating raw / interpolation parts (always one more raw chunk than interp).
        let mut parts = self.ast.vec_with_capacity(quasis.len() + exprs.len());
        let mut exprs_iter = exprs.into_iter();
        for chunk in quasis {
            let text = self.ast.nota_text(Span::empty(0), chunk);
            parts.push(NotaMathPart::Raw(self.ast.alloc(text)));
            if let Some(expr) = exprs_iter.next() {
                let sp = expr.span();
                let interp = self.ast.nota_interpolation(sp, expr);
                parts.push(NotaMathPart::Interpolation(self.ast.alloc(interp)));
            }
        }
        let element = self.ast.nota_math(span, display, parts);
        items.push(BodyItem::Child(NotaChild::Math(self.ast.alloc(element))));
        Some(after)
    }

    /// Parse one math `@`-interpolation at raw offset `at_off` (the `@`) → `(expr, after)` where
    /// `after` is the offset just past the interpolation. The interpolation is a *string value*
    /// (`String.raw` coerces it at runtime — the reader just splices the expr).
    ///
    /// `@(expr)` delegates to the JS parser (the parens bound it). `@name` is scanned over the **raw
    /// source** (NOT via the JS lexer) so the closing math `$` delimiter is not swallowed — `$` is a
    /// valid JS identifier-continue byte, so letting the lexer read `@i$` would eat the close.
    fn parse_math_interp(&mut self, at_off: u32) -> (Expression<'a>, u32) {
        let bytes = self.source_text.as_bytes();
        if bytes.get(at_off as usize + 1) == Some(&b'(') {
            // `@(expr)` — bounded by parens; the lexer cannot run past the `)` into the `$`.
            self.nota_seek_to(at_off);
            self.bump_any(); // `@`
            self.bump_any(); // `(`
            let expr = self.parse_expr();
            self.expect(Kind::RParen);
            let after = self.prev_token_end;
            (expr, after)
        } else {
            // `@name` — scan a JS-identifier run over the raw source, stopping at `$` (the delimiter).
            let name_start = at_off as usize + 1;
            let mut j = name_start;
            // Identifier start/continue over ASCII (the realistic case); `$` is excluded so the math
            // delimiter wins. (Unicode identifiers in math interpolation are out of scope.)
            while j < bytes.len() {
                let b = bytes[j];
                let is_part = b.is_ascii_alphanumeric() || b == b'_';
                if is_part {
                    j += 1;
                } else {
                    break;
                }
            }
            if j == name_start {
                // `@` not followed by an identifier: emit `@` literally as raw text by returning an
                // empty string expression? Simpler: treat `@` as literal — re-scan by returning a
                // string literal of "@". But to keep the template well-formed, splice `@`.
                let at_lit =
                    self.ast.expression_string_literal(Span::new(at_off, at_off + 1), "@", None);
                return (at_lit, at_off + 1);
            }
            let name: &'a str = &self.source_text[name_start..j];
            let name_span = Span::new(name_start as u32, j as u32);
            let expr = self.ast.expression_identifier(name_span, name);
            (expr, j as u32)
        }
    }
}

// ===============================================================================================
// Document mode, `%`/`%%%` statements, component hoisting, await→async, colon/block sugar.
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse the whole file body: top-level markup siblings interleaved with `%`/`%%%` statements.
    ///
    /// Statements are routed: `import`/`export` and component bindings (`%const/%let X =
    /// inlineComponent(...)|blockComponent(...)`) hoist to `module_items` (module scope, exported);
    /// other top-level `%` statements prepend into `doc_prelude` (no IIFE).
    /// `await` anywhere in a top-level statement makes `Doc` async. Returns the markup siblings.
    fn parse_document_body(&mut self) -> NotaDocument<'a> {
        let mut items: Vec<BodyItem<'a>> = Vec::new();

        // Skip a leading UTF-8 BOM (U+FEFF) so it is not collected as a text node. The BOM occupies
        // bytes 0..3, so every later byte offset is unchanged — content spans stay correct.
        let start = if self.source_text.starts_with('\u{feff}') { 3u32 } else { 0 };

        // The file may *open* with a run of line-start constructs — `%`/`%%%` statements, lists,
        // headings — none preceded by a `\n` that would trigger `collect_markup`'s line-start hooks.
        // Consume that run here with the same recognition as the `\n` arm (brace depth is 0 at the
        // document start, so lists/headings always apply), then resume markup text where it ends.
        let resume = self.consume_line_start_constructs(start, 0, BodyMode::Document, &mut items);
        self.nota_seek_markup(resume);

        // `collect_markup` (document mode) collects all markup + `%`/`%%%` statements (as faithful
        // `NotaChild::Statement` children) through EOF; routing / IIFE wrapping is the lowering pass.
        let _ = self.collect_markup(&mut items, BodyMode::Document);

        let children = self.body_items_to_children(items);
        let span = Span::new(0, self.source_text.len() as u32);
        self.ast.nota_document(span, children)
    }

    // ------------------------------------------------------------------------------------------
    // Colon / block sugar
    // ------------------------------------------------------------------------------------------

    /// `@head:` colon/block sugar → an element whose body is the rest of the line plus following
    /// lines indented past the `@head:` line (common indent stripped). Leading `|` lines of the
    /// body supply `[…]` props (accumulate). The `:` is the current head delimiter.
    ///
    /// Implementation: locate the sugar's source range (rest-of-line + indented continuation),
    /// synthesize an equivalent `{ … }` body by re-parsing that range as a markup body via the
    /// offset seam, and build the element. `|`-prop lines are parsed as `[…]` groups.
    fn parse_colon_body(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
        brace_significant: bool,
    ) -> NotaElement<'a> {
        // `commit_head` already consumed the head's boundary token and left `:` as the current
        // token; its end is the body start.
        debug_assert!(self.at(Kind::Colon), "colon sugar entered not at `:`");
        let colon_end = self.cur_token().end();
        let head_line_indent = line_indent_of(self.source_text, span_start);

        // Determine the sugar's source extent: rest of the `@head:` line + lines indented strictly
        // past `head_line_indent`.
        let (body_src_start, body_src_end) =
            colon_block_extent(self.source_text, colon_end, head_line_indent, brace_significant);

        // Collect props from leading `|` lines, and the markup body (text + `@`-forms).
        let mut props = self.ast.vec();
        let mut items: Vec<BodyItem<'a>> = Vec::new();
        self.collect_colon_body(body_src_start, body_src_end, &mut props, &mut items);

        let children = self.body_items_to_children(items);
        let span = Span::new(span_start, body_src_end);

        // Resume the outer context after the consumed block.
        if in_body {
            self.nota_seek_markup(body_src_end);
        } else {
            self.nota_seek_to(body_src_end);
        }
        let tag = self.head_to_tag(head);
        self.ast.nota_element(span, tag, props, children, /* is_colon */ true)
    }

    /// Collect the colon-sugar body over `[start, end)`: leading `|` lines → `[…]` prop groups; the
    /// remaining lines → markup body items.
    fn collect_colon_body(
        &mut self,
        start: u32,
        end: u32,
        props: &mut ArenaVec<'a, NotaProp<'a>>,
        items: &mut Vec<BodyItem<'a>>,
    ) {
        // Walk leading `|` prop lines (a line whose first non-ws char is `|`, indented past head).
        let mut body_start = start;
        // Props lines only apply to the *continuation* lines (not the rest-of-`@head:`-line).
        // Find the first continuation line.
        let first_cont = next_line_start(self.source_text, start);
        let mut scan = first_cont;
        while scan < end {
            let Some(content_start) = colon_prop_line_at(self.source_text, scan) else {
                break;
            };
            // A `| k: v` prop line. Parse `[k: v]`-style entries from after `|` to line end.
            let line_end = next_line_start(self.source_text, scan);
            self.parse_pipe_prop_line(content_start, line_end, props);
            scan = line_end;
            body_start = scan; // props consume the prefix; body starts after them
        }
        // If `|` lines were consumed, the rest-of-line content of `@head:` is dropped (kept simple);
        // the body is the remaining suffix. Back up to include the `\n` that precedes that suffix, so
        // the whitespace pass sees the body as "opened with a newline" and treats its first line as
        // an *indent* line (stripping the common indent) rather than as the inline `{`-line — without
        // this, `@foo:⏎  | x:1⏎  hello` leaks the leading indent as `"  hello"` instead of `"hello"`.
        let body_range_start = if body_start > start {
            if byte_at(self.source_text, body_start - 1) == Some(b'\n') {
                body_start - 1
            } else {
                body_start
            }
        } else {
            start
        };
        self.collect_markup_range(body_range_start, end, items);
    }

    /// Parse `| k: v, …` prop entries from `[content_start, line_end)` into `props`.
    fn parse_pipe_prop_line(
        &mut self,
        content_start: u32,
        line_end: u32,
        props: &mut ArenaVec<'a, NotaProp<'a>>,
    ) {
        self.nota_seek_to(content_start);
        while self.cur_token().start() < line_end && !self.at(Kind::Eof) && !self.has_fatal_error()
        {
            if self.at(Kind::Dot3) {
                let span_start = self.start_span();
                self.bump_any();
                let argument = self.parse_assignment_expression_or_higher();
                let span = self.end_span(span_start);
                let spread = self.ast.nota_spread_prop(span, argument);
                props.push(NotaProp::Spread(self.ast.alloc(spread)));
            } else {
                let prop = self.parse_prop_entry();
                props.push(prop);
            }
            if !self.eat(Kind::Comma) {
                break;
            }
        }
    }

    /// Collect markup over a bounded raw source range `[start, end)` — shared by emphasis bodies
    /// ([`Self::parse_emphasis`]), colon-sugar bodies ([`Self::collect_colon_body`]), list-item bodies
    /// ([`Self::collect_list_item_body`]), and heading bodies ([`Self::try_heading`]). Seeks the lexer
    /// to `start`, then delegates to [`Self::collect_markup`] in [`BodyMode::Bounded`]: a `}` is always
    /// literal (it never closes the range), there are no `%`/`%%%` statement lines, and line-start
    /// list/heading sugar is still recognized (at brace depth 0). Leading common indentation is left to
    /// the Scribble whitespace pass.
    fn collect_markup_range(&mut self, start: u32, end: u32, items: &mut Vec<BodyItem<'a>>) {
        self.nota_seek_markup(start);
        let _ = self.collect_markup(items, BodyMode::Bounded { end });
    }
}

/// A parsed `@`-form head, with the classification needed to branch element-vs-interpolation and
/// host-vs-component-vs-dynamic.
struct NotaHead<'a> {
    kind: HeadKind<'a>,
    /// Byte offset immediately after the head (the element/interpolation switch position).
    end: u32,
}

/// The element trigger immediately following an `@`-form head — the typed result of the
/// whitespace-sensitive head→body switch (see [`ParserImpl::peek_markup_trigger`]).
#[derive(Clone, Copy)]
pub enum MarkupTrigger {
    /// `@head{…}` — a `{`-body element.
    Brace,
    /// `@head[…]` — a `[props]` element (body optional).
    Bracket,
    /// `@head:…` — colon / block sugar.
    Colon,
    /// `@head|{…}|` — a verbatim body.
    Verbatim,
    /// No trigger glued to the head ⇒ interpolation (`@name` / `@(expr)`).
    None,
}

enum HeadKind<'a> {
    /// A bare identifier head: host (lowercase string tag) or component (Capitalized identifier).
    Named { name: &'a str, span: Span },
    /// `@(expr)` — a dynamic head; the inner expression.
    Dynamic(Expression<'a>),
}

/// A tag name is a *component* (identifier) iff it starts with an uppercase ASCII letter;
/// otherwise it is a *host* element (string tag).
fn is_component_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}
