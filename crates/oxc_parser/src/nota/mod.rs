//! Nota `@`-markup → a faithful Nota AST (the *reader*).
//!
//! This module parses `@`-markup into the Nota AST nodes ([`NotaMarkup`] & friends, in
//! `oxc_ast::ast::nota`): each `@`-form stays in place as `Expression::NotaMarkup`, and a whole
//! `.nota` file becomes a single `NotaMarkupKind::Document` statement. Lowering to hyperscript
//! (`h`/`Fragment`/`decode`), the Scribble whitespace pass, `%`-statement routing, and F1 component
//! hoisting all run *later*, in `oxc_transformer::NotaLowering` — the deferred-pass analog of how
//! oxc lowers JSX.
//!
//! Mechanics: the parser drives the lexer between JS mode and markup mode. Inside a body it pulls
//! typed markup child tokens (`advance_for_nota_child`, the JSX `advance_for_jsx_child` analog) and
//! dispatches on [`Kind`]; multi-byte constructs (raw spans, emphasis closes, line blocks) are
//! measured by the pure scans in [`crate::lexer::nota`] over the raw source, then the lexer is
//! re-seeked (`nota_seek_markup`/`nota_seek_to`) past the consumed extent. Embedded JS delegates to
//! oxc's own expression/statement parser, so its nodes carry real source spans.

// Source offsets and substring lengths are cast to `u32` throughout: oxc's `Span` is `u32`-based
// (sources are bounded to 4 GiB), so these `as u32` casts cannot truncate in practice.
#![expect(
    clippy::cast_possible_truncation,
    reason = "source offsets/lengths fit in u32 (oxc's Span model)"
)]

pub mod highlight;

use oxc_allocator::Vec as ArenaVec;
use oxc_ast::ast::*;
use oxc_diagnostics::OxcDiagnostic;
use oxc_span::{GetSpan, SourceType, Span};

use crate::{
    ParserConfig as Config, ParserImpl, diagnostics,
    error_handler::FatalError,
    lexer::Kind,
    lexer::nota::{
        CodeScan, ElsePeek, MarkupTrigger, MathBoundary, VerbatimBoundary, brace_clip_on_line,
        byte_at, colon_block_extent, colon_prop_line_at, else_peek, escape_span,
        find_emphasis_close, find_fence_close, heading_at, is_ident_start_at, is_statement_line,
        lex_code_span, line_content_end, line_indent_of, list_item_extent, list_marker_at,
        markup_trigger, math_boundary, next_line_start, percent_line_is_empty, scan_hyphen_tail,
        sigil_run_end, statement_bound, statement_kind, verbatim_boundary,
    },
};

/// How a markup-collection loop ([`ParserImpl::collect_markup`]) terminated.
enum MarkupClose {
    /// Closed by the body's `}` (depth 0); `end` is one byte past it.
    Curly { end: u32 },
    /// Reached end of file (the document body, a bounded range, or an unterminated body).
    Eof,
}

/// Selects the behaviours that differ between [`ParserImpl::collect_markup`]'s callers.
#[derive(Clone, Copy)]
enum BodyMode {
    /// An element / control-flow `{ … }` body: a depth-0 `}` closes it; `%`/`%%%` lines are
    /// statements.
    Body,
    /// The whole-file body: a depth-0 `}` is literal; `%`/`%%%` lines are statements; runs to EOF.
    Document,
    /// A bounded sub-range `[.., end)` (emphasis / colon-sugar / list-item / heading body): a `}`
    /// is always literal and there are no `%`/`%%%` statement lines.
    Bounded { end: u32 },
}

impl BodyMode {
    /// The exclusive end offset of a [`BodyMode::Bounded`] range.
    fn bound(self) -> Option<u32> {
        match self {
            BodyMode::Bounded { end } => Some(end),
            BodyMode::Body | BodyMode::Document => None,
        }
    }

    /// Do line-start `%`/`%%%` statements fire in this mode?
    fn allows_statements(self) -> bool {
        !matches!(self, BodyMode::Bounded { .. })
    }
}

impl<'a, C: Config> ParserImpl<'a, C> {
    // ===========================================================================================
    // Entry points
    // ===========================================================================================

    /// Parse a whole source string as a single Nota *expression* (mirrors
    /// [`ParserImpl::parse_expression`]). Document mode is [`Self::parse_nota_document`].
    ///
    /// # Errors
    /// If the source is not a well-formed Nota expression.
    pub(crate) fn parse_nota_expression(mut self) -> Result<Expression<'a>, Vec<OxcDiagnostic>> {
        self.nota_markup = true;
        self.bump_any(); // prime `token` onto the first token
        let expr = self.parse_nota_markup_expression(false, false);
        self.finish_nota(expr)
    }

    /// Parse a whole `.nota` file in *document mode* → a [`Program`] holding the un-lowered
    /// document as a single `Expression::NotaMarkup(Document)` statement.
    ///
    /// # Errors
    /// If the file is not well-formed Nota.
    pub(crate) fn parse_nota_document(mut self) -> Result<Program<'a>, Vec<OxcDiagnostic>> {
        self.nota_markup = true;
        // No JS `bump_any` priming: the file starts as markup (or a `%` line), and a leading
        // `\`/`%`/etc. would choke the JS lexer. `parse_document_body` seeks from offset 0 itself.
        let document = self.parse_document_body();
        let program = self.wrap_document_program(document);
        match self.finish_nota(()) {
            Ok(()) => Ok(program),
            Err(errors) => Err(errors),
        }
    }

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
    // `@`-form dispatch (element / fragment / interpolation / control flow / verbatim)
    // ===========================================================================================

    /// Parse one `@`-form in JS *expression position*, wrapped as `Expression::NotaMarkup`. The
    /// umbrella [`NotaMarkup`] span covers the whole form from its `@` (an inner node's span may
    /// exclude it — an interpolation's span is its expression's).
    pub(crate) fn parse_nota_markup_expression(
        &mut self,
        in_body: bool,
        brace_significant: bool,
    ) -> Expression<'a> {
        let span_start = self.start_span();
        let form = self.parse_nota_form(in_body, brace_significant);
        let span = self.end_span(span_start);
        let markup = self.ast.nota_markup(span, NotaMarkupKind::from(form));
        Expression::NotaMarkup(self.ast.alloc(markup))
    }

    /// Parse one `@`-form. Entered with the current token at [`Kind::At`]. The returned
    /// [`NotaForm`] converts into any form-holding position (`NotaChild`, `NotaPropValue`,
    /// `NotaVerbatimPart`, `NotaMarkupKind`) via the inherited-variant `From` impls.
    ///
    /// `in_body`: this form is a child of a markup body, so its trailing context resumes as markup
    /// text (the JSX `in_jsx_child` analog); `false` in JS expression position.
    /// `brace_significant`: a depth-0 `}` here closes an *enclosing* `{…}` body, so `@head:` colon
    /// sugar must clip its body before it (`@p{@a: b}`); `false` where `}` is literal text.
    pub(crate) fn parse_nota_form(
        &mut self,
        in_body: bool,
        brace_significant: bool,
    ) -> NotaForm<'a> {
        let span_start = self.start_span();

        // Consume `@` and lex the head. An identifier-start head (`@foo`, `@if`, `@café`) is lexed
        // with Nota identifier rules (`next_nota_head`): a `\` *terminates* the head — so `@foo\:`
        // is `@foo` + the literal `\:` — instead of starting a JS `\u` escape. Keyword heads still
        // produce `Kind::If`/`Kind::For`. Non-identifier heads (`@(expr)`, `@{…}`) stay JS-lexed.
        debug_assert!(self.at(Kind::At), "parse_nota_form entered not at `@`");
        let after_at = self.cur_token().end();
        if is_ident_start_at(self.source_text, after_at) {
            self.nota_seek_head(after_at);
        } else {
            self.bump_any();
        }

        match self.cur_kind() {
            Kind::If => NotaForm::If({
                let n = self.parse_nota_if(span_start, in_body);
                self.ast.alloc(n)
            }),
            Kind::For => NotaForm::For({
                let n = self.parse_nota_for(span_start, in_body);
                self.ast.alloc(n)
            }),
            Kind::LCurly => NotaForm::Fragment({
                let f = self.parse_fragment(span_start, in_body);
                self.ast.alloc(f)
            }),
            _ => {
                let Some(head) = self.parse_nota_head() else {
                    // `@` with no valid head (`@@`, `@ `, `@1`, EOF, …): diagnose, but recover as
                    // an empty fragment so the form stays well-shaped in any position.
                    self.set_unexpected();
                    let span = self.end_span(span_start);
                    let frag = self.ast.nota_fragment(span, self.ast.vec());
                    return NotaForm::Fragment(self.ast.alloc(frag));
                };
                match self.commit_head(&head, in_body) {
                    MarkupTrigger::Brace | MarkupTrigger::Bracket => NotaForm::Element({
                        let e = self.parse_element(span_start, head, in_body);
                        self.ast.alloc(e)
                    }),
                    MarkupTrigger::Colon => NotaForm::Element({
                        let e = self.parse_colon_body(span_start, head, in_body, brace_significant);
                        self.ast.alloc(e)
                    }),
                    MarkupTrigger::Verbatim => NotaForm::Verbatim({
                        let v = self.parse_verbatim_element(span_start, head, in_body);
                        self.ast.alloc(v)
                    }),
                    // No trigger glued to the head ⇒ interpolation.
                    MarkupTrigger::None => NotaForm::Interpolation({
                        let i = self.finish_interpolation(head);
                        self.ast.alloc(i)
                    }),
                }
            }
        }
    }

    /// Parse an `@`-form head: `@(expr)` or a bare (possibly hyphenated) identifier. The head's
    /// boundary token (the identifier, or the `)`) is validated but NOT consumed — it stays as the
    /// one-token lookahead so [`Self::commit_head`] can classify the trigger glued to it and then
    /// consume it in the right lexer mode. Returns `None` for an invalid head.
    fn parse_nota_head(&mut self) -> Option<NotaHead<'a>> {
        if self.eat(Kind::LParen) {
            let expr = self.parse_expr();
            self.expect_without_advance(Kind::RParen);
            let close_end = self.cur_token().end();
            Some(NotaHead { kind: HeadKind::Dynamic(expr), end: close_end })
        } else if self.cur_kind().is_identifier_name() {
            // `is_identifier_name` also admits keyword-spelled tags (`@section`, `@for`-as-name).
            let token = self.cur_token();
            let name = self.token_source(&token);
            let span = token.span();
            // Hyphenated host tag (`@my-widget`): a lowercase head may continue over `-`-joined
            // segments, but only when an element trigger follows the full name — otherwise the `-`
            // is literal text after an interpolation (`@my-foo bar` is `@my` + `-foo bar`).
            if !is_component_name(name)
                && let Some(ext_end) = scan_hyphen_tail(self.source_text, span.end)
                && !matches!(markup_trigger(self.source_text, ext_end), MarkupTrigger::None)
            {
                let full = &self.source_text[span.start as usize..ext_end as usize];
                let span = Span::new(span.start, ext_end);
                return Some(NotaHead { kind: HeadKind::Named { name: full, span }, end: ext_end });
            }
            Some(NotaHead { kind: HeadKind::Named { name, span }, end: span.end })
        } else {
            None
        }
    }

    /// Classify the trigger glued to the head, then consume the head's boundary token in the lexer
    /// mode that trigger implies. This is the one place the boundary token is consumed, uniform
    /// across named and dynamic heads. (Seeks, not bumps: an *extended* hyphenated head runs past
    /// the lexer's current boundary token, and a seek from `head.end` covers both cases.)
    fn commit_head(&mut self, head: &NotaHead<'a>, in_body: bool) -> MarkupTrigger {
        let trigger = markup_trigger(self.source_text, head.end);
        match trigger {
            // Lex the trigger (`{` / `[` / `:`) as the next JS token.
            MarkupTrigger::Brace | MarkupTrigger::Bracket | MarkupTrigger::Colon => {
                self.nota_seek_to(head.end);
            }
            // The verbatim body is scanned by absolute offset from `head.end`; the boundary token
            // stays current (the JS lexer must not eat the `|`).
            MarkupTrigger::Verbatim => {}
            // No trigger ⇒ interpolation: resume the surrounding context past the head.
            MarkupTrigger::None => self.resume_at(head.end, in_body),
        }
        trigger
    }

    /// Finish an `@`-form that turned out to be an interpolation: `@name` → `name`;
    /// `@(expr)` → `expr`. The boundary token was already consumed by [`Self::commit_head`].
    fn finish_interpolation(&self, head: NotaHead<'a>) -> NotaInterpolation<'a> {
        let expr = match head.kind {
            HeadKind::Named { name, span } => self.ast.expression_identifier(span, name),
            HeadKind::Dynamic(expr) => expr,
        };
        let span = expr.span();
        self.ast.nota_interpolation(span, expr)
    }

    /// Build the [`NotaTag`] for a parsed head: lowercase → host string, Capitalized → component
    /// identifier, `@(expr)` → dynamic.
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

    /// Parse `@head [props]* { body }?`. Entered with the `{`/`[` delimiter as the current token.
    fn parse_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
    ) -> NotaElement<'a> {
        let mut props = self.ast.vec();
        while self.at(Kind::LBrack) {
            self.parse_props_group(&mut props);
        }

        let (children, end) = if self.at(Kind::LCurly) {
            let (children, end) = self.parse_braced_body();
            self.resume_at(end, in_body);
            (children, end)
        } else {
            // Self-closing. The current JS token already ran past the `]`; re-lex the trailing
            // markup text from right after it so no body text is dropped.
            let end = self.prev_token_end;
            if in_body {
                self.resume_at(end, true);
            }
            (self.ast.vec(), end)
        };

        let span = Span::new(span_start, end);
        let tag = self.head_to_tag(head);
        self.ast.nota_element(span, tag, props, children, /* is_colon */ false)
    }

    /// `@{ body }` → a fragment node. `@` already consumed.
    fn parse_fragment(&mut self, span_start: u32, in_body: bool) -> NotaFragment<'a> {
        let (children, end) = self.parse_braced_body();
        self.resume_at(end, in_body);
        let span = Span::new(span_start, end);
        self.ast.nota_fragment(span, children)
    }

    // ===========================================================================================
    // Body collection
    // ===========================================================================================

    /// Parse a `{ … }` markup body → `(children, end)`, `end` one past the closing `}`. The `}`
    /// is left as the current token — element/fragment callers [`Self::resume_at`] `end`
    /// immediately; control-flow callers first scan past it for an `else` continuation. Entered
    /// at the body `{`; a missing `{` is fatal (only reachable from control flow — element
    /// dispatch guarantees the brace).
    fn parse_braced_body(&mut self) -> (ArenaVec<'a, NotaChild<'a>>, u32) {
        if !self.at(Kind::LCurly) {
            let error = diagnostics::nota_control_expects_body(self.cur_token().span());
            self.set_fatal_error(error);
            return (self.ast.vec(), self.prev_token_end);
        }
        let open = self.cur_token().span();
        self.advance_for_nota_child(); // switch the lexer into markup-body mode

        let mut items = self.ast.vec();
        match self.collect_markup(&mut items, BodyMode::Body) {
            MarkupClose::Curly { end } => (items, end),
            MarkupClose::Eof => {
                self.expect_markup_body_close(open);
                (items, self.prev_token_end)
            }
        }
    }

    /// Resume lexing at raw offset `offset` in the mode the surrounding context implies: markup
    /// text when the just-parsed form is a markup-body child, a normal JS token otherwise. The
    /// single mode-switch primitive every construct exits through. No-op after a fatal error
    /// (the collectors bail out; nothing left to lex).
    fn resume_at(&mut self, offset: u32, in_body: bool) {
        if self.has_fatal_error() {
            return;
        }
        if in_body {
            self.nota_seek_markup(offset);
        } else {
            self.nota_seek_to(offset);
        }
    }

    /// Push the source slice `[start, end)` as a literal text child (skipped if empty). Every text
    /// child — including single-byte sigils that turned out literal — is a real source slice, so
    /// [`NotaText`] spans are always true source positions.
    fn push_text(&self, items: &mut ArenaVec<'a, NotaChild<'a>>, start: u32, end: u32) {
        if end <= start {
            return;
        }
        let slice = &self.source_text[start as usize..end as usize];
        let text = self.ast.nota_text(Span::new(start, end), slice);
        items.push(NotaChild::Text(self.ast.alloc(text)));
    }

    /// The core markup-collection loop, shared by element/control bodies, the document body, and
    /// bounded sub-ranges. Dispatches on the typed child tokens from `next_nota_child`; balanced
    /// `{…}` braces are literal text (Scribble `@foo{f{o}o}` → `"f{o}o"`); `\n` runs stay in the
    /// text stream verbatim (the Scribble whitespace pass owns line handling at lowering time).
    /// Entered with the current token already lexed as a markup child.
    fn collect_markup(
        &mut self,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
        mode: BodyMode,
    ) -> MarkupClose {
        let mut depth = 0u32; // balanced-brace depth inside the body
        // R9: the start of a body/range is a line start. A body opening directly with a marker —
        // `@{- item}`, `@foo: - item`, `*- item*`, the document's first line — opens the construct
        // exactly as it would after a `\n`. (Literal braces in prose never re-enter here, so a
        // `{- x}` inside a paragraph stays text.)
        if !self.has_fatal_error() {
            let at = self.cur_token().start();
            let resume = self.consume_line_start_constructs(at, 0, mode, items);
            if resume != at {
                self.nota_seek_markup(resume);
            }
        }
        loop {
            if self.has_fatal_error() {
                return MarkupClose::Eof;
            }
            // A bounded range stops once the cursor reaches `end`: every sigil is its own token,
            // so this one check bounds the whole range.
            if let BodyMode::Bounded { end } = mode
                && self.cur_token().start() >= end
            {
                return MarkupClose::Eof;
            }
            match self.cur_kind() {
                Kind::MarkupText => {
                    let token = self.cur_token();
                    let (end, reached_bound) = match mode {
                        // Clip the run to a bounded range's end.
                        BodyMode::Bounded { end } => (token.end().min(end), token.end() >= end),
                        BodyMode::Body | BodyMode::Document => (token.end(), false),
                    };
                    self.push_text(items, token.start(), end);
                    if reached_bound {
                        return MarkupClose::Eof;
                    }
                    self.advance_for_nota_child();
                }
                Kind::NotaNewline => {
                    // Line boundary: keep the `\n` as text, then consume any run of line-start
                    // constructs (`%`/`%%%` statements, lists, a heading) on the following lines.
                    let nl = self.cur_token().start();
                    self.push_text(items, nl, nl + 1);
                    let resume = self.consume_line_start_constructs(nl + 1, depth, mode, items);
                    self.nota_seek_markup(resume);
                }
                Kind::LCurly => {
                    depth += 1;
                    let s = self.cur_token().start();
                    self.push_text(items, s, s + 1);
                    self.advance_for_nota_child();
                }
                Kind::RCurly if depth > 0 => {
                    depth -= 1;
                    let s = self.cur_token().start();
                    self.push_text(items, s, s + 1);
                    self.advance_for_nota_child();
                }
                // Body close: leave the `}` current so `parse_body` can consume it in the right
                // mode. Document / bounded bodies treat a depth-0 `}` as literal text instead.
                Kind::RCurly if matches!(mode, BodyMode::Body) => {
                    return MarkupClose::Curly { end: self.cur_token().end() };
                }
                Kind::RCurly => {
                    let s = self.cur_token().start();
                    self.push_text(items, s, s + 1);
                    self.advance_for_nota_child();
                }
                Kind::At => {
                    // A depth-0 `}` closes an element/control body, so a child form's colon sugar
                    // must clip before it; not so where `}` is literal.
                    let brace_significant = matches!(mode, BodyMode::Body);
                    let form = self.parse_nota_form(true, brace_significant);
                    items.push(NotaChild::from(form));
                    // A colon-sugar body consumes through its final line's `\n` (and any trailing
                    // blank lines), so the form can resume AT a line start — a position the `\n`
                    // arm's line-start hook never sees. Run the same hook here: a heading, list,
                    // or `%` statement directly after a colon block is sugar, not literal text.
                    let at = self.cur_token().start();
                    if !self.has_fatal_error()
                        && at > 0
                        && byte_at(self.source_text, at - 1) == Some(b'\n')
                    {
                        let resume = self.consume_line_start_constructs(at, depth, mode, items);
                        self.nota_seek_markup(resume);
                    }
                }
                Kind::Star | Kind::NotaUnderscore => {
                    // The lexer emits these only for a valid opener (the Typst word-boundary
                    // rule); close-matching still decides marker-vs-literal.
                    let m = if self.cur_kind() == Kind::Star { b'*' } else { b'_' };
                    self.parse_emphasis(m, self.cur_token().start(), items);
                }
                Kind::NotaBackslash => {
                    // General escape: `\<c>` → literal `<c>`, the `\` dropped.
                    let span = escape_span(self.source_text, self.cur_token().start());
                    self.push_text(items, span.start, span.end);
                    self.nota_seek_markup(span.end);
                }
                Kind::NotaBacktick => {
                    self.parse_code_or_literal(items, self.cur_token().start());
                }
                Kind::NotaDollar => {
                    self.parse_math_or_literal(items, self.cur_token().start());
                }
                Kind::Pipe => {
                    // A bare `|` in a markup body is literal (`|{`/`|@` are handled at the head
                    // switch / inside verbatim bodies, never here).
                    let s = self.cur_token().start();
                    self.push_text(items, s, s + 1);
                    self.advance_for_nota_child();
                }
                Kind::Eof => return MarkupClose::Eof,
                _ => {
                    // Defensive: lexing resumed in JS mode (shouldn't happen mid-body). Re-enter
                    // markup from the current position.
                    self.advance_for_nota_child();
                }
            }
        }
    }

    /// Consume a run of line-start constructs starting at `at` (a line start): `%`/`%%%`
    /// statements (when `mode` permits), list runs, then a trailing heading — pushing each as a
    /// child. Statements and lists each resume at a line start that may itself open another, so
    /// the loop chains. `depth` gates lists/headings (they fire only at brace depth 0). Returns
    /// the offset to resume markup text from; a heading resumes at its trailing `\n` so the
    /// caller's next `\n` iteration chains into whatever follows.
    fn consume_line_start_constructs(
        &mut self,
        mut at: u32,
        depth: u32,
        mode: BodyMode,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
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
                at = self.parse_list(at, mode, items);
                continue;
            }
            break;
        }
        if depth == 0
            && mode.bound().is_none_or(|end| at < end)
            && let Some((heading, h_end)) = self.try_heading(at, mode)
        {
            items.push(NotaChild::Heading(self.ast.alloc(heading)));
            return h_end;
        }
        at
    }

    /// The first-line extent limit for line-start sugar at `at` (a list marker or heading line):
    /// the line's content end, clipped to the enclosing braced body's depth-0 `}` (the construct
    /// must not eat the closer — `@{- item}`) and to a bounded range's end (`*- item*`).
    fn sugar_line_end(&self, at: u32, mode: BodyMode) -> u32 {
        let mut end = line_content_end(self.source_text, at);
        if matches!(mode, BodyMode::Body)
            && let Some(clip) = brace_clip_on_line(self.source_text, at)
        {
            end = end.min(clip);
        }
        if let Some(bound) = mode.bound() {
            end = end.min(bound);
        }
        end
    }

    /// Collect markup over a bounded raw source range `[start, end)` — emphasis, colon-sugar,
    /// list-item, and heading bodies. A `}` is always literal, there are no statement lines, and
    /// line-start list/heading sugar is still recognized.
    fn collect_markup_range(
        &mut self,
        start: u32,
        end: u32,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
    ) {
        self.nota_seek_markup(start);
        let _ = self.collect_markup(items, BodyMode::Bounded { end });
    }

    // ===========================================================================================
    // `%` / `%%%` statements
    // ===========================================================================================

    /// Run `f` with the lexer's source end temporarily clamped to `bound` (so it lexes `Eof`
    /// there), restoring the prior end afterwards. Bounds a `%`/`%%%` statement's JS parse to its
    /// extent.
    fn with_source_end_bound<R>(&mut self, bound: u32, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.lexer.nota_source_end();
        self.lexer.nota_set_source_end(bound);
        let result = f(self);
        self.lexer.nota_set_source_end(saved);
        result
    }

    /// Parse a run of consecutive `%`/`%%%` statement lines from `line_start`, pushing each parsed
    /// statement as a `NotaChild::Statement` (routing/IIFE-wrapping is the lowering's job).
    /// Returns the offset of the first non-statement line.
    fn collect_statements(
        &mut self,
        line_start: u32,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
    ) -> u32 {
        let mut at = line_start;
        while let Some((content, is_fence)) = statement_kind(self.source_text, at) {
            let end = if is_fence {
                self.collect_fence_statements(content, items)
            } else if percent_line_is_empty(self.source_text, content) {
                // An empty / comment-only `%` line is a no-op; it must not swallow the following
                // markup as a statement.
                content
            } else {
                self.collect_percent_statements(content, items)
            };
            // A fence resumes at the line after its closing `%%%`; a `%` statement's `end` is
            // mid-line, so advance to the next line.
            at = if is_fence { end } else { next_line_start(self.source_text, end) };
            if !is_statement_line(self.source_text, at) {
                break;
            }
        }
        at
    }

    /// Parse one `%` statement region whose JS begins at `content`: **the rest of the line is
    /// JS** — arbitrary statements under JS's own rules (`;` and ASI; a statement continues across
    /// single newlines exactly where JS grammar allows), transitioning back to markup at a clear
    /// boundary: end of line once a statement completes there, a blank line (the lexer is clamped
    /// at [`statement_bound`], so ASI applies as at end of input), or the next line-leading `%`.
    /// Returns the offset just past the last statement.
    fn collect_percent_statements(
        &mut self,
        content: u32,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
    ) -> u32 {
        let bound = statement_bound(self.source_text, content);
        debug_assert!(self.source_text.is_char_boundary(bound as usize));
        let lexer_errors_before = self.lexer.errors.len();
        let parser_errors_before = self.errors.len();
        self.with_source_end_bound(bound, |p| {
            p.nota_seek_to(content);
            loop {
                let stmt =
                    p.parse_statement_list_item(crate::context::StatementContext::StatementList);
                p.push_statement(items, stmt);
                if p.has_fatal_error() || p.errors.len() > parser_errors_before || p.at(Kind::Eof) {
                    break;
                }
                // End-of-line transition: another statement follows only on the SAME line as the
                // previous one's end (`% a(); b();`); the next line is markup again.
                let gap = &p.source_text[p.prev_token_end as usize..p.cur_token().start() as usize];
                if gap.contains('\n') {
                    break;
                }
            }
        });
        let end = self.prev_token_end;
        // The trailing one-token lookahead may have JS-lexed bytes that belong to the following
        // markup (a heading's `#·` is not lexable JS). When the statement run itself is clean,
        // bytes from the NEXT line onward are about to be re-lexed as markup, so a lexer
        // diagnostic wholly in that region is a stale artifact — drop it. Same-line diagnostics
        // stay: the rest of the statement's line is JS, so garbage there is a real error.
        // (`self.fatal_error`, not `has_fatal_error()` — the latter also fires on a benign
        // clamped `Eof` / markup-lookahead `Undetermined` current token.)
        let markup_resume = next_line_start(self.source_text, end);
        if self.fatal_error.is_none() && self.errors.len() == parser_errors_before {
            let mut i = lexer_errors_before;
            while i < self.lexer.errors.len() {
                let in_markup = self.lexer.errors[i].labels.as_ref().is_some_and(|labels| {
                    labels.iter().all(|l| l.offset() as u32 >= markup_resume)
                });
                if in_markup {
                    let _stale = self.lexer.errors.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        // If the parse failed and the region was clipped at a blank line, that clip is the likely
        // cause — attach the pointer (onto the fatal error itself when there is one; `finish_nota`
        // surfaces only the fatal).
        let failed = self.fatal_error.is_some()
            || self.errors.len() > parser_errors_before
            || self.lexer.errors.len() > lexer_errors_before;
        if failed
            && bound < self.source_text.len() as u32
            && !is_statement_line(self.source_text, bound)
        {
            if let Some(fatal) = self.fatal_error.as_mut() {
                fatal.error = fatal.error.clone().with_note(
                    "a blank line ends a `%` statement — remove the blank line, or move the code into a `%%% … %%%` fence",
                );
            } else {
                self.error(diagnostics::nota_statement_ends_at_blank_line(Span::new(
                    bound,
                    bound + 1,
                )));
            }
        }
        end
    }

    /// Parse the inner statements of a `%%%`…`%%%` fence (from `inner_start`). Returns the offset
    /// past the closing fence.
    fn collect_fence_statements(
        &mut self,
        inner_start: u32,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
    ) -> u32 {
        let (inner_end, after_fence) = find_fence_close(self.source_text, inner_start);
        debug_assert!(self.source_text.is_char_boundary(inner_end as usize));
        // Bound the fence body's parse to `[inner_start, inner_end)` so the closing `%%%` is never
        // read as JS (`x⏎%%%` would mis-lex as `x % % %`); a bare-expression body gets clean ASI.
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

    fn push_statement(&self, items: &mut ArenaVec<'a, NotaChild<'a>>, stmt: Statement<'a>) {
        let span = stmt.span();
        let node = self.ast.nota_statement(span, stmt);
        items.push(NotaChild::Statement(self.ast.alloc(node)));
    }

    // ===========================================================================================
    // Props
    // ===========================================================================================

    /// Parse one `[ k:v, bare, ...spread, k:@markup ]` group into `props`. Multiple groups
    /// accumulate (union). Entered with the current token at `[`.
    fn parse_props_group(&mut self, props: &mut ArenaVec<'a, NotaProp<'a>>) {
        let open = self.cur_token().span();
        self.bump_any(); // consume `[`
        while !self.at(Kind::RBrack) && !self.at(Kind::Eof) && !self.has_fatal_error() {
            self.parse_prop_or_spread(props);
            if !self.eat(Kind::Comma) {
                break;
            }
        }
        self.expect_closing(Kind::RBrack, open);
    }

    /// Parse `| k: v, …` prop entries of a colon-sugar prop line, from `[content_start, line_end)`.
    fn parse_pipe_prop_line(
        &mut self,
        content_start: u32,
        line_end: u32,
        props: &mut ArenaVec<'a, NotaProp<'a>>,
    ) {
        self.nota_seek_to(content_start);
        while self.cur_token().start() < line_end && !self.at(Kind::Eof) && !self.has_fatal_error()
        {
            self.parse_prop_or_spread(props);
            if !self.eat(Kind::Comma) {
                break;
            }
        }
    }

    /// One prop-list entry: `...spread`, `key: value`, or bare `key`.
    fn parse_prop_or_spread(&mut self, props: &mut ArenaVec<'a, NotaProp<'a>>) {
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
    }

    /// Parse a single `key:value` or bare `key` property entry.
    fn parse_prop_entry(&mut self) -> NotaProp<'a> {
        let span_start = self.start_span();
        let key_token = self.cur_token();
        let key_span = key_token.span();
        // (The bare-vs-quoted key distinction — `["data-x": v]` — is not preserved in the AST; the
        // lowering re-derives quoting from identifier validity.)
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
            // `key: value` — the value is embedded JS, or markup (`@`-form).
            let value = if self.at(Kind::At) {
                NotaPropValue::from(self.parse_nota_form(false, false))
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
            // Bare key → shorthand. A string-literal key with no value is malformed.
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
// Control flow (`@if` / `else` / `@for`). All are expressions, so they nest in markup and
// embedded code alike. `else`/`else if` are contextual continuations (only as the next token
// after `}`, no blank line between), matched over the RAW SOURCE — robust to the markup lexer
// mode and to `\else` (not a clean JS token).
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// `@if (cond) {branch}` with optional `else`/`else if` continuations. Entered at the `if`
    /// keyword.
    fn parse_nota_if(&mut self, span_start: u32, in_body: bool) -> NotaIf<'a> {
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
        let (cons, close_end) = self.parse_branch_fragment(span_start);
        if self.has_fatal_error() {
            let span = self.end_span(span_start);
            return self.ast.nota_if(span, cond, cons, None);
        }
        // The branch's `}` is the current token; scan for a continuation from one past it.
        let alternate = self.parse_else_continuation(close_end, in_body);
        let span = Span::new(span_start, self.prev_token_end);
        self.ast.nota_if(span, cond, cons, alternate)
    }

    /// Parse what follows an `@if`/`else if` branch's `}`: an `else`/`else if` continuation, or
    /// nothing. The whole chain resumes the outer context exactly once, at its end (the `else if`
    /// recursion owns the resume of its own tail).
    fn parse_else_continuation(&mut self, close_end: u32, in_body: bool) -> Option<NotaElse<'a>> {
        match else_peek(self.source_text, close_end) {
            ElsePeek::None => {
                self.resume_at(close_end, in_body);
                None
            }
            ElsePeek::ElseIf { if_offset } => {
                self.nota_seek_to(if_offset);
                let span_start = self.cur_token().start();
                let nif = self.parse_nota_if(span_start, in_body);
                Some(NotaElse::ElseIf(self.ast.alloc(nif)))
            }
            ElsePeek::Else { brace_offset } => {
                self.nota_seek_to(brace_offset);
                let span_start = self.cur_token().start();
                let (alt, else_end) = self.parse_branch_fragment(span_start);
                self.resume_at(else_end, in_body);
                Some(NotaElse::Else(self.ast.alloc(alt)))
            }
        }
    }

    /// `@for (bind of iter) {body}`; `bind` is any binding pattern. Entered at the `for` keyword.
    fn parse_nota_for(&mut self, span_start: u32, in_body: bool) -> NotaFor<'a> {
        self.bump_any(); // → `(`
        let open = self.cur_token().span();
        self.expect(Kind::LParen);
        let bind = self.parse_binding_pattern();
        if !self.at(Kind::Of) {
            // `@for` is the comprehension form; C-style `for(;;)` has no `@`-form (write it in `%`).
            let error = diagnostics::nota_for_expects_of(self.cur_token().span());
            return self.fatal_error(error);
        }
        self.bump_any(); // consume `of`
        let iter = self.parse_assignment_expression_or_higher();
        self.expect_closing(Kind::RParen, open);
        let (body, body_end) = self.parse_branch_fragment(span_start);
        let span = Span::new(span_start, body_end);
        self.resume_at(body_end, in_body);
        self.ast.nota_for(span, bind, iter, body)
    }

    /// Parse an `@if`/`else`/`@for` branch body into a fragment + its end offset (`span_start` is
    /// the form's start). The body's `}` is left current ([`Self::parse_braced_body`]).
    fn parse_branch_fragment(&mut self, span_start: u32) -> (NotaFragment<'a>, u32) {
        let (children, end) = self.parse_braced_body();
        (self.ast.nota_fragment(Span::new(span_start, end), children), end)
    }
}

// ===============================================================================================
// Markup sugar: emphasis `*`/`_`, headings `#`, lists `-`/`+`/`N.`. Each parses to a faithful
// node; the runtime `struct` pass does list/paragraph/section grouping (the reader emits flat
// per-line/per-span nodes).
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse an emphasis span opened by `marker` (`*`→strong, `_`→em) at raw offset `open`. With
    /// no matching close in scope the marker is literal (Typst behavior).
    fn parse_emphasis(&mut self, marker: u8, open: u32, items: &mut ArenaVec<'a, NotaChild<'a>>) {
        if let Some(close) = find_emphasis_close(self.source_text, open, marker) {
            let mut children = self.ast.vec();
            self.collect_markup_range(open + 1, close, &mut children);
            let marker =
                if marker == b'*' { NotaEmphasisMarker::Strong } else { NotaEmphasisMarker::Em };
            let span = Span::new(open, close + 1);
            let element = self.ast.nota_emphasis(span, marker, children);
            items.push(NotaChild::Emphasis(self.ast.alloc(element)));
            self.nota_seek_markup(close + 1);
        } else {
            self.push_text(items, open, open + 1);
            self.nota_seek_markup(open + 1);
        }
    }

    /// If the line at `line_start` opens with a heading marker, parse it and return
    /// `(heading, end)` where `end` is the line's terminating `\n` (or the sugar clip: a body's
    /// depth-0 `}` / a bounded range's end); else `None`.
    fn try_heading(&mut self, line_start: u32, mode: BodyMode) -> Option<(NotaHeading<'a>, u32)> {
        let (level, body_start, line_end) = heading_at(self.source_text, line_start)?;
        let line_end = line_end.min(self.sugar_line_end(line_start, mode));
        let body_start = body_start.min(line_end);
        let mut children = self.ast.vec();
        self.collect_markup_range(body_start, line_end, &mut children);
        let span = Span::new(line_start, line_end);
        Some((self.ast.nota_heading(span, level, children), line_end))
    }

    /// Parse a run of list items starting at `line_start` (known to be a marker line), pushing
    /// each as a child. Markers at the run's indent are siblings; a deeper marker line falls
    /// inside the preceding item's body extent and nests via the recursive body collection; a
    /// shallower one ends the run (it belongs to an enclosing list). Returns the resume offset.
    fn parse_list(
        &mut self,
        line_start: u32,
        mode: BodyMode,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
    ) -> u32 {
        let base_indent = list_marker_at(self.source_text, line_start)
            .expect("parse_list: not a marker line")
            .indent;
        let mut at = line_start;

        while let Some(marker) = list_marker_at(self.source_text, at) {
            if marker.indent < base_indent {
                break;
            }
            // Body extent: rest of the marker line + lines indented strictly past the marker.
            // A sugar clip (the enclosing body's `}` / a bounded end) ends the item ON this line —
            // the body is closing, so there are no continuation lines to collect.
            let full_line_end = line_content_end(self.source_text, at);
            let line_end = full_line_end.min(self.sugar_line_end(at, mode));
            let body_start = marker.body_col.min(line_end);
            let item_end = if line_end < full_line_end {
                line_end
            } else {
                let extent = list_item_extent(self.source_text, line_end, marker.indent);
                match mode {
                    BodyMode::Bounded { end } => extent.min(end),
                    BodyMode::Body | BodyMode::Document => extent,
                }
            };

            let mut children = self.ast.vec();
            self.collect_markup_range(body_start, item_end, &mut children);
            let kind = if marker.ordered { NotaListKind::Ordered } else { NotaListKind::Unordered };
            let span = Span::new(marker.offset, item_end);
            let item = self.ast.nota_list_item(span, kind, children);
            items.push(NotaChild::ListItem(self.ast.alloc(item)));

            at = item_end;
        }
        at
    }
}

// ===============================================================================================
// Raw spans: verbatim `|{ … }|`, code `` `…` ``/fenced, math `$…$`/`$$…$$`. Extents are scanned
// over the raw source; the lexer is re-seeked only on resume. Content lowers to `String.raw`
// templates. Math `@`-interpolation becomes a `${…}` substitution; verbatim `|@` re-enters Nota
// as a *sibling* child.
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse `@head|{ … }|` — a verbatim-body element. `head.end` points at the `|` of `|{`.
    fn parse_verbatim_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
    ) -> NotaVerbatim<'a> {
        let body_start = head.end + 2; // past `|{`
        let (parts, after) = self.collect_verbatim_body(body_start);
        let span = Span::new(span_start, after);
        let tag = self.head_to_tag(head);
        let element = self.ast.nota_verbatim(span, tag, parts);
        self.resume_at(after, in_body);
        element
    }

    /// Collect a verbatim body from `start` (just past `|{`): raw runs bounded by
    /// [`verbatim_boundary`], with each `|@` re-arming one Nota `@`-form as a sibling child; ends
    /// at `}|`. Returns `(parts, after)` where `after` is one past the closing `}|` (or EOF, with
    /// a diagnostic).
    fn collect_verbatim_body(&mut self, start: u32) -> (ArenaVec<'a, NotaVerbatimPart<'a>>, u32) {
        let mut children = self.ast.vec();
        // Drop a single leading newline right after `|{` (the Scribble `{`-newline rule);
        // otherwise the body is fully raw — no indent strip, no trimming.
        let start = if byte_at(self.source_text, start) == Some(b'\n') { start + 1 } else { start };
        let mut run_start = start;
        loop {
            let (run_end, boundary) = verbatim_boundary(self.source_text, run_start);
            self.push_raw_run(&mut children, run_start, run_end);
            match boundary {
                VerbatimBoundary::Close { after } => return (children, after),
                VerbatimBoundary::ArmedAt { at } => {
                    // Armed escape: parse one `@`-form, resume the raw scan after it.
                    self.nota_seek_to(at);
                    debug_assert!(self.at(Kind::At), "verbatim `|@` not at `@`");
                    let form = self.parse_nota_form(false, false);
                    children.push(NotaVerbatimPart::from(form));
                    run_start = self.prev_token_end;
                }
                VerbatimBoundary::Eof => {
                    let span = Span::new(start, run_end);
                    self.set_fatal_error(diagnostics::nota_unterminated_verbatim(span));
                    return (children, run_end);
                }
            }
        }
    }

    /// Push the raw slice `[from, to)` as a verbatim raw part (skipped if empty).
    fn push_raw_run(&self, children: &mut ArenaVec<'a, NotaVerbatimPart<'a>>, from: u32, to: u32) {
        if to <= from {
            return;
        }
        let raw: &'a str = &self.source_text[from as usize..to as usize];
        let text = self.ast.nota_text(Span::new(from, to), raw);
        children.push(NotaVerbatimPart::Raw(self.ast.alloc(text)));
    }

    /// Parse a code span at `tick_off`, or — with no valid close — emit the opening backtick run
    /// as literal text. Re-seeks markup at the resume offset either way.
    fn parse_code_or_literal(&mut self, items: &mut ArenaVec<'a, NotaChild<'a>>, tick_off: u32) {
        match lex_code_span(self.source_text, tick_off) {
            CodeScan::Code { span, is_block, lang, content, resume } => {
                let language = lang.map(|l| self.ast.str(self.ast.allocator.alloc_str(l)));
                let element = self.ast.nota_code(span, language, content, is_block);
                items.push(NotaChild::Code(self.ast.alloc(element)));
                self.nota_seek_markup(resume);
            }
            CodeScan::Literal { resume } => {
                self.push_text(items, tick_off, resume);
                self.nota_seek_markup(resume);
            }
        }
    }

    /// Parse a math span at `dollar_off`, or — if unterminated — emit the opening `$`-run as
    /// literal text. Re-seeks markup at the resume offset either way.
    fn parse_math_or_literal(&mut self, items: &mut ArenaVec<'a, NotaChild<'a>>, dollar_off: u32) {
        if let Some(resume) = self.parse_math_span(items, dollar_off) {
            self.nota_seek_markup(resume);
        } else {
            let run_end = sigil_run_end(self.source_text, dollar_off, b'$');
            self.push_text(items, dollar_off, run_end);
            self.nota_seek_markup(run_end);
        }
    }

    /// Parse a math span whose opening `$`-run starts at `dollar_off` (`$` inline, `$$` display).
    /// Raw-content extents come from [`math_boundary`]; only an `@(expr)` interpolation delegates
    /// to the JS parser here (the parens bound it — an `@name` is scanned lexically because the JS
    /// lexer would swallow the closing math delimiter: `$` is an identifier-continue byte, so
    /// `@i$` would lex as `i$`). Returns the resume offset, or `None` if unterminated (the `$` run
    /// is then literal).
    fn parse_math_span(
        &mut self,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
        dollar_off: u32,
    ) -> Option<u32> {
        let display = byte_at(self.source_text, dollar_off + 1) == Some(b'$');
        let delim_len: u32 = if display { 2 } else { 1 };

        let mut parts = self.ast.vec();
        let mut run_start = dollar_off + delim_len;
        let after = loop {
            let (run_end, boundary) = math_boundary(self.source_text, run_start, display);
            self.push_math_raw(&mut parts, run_start, run_end);
            run_start = match boundary {
                MathBoundary::Close { after } => break after,
                MathBoundary::Unterminated => return None,
                MathBoundary::InterpName { name_end } => {
                    let span = Span::new(run_end + 1, name_end);
                    let name: &'a str = &self.source_text[span.start as usize..span.end as usize];
                    self.push_math_interp(&mut parts, self.ast.expression_identifier(span, name));
                    name_end
                }
                MathBoundary::InterpParen => {
                    self.nota_seek_to(run_end);
                    self.bump_any(); // `@`
                    self.bump_any(); // `(`
                    let expr = self.parse_expr();
                    self.expect(Kind::RParen);
                    self.push_math_interp(&mut parts, expr);
                    self.prev_token_end
                }
                MathBoundary::LiteralAt => {
                    // `@` with no head: splice a literal `"@"` so the template stays well-formed.
                    let span = Span::new(run_end, run_end + 1);
                    self.push_math_interp(
                        &mut parts,
                        self.ast.expression_string_literal(span, "@", None),
                    );
                    run_end + 1
                }
            };
        };

        let element = self.ast.nota_math(Span::new(dollar_off, after), display, parts);
        items.push(NotaChild::Math(self.ast.alloc(element)));
        Some(after)
    }

    /// Push the raw LaTeX slice `[start, end)` as a math part (skipped if empty).
    fn push_math_raw(&self, parts: &mut ArenaVec<'a, NotaMathPart<'a>>, start: u32, end: u32) {
        if end <= start {
            return;
        }
        let slice = &self.source_text[start as usize..end as usize];
        let text = self.ast.nota_text(Span::new(start, end), slice);
        parts.push(NotaMathPart::Raw(self.ast.alloc(text)));
    }

    /// Push `expr` as a math interpolation part.
    fn push_math_interp(&self, parts: &mut ArenaVec<'a, NotaMathPart<'a>>, expr: Expression<'a>) {
        let interp = self.ast.nota_interpolation(expr.span(), expr);
        parts.push(NotaMathPart::Interpolation(self.ast.alloc(interp)));
    }
}

// ===============================================================================================
// Document mode + colon/block sugar
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse the whole file body: top-level markup siblings interleaved with `%`/`%%%` statements,
    /// all kept as faithful children (the lowering routes statements: hoist / Doc prelude / F1).
    fn parse_document_body(&mut self) -> NotaDocument<'a> {
        let mut items = self.ast.vec();

        // Skip a leading UTF-8 BOM so it is not collected as text (offsets after it are unchanged).
        let start = if self.source_text.starts_with('\u{feff}') { 3u32 } else { 0 };

        // A file opening with line-start constructs is handled by `collect_markup`'s entry arming
        // (R9: a body/range start is a line start — the document body included).
        self.nota_seek_markup(start);
        let _ = self.collect_markup(&mut items, BodyMode::Document);

        let span = Span::new(0, self.source_text.len() as u32);
        self.ast.nota_document(span, items)
    }

    /// `@head:` colon/block sugar → an element whose body is the rest of the line plus following
    /// lines indented past the `@head:` line. Leading `|` lines of the body supply `[…]` props.
    /// Entered with `:` as the current token.
    fn parse_colon_body(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
        brace_significant: bool,
    ) -> NotaElement<'a> {
        debug_assert!(self.at(Kind::Colon), "colon sugar entered not at `:`");
        let colon_end = self.cur_token().end();
        let head_line_indent = line_indent_of(self.source_text, span_start);

        let (body_src_start, body_src_end) =
            colon_block_extent(self.source_text, colon_end, head_line_indent, brace_significant);

        let mut props = self.ast.vec();
        let mut items = self.ast.vec();
        self.collect_colon_body(body_src_start, body_src_end, &mut props, &mut items);

        let span = Span::new(span_start, body_src_end);
        self.resume_at(body_src_end, in_body);
        let tag = self.head_to_tag(head);
        self.ast.nota_element(span, tag, props, items, /* is_colon */ true)
    }

    /// Collect the colon-sugar body over `[start, end)`: leading `|` lines (continuation lines
    /// whose first non-whitespace is `|`) become prop groups; the rest is the markup body.
    fn collect_colon_body(
        &mut self,
        start: u32,
        end: u32,
        props: &mut ArenaVec<'a, NotaProp<'a>>,
        items: &mut ArenaVec<'a, NotaChild<'a>>,
    ) {
        let mut body_start = start;
        let first_cont = next_line_start(self.source_text, start);
        let mut scan = first_cont;
        while scan < end {
            let Some(content_start) = colon_prop_line_at(self.source_text, scan) else {
                break;
            };
            let line_end = next_line_start(self.source_text, scan);
            self.parse_pipe_prop_line(content_start, line_end, props);
            scan = line_end;
            body_start = scan;
        }
        // If `|` lines were consumed, the rest-of-line content of `@head:` is dropped and the body
        // is the remaining suffix. Include the `\n` preceding that suffix so the whitespace pass
        // sees a body "opened with a newline" and treats its first line as an indent line —
        // without it, `@foo:⏎  | x:1⏎  hello` would leak the indent as `"  hello"`.
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
}

/// A parsed `@`-form head, with the classification needed to branch element-vs-interpolation and
/// host-vs-component-vs-dynamic.
struct NotaHead<'a> {
    kind: HeadKind<'a>,
    /// Byte offset immediately after the head (the element/interpolation switch position).
    end: u32,
}

enum HeadKind<'a> {
    /// A bare identifier head: host (lowercase string tag) or component (Capitalized identifier).
    Named { name: &'a str, span: Span },
    /// `@(expr)` — a dynamic head.
    Dynamic(Expression<'a>),
}

/// A tag name is a *component* (identifier) iff it starts with an uppercase ASCII letter;
/// otherwise it is a *host* element (string tag).
fn is_component_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}
