//! Nota `@`-markup → a faithful Nota AST (the *reader*).
//!
//! This module parses `@`-markup into the Nota AST nodes ([`NotaMarkup`] & friends, in
//! `oxc_ast::ast::nota`): each `@`-form stays in place as `Expression::NotaMarkup`, and a whole
//! `.nota` file becomes a single `NotaMarkupKind::Document` statement. Lowering to hyperscript
//! (`h`/`Fragment`/`decode`), the Scribble whitespace pass, `%`-statement routing, and component-
//! binding routing all run *later*, in `oxc_transformer::NotaLowering` — the deferred-pass analog
//! of how oxc lowers JSX.
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
        ArmedBoundary, CodeScan, ElsePeek, LinkSpans, MarkupTrigger, MathScan, VerbatimBoundary,
        armed_boundary, at_line_start_in_frame, brace_clip_on_line, byte_at, colon_block_extent,
        colon_prop_line_at, docstate_left_guard, else_peek, escape_span, find_emphasis_close,
        find_fence_close, find_strike_close, footnote_sugar_at, heading_at, is_ident_start_at,
        is_statement_line, label_sugar_at, lex_code_span, lex_comment, lex_link_span,
        lex_math_span, line_content_end, line_indent_of, list_item_extent, list_marker_at,
        markup_trigger, next_line_start, percent_line_is_empty, ref_sugar_at, scan_hyphen_tail,
        statement_bound, statement_kind, thematic_break_at, verbatim_boundary,
    },
};

/// How a markup-collection loop ([`ParserImpl::collect_markup`]) terminated.
enum MarkupClose {
    /// Closed by the body's `}` (depth 0); `end` is one byte past it.
    Curly { end: u32 },
    /// Reached end of file (the document body, a bounded range, or an unterminated body).
    Eof,
}

/// The *collection semantics* of a markup body — the behaviours that differ between
/// [`ParserImpl::collect_markup`]'s callers. This is Axis 1: it says nothing about the host a
/// form's tail resumes into (that is [`NotaRegion`], Axis 2).
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

    /// Parse a whole `.nota` file in *document mode* → a [`Program`] holding the un-lowered
    /// document as a single `Expression::NotaMarkup(Document)` statement.
    ///
    /// # Errors
    /// If the file is not well-formed Nota.    
    pub(crate) fn parse_nota_document(mut self) -> Result<Program<'a>, Vec<OxcDiagnostic>> {
        // No JS `bump_any` priming: the file starts as markup (or a `%` line), and a leading
        // `\`/`%`/etc. would choke the JS lexer. `parse_document_body` seeks from offset 0 itself.
        let document = self.parse_document_body();
        let program = self.wrap_document_program(document);
        match self.finish_nota(()) {
            Ok(()) => Ok(program),
            Err(errors) => Err(errors),
        }
    }

    fn wrap_document_program(&mut self, document: NotaDocument<'a>) -> Program<'a> {
        let span = document.span;
        let markup = self.ast.nota_markup(span, NotaMarkupKind::Document(self.ast.alloc(document)));
        let expr = Expression::NotaMarkup(self.ast.alloc(markup));
        let stmt = self.ast.statement_expression(span, expr);
        // Markup comments are trivia, not children — they ride the Program's comments vec (the
        // faithful-tree channel: the ESTree view and the highlight pass read them there; the
        // lowering rebuilds the Program without them, so the emit never sees one).
        let comments = self.ast.vec_from_iter(std::mem::take(&mut self.state.nota.comments));
        self.ast.program(
            span,
            SourceType::default().with_module(true),
            self.source_text,
            comments,
            None,
            self.ast.vec(),
            self.ast.vec1(stmt),
        )
    }

    /// Parse a whole `.nota` file in document mode with EOF error-recovery (the `--virtual`
    /// language-server path). Returns the partial [`Program`] **and** all diagnostics — the tree is
    /// never discarded (see [`crate::Parser::parse_nota_document_recover`]).
    pub(crate) fn parse_nota_document_recover(mut self) -> crate::NotaDocumentRecover<'a> {
        self.nota_recover = true;
        let document = self.parse_document_body();
        let program = self.wrap_document_program(document);
        let errors = self.finish_nota_recover();
        crate::NotaDocumentRecover { program, errors }
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

    /// Recover-path finalize: collect **all** diagnostics (fatal + lexer + parser) without
    /// discarding the partial AST. Mirrors `parse()`'s post-fatal cleanup — truncate the parser
    /// errors accumulated *after* the fatal was recorded (they are downstream noise from the
    /// aborted construct) — but keeps the fatal itself as a real diagnostic rather than swallowing
    /// the whole parse.
    fn finish_nota_recover(mut self) -> Vec<OxcDiagnostic> {
        let fatal = self.fatal_error.take();
        if let Some(FatalError { errors_len, .. }) = &fatal {
            self.errors.truncate(*errors_len);
        }
        self.check_unfinished_errors();
        let mut errors: Vec<OxcDiagnostic> =
            self.lexer.errors.into_iter().chain(self.errors).collect();
        if let Some(FatalError { error, .. }) = fatal {
            errors.push(error);
        }
        errors
    }

    // ===========================================================================================
    // `@`-form dispatch (element / fragment / interpolation / control flow / verbatim)
    // ===========================================================================================

    /// Parse one `@`-form in JS *expression position*, wrapped as `Expression::NotaMarkup`. The
    /// umbrella [`NotaMarkup`] span covers the whole form from its `@` (an inner node's span may
    /// exclude it — an interpolation's span is its expression's).
    pub(crate) fn parse_nota_markup_expression(&mut self) -> Expression<'a> {
        let span_start = self.start_span();
        let (form, _) = self.enter_region(NotaRegion::Js, Self::parse_nota_form);
        let span = self.end_span(span_start);
        let markup = self.ast.nota_markup(span, NotaMarkupKind::from(form));
        Expression::NotaMarkup(self.ast.alloc(markup))
    }

    /// Parse one `@`-form. Entered with the current token at [`Kind::At`]. The returned
    /// [`NotaForm`] converts into any form-holding position (`NotaChild`, `NotaPropValue`,
    /// `NotaVerbatimPart`, `NotaMarkupKind`) via the inherited-variant `From` impls.
    pub(crate) fn parse_nota_form(&mut self) -> NotaForm<'a> {
        let span_start = self.start_span();

        // The positional colon-sugar gate is fixed by where this form's `@` sits
        // (notation.md §Colon & block sugar):
        // classified once here, from the *entry* region and position, and threaded into both
        // trigger consumers below so a dead colon interpolates consistently.
        let colon_live = self.colon_trigger_live(span_start);

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
                let n = self.parse_nota_if(span_start);
                self.ast.alloc(n)
            }),
            Kind::For => NotaForm::For({
                let n = self.parse_nota_for(span_start);
                self.ast.alloc(n)
            }),
            Kind::LCurly => NotaForm::Fragment({
                let f = self.parse_fragment(span_start);
                self.ast.alloc(f)
            }),
            _ => {
                let Some(head) = self.parse_nota_head(colon_live) else {
                    // `@` with no valid head (`@@`, `@ `, `@1`, EOF, …): diagnose, but recover as
                    // an empty fragment so the form stays well-shaped in any position.
                    self.set_unexpected();
                    let span = self.end_span(span_start);
                    let frag = self.ast.nota_fragment(span, self.ast.vec());
                    return NotaForm::Fragment(self.ast.alloc(frag));
                };
                match self.commit_head(&head, colon_live) {
                    MarkupTrigger::Brace | MarkupTrigger::Bracket => {
                        self.parse_element(span_start, head, colon_live)
                    }
                    MarkupTrigger::Colon => NotaForm::Element({
                        let props = self.ast.vec();
                        let e = self.parse_colon_body(span_start, head, props);
                        self.ast.alloc(e)
                    }),
                    MarkupTrigger::Verbatim => NotaForm::Verbatim({
                        let body_start = head.end + 2;
                        let v = self.parse_verbatim_element(
                            span_start,
                            head,
                            self.ast.vec(),
                            body_start,
                        );
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
    fn parse_nota_head(&mut self, colon_live: bool) -> Option<NotaHead<'a>> {
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
            // is literal text after an interpolation (`@my-foo bar` is `@my` + `-foo bar`). A dead
            // colon (`t @my-foo:` mid-line) is not a trigger, so it does not pull in the tail either
            // — `effective_trigger` demotes it, keeping this site in step with `commit_head`.
            if !is_component_name(name)
                && let Some(ext_end) = scan_hyphen_tail(self.source_text, span.end)
                && !matches!(self.effective_trigger(ext_end, colon_live), MarkupTrigger::None)
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
    fn commit_head(&mut self, head: &NotaHead<'a>, colon_live: bool) -> MarkupTrigger {
        // A dead colon (the positional colon trigger) is demoted to `None`, so the head interpolates and the
        // `:` is left un-consumed for the surrounding host to lex as literal text / JS.
        let trigger = self.effective_trigger(head.end, colon_live);
        match trigger {
            // Lex the trigger (`{` / `[` / `:`) as the next JS token.
            MarkupTrigger::Brace | MarkupTrigger::Bracket | MarkupTrigger::Colon => {
                self.nota_seek_to(head.end);
            }
            // The verbatim body is scanned by absolute offset from `head.end`; the boundary token
            // stays current (the JS lexer must not eat the `|`).
            MarkupTrigger::Verbatim => {}
            // No trigger ⇒ interpolation: resume the surrounding context past the head.
            MarkupTrigger::None => self.resume_at(head.end),
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

    /// Parse `@head [props]* { body }?`, `@head [props]* |{ body }|`, or `@head [props]* : body`.
    /// Entered with the `{`/`[` delimiter as the current token. `colon_live` is the positional
    /// colon gate judged at the head's `@` ([`Self::colon_trigger_live`], threaded from
    /// [`Self::parse_nota_form`]) — the same gate a bare `@head:` uses; a glued `:` after the last
    /// `]` opens a colon body only when it holds (props compose with a colon body —
    /// notation.md §Colon & block sugar).
    fn parse_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        colon_live: bool,
    ) -> NotaForm<'a> {
        let mut props = self.ast.vec();

        // `[props]` groups, then the body-vs-self-closing decision. A group's `]` is validated but
        // NOT advanced past ([`Self::parse_props_group`]): the JS lexer's one-token lookahead must
        // never read the bytes after the `]`, which in a markup / verbatim host are raw text. The
        // continuation is a raw byte peek at the `]`'s end, re-lexed in the deliberate mode: `[` →
        // another group, `{` → a braced body, `|{` → a verbatim body (props compose with verbatim —
        // notation.md §Verbatim), `:` → a colon body when the positional gate is live (props compose
        // with a colon body exactly as with a braced/verbatim one — notation.md §Colon & block
        // sugar), anything else →
        // self-closing (a `:` under a dead gate stays literal text, exactly as for a bare head).
        let mut self_closing_end = None;
        let mut verbatim_start = None;
        let mut colon_body = false;
        while self.at(Kind::LBrack) {
            self.parse_props_group(&mut props);
            if self.has_fatal_error() {
                break;
            }
            let bracket_end = self.cur_token().end();
            match byte_at(self.source_text, bracket_end) {
                Some(b'[') => self.nota_seek_to(bracket_end),
                Some(b'{') => {
                    self.nota_seek_to(bracket_end);
                    break;
                }
                Some(b'|') if byte_at(self.source_text, bracket_end + 1) == Some(b'{') => {
                    // Verbatim body: scanned by absolute offset, like the bare `@head|{…}|` form —
                    // the boundary token stays current (the JS lexer must not eat the `|`; mirrors
                    // `commit_head`'s `MarkupTrigger::Verbatim` arm).
                    verbatim_start = Some(bracket_end + 2);
                    break;
                }
                Some(b':') if colon_live => {
                    // A glued `:` under a live positional gate opens the same colon body a bare
                    // `@head:` would. Lex the `:` (like the `{` arm) so `parse_colon_body` enters at
                    // `Kind::Colon`, exactly as `commit_head`'s `MarkupTrigger::Colon` path does; the
                    // already-collected `props` thread through unchanged.
                    self.nota_seek_to(bracket_end);
                    colon_body = true;
                    break;
                }
                _ => {
                    // Self-closing: resume the host region past the `]`, uniformly across hosts —
                    // in a `Js` host, seeking at `bracket_end` re-lexes exactly the token a plain
                    // advance past `]` would, so no body text is dropped and no raw byte is JS-lexed.
                    self.resume_at(bracket_end);
                    self_closing_end = Some(bracket_end);
                    break;
                }
            }
        }

        if let Some(body_start) = verbatim_start {
            let v = self.parse_verbatim_element(span_start, head, props, body_start);
            return NotaForm::Verbatim(self.ast.alloc(v));
        }

        if colon_body {
            let e = self.parse_colon_body(span_start, head, props);
            return NotaForm::Element(self.ast.alloc(e));
        }

        let (children, end) = if let Some(end) = self_closing_end {
            (self.ast.vec(), end)
        } else if self.at(Kind::LCurly) && !self.has_fatal_error() {
            let (children, end) = self.parse_braced_body();
            self.resume_at(end);
            (children, end)
        } else {
            // Reached only after a fatal error in a prop group; keep a faithful span.
            (self.ast.vec(), self.prev_token_end)
        };

        let span = Span::new(span_start, end);
        let tag = self.head_to_tag(head);
        // In recovery, an unclosed `[props]` group left a completion anchor (the `[` span) — thread
        // it onto the element so the lowering can map a prop-completion cursor into the props object.
        let props_recovery = self.nota_prop_anchor.take();
        NotaForm::Element(self.ast.alloc(self.ast.nota_element(
            span,
            tag,
            props,
            children,
            /* is_colon */ false,
            props_recovery,
        )))
    }

    /// `@{ body }` → a fragment node. `@` already consumed.
    fn parse_fragment(&mut self, span_start: u32) -> NotaFragment<'a> {
        let (children, end) = self.parse_braced_body();
        self.resume_at(end);
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
    fn parse_braced_body(&mut self) -> (NotaChildren<'a>, u32) {
        if !self.at(Kind::LCurly) {
            let error = diagnostics::nota_control_expects_body(self.cur_token().span());
            self.set_fatal_error(error);
            return (self.ast.vec(), self.prev_token_end);
        }
        let open = self.cur_token().span();
        self.advance_for_nota_child(); // switch the lexer into markup-body mode

        // The body content starts one past `{` (a body start counts as a line start).
        let (close, items) = self.collect_markup(BodyMode::Body, open.end);
        let end = match close {
            MarkupClose::Curly { end } => end,
            MarkupClose::Eof => {
                self.expect_markup_body_close(open);
                self.prev_token_end
            }
        };
        (items, end)
    }

    /// Resume the enclosing host at raw offset `offset` — the single exit primitive every `@`-form
    /// construct returns through. A three-way dispatch on the top [`NotaRegion`]: a `Markup` body
    /// re-lexes markup text, a `Js` island re-lexes a JS token, a `Raw` scan parks (no lex — it
    /// re-seeks itself from `prev_token_end`). No-op after a fatal error (the collectors bail out;
    /// nothing left to lex).
    fn resume_at(&mut self, offset: u32) {
        match self.nota_top_region() {
            NotaRegion::Markup { .. } => self.nota_seek_markup(offset),
            NotaRegion::Js => self.nota_seek_to(offset),
            NotaRegion::Raw => self.nota_park(offset),
        }
    }

    /// Push the source slice `[start, end)` as a literal text child (skipped if empty). Every text
    /// child — including single-byte sigils that turned out literal — is a real source slice, so
    /// [`NotaText`] spans are always true source positions.
    fn push_text(&mut self, start: u32, end: u32) {
        if end <= start {
            return;
        }
        let slice = &self.source_text[start as usize..end as usize];
        let text = self.ast.nota_text(Span::new(start, end), slice);
        let item = NotaChild::Text(self.ast.alloc(text));
        self.push_nota_item(item);
    }

    fn collect_markup(&mut self, mode: BodyMode, start: u32) -> (MarkupClose, NotaChildren<'a>) {
        let (close, region) = self.enter_region(
            NotaRegion::Markup { mode, start, items: self.ast.vec() },
            Self::collect_markup_inner,
        );
        let NotaRegion::Markup { items, .. } = region else {
            unreachable!("enter_region returned unexpected region")
        };
        (close, items)
    }

    /// The core markup-collection loop, shared by element/control bodies, the document body, and
    /// bounded sub-ranges. Dispatches on the typed child tokens from `next_nota_child`; balanced
    /// `{…}` braces are literal text (Scribble `@foo{f{o}o}` → `"f{o}o"`); `\n` runs stay in the
    /// text stream verbatim (the Scribble whitespace pass owns line handling at lowering time).
    /// Entered with the current token already lexed as a markup child.x
    fn collect_markup_inner(&mut self) -> MarkupClose {
        let mut depth = 0u32; // balanced-brace depth inside the body
        // The start of a body/range is a line start (notation.md §Markup sugar). A body opening
        // directly with a marker —
        // `@{- item}`, `@foo: - item`, `*- item*`, the document's first line — opens the construct
        // exactly as it would after a `\n`. (Literal braces in prose never re-enter here, so a
        // `{- x}` inside a paragraph stays text.)
        if !self.has_fatal_error() {
            let at = self.cur_token().start();
            let resume = self.consume_line_start_constructs(at, 0);
            if resume != at {
                self.nota_seek_markup(resume);
            }
        }
        let mode = self.nota_body_mode();
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
                    let (end, reached_bound) = match mode.bound() {
                        // Clip the run to a bounded range's end.
                        Some(end) => (token.end().min(end), token.end() >= end),
                        None => (token.end(), false),
                    };
                    self.push_text(token.start(), end);
                    if reached_bound {
                        return MarkupClose::Eof;
                    }
                    self.advance_for_nota_child();
                }
                Kind::NotaNewline => {
                    // Line boundary: keep the `\n` as text, then consume any run of line-start
                    // constructs (`%`/`%%%` statements, lists, a heading) on the following lines.
                    let nl = self.cur_token().start();
                    self.push_text(nl, nl + 1);
                    let resume = self.consume_line_start_constructs(nl + 1, depth);
                    self.nota_seek_markup(resume);
                }
                Kind::LCurly => {
                    depth += 1;
                    let s = self.cur_token().start();
                    self.push_text(s, s + 1);
                    self.advance_for_nota_child();
                }
                Kind::RCurly if depth > 0 => {
                    depth -= 1;
                    let s = self.cur_token().start();
                    self.push_text(s, s + 1);
                    self.advance_for_nota_child();
                }
                // Body close: leave the `}` current so `parse_body` can consume it in the right
                // mode. Document / bounded bodies treat a depth-0 `}` as literal text instead.
                Kind::RCurly if matches!(mode, BodyMode::Body) => {
                    return MarkupClose::Curly { end: self.cur_token().end() };
                }
                Kind::RCurly => {
                    let s = self.cur_token().start();
                    self.push_text(s, s + 1);
                    self.advance_for_nota_child();
                }
                Kind::At => {
                    let form = self.parse_nota_form();
                    self.push_nota_item(NotaChild::from(form));
                    self.consume_line_start_after_form(depth);
                }
                Kind::Star | Kind::NotaUnderscore => {
                    // The lexer emits these only for a valid opener (the Typst word-boundary
                    // rule); close-matching still decides marker-vs-literal.
                    let m = if self.cur_kind() == Kind::Star { b'*' } else { b'_' };
                    self.parse_emphasis(m, self.cur_token().start());
                }
                Kind::NotaBackslash => {
                    // General escape: `\<c>` → literal `<c>`, the `\` dropped. The clamped scan view
                    // keeps `<c>` within a bounded raw span's extent (an escape can't reach past it).
                    let span = escape_span(self.nota_scan_source(), self.cur_token().start());
                    self.push_text(span.start, span.end);
                    self.nota_seek_markup(span.end);
                }
                Kind::NotaBacktick => {
                    self.parse_code_or_literal(self.cur_token().start());
                }
                Kind::NotaDollar => {
                    self.parse_math_or_literal(self.cur_token().start());
                }
                // Doc-state sugar (notation.md §Doc-state references): the lexer emits these only
                // for a valid *shape*
                // (`<`/`&` + ident-start, the `[^`+ident digraph); the parser resolves the left
                // guard (`<`/`&`), the terminator scan, and marker-vs-literal.
                Kind::LAngle => {
                    self.parse_label_sugar(self.cur_token().start());
                }
                Kind::Amp => {
                    self.parse_ref_sugar(self.cur_token().start());
                }
                Kind::LBrack => {
                    self.parse_bracket_sugar(self.cur_token().start());
                    // A `[^x]: body` definition reuses the colon-body extent machinery, so it can
                    // resume at a line start exactly like an `@head:` form — same hook (else a
                    // heading/list/`%` after the definition lexes as literal text; the mid-line
                    // forms — mark/link/literal — resume mid-line and fall through it).
                    self.consume_line_start_after_form(depth);
                }
                Kind::Bang => {
                    // The lexer emits `Bang` only at an `![` digraph; the parser validates the
                    // full `![alt](src)` shape, else the `!` is literal.
                    self.parse_image_or_literal(self.cur_token().start());
                }
                Kind::Pipe => {
                    // A bare `|` in a markup body is literal (`|{`/`|@` are handled at the head
                    // switch / inside verbatim bodies, never here).
                    let s = self.cur_token().start();
                    self.push_text(s, s + 1);
                    self.advance_for_nota_child();
                }
                Kind::Slash => {
                    // The lexer emits `Slash` only at a comment opener (`//` or `/*`,
                    // notation.md §Comments). A comment is trivia — no child.
                    self.parse_nota_comment(self.cur_token().start(), depth);
                }
                Kind::Tilde => {
                    // The lexer emits `Tilde` (a 2-byte `~~` token) only at a valid opener;
                    // close-matching still decides marker-vs-literal, like emphasis.
                    self.parse_strike(self.cur_token().start());
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

    /// Consume a markup comment opened at `open` (`//` to end of line, `/* … */` nestable —
    /// notation.md §Comments). A comment is **trivia**: it produces no child and is recorded on
    /// the parser state (→ the Program's comments vec — the ESTree view and the highlight pass).
    /// A comment that has its line to itself — line-leading (whitespace-only since the line /
    /// frame start) with nothing but whitespace after its close on the closing line — is consumed
    /// *with* that line's `\n`, so a comment-only line contributes no phantom soft/paragraph
    /// break; the resume then sits at a line start and runs the line-start hook (a heading, list,
    /// or `%` statement directly after a comment line is sugar, not literal text). An unterminated
    /// block comment (no matching `*/` within the frame) is a fatal diagnostic.
    fn parse_nota_comment(&mut self, open: u32, depth: u32) {
        let limit = self.docstate_scan_limit();
        let scan = lex_comment(self.nota_scan_source(), open, limit);
        if scan.block && !scan.terminated {
            let error = diagnostics::nota_unterminated_comment(Span::new(open, scan.end));
            self.set_fatal_error(error);
            return;
        }
        let kind = if !scan.block {
            CommentKind::Line
        } else if self.source_text[open as usize..scan.end as usize].contains('\n') {
            CommentKind::MultiLineBlock
        } else {
            CommentKind::SingleLineBlock
        };
        self.state.nota.comments.push(Comment::new(open, scan.end, kind));

        let frame_start = match self.nota_top_region() {
            NotaRegion::Markup { start, .. } => *start,
            NotaRegion::Js | NotaRegion::Raw => 0, // unreachable: Slash only fires in markup
        };
        let close_line_end = line_content_end(self.source_text, scan.end);
        let own_line = at_line_start_in_frame(self.source_text, open, frame_start)
            && self.source_text[scan.end as usize..close_line_end as usize]
                .bytes()
                .all(|b| matches!(b, b' ' | b'\t' | b'\r'))
            && close_line_end < limit
            && byte_at(self.source_text, close_line_end) == Some(b'\n');
        if own_line {
            self.nota_seek_markup(close_line_end + 1);
            self.consume_line_start_after_form(depth);
        } else {
            self.nota_seek_markup(scan.end);
        }
    }

    /// A colon-sugar body (`@head: …` — or a `[^x]: …` footnote definition, which reuses the same
    /// extent machinery) consumes through its final line's `\n` and any trailing blank lines, so
    /// the form can resume AT a line start — a position the `NotaNewline` arm's line-start hook
    /// never sees. Run the same hook after such a form: a heading, list, or `%` statement directly
    /// after a colon block is sugar, not literal text. (Forms that resume mid-line fail the
    /// preceding-`\n` check and fall through — the hook is safe after any form.)
    fn consume_line_start_after_form(&mut self, depth: u32) {
        let at = self.cur_token().start();
        if !self.has_fatal_error() && at > 0 && byte_at(self.source_text, at - 1) == Some(b'\n') {
            let resume = self.consume_line_start_constructs(at, depth);
            self.nota_seek_markup(resume);
        }
    }

    /// Consume a run of line-start constructs starting at `at` (a line start): `%`/`%%%`
    /// statements (when `mode` permits), list runs, then a trailing heading — pushing each as a
    /// child. Statements and lists each resume at a line start that may itself open another, so
    /// the loop chains. `depth` gates lists/headings (they fire only at brace depth 0). Returns
    /// the offset to resume markup text from; a heading resumes at its trailing `\n` so the
    /// caller's next `\n` iteration chains into whatever follows.
    fn consume_line_start_constructs(&mut self, mut at: u32, depth: u32) -> u32 {
        let mode = self.nota_body_mode();
        loop {
            if mode.allows_statements() && is_statement_line(self.source_text, at) {
                at = self.collect_statements(at);
                continue;
            }
            if depth == 0
                && mode.bound().is_none_or(|end| at < end)
                && list_marker_at(self.source_text, at).is_some()
            {
                at = self.parse_list(at, mode);
                continue;
            }
            // A `---` thematic-break line → one `<hr/>` child; resume at the line's trailing
            // `\n` (like a heading) so the caller's next `\n` iteration chains into what follows.
            if depth == 0
                && mode.bound().is_none_or(|end| at < end)
                && let Some(span) =
                    thematic_break_at(self.source_text, at, self.sugar_line_end(at, mode))
            {
                let node = self.ast.nota_thematic_break(span);
                self.push_nota_item(NotaChild::ThematicBreak(self.ast.alloc(node)));
                at = self.sugar_line_end(at, mode);
                continue;
            }
            break;
        }
        if depth == 0
            && mode.bound().is_none_or(|end| at < end)
            && let Some((heading, h_end)) = self.try_heading(at, mode)
        {
            self.push_nota_item(NotaChild::Heading(self.ast.alloc(heading)));
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
    fn collect_markup_range(&mut self, start: u32, end: u32) -> NotaChildren<'a> {
        self.nota_seek_markup(start);
        let (_, items) = self.collect_markup(BodyMode::Bounded { end }, start);
        items
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
    fn collect_statements(&mut self, line_start: u32) -> u32 {
        let mut at = line_start;
        while let Some((content, is_fence)) = statement_kind(self.source_text, at) {
            let end = if is_fence {
                self.collect_fence_statements(content)
            } else if percent_line_is_empty(self.source_text, content) {
                // An empty / comment-only `%` line is a no-op; it must not swallow the following
                // markup as a statement.
                content
            } else {
                self.collect_percent_statements(content)
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
    fn collect_percent_statements(&mut self, content: u32) -> u32 {
        let bound = statement_bound(self.source_text, content);
        debug_assert!(self.source_text.is_char_boundary(bound as usize));
        let lexer_errors_before = self.lexer.errors.len();
        let parser_errors_before = self.errors.len();
        self.with_source_end_bound(bound, |p| {
            p.nota_seek_to(content);
            loop {
                let stmt =
                    p.parse_statement_list_item(crate::context::StatementContext::StatementList);
                p.push_statement(stmt);
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
    fn collect_fence_statements(&mut self, inner_start: u32) -> u32 {
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
                p.push_statement(stmt);
            }
        });
        after_fence
    }

    fn push_statement(&mut self, stmt: Statement<'a>) {
        let span = stmt.span();
        let node = self.ast.nota_statement(span, stmt);
        self.push_nota_item(NotaChild::Statement(self.ast.alloc(node)));
    }

    // ===========================================================================================
    // Props
    // ===========================================================================================

    /// Parse one `[ k:v, bare, ...spread, k:@markup ]` group into `props`. Multiple groups
    /// accumulate (union). Entered with the current token at `[`. The closing `]` is validated but
    /// left as the current token — [`Self::parse_element`] chooses the continuation by a raw byte
    /// peek past it, so the JS lexer never reads the (possibly raw-markup) bytes after the `]`.
    fn parse_props_group(&mut self, props: &mut NotaProps<'a>) {
        let open = self.cur_token().span();
        self.bump_any(); // consume `[`
        while !self.at(Kind::RBrack) && !self.at(Kind::Eof) && !self.has_fatal_error() {
            self.parse_prop_or_spread(props);
            if !self.eat(Kind::Comma) {
                break;
            }
        }
        // EOF error-recovery (`--virtual`): a `[props]` group that ran into end of file records a
        // completion anchor at its `[` so the lowering can offer prop completions at `@tag[|`. The
        // `expect_closing` diagnostic below still fires (demoted from fatal to recoverable by the
        // recover entry), so the editor also shows "expected `]`".
        if self.nota_recover && self.at(Kind::Eof) {
            self.nota_prop_anchor = Some(open);
        }
        self.expect_closing_without_advance(Kind::RBrack, open);
    }

    /// Parse `| k: v, …` prop entries of a colon-sugar prop line, from `[content_start, line_end)`.
    fn parse_pipe_prop_line(
        &mut self,
        content_start: u32,
        line_end: u32,
        props: &mut NotaProps<'a>,
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
    fn parse_prop_or_spread(&mut self, props: &mut NotaProps<'a>) {
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
                let (form, _) = self.enter_region(NotaRegion::Js, Self::parse_nota_form);
                NotaPropValue::from(form)
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
    fn parse_nota_if(&mut self, span_start: u32) -> NotaIf<'a> {
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
        let alternate = self.parse_else_continuation(close_end);
        let span = Span::new(span_start, self.prev_token_end);
        self.ast.nota_if(span, cond, cons, alternate)
    }

    /// Parse what follows an `@if`/`else if` branch's `}`: an `else`/`else if` continuation, or
    /// nothing. The whole chain resumes the outer context exactly once, at its end (the `else if`
    /// recursion owns the resume of its own tail).
    fn parse_else_continuation(&mut self, close_end: u32) -> Option<NotaElse<'a>> {
        match else_peek(self.source_text, close_end) {
            ElsePeek::None => {
                self.resume_at(close_end);
                None
            }
            ElsePeek::ElseIf { if_offset } => {
                self.nota_seek_to(if_offset);
                let span_start = self.cur_token().start();
                let nif = self.parse_nota_if(span_start);
                Some(NotaElse::ElseIf(self.ast.alloc(nif)))
            }
            ElsePeek::Else { brace_offset } => {
                self.nota_seek_to(brace_offset);
                let span_start = self.cur_token().start();
                let (alt, else_end) = self.parse_branch_fragment(span_start);
                self.resume_at(else_end);
                Some(NotaElse::Else(self.ast.alloc(alt)))
            }
        }
    }

    /// `@for (bind of iter) {body}`; `bind` is any binding pattern. Entered at the `for` keyword.
    fn parse_nota_for(&mut self, span_start: u32) -> NotaFor<'a> {
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
        self.resume_at(body_end);
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
    fn parse_emphasis(&mut self, marker: u8, open: u32) {
        if let Some(close) = find_emphasis_close(self.nota_scan_source(), open, marker) {
            let children = self.collect_markup_range(open + 1, close);
            let marker =
                if marker == b'*' { NotaEmphasisMarker::Strong } else { NotaEmphasisMarker::Em };
            let span = Span::new(open, close + 1);
            let element = self.ast.nota_emphasis(span, marker, children);
            self.push_nota_item(NotaChild::Emphasis(self.ast.alloc(element)));
            self.nota_seek_markup(close + 1);
        } else {
            self.push_text(open, open + 1);
            self.nota_seek_markup(open + 1);
        }
    }

    /// Parse a `~~…~~` strikethrough span opened at `open` (notation.md §Markup sugar) — the
    /// emphasis machinery with a two-byte marker, lowering to `<s>`. With no matching close in
    /// scope both opener bytes are literal.
    fn parse_strike(&mut self, open: u32) {
        if let Some(close) = find_strike_close(self.nota_scan_source(), open) {
            let children = self.collect_markup_range(open + 2, close);
            let span = Span::new(open, close + 2);
            let element = self.ast.nota_emphasis(span, NotaEmphasisMarker::Strike, children);
            self.push_nota_item(NotaChild::Emphasis(self.ast.alloc(element)));
            self.nota_seek_markup(close + 2);
        } else {
            self.push_text(open, open + 2);
            self.nota_seek_markup(open + 2);
        }
    }

    /// If the line at `line_start` opens with a heading marker, parse it and return
    /// `(heading, end)` where `end` is the line's terminating `\n` (or the sugar clip: a body's
    /// depth-0 `}` / a bounded range's end); else `None`.
    fn try_heading(&mut self, line_start: u32, mode: BodyMode) -> Option<(NotaHeading<'a>, u32)> {
        let (level, body_start, line_end) = heading_at(self.source_text, line_start)?;
        let line_end = line_end.min(self.sugar_line_end(line_start, mode));
        let body_start = body_start.min(line_end);
        let children = self.collect_markup_range(body_start, line_end);
        let span = Span::new(line_start, line_end);
        Some((self.ast.nota_heading(span, level, children), line_end))
    }

    /// Parse a run of list items starting at `line_start` (known to be a marker line), pushing
    /// each as a child. Markers at the run's indent are siblings; a deeper marker line falls
    /// inside the preceding item's body extent and nests via the recursive body collection; a
    /// shallower one ends the run (it belongs to an enclosing list). Returns the resume offset.
    fn parse_list(&mut self, line_start: u32, mode: BodyMode) -> u32 {
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
                match mode.bound() {
                    Some(end) => extent.min(end),
                    None => extent,
                }
            };

            let children = self.collect_markup_range(body_start, item_end);
            let kind = if marker.ordered { NotaListKind::Ordered } else { NotaListKind::Unordered };
            let span = Span::new(marker.offset, item_end);
            let item = self.ast.nota_list_item(span, kind, children);
            self.push_nota_item(NotaChild::ListItem(self.ast.alloc(item)));

            at = item_end;
        }
        at
    }
}

// ===============================================================================================
// Doc-state sugar (notation.md §Doc-state references): `<label>` / `&ref` / `[^mark]` /
// line-start `[^label]: body`.
// Each is surface sugar for an element form (`@Label[id: "…"]{}` / `@Ref[id: "…"]{}` /
// `@FootnoteMark[label: "…"]{}` / `@FootnoteText[label: "…"]: body`) and inherits the element
// machinery — the bounded-frame clip, the positional colon gate, the colon-body extent —
// rather than growing extent rules of its own. A non-matching open is literal text (1-byte sigil).
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// The exclusive scan limit for a doc-state sugar: the clamped scan view's end (an armed form
    /// inside a raw span must not read past its extent), further clipped to a bounded frame's end
    /// (a match may not reach past the frame — `*<ab_-x>_` must not steal the `>` beyond the
    /// emphasis close).
    fn docstate_scan_limit(&self) -> u32 {
        let src_end = self.lexer.nota_source_end();
        match self.nota_body_mode().bound() {
            Some(bound) => bound.min(src_end),
            None => src_end,
        }
    }

    /// The left-boundary guard on `<`/`&`: start-of-body (the enclosing markup
    /// frame's content start — `*<x>*`, `@a:<x>`), or the byte-level guard (start of source /
    /// line, whitespace, opening punctuation).
    fn docstate_guard_ok(&self, at: u32) -> bool {
        match self.nota_top_region() {
            NotaRegion::Markup { start, .. } => {
                at == *start || docstate_left_guard(self.source_text, at)
            }
            NotaRegion::Js | NotaRegion::Raw => false,
        }
    }

    /// Build + push a [`NotaDocState`] child.
    fn push_doc_state(
        &mut self,
        kind: NotaDocStateKind,
        span: Span,
        label_span: Span,
        children: NotaChildren<'a>,
    ) {
        let label = &self.source_text[label_span.start as usize..label_span.end as usize];
        let node = self.ast.nota_doc_state(span, kind, label, label_span, children);
        self.push_nota_item(NotaChild::DocState(self.ast.alloc(node)));
    }

    /// `<label>` at `open` (≡ `@Label[id: "label"]{}`), or a literal `<`.
    fn parse_label_sugar(&mut self, open: u32) {
        let limit = self.docstate_scan_limit();
        if self.docstate_guard_ok(open)
            && let Some(label_span) = label_sugar_at(self.nota_scan_source(), open, limit)
        {
            let end = label_span.end + 1; // past `>`
            self.push_doc_state(
                NotaDocStateKind::Label,
                Span::new(open, end),
                label_span,
                self.ast.vec(),
            );
            self.nota_seek_markup(end);
        } else {
            self.push_text(open, open + 1);
            self.nota_seek_markup(open + 1);
        }
    }

    /// `&ref` at `open` (≡ `@Ref[id: "ref"]{}`), or a literal `&`.
    fn parse_ref_sugar(&mut self, open: u32) {
        let limit = self.docstate_scan_limit();
        if self.docstate_guard_ok(open)
            && let Some(label_span) = ref_sugar_at(self.nota_scan_source(), open, limit)
        {
            self.push_doc_state(
                NotaDocStateKind::Ref,
                Span::new(open, label_span.end),
                label_span,
                self.ast.vec(),
            );
            self.nota_seek_markup(label_span.end);
        } else {
            self.push_text(open, open + 1);
            self.nota_seek_markup(open + 1);
        }
    }

    /// Dispatch a markup `[` at `open` between the bracket sugars, in fixed precedence
    /// (notation.md §Links): the footnote digraph `[^mark]` / `[^label]: body` first, then a
    /// `[text](url)` link, else a literal `[`.
    fn parse_bracket_sugar(&mut self, open: u32) {
        let limit = self.docstate_scan_limit();
        if let Some(label_span) = footnote_sugar_at(self.nota_scan_source(), open, limit) {
            self.parse_footnote_sugar(open, label_span, limit);
        } else if let Some(link) = lex_link_span(self.nota_scan_source(), open, limit) {
            self.parse_link(open, &link);
        } else {
            self.push_text(open, open + 1);
            self.nota_seek_markup(open + 1);
        }
    }

    /// `[text](url)` at `open` — an inline link (notation.md §Links) ≡ `@a[href: "url"]{text}`.
    /// The text is a bounded markup body; the url stays a raw slice (trimmed/cooked at lowering).
    fn parse_link(&mut self, open: u32, link: &LinkSpans) {
        let children = self.collect_markup_range(link.text.start, link.text.end);
        let url = &self.source_text[link.url.start as usize..link.url.end as usize];
        let span = Span::new(open, link.resume);
        let node = self.ast.nota_link(span, url, link.url, children);
        self.push_nota_item(NotaChild::Link(self.ast.alloc(node)));
        self.nota_seek_markup(link.resume);
    }

    /// `![alt](src)` at `open` (the `!`) — an image (notation.md §Links) ≡
    /// `@img[src: "src", alt: "alt"]{}`; the alt is plain text (no markup). Without the full
    /// shape the `!` is literal (the following `[` then re-dispatches on its own).
    fn parse_image_or_literal(&mut self, open: u32) {
        let limit = self.docstate_scan_limit();
        if let Some(link) = lex_link_span(self.nota_scan_source(), open + 1, limit) {
            let alt = &self.source_text[link.text.start as usize..link.text.end as usize];
            let src = &self.source_text[link.url.start as usize..link.url.end as usize];
            let span = Span::new(open, link.resume);
            let node = self.ast.nota_image(span, alt, link.text, src, link.url);
            self.push_nota_item(NotaChild::Image(self.ast.alloc(node)));
            self.nota_seek_markup(link.resume);
        } else {
            self.push_text(open, open + 1);
            self.nota_seek_markup(open + 1);
        }
    }

    /// `[^mark]` at `open` (≡ `@FootnoteMark[label: "mark"]{}`; unguarded — `text[^1]` glues,
    /// Markdown-style), or — with a glued `:` under the positional line-start gate
    /// ([`Self::colon_trigger_live`], the same gate as `@head:`) — a `[^label]: body` footnote
    /// *text* definition (≡ `@FootnoteText[label: "label"]: body`, the colon-body extent
    /// machinery verbatim). `label_span` comes from the caller's `footnote_sugar_at` match.
    fn parse_footnote_sugar(&mut self, open: u32, label_span: Span, limit: u32) {
        let after_rbrack = label_span.end + 1; // past `]`
        let colon_glued =
            after_rbrack < limit && byte_at(self.source_text, after_rbrack) == Some(b':');
        if colon_glued && self.colon_trigger_live(open) {
            self.parse_footnote_text(open, label_span, after_rbrack + 1);
        } else {
            self.push_doc_state(
                NotaDocStateKind::FootnoteMark,
                Span::new(open, after_rbrack),
                label_span,
                self.ast.vec(),
            );
            self.nota_seek_markup(after_rbrack);
        }
    }

    /// The `[^label]: body` footnote-text body: the colon-body extent machinery verbatim
    /// (mirrors [`Self::parse_colon_body`] — rest of line + lines indented past the opening
    /// line, first-line brace clip in a braced body, clamped to a bounded frame's end).
    fn parse_footnote_text(&mut self, open: u32, label_span: Span, colon_end: u32) {
        let head_line_indent = line_indent_of(self.source_text, open);
        let (clip_at_brace, bound) = match self.nota_top_region() {
            NotaRegion::Markup { mode, .. } => (matches!(mode, BodyMode::Body), mode.bound()),
            // Unreachable (the positional gate requires a Markup top); clip defensively.
            NotaRegion::Js | NotaRegion::Raw => (false, None),
        };
        let (body_start, mut body_end) =
            colon_block_extent(self.source_text, colon_end, head_line_indent, clip_at_brace);
        if let Some(end) = bound {
            body_end = body_end.min(end);
        }
        let children = self.collect_markup_range(body_start.min(body_end), body_end);
        self.push_doc_state(
            NotaDocStateKind::FootnoteText,
            Span::new(open, body_end),
            label_span,
            children,
        );
        self.nota_seek_markup(body_end);
    }
}

// ===============================================================================================
// Raw spans: verbatim `|{ … }|`, code `` `…` ``/fenced, math `$…$` inline / `$$⏎…⏎$$` fence. All
// share ONE content model — raw text runs interleaved with `|@`-armed `@`-forms. Each extent is a
// pure pre-scan over the raw source (`lex_code_span` / `lex_math_span` / `verbatim_boundary`); a
// SECOND bounded scan (`armed_boundary`) then walks the fixed content extent for `|@`, each of
// which re-enters Nota as a *sibling* child parsed under a `Raw` region (its tail parks; the scan
// resumes from `prev_token_end`). A bare `@` is literal. Content lowers to `String.raw` templates.
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse `@head|{ … }|` — a verbatim-body element, with `props` already collected (empty for
    /// the bare `@head|{…}|` form; from preceding `[props]` groups when reached via
    /// [`Self::parse_element`]). `body_start` points just past the opening `|{`.
    fn parse_verbatim_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        props: NotaProps<'a>,
        body_start: u32,
    ) -> NotaVerbatim<'a> {
        let (parts, after) = self.collect_verbatim_body(body_start);
        let span = Span::new(span_start, after);
        let tag = self.head_to_tag(head);
        let element = self.ast.nota_verbatim(span, tag, props, parts);
        self.resume_at(after);
        element
    }

    /// Collect a verbatim body from `start` (just past `|{`): raw runs bounded by
    /// [`verbatim_boundary`], with each `|@` re-arming one Nota `@`-form as a sibling child; ends
    /// at `}|`. Returns `(parts, after)` where `after` is one past the closing `}|` (or EOF, with
    /// a diagnostic).
    fn collect_verbatim_body(&mut self, start: u32) -> (ArenaVec<'a, NotaVerbatimPart<'a>>, u32) {
        let mut children = self.ast.vec();
        // The clamped scan view (real length, or a bounded raw span's extent when this verbatim is
        // itself a `|@`-armed form) caps the `}|` scan — a close past the clamp is unreachable, so
        // the body reports `Eof` (overruns) rather than parking past the clamp.
        let source = self.nota_scan_source();
        // Drop a single leading newline right after `|{` (the Scribble `{`-newline rule);
        // otherwise the body is fully raw — no indent strip, no trimming.
        let start = if byte_at(source, start) == Some(b'\n') { start + 1 } else { start };
        let mut run_start = start;
        loop {
            let (run_end, boundary) = verbatim_boundary(source, run_start);
            self.push_raw_run(&mut children, run_start, run_end);
            match boundary {
                VerbatimBoundary::Close { after } => return (children, after),
                VerbatimBoundary::ArmedAt { at } => {
                    run_start = self.push_armed_form(&mut children, at);
                }
                VerbatimBoundary::Eof => {
                    let span = Span::new(start, run_end);
                    self.set_fatal_error(diagnostics::nota_unterminated_verbatim(span));
                    return (children, run_end);
                }
            }
        }
    }

    /// Collect the raw content `[content_start, content_end)` of a *bounded* raw span (inline/block
    /// code or math) into interleaved parts: raw runs plus each `|@`-armed `@`-form. The extent is
    /// fixed by the caller's pure pre-scan, so an armed form whose parse overruns it
    /// (`prev_token_end > content_end` — it ate the span's close) is a fatal diagnostic. Shares the
    /// armed-form machinery with verbatim ([`Self::push_armed_form`]).
    fn collect_bounded_armed(
        &mut self,
        content_start: u32,
        content_end: u32,
    ) -> ArenaVec<'a, NotaVerbatimPart<'a>> {
        let mut parts = self.ast.vec();
        let mut run_start = content_start;
        loop {
            let (run_end, boundary) = armed_boundary(self.source_text, run_start, content_end);
            self.push_raw_run(&mut parts, run_start, run_end);
            match boundary {
                ArmedBoundary::Bound => return parts,
                ArmedBoundary::ArmedAt { at } => {
                    // Clamp the armed form's parse to the span's fixed extent. Unlike verbatim
                    // (`}|`-closed, and `}`/`|` are not identifier bytes), a code/math close can be
                    // an identifier-continue byte — `$` is, so `|@energy$` would otherwise lex the
                    // head as `energy$`, eating the close. The clamp makes `content_end` an `Eof`
                    // for the inner parse (the same device `%` statements use).
                    run_start = self
                        .with_source_end_bound(content_end, |p| p.push_armed_form(&mut parts, at));
                    // `fatal_error`, not `has_fatal_error()`: the armed form's exit parks (an
                    // `Undetermined` current token), which `has_fatal_error()` also reports — only a
                    // real diagnostic from the inner parse should abort the run.
                    if self.fatal_error.is_some() {
                        return parts;
                    }
                    // The clamp bounds the *lexer*, but an armed form whose own extent comes from a
                    // pure source scan — a nested verbatim `}|`, another raw span — can still run
                    // past `content_end` (the scan ignores the clamp). That is malformed: the inner
                    // form swallowed this span's close.
                    if run_start > content_end {
                        let span = Span::new(at - 1, run_start);
                        self.set_fatal_error(diagnostics::nota_armed_form_overruns_span(span));
                        return parts;
                    }
                }
            }
        }
    }

    /// Parse one `|@`-armed `@`-form at `at` (the `@`) under a [`NotaRegion::Raw`] region, pushing
    /// it as a sibling part, and return the offset the raw scan resumes from (the park's
    /// `prev_token_end`). Under `Raw` every exit parks — no lex — so the following bytes stay the
    /// raw scan's, not JS. The single re-entry point shared by verbatim bodies and bounded spans.
    fn push_armed_form(&mut self, parts: &mut ArenaVec<'a, NotaVerbatimPart<'a>>, at: u32) -> u32 {
        self.nota_seek_to(at);
        debug_assert!(self.at(Kind::At), "armed `|@` not at `@`");
        let (form, _) = self.enter_region(NotaRegion::Raw, Self::parse_nota_form);
        parts.push(NotaVerbatimPart::from(form));
        self.prev_token_end
    }

    /// The source view the pure raw-scans (code / math / emphasis / verbatim closes) see: the whole
    /// source, clamped to the lexer's current source end. Normally the full source; inside a bounded
    /// raw span's `|@`-armed parse it is a prefix bounded to the span's extent, so a nested close
    /// past that extent is unreachable — the span overruns / errors — rather than seeking the lexer
    /// past the clamp (which would be a fatal invariant break). A prefix preserves absolute offsets.
    fn nota_scan_source(&self) -> &'a str {
        &self.source_text[..self.lexer.nota_source_end() as usize]
    }

    /// Push the raw slice `[from, to)` as a raw part (skipped if empty).
    fn push_raw_run(&self, children: &mut ArenaVec<'a, NotaVerbatimPart<'a>>, from: u32, to: u32) {
        if to <= from {
            return;
        }
        let raw: &'a str = &self.source_text[from as usize..to as usize];
        let text = self.ast.nota_text(Span::new(from, to), raw);
        children.push(NotaVerbatimPart::Raw(self.ast.alloc(text)));
    }

    /// Parse a code span at `tick_off`, or — with no valid close — emit the opening backtick run
    /// as literal text. The content extent is collected as raw runs + `|@`-armed forms.
    fn parse_code_or_literal(&mut self, tick_off: u32) {
        match lex_code_span(self.nota_scan_source(), tick_off) {
            CodeScan::Code { span, is_block, lang, content, resume } => {
                let lang = lang.map(|l| self.ast.str(self.ast.allocator.alloc_str(l)));
                let parts = self.collect_bounded_armed(content.start, content.end);
                let element = self.ast.nota_code(span, lang, is_block, parts);
                self.push_nota_item(NotaChild::Code(self.ast.alloc(element)));
                // A park (armed form) leaves an `Undetermined` token that `has_fatal_error()` would
                // report; only a real overrun/inner diagnostic (`fatal_error`) suppresses the resume.
                if self.fatal_error.is_none() {
                    self.nota_seek_markup(resume);
                }
            }
            CodeScan::Literal { resume } => {
                self.push_text(tick_off, resume);
                self.nota_seek_markup(resume);
            }
        }
    }

    /// Parse a math span at `dollar_off`, or — with no valid close — emit the opening `$`-run as
    /// literal text. Mirrors [`Self::parse_code_or_literal`]: the content extent is collected as
    /// raw runs + `|@`-armed forms; `is_block` is the display fence.
    fn parse_math_or_literal(&mut self, dollar_off: u32) {
        match lex_math_span(self.nota_scan_source(), dollar_off) {
            MathScan::Math { span, is_block, content, resume } => {
                let parts = self.collect_bounded_armed(content.start, content.end);
                let element = self.ast.nota_math(span, is_block, parts);
                self.push_nota_item(NotaChild::Math(self.ast.alloc(element)));
                // See `parse_code_or_literal`: guard on `fatal_error`, not the park-sensitive
                // `has_fatal_error()`.
                if self.fatal_error.is_none() {
                    self.nota_seek_markup(resume);
                }
            }
            MathScan::Literal { resume } => {
                self.push_text(dollar_off, resume);
                self.nota_seek_markup(resume);
            }
        }
    }
}

// ===============================================================================================
// Document mode + colon/block sugar
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Parse the whole file body: top-level markup siblings interleaved with `%`/`%%%` statements,
    /// all kept as faithful children (the lowering routes statements: hoist / Doc prelude /
    /// document-local component bindings).
    fn parse_document_body(&mut self) -> NotaDocument<'a> {
        // Skip a leading UTF-8 BOM so it is not collected as text (offsets after it are unchanged).
        let start = if self.source_text.starts_with('\u{feff}') { 3u32 } else { 0 };

        // A file opening with line-start constructs is handled by `collect_markup`'s entry arming
        // (a body/range start is a line start — the document body included).
        self.nota_seek_markup(start);

        let (_, items) = self.collect_markup(BodyMode::Document, start);

        let span = Span::new(0, self.source_text.len() as u32);
        self.ast.nota_document(span, items)
    }

    /// `@head:` colon/block sugar → an element whose body is the rest of the line plus following
    /// lines indented past the `@head:` line. Leading `|` lines of the body supply `[…]` props.
    /// `props` holds any `[props]` groups threaded from the head (empty for a bare `@head:`; from
    /// [`Self::parse_element`] for `@head[props]: body` — props compose with a colon body); the
    /// `|`-line props append.
    /// Entered with `:` as the current token.
    fn parse_colon_body(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        mut props: NotaProps<'a>,
    ) -> NotaElement<'a> {
        debug_assert!(self.at(Kind::Colon), "colon sugar entered not at `:`");
        // The positional gate ([`Self::colon_trigger_live`]) classifies a `:` as `Colon` only under
        // a `Markup` top at a line start, so colon sugar is never entered in a `Js`/`Raw` host — the
        // line-oriented body ("rest of the line + following indented lines") is only well-defined in
        // a markup body. (Was a runtime diagnostic; the gate makes it unreachable.)
        debug_assert!(
            matches!(self.nota_top_region(), NotaRegion::Markup { .. }),
            "colon sugar entered outside a markup body"
        );
        let colon_end = self.cur_token().end();
        let head_line_indent = line_indent_of(self.source_text, span_start);

        // Clip the first line at a depth-0 `}` only inside an element/control body, where that `}`
        // is the enclosing closer (never in a Document / Bounded host). A bounded host additionally
        // clips the whole body at its end: an emphasis / heading / list-item / colon body that
        // contains a `@head:` child must not let it escape the range — `*@a: bar* rest`.
        let (clip_at_brace, bound) = match self.nota_top_region() {
            NotaRegion::Markup { mode, .. } => (matches!(mode, BodyMode::Body), mode.bound()),
            // Unreachable per the debug_assert above; interpolate defensively rather than panic.
            NotaRegion::Js | NotaRegion::Raw => (false, None),
        };
        let (body_src_start, mut body_src_end) =
            colon_block_extent(self.source_text, colon_end, head_line_indent, clip_at_brace);
        if let Some(end) = bound {
            body_src_end = body_src_end.min(end);
        }

        let items = self.collect_colon_body(body_src_start, body_src_end, &mut props);

        let span = Span::new(span_start, body_src_end);
        self.resume_at(body_src_end);
        let tag = self.head_to_tag(head);
        // A colon body cannot leave an unclosed `[props]` group (it clips to the line/block), so no
        // recovery anchor applies here — but consume any pending one to keep the field in step.
        let props_recovery = self.nota_prop_anchor.take();
        self.ast.nota_element(span, tag, props, items, /* is_colon */ true, props_recovery)
    }

    /// Collect the colon-sugar body over `[start, end)`: leading `|` lines (continuation lines
    /// whose first non-whitespace is `|`) append prop groups into `props` (which already holds any
    /// `[props]` groups threaded from the head); the rest is the markup body.
    fn collect_colon_body(
        &mut self,
        start: u32,
        end: u32,
        props: &mut NotaProps<'a>,
    ) -> NotaChildren<'a> {
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
        self.collect_markup_range(body_range_start, end)
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

type NotaChildren<'a> = ArenaVec<'a, NotaChild<'a>>;
type NotaProps<'a> = ArenaVec<'a, NotaProp<'a>>;

/// The host region an inner `@`-form's tail resumes into (Axis 2 — orthogonal to [`BodyMode`],
/// which is the *collection* semantics of a markup body, Axis 1). The parser owns a stack of these
/// in [`NotaParserState`]; the top drives [`ParserImpl::resume_at`] and gates markup-child pushes.
enum NotaRegion<'a> {
    /// Collecting a markup body with these semantics; a form's tail resumes by markup-lexing.
    /// The only region [`ParserImpl::push_nota_item`] may push a child into. `start` is the body's
    /// content start (one past `{`, the post-BOM document start, or a bounded range's start): it
    /// exists for the positional colon-sugar check ([`ParserImpl::colon_trigger_live`]), which
    /// counts a markup body's own start as a line start.
    Markup { mode: BodyMode, start: u32, items: NotaChildren<'a> },
    /// An embedded-JS island (an expression-position form, a `k: @form` prop value): a form's tail
    /// resumes by JS-lexing.
    Js,
    /// A raw scan owns the cursor by offset (the tail after a verbatim `|@` armed form): a form's
    /// tail is *parked* — no lex — and the scan re-seeks itself from `prev_token_end`.
    Raw,
}

#[derive(Default)]
pub struct NotaParserState<'a> {
    regions: Vec<NotaRegion<'a>>,
    /// Markup comments (`//` / `/* … */`) in source order — trivia, carried onto the document
    /// Program's comments vec by [`ParserImpl::wrap_document_program`].
    comments: Vec<Comment>,
}

impl<'a, C: Config> ParserImpl<'a, C> {
    fn enter_region<T>(
        &mut self,
        region: NotaRegion<'a>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> (T, NotaRegion<'a>) {
        self.state.nota.regions.push(region);
        let t = f(self);
        let region = self.state.nota.regions.pop().expect("enter_region: region underflow");
        (t, region)
    }

    fn nota_top_region(&self) -> &NotaRegion<'a> {
        self.state.nota.regions.last().expect("Nota region stack is empty")
    }

    /// The positional colon-sugar gate (notation.md §Colon & block sugar): the `:` glued to an
    /// `@head:` at `span_start`
    /// (the form's `@`) is an element trigger iff BOTH the form is a markup-body child (the top
    /// region is `Markup`, never a `Js` island or a `Raw` scan) AND its `@` sits at a line start
    /// modulo whitespace — walking back over spaces/tabs reaches file offset 0, a `\n`, or the top
    /// markup frame's body start. Everywhere the gate is dead the head falls back to interpolation
    /// and the trailing `: …` is literal text. Computed once per form (at `parse_nota_form` entry)
    /// and threaded into both trigger consumers so they agree.
    fn colon_trigger_live(&self, span_start: u32) -> bool {
        match self.nota_top_region() {
            NotaRegion::Markup { start, .. } => {
                at_line_start_in_frame(self.source_text, span_start, *start)
            }
            NotaRegion::Js | NotaRegion::Raw => false,
        }
    }

    /// The trigger glued to a head at `after`, with a dead colon (positional rule, `colon_live`
    /// false) demoted to [`MarkupTrigger::None`] so the head interpolates and the `:` stays literal.
    /// The one place both trigger consumers ([`Self::commit_head`] and the hyphen-extension check in
    /// [`Self::parse_nota_head`]) route through, so they classify identically.
    fn effective_trigger(&self, after: u32, colon_live: bool) -> MarkupTrigger {
        match markup_trigger(self.source_text, after) {
            MarkupTrigger::Colon if !colon_live => MarkupTrigger::None,
            trigger => trigger,
        }
    }

    /// The collection semantics of the markup body being collected. Only reachable while the top
    /// region is `Markup` — every `collect_markup` caller pushes one first.
    fn nota_body_mode(&self) -> BodyMode {
        match self.nota_top_region() {
            NotaRegion::Markup { mode, .. } => *mode,
            NotaRegion::Js | NotaRegion::Raw => {
                panic!("nota_body_mode called with a non-Markup top region")
            }
        }
    }

    /// Append a markup child to the top region, which MUST be `Markup`. A push under a `Js` / `Raw`
    /// region is a routing bug (a child produced where none belongs) — fail loudly.
    fn push_nota_item(&mut self, item: NotaChild<'a>) {
        match self.state.nota.regions.last_mut() {
            Some(NotaRegion::Markup { items, .. }) => items.push(item),
            _ => {
                panic!("push_nota_item: top region is not Markup — markup child routed into JS/raw")
            }
        }
    }
}

#[cfg(test)]
mod recover_tests {
    use oxc_allocator::Allocator;
    use oxc_span::SourceType;

    use crate::Parser;

    /// Recover-parse `source`; return the diagnostic messages (source-ordered as collected).
    fn recover_errors(source: &str) -> Vec<String> {
        let allocator = Allocator::default();
        let r = Parser::new(&allocator, source, SourceType::nota()).parse_nota_document_recover();
        r.errors.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn well_formed_input_recovers_without_errors() {
        assert!(recover_errors("@p{hi}\n").is_empty());
        assert!(recover_errors("Hi @em{x} and @Aside[k: 1]{y}.\n").is_empty());
    }

    #[test]
    fn unclosed_props_group_reports_expected_bracket() {
        let errs = recover_errors("@a[");
        assert_eq!(errs.len(), 1, "one diagnostic: {errs:?}");
        assert!(errs[0].contains('`') && errs[0].contains(']'), "mentions `]`: {errs:?}");
    }

    #[test]
    fn unclosed_body_reports_expected_brace() {
        let errs = recover_errors("@p{unterminated");
        assert_eq!(errs.len(), 1, "one diagnostic: {errs:?}");
        assert!(errs[0].contains('}'), "mentions `}}`: {errs:?}");
    }

    #[test]
    fn bare_at_reports_unexpected() {
        let errs = recover_errors("@");
        assert_eq!(errs.len(), 1, "one diagnostic: {errs:?}");
    }

    #[test]
    fn mid_document_unclosed_bracket_still_recovers() {
        // A mid-document `@a[` swallows to EOF; recovery still yields a diagnostic + a tree.
        let errs = recover_errors("before\n\n@a[");
        assert_eq!(errs.len(), 1, "one diagnostic: {errs:?}");
    }
}
