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
//! The re-lex seam (`advance_for_markup_text`) mirrors JSX's `advance_for_jsx_child`: after a markup
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
    ParserConfig as Config, ParserImpl, diagnostics, error_handler::FatalError, lexer::Kind,
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
        let markup = self.parse_nota_form(false);
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
    pub(crate) fn parse_nota_form(&mut self, in_body: bool) -> NotaMarkup<'a> {
        let span_start = self.start_span();
        self.expect(Kind::At);

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
                let Some(head) = self.parse_nota_head() else { return self.unexpected() };
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
                        let e = self.parse_colon_element(span_start, head, in_body);
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
                && let Some(ext_end) = self.scan_hyphenated_tag_tail(span.end)
                && !matches!(self.peek_markup_trigger(ext_end), MarkupTrigger::None)
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

    /// Scan a custom-element name tail starting at `at`: one or more `-`-joined runs of identifier
    /// characters (`@my-widget`, `@x-y-z`). Returns the offset past the tail, or `None` if `at` is
    /// not a `-` directly followed by an identifier character. Pure raw-source scan (the JS lexer
    /// stops a bare identifier at `-`, so the tail is read here over the source bytes).
    fn scan_hyphenated_tag_tail(&self, at: u32) -> Option<u32> {
        let bytes = self.source_text.as_bytes();
        let mut i = at as usize;
        let mut consumed = false;
        while bytes.get(i) == Some(&b'-')
            && bytes.get(i + 1).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            i += 1; // the `-`
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            consumed = true;
        }
        consumed.then_some(i as u32)
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
                // Re-lex from the current position (right after the last `]`).
                self.advance_for_markup_text();
            }
            (self.ast.vec(), end)
        };

        let span = Span::new(span_start, end);
        let tag = self.head_to_tag(head);
        self.ast.nota_element(span, tag, props, children)
    }

    /// `@head:` colon/block sugar. Handled in the document/colon module.
    fn parse_colon_element(
        &mut self,
        span_start: u32,
        head: NotaHead<'a>,
        in_body: bool,
    ) -> NotaElement<'a> {
        self.parse_colon_body(span_start, head, in_body)
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
    fn peek_markup_trigger(&self, after: u32) -> MarkupTrigger {
        match self.byte_at(after) {
            Some(b'{') => MarkupTrigger::Brace,
            Some(b'[') => MarkupTrigger::Bracket,
            Some(b':') => MarkupTrigger::Colon,
            // `|{` opens a verbatim body; a lone `|` is not a trigger.
            Some(b'|') if self.byte_at(after + 1) == Some(b'{') => MarkupTrigger::Verbatim,
            _ => MarkupTrigger::None,
        }
    }

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
        let trigger = self.peek_markup_trigger(head.end);
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
                    self.advance_for_markup_text();
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
        self.advance_for_markup_text(); // switch the lexer into markup-body mode

        let mut items: Vec<BodyItem<'a>> = Vec::new();
        let mut depth = 0u32; // balanced-brace depth inside the body
        let close = self.collect_markup(&mut items, &mut depth, /* document */ false);
        match close {
            MarkupClose::Curly { end } => {
                // Body close `}`. Consume it, resuming markup text iff this element is a child.
                if in_body {
                    self.advance_for_markup_text();
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
    /// pass runs at lowering time). Text runs become [`NotaText`] children (spans are not tracked for
    /// literal text — it is never a navigation target).
    fn body_items_to_children(&self, items: Vec<BodyItem<'a>>) -> ArenaVec<'a, NotaChild<'a>> {
        let mut out = self.ast.vec_with_capacity(items.len());
        for item in items {
            out.push(match item {
                BodyItem::Text(t) => {
                    let text = self.ast.nota_text(Span::empty(0), t);
                    NotaChild::Text(self.ast.alloc(text))
                }
                BodyItem::Child(child) => child,
            });
        }
        out
    }

    /// Parse a run of consecutive `%`/`%%%` statement lines from `line_start`, pushing each parsed
    /// statement as a `NotaChild::Statement`. Returns the offset of the first non-statement line.
    /// Used for both document and element bodies (full-document deferral; lowering routes vs IIFEs).
    fn collect_statements(&mut self, line_start: u32, items: &mut Vec<BodyItem<'a>>) -> u32 {
        let mut at = line_start;
        while let Some((content, is_fence)) = self.statement_kind(at) {
            let end = if is_fence {
                self.collect_fence_statements(content, items)
            } else {
                self.nota_seek_to(content);
                let stmt =
                    self.parse_statement_list_item(crate::context::StatementContext::StatementList);
                let e = self.prev_token_end;
                self.push_statement(items, stmt);
                e
            };
            at = self.next_line_start(end);
            if !self.is_statement_line(at) {
                break;
            }
        }
        at
    }

    /// Parse the inner statements of a `%%%`…`%%%` fence (from `inner_start`), pushing each as a
    /// `NotaChild::Statement`. Returns the offset past the closing fence.
    fn collect_fence_statements(&mut self, inner_start: u32, items: &mut Vec<BodyItem<'a>>) -> u32 {
        let (inner_end, after_fence) = self.find_fence_close(inner_start);
        self.nota_seek_to(inner_start);
        while self.prev_token_end < inner_end && !self.at(Kind::Eof) && !self.has_fatal_error() {
            if self.cur_token().start() >= inner_end {
                break;
            }
            let stmt =
                self.parse_statement_list_item(crate::context::StatementContext::StatementList);
            self.push_statement(items, stmt);
        }
        after_fence
    }

    /// Push a parsed `%`/`%%%` statement as a `NotaChild::Statement` body item.
    fn push_statement(&self, items: &mut Vec<BodyItem<'a>>, stmt: Statement<'a>) {
        let span = stmt.span();
        let node = self.ast.nota_statement(span, stmt);
        items.push(BodyItem::Child(NotaChild::Statement(self.ast.alloc(node))));
    }

    /// The core markup-collection loop, shared by element bodies and (with `document=true`) the
    /// whole-file body. Collects [`BodyItem`]s (text runs / nested `@`-forms), tracking balanced
    /// `{…}` braces as literal text and emitting `\n` runs verbatim into the text buffer (the
    /// Scribble whitespace pass owns line handling). Entered with the current token already lexed
    /// as the first markup-text run.
    ///
    /// Returns how the body terminated. `document=true` collects to EOF (`}` at depth 0 is literal,
    /// not a close); otherwise a depth-0 `}` closes the body.
    fn collect_markup(
        &mut self,
        items: &mut Vec<BodyItem<'a>>,
        depth: &mut u32,
        document: bool,
    ) -> MarkupClose {
        loop {
            if self.has_fatal_error() {
                return MarkupClose::Eof;
            }
            match self.cur_kind() {
                Kind::MarkupText => {
                    let token = self.cur_token();
                    let text = self.token_source(&token);
                    if !text.is_empty() {
                        items.push(BodyItem::Text(text));
                    }
                    // The run stopped *at* (unconsumed) the terminator: peek it via the raw source.
                    let term_off = token.end();
                    match self.byte_at(term_off) {
                        Some(b'\n') => {
                            // Line boundary. Keep the `\n` as literal text; resume on the next line.
                            items.push(BodyItem::Text("\n"));
                            let next_line = term_off + 1;
                            if self.is_statement_line(next_line) {
                                // `%`/`%%%` statements collect as faithful `NotaChild::Statement`
                                // children — document and element bodies alike (full-document
                                // deferral). Lowering routes document-level statements (import/
                                // export/F1 hoist + Doc prelude) and wraps element-body ones in a
                                // suffix-scoping IIFE.
                                let resume = self.collect_statements(next_line, items);
                                self.nota_seek_markup(resume);
                                continue;
                            }
                            // Line-start markup sugar: lists (`-`/`+`/`N.`) and headings
                            // (`#`). Only at brace depth 0 (a balanced `{…}` is literal body text).
                            if *depth == 0 && self.list_marker_at(next_line).is_some() {
                                let (els, resume) = self.parse_list(next_line);
                                for e in els {
                                    items.push(BodyItem::Child(NotaChild::ListItem(
                                        self.ast.alloc(e),
                                    )));
                                }
                                self.nota_seek_markup(resume);
                                continue;
                            }
                            if *depth == 0
                                && let Some((heading, h_end)) = self.try_heading(next_line)
                            {
                                items.push(BodyItem::Child(NotaChild::Heading(
                                    self.ast.alloc(heading),
                                )));
                                self.nota_seek_markup(h_end);
                                continue;
                            }
                            self.nota_seek_markup(next_line);
                        }
                        Some(b'{') => {
                            *depth += 1;
                            items.push(BodyItem::Text("{"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'}') if *depth > 0 => {
                            *depth -= 1;
                            items.push(BodyItem::Text("}"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'}') if !document => {
                            // Body close. Lex the `}` as a JS token so the caller can consume it;
                            // leave it as the current token and report its end (one past `}`).
                            self.bump_any();
                            return MarkupClose::Curly { end: self.cur_token().end() };
                        }
                        Some(b'}') => {
                            // Document mode: a depth-0 `}` is literal text.
                            items.push(BodyItem::Text("}"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'@') => {
                            self.bump_any(); // lex `@`
                            let child = self.parse_nota_form(true);
                            items.push(BodyItem::Child(markup_to_child(child)));
                        }
                        Some(m @ (b'*' | b'_')) => {
                            // Emphasis sigil. Marker iff NOT intra-word (Typst rule); else
                            // literal. An *opening* marker recurses into the emphasis body.
                            if self.can_open_emphasis(term_off, m) {
                                self.parse_emphasis(m, term_off, items);
                            } else {
                                self.push_literal_byte(items, m);
                                self.nota_seek_markup(term_off + 1);
                            }
                        }
                        Some(b'\\') => {
                            // General backslash escape: `\<c>` → literal `<c>`, `\` dropped.
                            let resume = self.push_escape(items, term_off);
                            self.nota_seek_markup(resume);
                        }
                        Some(b'`') => {
                            // Code: inline `` `…` `` / fenced ```` ```…``` ````.
                            self.parse_code_or_literal(items, term_off);
                        }
                        Some(b'$') => {
                            // Math: `$…$` / `$$…$$`.
                            self.parse_math_or_literal(items, term_off);
                        }
                        Some(b'|') => {
                            // A bare `|` in a markup body is literal (the `|{`/`|@` forms are handled
                            // by the head switch / inside verbatim bodies, never here).
                            items.push(BodyItem::Text("|"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        _ => {
                            // EOF.
                            return MarkupClose::Eof;
                        }
                    }
                }
                Kind::At => {
                    let child = self.parse_nota_form(true);
                    items.push(BodyItem::Child(markup_to_child(child)));
                }
                Kind::RCurly if *depth == 0 && !document => {
                    return MarkupClose::Curly { end: self.cur_token().end() };
                }
                Kind::Eof => return MarkupClose::Eof,
                _ => {
                    // Any non-markup token here means lexing resumed in JS mode (after an `@`-form
                    // whose trailing context was not markup, e.g. in expression position). Re-enter
                    // markup from the current position.
                    self.advance_for_markup_text();
                }
            }
        }
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
                let markup = self.parse_nota_form(false);
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
        match self.peek_else(close_end) {
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
        self.advance_for_markup_text(); // switch the lexer into markup-body mode

        let mut items: Vec<BodyItem<'a>> = Vec::new();
        let mut depth = 0u32;
        match self.collect_markup(&mut items, &mut depth, /* document */ false) {
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

    /// Scan the raw source from `close_end` (one past a branch's `}`) for an `else`/`else if`
    /// contextual continuation. A continuation requires `else` to be the *next token* with **no
    /// blank line** between (≥2 newlines in the gap breaks it); `\else` forces a literal (→ no
    /// continuation). Returns where to resume parsing the continuation, or [`ElsePeek::None`].
    fn peek_else(&self, close_end: u32) -> ElsePeek {
        let bytes = self.source_text.as_bytes();
        let mut i = close_end as usize;
        let mut newlines = 0u32;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b'\r' => i += 1,
                b'\n' => {
                    newlines += 1;
                    if newlines >= 2 {
                        return ElsePeek::None; // blank line: continuation broken
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        // `\else` — an escaped literal, not a continuation.
        if i < bytes.len() && bytes[i] == b'\\' {
            return ElsePeek::None;
        }
        // Match the keyword `else` followed by a word boundary.
        if !matches_keyword(bytes, i, b"else") {
            return ElsePeek::None;
        }
        // After `else`, skip whitespace and look for `if` (→ `else if`) or `{` (→ `else {`).
        let mut j = i + 4;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        if matches_keyword(bytes, j, b"if") {
            ElsePeek::ElseIf { if_offset: j as u32 }
        } else if j < bytes.len() && bytes[j] == b'{' {
            ElsePeek::Else { brace_offset: j as u32 }
        } else {
            // `else` not followed by `if`/`{` — malformed `else`; treat as no continuation so the
            // text surfaces (and the misuse is caught by the un-lowered `else` / scope checks).
            ElsePeek::None
        }
    }
}

/// A list marker found at a line start.
struct ListMarker {
    /// `true` for an ordered marker (`+` / `N.`); `false` for a bullet (`-`).
    ordered: bool,
    /// Offset of the marker's first char (= the line's first non-whitespace; the item's indent).
    indent: u32,
    /// Offset where the item body begins (just past the marker and its one separating space).
    body_col: u32,
}

/// The result of scanning for an `else`/`else if` contextual continuation after an `@if` branch.
enum ElsePeek {
    /// No continuation (the alternate is `null`).
    None,
    /// `else if (…) {…}` — resume parsing at the `if` keyword (`if_offset`).
    ElseIf { if_offset: u32 },
    /// `else {…}` — resume parsing at the body `{` (`brace_offset`).
    Else { brace_offset: u32 },
}

/// Is `c` a "wordy" char for the emphasis word-boundary rule (Typst `in_word`): alphanumeric, with
/// CJK scripts excluded (so CJK text gets emphasis without spaces)? `None` (start/end of source) is
/// not wordy, so a marker at a boundary opens/closes. (CJK exclusion is approximated by Unicode
/// block ranges — we have no `unicode-script` dep; ASCII + common Latin/Greek/Cyrillic are the
/// realistic cases and classify exactly.)
fn is_wordy(c: Option<char>) -> bool {
    match c {
        None => false,
        Some(c) => c.is_alphanumeric() && !is_cjk(c),
    }
}

/// Approximate the CJK scripts Typst excludes from `in_word` (Han/Hiragana/Katakana/Hangul) by
/// codepoint range — enough that CJK prose gets emphasis without surrounding spaces.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF        // Hiragana + Katakana
        | 0x3400..=0x4DBF      // CJK Ext A
        | 0x4E00..=0x9FFF      // CJK Unified
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compat
        | 0x20000..=0x2FA1F    // CJK Ext B+ / compat supplement
    )
}

/// Does `bytes[at..]` begin with the keyword `kw` followed by a word boundary (not an
/// identifier-continue char)? Used to match the contextual `else`/`else if`/`if` keywords over the
/// raw source without lexing.
fn matches_keyword(bytes: &[u8], at: usize, kw: &[u8]) -> bool {
    if at + kw.len() > bytes.len() || &bytes[at..at + kw.len()] != kw {
        return false;
    }
    // Word boundary: the following byte must not continue an identifier.
    match bytes.get(at + kw.len()) {
        None => true,
        Some(b) => !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$'),
    }
}

// ===============================================================================================
// Markup sugar (emphasis `*`/`_`, headings `#`, lists `-`/`+`/`N.`). Each lowers to an ordinary
// element; the runtime `decode`/`struct` does the grouping (the reader emits the flat
// per-line/per-span sentinels). Escaped `\* \_ \# \- \+` are the literal char.
// ===============================================================================================

impl<'a, C: Config> ParserImpl<'a, C> {
    /// Is the `*`/`_` at `marker_off` (raw offset) a significant emphasis *marker*, or literal?
    ///
    /// Typst's rule (`references/typst/.../lexer.rs` `in_word`): a `*`/`_` is literal **only**
    /// intra-word — when *both* the preceding and following chars are "wordy" (alphanumeric, with
    /// CJK excluded). Otherwise it is a marker. So `my_var_name` keeps its `_` literal, while
    /// `_italic_` and `*a _b_ c*` use them as markers. The open-vs-close determination is the
    /// matcher's job ([`Self::find_emphasis_close`]); this only gates marker-vs-literal.
    fn is_emphasis_marker(&self, marker_off: u32) -> bool {
        // An escaped marker (`\*`/`\_`, odd run of preceding `\`) is literal (the `\`-stripping
        // itself happens elsewhere — here we only suppress the marker).
        if self.is_escaped(marker_off) {
            return false;
        }
        let prev = self.char_before(marker_off);
        let next = self.char_at(marker_off + 1);
        !(is_wordy(prev) && is_wordy(next))
    }

    /// Can a `*`/`_` at `off` **open** an emphasis span? Beyond being a marker (not intra-word, not
    /// escaped — [`Self::is_emphasis_marker`]), Typst requires the opener to be immediately followed
    /// by *content*: a non-whitespace char that is not another copy of the same marker. So `* foo`
    /// (space after), and marker runs `**`/`***`/`****` do **not** open — they stay literal instead
    /// of producing empty or garbled spans.
    fn can_open_emphasis(&self, off: u32, marker: u8) -> bool {
        if !self.is_emphasis_marker(off) {
            return false;
        }
        match self.source_text.as_bytes().get(off as usize + 1) {
            Some(&b) => !b.is_ascii_whitespace() && b != marker,
            None => false,
        }
    }

    /// Can a `*`/`_` at `off` **close** an emphasis span? It must be a marker and immediately
    /// *preceded* by content (a non-whitespace byte), so `foo *` (space before the marker) does not
    /// close (Typst). Non-emptiness of the span is enforced by the caller ([`Self::find_emphasis_close`]).
    fn can_close_emphasis(&self, off: u32) -> bool {
        if !self.is_emphasis_marker(off) {
            return false;
        }
        match (off as usize).checked_sub(1).and_then(|p| self.source_text.as_bytes().get(p)) {
            Some(&b) => !b.is_ascii_whitespace(),
            None => false,
        }
    }

    /// Is the byte at `off` preceded by an *odd* run of backslashes (i.e. escaped)?
    fn is_escaped(&self, off: u32) -> bool {
        let bytes = self.source_text.as_bytes();
        let mut n = 0usize;
        let mut i = off as usize;
        while i > 0 && bytes[i - 1] == b'\\' {
            n += 1;
            i -= 1;
        }
        n % 2 == 1
    }

    /// Parse an emphasis span opened by `marker` (`*`→`strong`, `_`→`em`) at raw offset `open`.
    ///
    /// Finds the matching close marker over the raw source ([`Self::find_emphasis_close`]); if one
    /// exists in this paragraph/brace scope, collects `[open+1, close)` as an (inline) markup body
    /// (nesting `@`-forms and nested emphasis) → `h(tag, {}, [...])` and resumes after `close`. With
    /// no matching close the marker is **literal** (Typst behavior) and we resume right after it.
    fn parse_emphasis(&mut self, marker: u8, open: u32, items: &mut Vec<BodyItem<'a>>) {
        if let Some(close) = self.find_emphasis_close(open, marker) {
            let mut body: Vec<BodyItem<'a>> = Vec::new();
            self.collect_markup_range(open + 1, close, 0, &mut body);
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

    /// Find the matching close marker for an emphasis opened at `open` (raw offset of the marker).
    ///
    /// Scans the raw source for the next `marker` byte that is a valid marker (not intra-word). The
    /// search is bounded by the emphasis's *scope*: it stops (returning `None`) at a blank line
    /// (paragraph break — emphasis is intra-paragraph, à la Typst), at the `}` that closes the
    /// enclosing body (brace depth dropping below the open level), or at EOF. Nested balanced `{…}`
    /// is skipped. `@`-forms are skipped wholesale so a `*` *inside* an embedded expression cannot
    /// close the span (the embedded JS owns its own `*`).
    fn find_emphasis_close(&self, open: u32, marker: u8) -> Option<u32> {
        let bytes = self.source_text.as_bytes();
        let mut i = open as usize + 1;
        let mut depth: i32 = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'\\' => {
                    // Escape: skip the escaped char (so `\*` cannot close).
                    i += 2;
                }
                b'\n' => {
                    // A blank line (this `\n` then optional-ws then another `\n`) ends the scope.
                    let mut j = i + 1;
                    while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r') {
                        j += 1;
                    }
                    if j >= bytes.len() || bytes[j] == b'\n' {
                        return None;
                    }
                    i += 1;
                }
                b'{' => {
                    depth += 1;
                    i += 1;
                }
                b'}' => {
                    if depth == 0 {
                        return None; // the enclosing body closes before a matching marker
                    }
                    depth -= 1;
                    i += 1;
                }
                // Skip over a raw span (code/math/verbatim) so a `*`/`_` *inside* raw content
                // cannot close the emphasis (the raw span owns its own markers).
                b'`' | b'$' => i = self.skip_raw_span_for_emphasis(i),
                b'|' if bytes.get(i + 1) == Some(&b'{') => {
                    i = self.skip_raw_span_for_emphasis(i);
                }
                // Skip over an `@`-form's head and any `(…)`/`[…]` group so a `*`/`_` *inside* an
                // embedded expression cannot close the emphasis (the embedded JS owns its own
                // markers), and a stray `(`/`{`/`}` inside that JS cannot perturb `depth`. A trailing
                // `{…}` markup body is left to the brace arms above (depth-tracked, escape-aware).
                b'@' => i = self.skip_at_form_for_emphasis(i),
                _ if b == marker && depth == 0 => {
                    // A candidate close: a marker preceded by content, enclosing ≥1 byte (no empty
                    // span). Empty (`**`) or space-before (`foo *`) closes are skipped → stay literal.
                    if i as u32 > open + 1 && self.can_close_emphasis(i as u32) {
                        return Some(i as u32);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        None
    }

    /// Skip a raw span (inline/fenced code, math, or `|{ … }|` verbatim) whose opener byte is at
    /// `at`, returning the offset just past its close (or just past the opener if it has no valid
    /// close — then the opener byte was literal and we advance by one to make progress). Used by
    /// [`Self::find_emphasis_close`] so emphasis matching steps over raw content.
    fn skip_raw_span_for_emphasis(&self, at: usize) -> usize {
        let bytes = self.source_text.as_bytes();
        match bytes[at] {
            b'`' => {
                let mut k = at;
                while k < bytes.len() && bytes[k] == b'`' {
                    k += 1;
                }
                let fence_len = k - at;
                match self.find_backtick_close(k, fence_len) {
                    Some(close) => close + fence_len, // past the closing run
                    None => at + 1,                   // unterminated → the backtick is literal
                }
            }
            b'$' => {
                let display = bytes.get(at + 1) == Some(&b'$');
                let delim = if display { 2 } else { 1 };
                let mut k = at + delim;
                while k < bytes.len() {
                    match bytes[k] {
                        b'\\' => k += 2,
                        b'$' if !display => return k + 1,
                        b'$' if display && bytes.get(k + 1) == Some(&b'$') => return k + 2,
                        _ => k += 1,
                    }
                }
                at + 1 // unterminated → the `$` is literal
            }
            b'|' => {
                // `|{ … }|` — scan to the closing `}|`.
                let mut k = at + 2;
                while k < bytes.len() {
                    if bytes[k] == b'}' && bytes.get(k + 1) == Some(&b'|') {
                        return k + 2;
                    }
                    k += 1;
                }
                at + 1 // unterminated
            }
            _ => at + 1,
        }
    }

    /// Skip an `@`-form whose `@` byte is at `at` (raw offset), returning the offset just past the
    /// form's *head* and any immediately-following `(…)`/`[…]` group — the parenthesized
    /// interpolation `@(expr)` or an attribute list `@name[…]`. Used by [`Self::find_emphasis_close`]
    /// so a `*`/`_` *inside* an embedded expression cannot be mistaken for the emphasis close, and a
    /// stray bracket inside that JS cannot perturb the caller's `{…}` depth counter.
    ///
    /// A trailing `{…}` markup body is deliberately left to the caller's main scan (its depth/escape/
    /// raw-span machinery already handles markup braces). The group skip ([`Self::skip_balanced`])
    /// matches brackets only — it does not interpret JS string/template literals, so a bracket char
    /// inside a string inside `@(…)` (e.g. `@(")")`) can mis-scan. That is vanishingly rare inside
    /// inline emphasis and was never handled before; the realistic case (`@(a * b)`) is exact.
    fn skip_at_form_for_emphasis(&self, at: usize) -> usize {
        let bytes = self.source_text.as_bytes();
        let mut i = at + 1; // past '@'
        // `@name` head: identifier bytes plus `.`-member chains (non-ASCII bytes are identifier
        // continuations, e.g. `@café`). `@(expr)` has no identifier head — the group loop below skips
        // the `(…)`. Over-consuming a trailing `.` is harmless: only `*`/`_` matter as close markers.
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric()
                || matches!(bytes[i], b'_' | b'$' | b'.')
                || bytes[i] >= 0x80)
        {
            i += 1;
        }
        // Adjacent `(…)`/`[…]` groups (the interpolation expr, attribute lists, call/index chains).
        while matches!(bytes.get(i), Some(b'(' | b'[')) {
            i = self.skip_balanced(i);
        }
        i
    }

    /// Skip a balanced bracket group (`(…)`/`[…]`/`{…}`, nesting all three) whose opener is at `at`,
    /// returning the offset just past the matching closer, or `at + 1` if unterminated so the caller
    /// makes progress. Brackets only — string/comment contents are not interpreted (see
    /// [`Self::skip_at_form_for_emphasis`]).
    fn skip_balanced(&self, at: usize) -> usize {
        let bytes = self.source_text.as_bytes();
        let mut depth = 0u32;
        let mut i = at;
        while i < bytes.len() {
            match bytes[i] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        at + 1 // unterminated → caller advances past the opener
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

    /// The `char` ending at byte `offset` (i.e. the char immediately *before* `offset`), or `None`
    /// at the start of source. Decodes a full UTF-8 scalar so non-ASCII word chars classify right.
    fn char_before(&self, offset: u32) -> Option<char> {
        if offset == 0 {
            return None;
        }
        self.source_text.get(..offset as usize).and_then(|s| s.chars().next_back())
    }

    /// The `char` starting at byte `offset`, or `None` at/after end of source.
    fn char_at(&self, offset: u32) -> Option<char> {
        self.source_text.get(offset as usize..).and_then(|s| s.chars().next())
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
        let bytes = self.source_text.as_bytes();
        let mut i = line_start as usize;
        // Count the `#` run at the very start of the line (no leading indent for headings).
        let run_start = i;
        while i < bytes.len() && bytes[i] == b'#' {
            i += 1;
        }
        let level = i - run_start;
        // 1–6 `#` followed by a single space.
        if !(1..=6).contains(&level) || i >= bytes.len() || bytes[i] != b' ' {
            return None;
        }
        let body_start = i as u32 + 1; // skip the one separating space
        let line_end = self.line_content_end(line_start); // offset of the line's `\n` (or EOF)

        let mut items: Vec<BodyItem<'a>> = Vec::new();
        self.collect_markup_range(body_start, line_end, 0, &mut items);
        let children = self.body_items_to_children(items);

        let span = Span::new(line_start, line_end);
        let element = self.ast.nota_heading(span, level as u8, children);
        Some((element, line_end))
    }

    /// The offset of the terminating `\n` of the line containing `line_start` (or EOF if none).
    fn line_content_end(&self, line_start: u32) -> u32 {
        let bytes = self.source_text.as_bytes();
        let mut i = line_start as usize;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        i as u32
    }

    /// Classify a list marker at the first non-whitespace of the line at `line_start`. Returns the
    /// marker kind, the marker's *content column* (offset just past the marker + its one space —
    /// where the item body begins), and the indent (offset of the first non-ws). `None` if the line
    /// does not open with a list marker.
    fn list_marker_at(&self, line_start: u32) -> Option<ListMarker> {
        let bytes = self.source_text.as_bytes();
        let mut i = line_start as usize;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let indent = i;
        if i >= bytes.len() {
            return None;
        }
        match bytes[i] {
            // `-`+space (bullet) / `+`+space (number).
            b'-' | b'+' if i + 1 < bytes.len() && bytes[i + 1] == b' ' => {
                let ordered = bytes[i] == b'+';
                Some(ListMarker { ordered, indent: indent as u32, body_col: i as u32 + 2 })
            }
            // `N.`+space — an explicit ordered marker (digits then `.` then space).
            b'0'..=b'9' => {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j < bytes.len()
                    && bytes[j] == b'.'
                    && j + 1 < bytes.len()
                    && bytes[j + 1] == b' '
                {
                    Some(ListMarker {
                        ordered: true,
                        indent: indent as u32,
                        body_col: j as u32 + 2,
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Parse a run of list items starting at `line_start` (the first line is known to be a list
    /// marker). Consecutive marker lines at the *same or deeper* indent form the run; a deeper
    /// marker nests inside the preceding item (its `struct`-coalesced inner list). Each item →
    /// `h("nota-ul-li"|"nota-ol-li", {}, [body])`. Returns `(elements, resume)` where `resume` is the offset
    /// where the run ended (a line that is neither a continuation nor a same-level marker).
    fn parse_list(&mut self, line_start: u32) -> (Vec<NotaListItem<'a>>, u32) {
        let base = self.list_marker_at(line_start).expect("parse_list: not a marker line");
        let base_indent = base.indent;
        let mut elements: Vec<NotaListItem<'a>> = Vec::new();
        let mut at = line_start;

        while let Some(marker) = self.list_marker_at(at) {
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
            let line_end = self.line_content_end(at);
            let body_start = marker.body_col.min(line_end);
            let item_end = self.list_item_extent(line_end, marker.indent);

            let children = self.collect_list_item_body(body_start, item_end, marker.indent);
            let kind = if marker.ordered { NotaListKind::Ordered } else { NotaListKind::Unordered };
            let span = Span::new(marker.indent, item_end);
            elements.push(self.ast.nota_list_item(span, kind, children));

            at = item_end;
            // Skip a single trailing newline already consumed by the extent; continue if the next
            // line is another marker at >= base_indent.
            if self.list_marker_at(at).is_none() {
                break;
            }
        }
        (elements, at)
    }

    /// The end offset of a list item's body: subsequent lines indented strictly past `marker_indent`
    /// (or blank) are part of the item; the item ends at the first line at/below `marker_indent` that
    /// is non-blank. Returns the offset of that line's start (the resume point).
    fn list_item_extent(&self, first_line_end: u32, marker_indent: u32) -> u32 {
        let bytes = self.source_text.as_bytes();
        let mut end = self.next_line_start(first_line_end);
        loop {
            if end as usize >= bytes.len() {
                break;
            }
            let line_start = end as usize;
            let mut i = line_start;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            let is_blank = i >= bytes.len() || bytes[i] == b'\n';
            let indent = (i - line_start) as u32;
            // A same/shallower-indented *marker* line ends this item (it is a sibling/uncle item).
            if !is_blank && self.list_marker_at(end).is_some() && indent <= marker_indent {
                break;
            }
            // Non-marker content indented strictly past the marker continues the item; a deeper
            // marker (nested list) also continues it.
            if is_blank || indent > marker_indent {
                end = self.next_line_start(end);
            } else {
                break;
            }
        }
        end
    }

    /// Collect a list item's body over `[start, end)`: the rest-of-marker-line content plus indented
    /// continuation lines, with nested list markers recursively lowered into `nota-ul-li`/`nota-ol-li` children.
    fn collect_list_item_body(
        &mut self,
        start: u32,
        end: u32,
        marker_indent: u32,
    ) -> ArenaVec<'a, NotaChild<'a>> {
        let mut items: Vec<BodyItem<'a>> = Vec::new();
        self.collect_block_body_range(start, end, marker_indent, &mut items);
        self.body_items_to_children(items)
    }

    /// Collect markup over `[start, end)` like [`Self::collect_markup_range`], but **also** detecting
    /// line-start headings/nested lists (so a list item's continuation can contain a nested list or a
    /// heading). Used for list-item bodies (and reusable for other block ranges).
    fn collect_block_body_range(
        &mut self,
        start: u32,
        end: u32,
        _base_indent: u32,
        items: &mut Vec<BodyItem<'a>>,
    ) {
        self.nota_seek_markup(start);
        let mut depth = 0u32;
        loop {
            if self.has_fatal_error() || self.cur_token().start() >= end {
                break;
            }
            match self.cur_kind() {
                Kind::MarkupText => {
                    let token = self.cur_token();
                    let text_end = token.end().min(end);
                    let text = &self.source_text[token.start() as usize..text_end as usize];
                    if !text.is_empty() {
                        items.push(BodyItem::Text(text));
                    }
                    let term_off = token.end();
                    if term_off >= end {
                        break;
                    }
                    match self.byte_at(term_off) {
                        Some(b'\n') => {
                            items.push(BodyItem::Text("\n"));
                            let next_line = term_off + 1;
                            // Line-start constructs inside the item body (clipped to `end`).
                            if next_line < end && self.list_marker_at(next_line).is_some() {
                                let (els, resume) = self.parse_list(next_line);
                                for e in els {
                                    items.push(BodyItem::Child(NotaChild::ListItem(
                                        self.ast.alloc(e),
                                    )));
                                }
                                if resume >= end {
                                    break;
                                }
                                self.nota_seek_markup(resume);
                                continue;
                            }
                            if next_line < end
                                && let Some((h, h_end)) = self.try_heading(next_line)
                            {
                                items.push(BodyItem::Child(NotaChild::Heading(self.ast.alloc(h))));
                                self.nota_seek_markup(h_end);
                                continue;
                            }
                            self.nota_seek_markup(next_line);
                        }
                        Some(b'{') => {
                            depth += 1;
                            items.push(BodyItem::Text("{"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'}') if depth > 0 => {
                            depth -= 1;
                            items.push(BodyItem::Text("}"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'}') => {
                            items.push(BodyItem::Text("}"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'@') => {
                            self.bump_any();
                            let child = self.parse_nota_form(true);
                            items.push(BodyItem::Child(markup_to_child(child)));
                        }
                        Some(m @ (b'*' | b'_')) if term_off < end => {
                            if self.can_open_emphasis(term_off, m) {
                                self.parse_emphasis(m, term_off, items);
                            } else {
                                self.push_literal_byte(items, m);
                                self.nota_seek_markup(term_off + 1);
                            }
                        }
                        Some(b'\\') if term_off < end => {
                            let resume = self.push_escape(items, term_off);
                            self.nota_seek_markup(resume);
                        }
                        Some(b'`') if term_off < end => self.parse_code_or_literal(items, term_off),
                        Some(b'$') if term_off < end => self.parse_math_or_literal(items, term_off),
                        Some(b'|') if term_off < end => {
                            items.push(BodyItem::Text("|"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        _ => break,
                    }
                }
                Kind::At => {
                    let child = self.parse_nota_form(true);
                    items.push(BodyItem::Child(markup_to_child(child)));
                }
                Kind::Eof => break,
                _ => self.advance_for_markup_text(),
            }
        }
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
        if let Some(c) = self.char_at(esc_off + 1) {
            // Emit the escaped char verbatim (the `\` is dropped); resume past `\<c>`.
            let lit: &'a str = self.alloc_char(c);
            items.push(BodyItem::Text(lit));
            esc_off + 1 + c.len_utf8() as u32
        } else {
            // Trailing lone backslash at EOF: literal `\`.
            items.push(BodyItem::Text("\\"));
            esc_off + 1
        }
    }

    /// Allocate a single `char` as an arena `&str` (for escaped-literal text items).
    fn alloc_char(&self, c: char) -> &'a str {
        let mut buf = [0u8; 4];
        self.ast.allocator.alloc_str(c.encode_utf8(&mut buf))
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
                let child = self.parse_nota_form(false);
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
        if let Some(resume) = self.parse_code_span(items, tick_off) {
            self.nota_seek_markup(resume);
        } else {
            // No close: the backtick run is literal text. Emit it, resume past it.
            let bytes = self.source_text.as_bytes();
            let mut i = tick_off as usize;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let lit: &'a str = &self.source_text[tick_off as usize..i];
            items.push(BodyItem::Text(lit));
            self.nota_seek_markup(i as u32);
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

    /// Parse a code span whose opening backtick run starts at raw offset `tick_off` (the run stopped
    /// there). Pushes the lowered `h(CodeInline|CodeBlock, …)` child into `items` and returns the
    /// resume offset, or returns `None` if this is not a valid code opener (run shorter than any
    /// close → the backticks are literal; the caller emits them as text).
    fn parse_code_span(&self, items: &mut Vec<BodyItem<'a>>, tick_off: u32) -> Option<u32> {
        let bytes = self.source_text.as_bytes();
        let mut i = tick_off as usize;
        while i < bytes.len() && bytes[i] == b'`' {
            i += 1;
        }
        let fence_len = i - tick_off as usize;
        let content_start = i;

        // A `≥3` run that is the last non-whitespace on its line (modulo a trailing language tag) is
        // a *fenced* block: ```lang⏎ … ⏎```. Otherwise it is inline code.
        if fence_len >= 3
            && let Some((element, resume)) =
                self.parse_fenced_code(tick_off, fence_len, content_start)
        {
            items.push(BodyItem::Child(NotaChild::Code(self.ast.alloc(element))));
            return Some(resume);
        }

        // Inline code: content up to the next run of exactly `fence_len` backticks on the same scope
        // (shorter runs are literal content). Search for the closing run.
        let close = self.find_backtick_close(content_start, fence_len)?;
        let raw: &'a str = &self.source_text[content_start..close];
        let span = Span::new(tick_off, close as u32 + fence_len as u32);
        let element = self.ast.nota_code(span, None, raw, false);
        items.push(BodyItem::Child(NotaChild::Code(self.ast.alloc(element))));
        Some(close as u32 + fence_len as u32)
    }

    /// Find the next run of *at least* `fence_len` backticks at/after `from`, returning the offset of
    /// the first backtick of that run (the close), or `None` if none exists. Shorter runs are skipped
    /// (they are literal content — "backtick runs shorter than the closing fence are literal").
    fn find_backtick_close(&self, from: usize, fence_len: usize) -> Option<usize> {
        let bytes = self.source_text.as_bytes();
        let mut i = from;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                let run_start = i;
                while i < bytes.len() && bytes[i] == b'`' {
                    i += 1;
                }
                if i - run_start >= fence_len {
                    return Some(run_start);
                }
                // A shorter run: literal, keep scanning past it.
            } else {
                i += 1;
            }
        }
        None
    }

    /// Parse a fenced code block opened by a `fence_len`-backtick run at `tick_off`, where
    /// `content_start` is just past the opening run. The rest of the opening line (trimmed) is the
    /// optional language tag. The block ends at a line whose first non-whitespace is a run of `≥
    /// fence_len` backticks. Returns `(h(CodeBlock,…), resume)`, or `None` if the opening run is not
    /// a bare fence line (then it is treated as inline code by the caller).
    fn parse_fenced_code(
        &self,
        tick_off: u32,
        fence_len: usize,
        content_start: usize,
    ) -> Option<(NotaCode<'a>, u32)> {
        let bytes = self.source_text.as_bytes();
        // The opening line's tail after the run: an optional language tag (no backticks), then `\n`.
        let mut j = content_start;
        while j < bytes.len() && bytes[j] != b'\n' {
            if bytes[j] == b'`' {
                return None; // backticks on the opener line ⇒ not a fenced block (inline run)
            }
            j += 1;
        }
        let lang = self.source_text[content_start..j].trim();
        if j >= bytes.len() {
            return None; // no newline after the opener ⇒ not a block
        }
        let body_start = j + 1; // first line of code content

        // Scan for the closing fence: a line whose first non-ws is a run of ≥ fence_len backticks.
        let mut line_start = body_start;
        loop {
            if line_start >= bytes.len() {
                // Unterminated fence: code runs to EOF.
                let raw: &'a str = &self.source_text[body_start..bytes.len()];
                return Some(self.finish_fenced(
                    tick_off,
                    bytes.len() as u32,
                    lang,
                    body_start,
                    raw,
                ));
            }
            let mut k = line_start;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                k += 1;
            }
            let run_start = k;
            while k < bytes.len() && bytes[k] == b'`' {
                k += 1;
            }
            if k - run_start >= fence_len {
                // Closing fence. Code body is [body_start, closing-line-start), dropping the `\n`
                // immediately before the fence line. Resume right after
                // the backtick run (NOT the rest of the line): trailing content — e.g. the `}` that
                // closes an enclosing `@d{ … }` body — is left for the collector to handle.
                let mut code_end = line_start;
                if code_end > body_start && bytes[code_end - 1] == b'\n' {
                    code_end -= 1;
                }
                let raw: &'a str = &self.source_text[body_start..code_end];
                return Some(self.finish_fenced(tick_off, k as u32, lang, body_start, raw));
            }
            line_start = self.next_line_start(line_start as u32) as usize;
        }
    }

    /// Build `h(CodeBlock, { lang: "…" }?, [String.raw`<code>`])` for a fenced block and return it
    /// with the `resume` offset.
    fn finish_fenced(
        &self,
        tick_off: u32,
        resume: u32,
        lang: &str,
        _code_start: usize,
        raw: &'a str,
    ) -> (NotaCode<'a>, u32) {
        let span = Span::new(tick_off, resume);
        let language = (!lang.is_empty()).then(|| self.ast.str(self.ast.allocator.alloc_str(lang)));
        let element = self.ast.nota_code(span, language, raw, true);
        (element, resume)
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

        // The file may *open* with a statement / list / heading (no preceding `\n` to trigger the
        // line-start hooks in `collect_markup`). Handle the start offset explicitly.
        if self.is_statement_line(start) {
            let resume = self.collect_statements(start, &mut items);
            self.nota_seek_markup(resume);
        } else if self.list_marker_at(start).is_some() {
            let (els, resume) = self.parse_list(start);
            for e in els {
                items.push(BodyItem::Child(NotaChild::ListItem(self.ast.alloc(e))));
            }
            self.nota_seek_markup(resume);
        } else if let Some((heading, h_end)) = self.try_heading(start) {
            items.push(BodyItem::Child(NotaChild::Heading(self.ast.alloc(heading))));
            self.nota_seek_markup(h_end);
        } else {
            self.nota_seek_markup(start);
        }

        // `collect_markup` (document=true) collects all markup + `%`/`%%%` statements (as faithful
        // `NotaChild::Statement` children) through EOF; routing / IIFE wrapping is the lowering pass.
        let mut depth = 0u32;
        let _ = self.collect_markup(&mut items, &mut depth, /* document */ true);

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
    ) -> NotaElement<'a> {
        // `commit_head` already consumed the head's boundary token and left `:` as the current
        // token; its end is the body start.
        debug_assert!(self.at(Kind::Colon), "colon sugar entered not at `:`");
        let colon_end = self.cur_token().end();
        let head_line_indent = self.line_indent_of(span_start);

        // Determine the sugar's source extent: rest of the `@head:` line + lines indented strictly
        // past `head_line_indent`.
        let (body_src_start, body_src_end) = self.colon_block_extent(colon_end, head_line_indent);

        // Collect props from leading `|` lines, and the markup body (text + `@`-forms).
        let mut props = self.ast.vec();
        let mut items: Vec<BodyItem<'a>> = Vec::new();
        self.collect_colon_body(
            body_src_start,
            body_src_end,
            head_line_indent,
            &mut props,
            &mut items,
        );

        let children = self.body_items_to_children(items);
        let span = Span::new(span_start, body_src_end);

        // Resume the outer context after the consumed block.
        if in_body {
            self.nota_seek_markup(body_src_end);
        } else {
            self.nota_seek_to(body_src_end);
        }
        let tag = self.head_to_tag(head);
        self.ast.nota_element(span, tag, props, children)
    }

    // ------------------------------------------------------------------------------------------
    // Line / statement / fence scanning (over the raw source)
    // ------------------------------------------------------------------------------------------

    /// Is the line beginning at `line_start` a `%`/`%%%` statement line? (first non-whitespace is
    /// `%`, not an escaped `\%`).
    fn is_statement_line(&self, line_start: u32) -> bool {
        let bytes = self.source_text.as_bytes();
        let mut i = line_start as usize;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        i < bytes.len() && bytes[i] == b'%'
    }

    /// Classify the statement line at `line_start`. Returns `(content_or_inner_start, is_fence)`:
    /// for a fence (`%%%`), the offset of the line *after* the opening fence; for a `%` statement,
    /// the offset just past the `%`. `None` if the line is not a statement line.
    fn statement_kind(&self, line_start: u32) -> Option<(u32, bool)> {
        let bytes = self.source_text.as_bytes();
        let mut i = line_start as usize;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'%' {
            return None;
        }
        // Count the run of `%`.
        let run_start = i;
        while i < bytes.len() && bytes[i] == b'%' {
            i += 1;
        }
        let run_len = i - run_start;
        // `%%%` (a run of >= 3 with nothing else on the line, modulo trailing ws) → fence.
        if run_len >= 3 {
            // Confirm rest of line is whitespace (an opening fence on its own line).
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'\n' {
                if bytes[j] != b' ' && bytes[j] != b'\t' && bytes[j] != b'\r' {
                    // Not a bare fence line; treat the first `%` as a statement (rare).
                    return Some((run_start as u32 + 1, false));
                }
                j += 1;
            }
            let inner_start = if j < bytes.len() { j as u32 + 1 } else { j as u32 };
            return Some((inner_start, true));
        }
        // A single `%` statement: content starts right after it.
        Some((run_start as u32 + 1, false))
    }

    /// Find the `%%%` fence close at/after `inner_start`. Returns `(inner_end, after_fence)` where
    /// `inner_end` is the offset of the closing-fence line start and `after_fence` is past the
    /// closing fence's line (the resume point).
    fn find_fence_close(&self, inner_start: u32) -> (u32, u32) {
        let bytes = self.source_text.as_bytes();
        let mut line_start = inner_start as usize;
        while line_start < bytes.len() {
            // Examine this line.
            let mut i = line_start;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            let run_start = i;
            while i < bytes.len() && bytes[i] == b'%' {
                i += 1;
            }
            if i - run_start >= 3 {
                // Closing fence. `inner_end` = this line's start; `after_fence` = next line start.
                let mut j = i;
                while j < bytes.len() && bytes[j] != b'\n' {
                    j += 1;
                }
                let after = if j < bytes.len() { j + 1 } else { j };
                return (line_start as u32, after as u32);
            }
            // Advance to next line.
            let mut j = line_start;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            line_start = if j < bytes.len() { j + 1 } else { j };
        }
        // Unterminated fence: treat the rest of the file as the inner body.
        (bytes.len() as u32, bytes.len() as u32)
    }

    /// The offset of the line start following the line containing `offset`.
    fn next_line_start(&self, offset: u32) -> u32 {
        let bytes = self.source_text.as_bytes();
        let mut i = offset as usize;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        if i < bytes.len() { i as u32 + 1 } else { i as u32 }
    }

    /// The indentation (leading-space count) of the line containing byte `offset`.
    fn line_indent_of(&self, offset: u32) -> usize {
        let bytes = self.source_text.as_bytes();
        // Find this line's start.
        let mut start = offset as usize;
        while start > 0 && bytes[start - 1] != b'\n' {
            start -= 1;
        }
        let mut i = start;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        i - start
    }

    // ------------------------------------------------------------------------------------------
    // Colon / block sugar extent + collection
    // ------------------------------------------------------------------------------------------

    /// Compute the source extent `[start, end)` of a `@head:` sugar body: the rest of the `@head:`
    /// line (from `colon_end`) plus subsequent lines indented strictly past `head_indent`.
    fn colon_block_extent(&self, colon_end: u32, head_indent: usize) -> (u32, u32) {
        let bytes = self.source_text.as_bytes();
        // The `:` consumes the immediately-following horizontal whitespace (separator), so
        // `@foo: hello` → body `hello`, not ` hello`. Newlines are NOT skipped (they delimit lines).
        let mut start = colon_end as usize;
        while start < bytes.len() && (bytes[start] == b' ' || bytes[start] == b'\t') {
            start += 1;
        }
        let start = start as u32;
        // The first line: up to and including its newline (if any).
        let mut end = self.next_line_start(colon_end);
        // Include subsequent lines indented strictly past `head_indent`, OR blank lines.
        loop {
            if end as usize >= bytes.len() {
                break;
            }
            let line_start = end as usize;
            // Compute indentation; detect blank line.
            let mut i = line_start;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            let is_blank = i >= bytes.len() || bytes[i] == b'\n';
            let indent = i - line_start;
            if is_blank || indent > head_indent {
                end = self.next_line_start(end);
            } else {
                break;
            }
        }
        (start, end)
    }

    /// Collect the colon-sugar body over `[start, end)`: leading `|` lines → `[…]` prop groups; the
    /// remaining lines → markup body items.
    fn collect_colon_body(
        &mut self,
        start: u32,
        end: u32,
        head_indent: usize,
        props: &mut ArenaVec<'a, NotaProp<'a>>,
        items: &mut Vec<BodyItem<'a>>,
    ) {
        let bytes = self.source_text.as_bytes();

        // Walk leading `|` prop lines (a line whose first non-ws char is `|`, indented past head).
        let mut body_start = start;
        // Props lines only apply to the *continuation* lines (not the rest-of-`@head:`-line).
        // Find the first continuation line.
        let first_cont = self.next_line_start(start);
        let mut scan = first_cont;
        while scan < end {
            let line_start = scan as usize;
            let mut i = line_start;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'|' {
                // A `| k: v` prop line. Parse `[k: v]`-style entries from after `|` to line end.
                let content_start = i as u32 + 1;
                let line_end = self.next_line_start(scan);
                self.parse_pipe_prop_line(content_start, line_end, props);
                scan = line_end;
                body_start = scan; // props consume the prefix; body starts after them
            } else {
                break;
            }
        }
        // If `|` lines were consumed, the rest-of-line content of `@head:` is dropped (kept simple);
        // the body is the remaining suffix. Back up to include the `\n` that precedes that suffix, so
        // the whitespace pass sees the body as "opened with a newline" and treats its first line as
        // an *indent* line (stripping the common indent) rather than as the inline `{`-line — without
        // this, `@foo:⏎  | x:1⏎  hello` leaks the leading indent as `"  hello"` instead of `"hello"`.
        let body_range_start = if body_start > start {
            if bytes.get(body_start as usize - 1) == Some(&b'\n') {
                body_start - 1
            } else {
                body_start
            }
        } else {
            start
        };
        self.collect_markup_range(body_range_start, end, head_indent, items);
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

    /// Collect markup over a raw source range `[start, end)` (colon-sugar body), seeking the lexer
    /// to `start` and collecting until `end`. Leading common indentation past `head_indent` is left
    /// to the whitespace pass; here we strip the block's base indentation by treating the range as a
    /// standalone body.
    fn collect_markup_range(
        &mut self,
        start: u32,
        end: u32,
        _head_indent: usize,
        items: &mut Vec<BodyItem<'a>>,
    ) {
        self.nota_seek_markup(start);
        let mut depth = 0u32;
        loop {
            if self.has_fatal_error() {
                break;
            }
            // Stop once we've consumed up to `end`.
            if self.cur_token().start() >= end {
                break;
            }
            match self.cur_kind() {
                Kind::MarkupText => {
                    let token = self.cur_token();
                    // Clip the text to `end`.
                    let text_end = token.end().min(end);
                    let text = &self.source_text[token.start() as usize..text_end as usize];
                    if !text.is_empty() {
                        items.push(BodyItem::Text(text));
                    }
                    let term_off = token.end();
                    if term_off >= end {
                        break;
                    }
                    match self.byte_at(term_off) {
                        Some(b'\n') => {
                            items.push(BodyItem::Text("\n"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'{') => {
                            depth += 1;
                            items.push(BodyItem::Text("{"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'}') if depth > 0 => {
                            depth -= 1;
                            items.push(BodyItem::Text("}"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'}') => {
                            items.push(BodyItem::Text("}"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        Some(b'@') => {
                            self.bump_any();
                            let child = self.parse_nota_form(true);
                            items.push(BodyItem::Child(markup_to_child(child)));
                        }
                        Some(m @ (b'*' | b'_')) if term_off < end => {
                            // Nested emphasis inside an emphasis / colon-sugar body.
                            if self.can_open_emphasis(term_off, m) {
                                self.parse_emphasis(m, term_off, items);
                            } else {
                                self.push_literal_byte(items, m);
                                self.nota_seek_markup(term_off + 1);
                            }
                        }
                        Some(b'\\') if term_off < end => {
                            // General backslash escape, inside an emphasis / colon body.
                            let resume = self.push_escape(items, term_off);
                            self.nota_seek_markup(resume);
                        }
                        Some(b'`') if term_off < end => self.parse_code_or_literal(items, term_off),
                        Some(b'$') if term_off < end => self.parse_math_or_literal(items, term_off),
                        Some(b'|') if term_off < end => {
                            items.push(BodyItem::Text("|"));
                            self.nota_seek_markup(term_off + 1);
                        }
                        _ => break,
                    }
                }
                Kind::At => {
                    let child = self.parse_nota_form(true);
                    items.push(BodyItem::Child(markup_to_child(child)));
                }
                Kind::Eof => break,
                _ => self.advance_for_markup_text(),
            }
        }
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
enum MarkupTrigger {
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
