//! Reader-faithful syntax highlighting: classified source spans from the *parsed* Nota AST.
//!
//! The TextMate grammar (`vscode-nota`) is regex-only and cannot track Nota's context-sensitivity
//! (`[` is props only after an element head), its markup⇄JS mutual nesting (`@`-forms inside
//! `%`/`[props]`/`@(expr)` JS), or the line/indent machinery (`%` continuations, colon blocks) —
//! so a markup-valued prop or a stray `[` derails it for the rest of the document. This pass is
//! the faithful alternative: parse with the real reader, then
//!
//!  1. **walk the Nota AST** ([`Highlighter`], an [`oxc_ast_visit::Visit`]) emitting structural
//!     spans — sigils, tag names, prop names, markers, raw runs — and collecting the extents of
//!     embedded JS (statements, prop values, `@(expr)` heads, control-flow heads), with *holes*
//!     punched where markup re-enters the JS (`Expression::NotaMarkup`);
//!  2. **re-lex the JS gap ranges** with the crate's own lexer
//!     ([`ParserImpl::nota_lex_highlight_ranges`]) to classify keywords, strings, numbers,
//!     comments, and operators.
//!
//! The result is a single sorted span list (start ascending, end descending — outer spans before
//! the spans they contain). Spans may nest: a heading emits an under-layer for its whole line
//! beneath its children's spans, so a client painting in list order gets correct layering.
//!
//! Known lexical approximation: regex literals in embedded JS re-lex as `/` operators (the pump
//! has no parser context to disambiguate division; the reader itself parses them correctly).
//!
//! Entry: [`crate::Parser::parse_nota_highlights`] — an editor-tooling view of the parse, consumed
//! directly by the wasm bindings (`napi/nota_wasm`, which own the kind→name table and the flat
//! triple encoding); an LSP semantic-tokens provider can consume the same entry later. The pass
//! lives *here*, not with its consumers, because both halves need crate-private machinery: the
//! pump drives `ParserImpl`/lexer internals (`UniquePromise` is unmintable outside the parser),
//! and the walker reuses the `lexer::nota` scans.

// Source offsets fit u32 (oxc's `Span` model) — same policy as the reader in `super`.
#![expect(
    clippy::cast_possible_truncation,
    reason = "source offsets/lengths fit in u32 (oxc's Span model)"
)]

use oxc_allocator::Vec as ArenaVec;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_span::{GetSpan, Span};

use crate::{
    ParserConfig as Config, ParserImpl,
    lexer::Kind,
    lexer::nota::{
        CodeScan, MathScan, byte_at, colon_prop_line_at, find_fence_close, lex_code_span,
        lex_math_span, line_content_end, list_marker_at, sigil_run_end, statement_kind,
    },
};

/// The classification of one highlight span.
///
/// Nota-specific (not LSP token types): each variant is one *visual* class; clients map variants
/// to colors/styles. Discriminants are a stable wire format (the wasm boundary ships
/// `(start, end, kind as u32)` triples) — append new variants, never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NotaHighlightKind {
    /// Form/statement punctuation that *is* Nota: `@`, `%`/`%%%`, `|{` `}|` `|@`, colon-sugar `:`,
    /// `|` prop-line markers, emphasis `*`/`_` marker bytes.
    Sigil = 0,
    /// Lowercase host tag name (`@div`).
    TagHost = 1,
    /// Capitalized component tag name (`@Aside`).
    TagComponent = 2,
    /// A prop key (field or shorthand).
    PropName = 3,
    /// A bare-name interpolation (`@name` in markup or math) — the name span.
    Interpolation = 4,
    /// Nota-level control keywords: `if` / `for` / `else` / `of`.
    ControlKeyword = 5,
    /// The `#`-run of a heading.
    HeadingMarker = 6,
    /// A whole heading line (under-layer beneath its children's spans).
    Heading = 7,
    /// A list-item marker (`-` / `+` / `N.`).
    ListMarker = 8,
    /// A whole `*…*` span (under-layer).
    EmphasisStrong = 9,
    /// A whole `_…_` span (under-layer).
    EmphasisEm = 10,
    /// A math delimiter (an inline `$`-run; for a fence, the whole opening `$$` line / closing run).
    MathDelim = 11,
    /// A raw math (LaTeX) run.
    Math = 12,
    /// A code delimiter (backtick run; for a fence, the whole fence line).
    CodeDelim = 13,
    /// A fence language tag (overlays the opening [`Self::CodeDelim`]).
    CodeLang = 14,
    /// Raw code content (inline or fenced).
    Code = 15,
    /// A raw verbatim (`|{…}|`) run.
    Verbatim = 16,
    /// A backslash escape (`\x`, both bytes).
    Escape = 17,
    /// Embedded-JS keyword (incl. `true`/`false`/`null`).
    JsKeyword = 18,
    /// Embedded-JS string or template part.
    JsString = 19,
    /// Embedded-JS numeric literal.
    JsNumber = 20,
    /// Embedded-JS comment.
    JsComment = 21,
    /// Embedded-JS operator/punctuation.
    JsOperator = 22,
}

impl NotaHighlightKind {
    /// Every kind, in discriminant order (index = discriminant, test-guarded). Clients build
    /// kind→name/style tables from this — the *names* are client-side (the wasm bindings own the
    /// kebab-case table their `highlightKindNames()` serves; this crate only owns the wire enum).
    pub const ALL: [Self; 23] = [
        Self::Sigil,
        Self::TagHost,
        Self::TagComponent,
        Self::PropName,
        Self::Interpolation,
        Self::ControlKeyword,
        Self::HeadingMarker,
        Self::Heading,
        Self::ListMarker,
        Self::EmphasisStrong,
        Self::EmphasisEm,
        Self::MathDelim,
        Self::Math,
        Self::CodeDelim,
        Self::CodeLang,
        Self::Code,
        Self::Verbatim,
        Self::Escape,
        Self::JsKeyword,
        Self::JsString,
        Self::JsNumber,
        Self::JsComment,
        Self::JsOperator,
    ];
}

/// One classified source span (`[start, end)` byte offsets into the `.nota` source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotaHighlightSpan {
    pub start: u32,
    pub end: u32,
    pub kind: NotaHighlightKind,
}

// ================================================================================================
// Structural walk
// ================================================================================================

/// An embedded-JS subtree being walked: its source extent, plus the spans of any `NotaMarkup`
/// islands encountered inside it (markup re-entering the JS). On pop, `range − holes` becomes the
/// gap ranges handed to the lexical pump.
struct JsFrame {
    range: Span,
    holes: Vec<Span>,
}

/// The AST walker: emits structural spans and collects embedded-JS gap ranges.
struct Highlighter<'a> {
    source: &'a str,
    spans: Vec<NotaHighlightSpan>,
    frames: Vec<JsFrame>,
    js_ranges: Vec<Span>,
    /// Offset past the `%%%` fence whose markers were already emitted (each inner statement of a
    /// fence is its own `NotaStatement`; the markers must be emitted once).
    fence_done_end: u32,
    /// The inner extent end of that fence (the closing-fence line start).
    fence_inner_end: u32,
    /// The last single-`%` marker emitted (a `% a(); b();` line yields several statements).
    last_stmt_marker: u32,
}

/// Walk `program` (a parsed Nota document) → (structural spans, embedded-JS gap ranges).
pub fn collect_structural(
    source: &str,
    program: &Program<'_>,
) -> (Vec<NotaHighlightSpan>, Vec<Span>) {
    let mut hl = Highlighter {
        source,
        spans: Vec::new(),
        frames: Vec::new(),
        js_ranges: Vec::new(),
        fence_done_end: 0,
        fence_inner_end: 0,
        last_stmt_marker: u32::MAX,
    };
    hl.visit_program(program);
    (hl.spans, hl.js_ranges)
}

/// Start of the line containing `off`.
fn line_start_of(source: &str, off: u32) -> u32 {
    source[..off as usize].rfind('\n').map_or(0, |i| (i + 1) as u32)
}

/// First byte at/after `from` that is not a space/tab/newline.
fn skip_ws(source: &str, from: u32) -> u32 {
    let bytes = source.as_bytes();
    let mut p = from as usize;
    while p < bytes.len() && matches!(bytes[p], b' ' | b'\t' | b'\r' | b'\n') {
        p += 1;
    }
    p as u32
}

impl<'a> Highlighter<'a> {
    fn emit(&mut self, start: u32, end: u32, kind: NotaHighlightKind) {
        if end > start {
            self.spans.push(NotaHighlightSpan { start, end, kind });
        }
    }

    /// Register an embedded-JS subtree: walk it with `f` (markup islands inside punch holes via
    /// [`Visit::visit_nota_markup`]), then convert the un-holed remainder into lexing ranges.
    fn js_subtree(&mut self, range: Span, f: impl FnOnce(&mut Self)) {
        self.frames.push(JsFrame { range, holes: Vec::new() });
        f(self);
        let frame = self.frames.pop().expect("js frame pushed above");
        let mut at = frame.range.start;
        for hole in frame.holes {
            if hole.start > at {
                self.js_ranges.push(Span::new(at, hole.start.min(frame.range.end)));
            }
            at = at.max(hole.end);
        }
        if frame.range.end > at {
            self.js_ranges.push(Span::new(at, frame.range.end));
        }
    }

    /// Emit the `@` sigil + tag-name span of an element/verbatim head. Returns the head's end
    /// offset (past the name, or past a dynamic head's `)`), where a trigger may be glued.
    fn emit_head(&mut self, form_start: u32, tag: &NotaTag<'a>) -> u32 {
        match tag {
            NotaTag::Host(host) => {
                self.emit(form_start, host.span.start, NotaHighlightKind::Sigil);
                self.emit(host.span.start, host.span.end, NotaHighlightKind::TagHost);
                host.span.end
            }
            NotaTag::Component(comp) => {
                self.emit(form_start, comp.span.start, NotaHighlightKind::Sigil);
                self.emit(comp.span.start, comp.span.end, NotaHighlightKind::TagComponent);
                comp.span.end
            }
            NotaTag::Dynamic(dynamic) => {
                self.emit(form_start, form_start + 1, NotaHighlightKind::Sigil);
                let range = dynamic.expression.span();
                self.js_subtree(range, |v| v.visit_expression(&dynamic.expression));
                // Head end: past the `)` after the expression (whitespace tolerated inside).
                let close = skip_ws(self.source, range.end);
                if byte_at(self.source, close) == Some(b')') { close + 1 } else { close }
            }
        }
    }

    /// Emit the `%`/`%%%` markers owning the statement starting at `stmt_start`. A single-`%`
    /// statement starts on its own marker line; a fence's inner statements start on plain JS
    /// lines, so the opener is found by scanning up (only its `%%%` line matches `is_fence`).
    fn emit_statement_markers(&mut self, stmt_start: u32) {
        if stmt_start < self.fence_done_end {
            return; // inside a fence whose markers are already emitted
        }
        let line = line_start_of(self.source, stmt_start);
        match statement_kind(self.source, line) {
            Some((content, false)) => {
                let marker = content - 1; // `statement_kind` returns the offset just past the `%`
                if marker != self.last_stmt_marker {
                    self.last_stmt_marker = marker;
                    self.emit(marker, content, NotaHighlightKind::Sigil);
                }
            }
            Some((content, true)) => self.emit_fence_markers(line, content),
            None => {
                // A fence-inner statement line: scan up for the `%%%` opener.
                let mut at = line;
                while at > 0 {
                    at = line_start_of(self.source, at - 1);
                    if let Some((content, true)) = statement_kind(self.source, at) {
                        self.emit_fence_markers(at, content);
                        break;
                    }
                    if at == 0 {
                        break;
                    }
                }
            }
        }
    }

    /// Emit the opening + closing `%%%` runs of the fence whose opener line starts at
    /// `open_line` (inner statements begin at `inner_start`).
    fn emit_fence_markers(&mut self, open_line: u32, inner_start: u32) {
        let open = skip_ws(self.source, open_line);
        self.emit(open, sigil_run_end(self.source, open, b'%'), NotaHighlightKind::Sigil);
        let (inner_end, after_fence) = find_fence_close(self.source, inner_start);
        let close = skip_ws(self.source, inner_end);
        if byte_at(self.source, close) == Some(b'%') {
            self.emit(close, sigil_run_end(self.source, close, b'%'), NotaHighlightKind::Sigil);
        }
        self.fence_inner_end = inner_end;
        self.fence_done_end = after_fence;
    }

    /// Handle a run of consecutive `NotaChild::Statement` siblings: emit each one's `%`/`%%%`
    /// markers, then register each statement as an embedded-JS subtree whose extent *bridges* to
    /// the next statement — a statement's span stops at its last token, but the source up to the
    /// next statement (or the fence close / its line's end) is JS trivia (`// comments`,
    /// standalone comment lines inside a fence) and must reach the lexical pump.
    fn statement_run(&mut self, stmts: &[&NotaStatement<'a>]) {
        for (k, stmt) in stmts.iter().enumerate() {
            let start = stmt.span.start;
            self.emit_statement_markers(start);
            let in_fence = start < self.fence_done_end;
            let trailing = if in_fence {
                self.fence_inner_end
            } else {
                line_content_end(self.source, stmt.span.end)
            };
            let end = match stmts.get(k + 1) {
                Some(next) => {
                    let next_start = next.span.start;
                    let same_fence = in_fence && next_start < self.fence_done_end;
                    let same_line = !in_fence
                        && line_start_of(self.source, next_start)
                            == line_start_of(self.source, start);
                    if same_fence || same_line { next_start } else { trailing }
                }
                None => trailing,
            };
            self.js_subtree(Span::new(start, end.max(stmt.span.end)), |v| {
                v.visit_statement(&stmt.statement);
            });
        }
    }
}

impl<'a> Visit<'a> for Highlighter<'a> {
    /// A `NotaMarkup` umbrella is the (only) point where markup re-enters embedded JS: punch a
    /// hole in the innermost active JS frame, then walk the markup normally.
    fn visit_nota_markup(&mut self, it: &NotaMarkup<'a>) {
        if let Some(frame) = self.frames.last_mut() {
            frame.holes.push(it.span);
        }
        walk::walk_nota_markup(self, it);
    }

    /// Children walk with sibling context, needed twice over: (1) a text child sitting exactly
    /// one byte after its predecessor, with that byte a `\`, is the literal an escape produced
    /// (the reader drops the `\` from the child span) — emit [`NotaHighlightKind::Escape`] over
    /// both bytes; (2) consecutive statement children form a run whose JS extents bridge
    /// ([`Highlighter::statement_run`]).
    fn visit_nota_children(&mut self, it: &ArenaVec<'a, NotaChild<'a>>) {
        let mut prev_end: Option<u32> = None;
        let mut i = 0;
        while i < it.len() {
            if matches!(it[i], NotaChild::Statement(_)) {
                let mut j = i;
                while j + 1 < it.len() && matches!(it[j + 1], NotaChild::Statement(_)) {
                    j += 1;
                }
                let run: Vec<&NotaStatement<'a>> = it[i..=j]
                    .iter()
                    .map(|child| match child {
                        NotaChild::Statement(stmt) => &**stmt,
                        _ => unreachable!("run contains only statements"),
                    })
                    .collect();
                self.statement_run(&run);
                prev_end = Some(it[j].span().end);
                i = j + 1;
                continue;
            }
            let child = &it[i];
            if let NotaChild::Text(text) = child {
                let start = text.span.start;
                if start > 0
                    && byte_at(self.source, start - 1) == Some(b'\\')
                    && prev_end.is_none_or(|end| end == start - 1)
                {
                    self.emit(start - 1, text.span.end, NotaHighlightKind::Escape);
                }
            }
            self.visit_nota_child(child);
            prev_end = Some(child.span().end);
            i += 1;
        }
    }

    fn visit_nota_element(&mut self, it: &NotaElement<'a>) {
        let head_end = self.emit_head(it.span.start, &it.tag);
        if it.is_colon {
            if byte_at(self.source, head_end) == Some(b':') {
                self.emit(head_end, head_end + 1, NotaHighlightKind::Sigil);
            }
            // `|` prop-line markers (colon-sugar props live on leading `| …` body lines).
            let mut last_line = u32::MAX;
            for prop in &it.props {
                let line = line_start_of(self.source, prop.span().start);
                if line != last_line {
                    last_line = line;
                    if let Some(content) = colon_prop_line_at(self.source, line) {
                        self.emit(content - 1, content, NotaHighlightKind::Sigil);
                    }
                }
            }
        }
        for prop in &it.props {
            self.visit_nota_prop(prop);
        }
        self.visit_nota_children(&it.children);
    }

    fn visit_nota_prop_name(&mut self, it: &NotaPropName<'a>) {
        self.emit(it.span.start, it.span.end, NotaHighlightKind::PropName);
    }

    fn visit_nota_shorthand_prop(&mut self, it: &NotaShorthandProp<'a>) {
        self.emit(it.span.start, it.span.end, NotaHighlightKind::PropName);
    }

    fn visit_nota_spread_prop(&mut self, it: &NotaSpreadProp<'a>) {
        self.js_subtree(it.argument.span(), |v| v.visit_expression(&it.argument));
    }

    /// A prop value is embedded JS (→ a lexing range) or markup (→ the normal walk).
    fn visit_nota_prop_value(&mut self, it: &NotaPropValue<'a>) {
        if let NotaPropValue::Expression(expr) = it {
            self.js_subtree(expr.expression.span(), |v| v.visit_expression(&expr.expression));
        } else {
            walk::walk_nota_prop_value(self, it);
        }
    }

    /// `@{…}` fragment. Branch fragments of `@if`/`@for` are visited manually (their spans start
    /// at the form, not at `@{`), so the byte check only fires for real fragments.
    fn visit_nota_fragment(&mut self, it: &NotaFragment<'a>) {
        let start = it.span.start;
        if byte_at(self.source, start) == Some(b'@')
            && byte_at(self.source, start + 1) == Some(b'{')
        {
            self.emit(start, start + 1, NotaHighlightKind::Sigil);
        }
        self.visit_nota_children(&it.children);
    }

    fn visit_nota_interpolation(&mut self, it: &NotaInterpolation<'a>) {
        match &it.expression {
            // `@name` — sigil + the name. (The `@` sits one byte before the identifier.)
            Expression::Identifier(ident) => {
                let start = ident.span.start;
                if start > 0 && byte_at(self.source, start - 1) == Some(b'@') {
                    self.emit(start - 1, start, NotaHighlightKind::Sigil);
                }
                self.emit(start, ident.span.end, NotaHighlightKind::Interpolation);
            }
            // `@(expr)` — sigil (scan back over the `(`), then the expression as embedded JS.
            expr => {
                let bytes = self.source.as_bytes();
                let mut p = expr.span().start as usize;
                while p > 0 && matches!(bytes[p - 1], b' ' | b'\t' | b'\r' | b'\n') {
                    p -= 1;
                }
                if p >= 2 && bytes[p - 1] == b'(' && bytes[p - 2] == b'@' {
                    self.emit(p as u32 - 2, p as u32 - 1, NotaHighlightKind::Sigil);
                }
                self.js_subtree(expr.span(), |v| v.visit_expression(expr));
            }
        }
    }

    fn visit_nota_if(&mut self, it: &NotaIf<'a>) {
        let start = it.span.start;
        if byte_at(self.source, start) == Some(b'@') {
            self.emit(start, start + 1, NotaHighlightKind::Sigil);
            self.emit(start + 1, start + 3, NotaHighlightKind::ControlKeyword);
        } else {
            // An `else if` continuation: the node's span starts at its `if`.
            self.emit(start, start + 2, NotaHighlightKind::ControlKeyword);
        }
        self.js_subtree(it.test.span(), |v| v.visit_expression(&it.test));
        self.visit_nota_children(&it.consequent.children);
        if let Some(alternate) = &it.alternate {
            // The `else` keyword lives in the gap between the branch's `}` and the continuation.
            let else_at = skip_ws(self.source, it.consequent.span.end);
            if self.source[else_at as usize..].starts_with("else") {
                self.emit(else_at, else_at + 4, NotaHighlightKind::ControlKeyword);
            }
            match alternate {
                NotaElse::ElseIf(nested) => self.visit_nota_if(nested),
                NotaElse::Else(fragment) => self.visit_nota_children(&fragment.children),
            }
        }
    }

    fn visit_nota_for(&mut self, it: &NotaFor<'a>) {
        let start = it.span.start;
        self.emit(start, start + 1, NotaHighlightKind::Sigil);
        self.emit(start + 1, start + 4, NotaHighlightKind::ControlKeyword);
        self.js_subtree(it.binding.span(), |v| v.visit_binding_pattern(&it.binding));
        let of_at = skip_ws(self.source, it.binding.span().end);
        if self.source[of_at as usize..].starts_with("of") {
            self.emit(of_at, of_at + 2, NotaHighlightKind::ControlKeyword);
        }
        self.js_subtree(it.iterable.span(), |v| v.visit_expression(&it.iterable));
        self.visit_nota_children(&it.body.children);
    }

    fn visit_nota_heading(&mut self, it: &NotaHeading<'a>) {
        self.emit(it.span.start, it.span.end, NotaHighlightKind::Heading); // under-layer
        let marker = skip_ws(self.source, it.span.start);
        self.emit(marker, marker + u32::from(it.level), NotaHighlightKind::HeadingMarker);
        self.visit_nota_children(&it.children);
    }

    fn visit_nota_list_item(&mut self, it: &NotaListItem<'a>) {
        // The item span starts at its marker; `list_marker_at` treats the given offset as a line
        // start, which also covers R9's body-start items (`@{- x}` — the marker is mid-line, so
        // deriving from the real line start would miss it).
        if let Some(marker) = list_marker_at(self.source, it.span.start) {
            self.emit(marker.offset, marker.body_col - 1, NotaHighlightKind::ListMarker);
        }
        self.visit_nota_children(&it.children);
    }

    fn visit_nota_emphasis(&mut self, it: &NotaEmphasis<'a>) {
        let kind = match it.marker {
            NotaEmphasisMarker::Strong => NotaHighlightKind::EmphasisStrong,
            NotaEmphasisMarker::Em => NotaHighlightKind::EmphasisEm,
        };
        self.emit(it.span.start, it.span.end, kind); // under-layer (incl. the marker bytes)
        self.emit(it.span.start, it.span.start + 1, NotaHighlightKind::Sigil);
        self.emit(it.span.end - 1, it.span.end, NotaHighlightKind::Sigil);
        self.visit_nota_children(&it.children);
    }

    fn visit_nota_code(&mut self, it: &NotaCode<'a>) {
        // Re-scan the span for delimiter/lang geometry (the node keeps only values); paint the
        // interior from `parts`, so a `|@`-armed form inside gets its normal element/JS paints.
        if let CodeScan::Code { span, lang, content, .. } =
            lex_code_span(self.source, it.span.start)
        {
            self.emit(span.start, content.start, NotaHighlightKind::CodeDelim);
            if let Some(lang) = lang {
                let lang_start = (lang.as_ptr() as usize - self.source.as_ptr() as usize) as u32;
                self.emit(lang_start, lang_start + lang.len() as u32, NotaHighlightKind::CodeLang);
            }
            self.emit_raw_parts(&it.parts, NotaHighlightKind::Code);
            self.emit(content.end, span.end, NotaHighlightKind::CodeDelim);
        }
    }

    fn visit_nota_math(&mut self, it: &NotaMath<'a>) {
        // Delimiters are run-length (inline) or whole-line (fence), so re-scan for the geometry
        // rather than assuming a fixed width; the interior paints from `parts` like code/verbatim.
        if let MathScan::Math { span, content, .. } = lex_math_span(self.source, it.span.start) {
            self.emit(span.start, content.start, NotaHighlightKind::MathDelim);
            self.emit_raw_parts(&it.parts, NotaHighlightKind::Math);
            self.emit(content.end, span.end, NotaHighlightKind::MathDelim);
        }
    }

    fn visit_nota_verbatim(&mut self, it: &NotaVerbatim<'a>) {
        let head_end = self.emit_head(it.span.start, &it.tag);
        self.emit(head_end, head_end + 2, NotaHighlightKind::Sigil); // `|{`
        let end = it.span.end;
        if end >= 2 && &self.source[end as usize - 2..end as usize] == "}|" {
            self.emit(end - 2, end, NotaHighlightKind::Sigil);
        }
        self.emit_raw_parts(&it.parts, NotaHighlightKind::Verbatim);
    }
}

impl<'a> Highlighter<'a> {
    /// Paint the shared raw-span parts (verbatim / code / math): a raw run in `raw_kind`; a
    /// `|@`-armed `@`-form via the normal walk (its arming `|` a sigil, its embedded element/JS its
    /// own paints). One painter for all three spans, mirroring the unified content model.
    fn emit_raw_parts(
        &mut self,
        parts: &ArenaVec<'a, NotaVerbatimPart<'a>>,
        raw_kind: NotaHighlightKind,
    ) {
        for part in parts {
            if let NotaVerbatimPart::Raw(text) = part {
                self.emit(text.span.start, text.span.end, raw_kind);
            } else {
                // A `|@`-re-armed form: its span starts at the `@`; the arming `|` sits before it.
                let form_start = part.span().start;
                if form_start > 0 && byte_at(self.source, form_start - 1) == Some(b'|') {
                    self.emit(form_start - 1, form_start, NotaHighlightKind::Sigil);
                }
                walk::walk_nota_verbatim_part(self, part);
            }
        }
    }
}

// ================================================================================================
// Lexical pump: classify embedded-JS gap ranges with the crate's own lexer
// ================================================================================================

/// Classify a lexed [`Kind`] into a highlight kind; `None` = default (identifiers, plain text).
fn classify_js_kind(kind: Kind) -> Option<NotaHighlightKind> {
    if kind.is_any_keyword() {
        return Some(NotaHighlightKind::JsKeyword);
    }
    if kind.is_number() {
        return Some(NotaHighlightKind::JsNumber);
    }
    match kind {
        Kind::Str
        | Kind::NoSubstitutionTemplate
        | Kind::TemplateHead
        | Kind::TemplateMiddle
        | Kind::TemplateTail
        | Kind::RegExp => Some(NotaHighlightKind::JsString),
        Kind::True | Kind::False | Kind::Null => Some(NotaHighlightKind::JsKeyword),
        Kind::Ident | Kind::PrivateIdentifier | Kind::Eof | Kind::Undetermined | Kind::Skip => None,
        _ => Some(NotaHighlightKind::JsOperator),
    }
}

impl<C: Config> ParserImpl<'_, C> {
    /// Lex the (sorted, disjoint) `ranges` and classify each token; comments land in the lexer's
    /// trivia and are drained at the end. Lexing is bounded per range (the same source-end clamp
    /// the reader uses), so a range never leaks tokens past its extent.
    pub(crate) fn nota_lex_highlight_ranges(
        &mut self,
        ranges: &[Span],
        out: &mut Vec<NotaHighlightSpan>,
    ) {
        for &range in ranges {
            if range.end <= range.start {
                continue;
            }
            self.with_source_end_bound(range.end, |p| {
                p.nota_seek_to(range.start);
                p.pump_highlight_tokens(range, out);
            });
        }
        for comment in &self.lexer.trivia_builder.comments {
            out.push(NotaHighlightSpan {
                start: comment.span.start,
                end: comment.span.end,
                kind: NotaHighlightKind::JsComment,
            });
        }
    }

    /// Classify tokens until the range (or the bounded source) ends. Templates re-lex their
    /// substitution tails through the parser's own primitive, so `}` closing a `${…}` is a
    /// template part, not a brace.
    fn pump_highlight_tokens(&mut self, range: Span, out: &mut Vec<NotaHighlightSpan>) {
        loop {
            let token = self.cur_token();
            let kind = self.cur_kind();
            if kind == Kind::Eof || token.start() >= range.end || self.has_fatal_error() {
                return;
            }
            if let Some(hl) = classify_js_kind(kind) {
                out.push(NotaHighlightSpan {
                    start: token.start(),
                    end: token.end().min(range.end),
                    kind: hl,
                });
            }
            if kind == Kind::TemplateHead {
                self.pump_template_substitutions(range, out);
            } else {
                self.bump_any();
            }
        }
    }

    /// Entered at a lexed `TemplateHead`: classify the substitution's tokens, re-lexing each
    /// depth-0 `}` as a template middle/tail; returns with the cursor past the closing tail.
    fn pump_template_substitutions(&mut self, range: Span, out: &mut Vec<NotaHighlightSpan>) {
        self.bump_any(); // into the first `${…}` substitution
        let mut depth = 0u32;
        loop {
            let token = self.cur_token();
            let kind = self.cur_kind();
            if kind == Kind::Eof || token.start() >= range.end || self.has_fatal_error() {
                return;
            }
            if kind == Kind::RCurly && depth == 0 {
                self.re_lex_template_substitution_tail();
                let tail = self.cur_token();
                let tail_kind = self.cur_kind();
                if let Some(hl) = classify_js_kind(tail_kind) {
                    out.push(NotaHighlightSpan {
                        start: tail.start(),
                        end: tail.end().min(range.end),
                        kind: hl,
                    });
                }
                self.bump_any();
                if tail_kind == Kind::TemplateMiddle {
                    continue; // next substitution
                }
                return; // tail (or recovery): template done
            }
            if let Some(hl) = classify_js_kind(kind) {
                out.push(NotaHighlightSpan {
                    start: token.start(),
                    end: token.end().min(range.end),
                    kind: hl,
                });
            }
            match kind {
                Kind::LCurly => {
                    depth += 1;
                    self.bump_any();
                }
                Kind::RCurly => {
                    depth -= 1;
                    self.bump_any();
                }
                Kind::TemplateHead => self.pump_template_substitutions(range, out),
                _ => self.bump_any(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_span::SourceType;

    use super::NotaHighlightKind as K;
    use crate::Parser;

    /// Highlight `source` → `(kind, excerpt)` pairs in paint order.
    fn hl(source: &str) -> Vec<(K, String)> {
        let allocator = Allocator::default();
        let spans = Parser::new(&allocator, source, SourceType::nota())
            .parse_nota_highlights()
            .expect("test doc must parse");
        spans
            .iter()
            .map(|s| (s.kind, source[s.start as usize..s.end as usize].to_string()))
            .collect()
    }

    fn has(spans: &[(K, String)], kind: K, text: &str) -> bool {
        spans.iter().any(|(k, t)| *k == kind && t == text)
    }

    #[test]
    fn element_heads_props_and_interpolation() {
        let spans = hl("Hi @em{x}, @Aside[k: 1]{y}, and @name.\n");
        assert!(has(&spans, K::Sigil, "@"));
        assert!(has(&spans, K::TagHost, "em"));
        assert!(has(&spans, K::TagComponent, "Aside"));
        assert!(has(&spans, K::PropName, "k"));
        assert!(has(&spans, K::JsNumber, "1"));
        assert!(has(&spans, K::Interpolation, "name"));
    }

    #[test]
    fn markup_valued_prop_does_not_poison_the_document() {
        // The TextMate grammar's fatal case: `@em{…}` inside `[props]` swallowed the rest of the
        // file as TS. The reader-driven pass must keep classifying past it.
        let spans = hl("@figure[cap: @em{a caption}]{body}\n\n# After\n");
        assert!(has(&spans, K::TagHost, "figure"));
        assert!(has(&spans, K::PropName, "cap"));
        assert!(has(&spans, K::TagHost, "em"));
        assert!(has(&spans, K::Heading, "# After"));
        assert!(has(&spans, K::HeadingMarker, "#"));
        // Nothing after the prop may be classified as embedded JS.
        assert!(
            !spans
                .iter()
                .any(|(k, t)| matches!(k, K::JsKeyword | K::JsString) && t.contains("After"))
        );
    }

    #[test]
    fn stray_brackets_in_prose_stay_plain() {
        let spans = hl("see [1] and {2} here\n\n# H\n");
        assert!(has(&spans, K::Heading, "# H"));
        assert!(!spans.iter().any(|(k, _)| matches!(
            k,
            K::JsKeyword | K::JsString | K::JsNumber | K::JsOperator | K::PropName
        )));
    }

    #[test]
    fn percent_statement_with_markup_reentry() {
        // The golden's shape: a multi-line `%` statement whose JS contains an `@`-form. JS
        // classifies as JS, the markup island as markup, and the tail after it as JS again.
        let src = "%let X = inlineComponent((c) => {\n  return @span[onClick: f]{@c};\n})\nprose\n";
        let spans = hl(src);
        assert!(has(&spans, K::Sigil, "%"));
        assert!(has(&spans, K::JsKeyword, "let"));
        assert!(has(&spans, K::JsKeyword, "return"));
        assert!(has(&spans, K::TagHost, "span"));
        assert!(has(&spans, K::PropName, "onClick"));
        assert!(has(&spans, K::Interpolation, "c"));
    }

    #[test]
    fn fence_markers_and_heading_after() {
        let spans = hl("%%%\nconst x = 1; // note\n%%%\n\n# H\n");
        assert_eq!(spans.iter().filter(|(k, t)| *k == K::Sigil && t == "%%%").count(), 2);
        assert!(has(&spans, K::JsKeyword, "const"));
        assert!(has(&spans, K::JsComment, "// note"));
        assert!(has(&spans, K::Heading, "# H"));
    }

    #[test]
    fn colon_block_with_pipe_props() {
        let spans = hl("@section:\n  | class: \"tip\"\n  body *b*\n");
        assert!(has(&spans, K::TagHost, "section"));
        assert!(has(&spans, K::Sigil, ":"));
        assert!(has(&spans, K::Sigil, "|"));
        assert!(has(&spans, K::PropName, "class"));
        assert!(has(&spans, K::JsString, "\"tip\""));
        assert!(has(&spans, K::EmphasisStrong, "*b*"));
    }

    #[test]
    fn body_start_list_marker() {
        // R9: a body opening with a marker (`@{- x}`) — the marker is mid-line and must still
        // classify.
        let spans = hl("@{- item} and @div{- other}\n");
        assert_eq!(spans.iter().filter(|(k, t)| *k == K::ListMarker && t == "-").count(), 2);
    }

    #[test]
    fn multi_statement_percent_line() {
        // `% a(); b();` — the rest of the line is JS: one `%` marker, both statements classified.
        let spans = hl("% let a = 1; let b = 2;\nprose\n");
        assert_eq!(spans.iter().filter(|(k, t)| *k == K::Sigil && t == "%").count(), 1);
        assert_eq!(spans.iter().filter(|(k, t)| *k == K::JsKeyword && t == "let").count(), 2);
    }

    #[test]
    fn heading_after_colon_block() {
        // TODO.md bug 7: sugar directly after a colon-sugar body (mega.nota's
        // `## Nested statements`) must classify as a heading, not literal text.
        let spans = hl("@section:\n  body\n\n## After\n");
        assert!(has(&spans, K::Heading, "## After"));
        assert!(has(&spans, K::HeadingMarker, "##"));
    }

    #[test]
    fn verbatim_with_rearm() {
        let spans = hl("@pre|{\nraw |@em{x} tail\n}|\n");
        assert!(has(&spans, K::TagHost, "pre"));
        assert!(has(&spans, K::Sigil, "|{"));
        assert!(has(&spans, K::Sigil, "}|"));
        assert!(has(&spans, K::Sigil, "|")); // the re-arm pipe
        assert!(has(&spans, K::TagHost, "em"));
        assert!(spans.iter().any(|(k, t)| *k == K::Verbatim && t.contains("raw ")));
        assert!(spans.iter().any(|(k, t)| *k == K::Verbatim && t.contains(" tail")));
    }

    #[test]
    fn math_inline_armed_and_fence() {
        // `|@` arms a form inside math (a bare `@` would be literal raw text now); a display fence
        // is standalone `$$` lines. Inline delimiters are the `$` runs; the fence opener paints as
        // a whole-line-leading `$$` delimiter.
        let spans = hl("$a_|@em{i}$ and $$\n\\sum x\n$$\n");
        assert!(has(&spans, K::MathDelim, "$")); // inline delimiter
        assert!(has(&spans, K::Sigil, "|")); // the arming pipe (element span starts at `@`)
        assert!(has(&spans, K::TagHost, "em")); // the armed element
        assert!(spans.iter().any(|(k, t)| *k == K::Math && t.contains("a_")));
        assert!(spans.iter().any(|(k, t)| *k == K::Math && t.contains("\\sum x")));
        assert!(spans.iter().any(|(k, t)| *k == K::MathDelim && t.starts_with("$$")));
    }

    #[test]
    fn code_inline_and_fenced() {
        let spans = hl("`f(x)` and\n```python\ndef g(): pass\n```\n");
        assert!(has(&spans, K::Code, "f(x)"));
        assert!(has(&spans, K::CodeLang, "python"));
        assert!(spans.iter().any(|(k, t)| *k == K::Code && t.contains("def g(): pass")));
        assert!(spans.iter().filter(|(k, _)| *k == K::CodeDelim).count() >= 4);
    }

    #[test]
    fn headings_lists_emphasis() {
        let spans = hl("## Two *b*\n\n- a\n+ b\n2. c\n_i_\n");
        assert!(has(&spans, K::HeadingMarker, "##"));
        assert!(has(&spans, K::Heading, "## Two *b*"));
        assert!(has(&spans, K::EmphasisStrong, "*b*"));
        assert!(has(&spans, K::EmphasisEm, "_i_"));
        assert!(has(&spans, K::ListMarker, "-"));
        assert!(has(&spans, K::ListMarker, "+"));
        assert!(has(&spans, K::ListMarker, "2."));
    }

    #[test]
    fn control_flow_keywords() {
        let spans = hl("@if (a > 1) {X} else if (b) {Y} else {Z}\n@for (x of xs) {@x}\n");
        assert!(has(&spans, K::ControlKeyword, "if"));
        assert!(has(&spans, K::ControlKeyword, "else"));
        assert!(has(&spans, K::ControlKeyword, "for"));
        assert!(has(&spans, K::ControlKeyword, "of"));
        assert!(has(&spans, K::JsNumber, "1"));
        assert!(has(&spans, K::Interpolation, "x"));
    }

    #[test]
    fn escapes() {
        let spans = hl("Escapes: \\@ \\{ and \\\\.\n");
        assert!(has(&spans, K::Escape, "\\@"));
        assert!(has(&spans, K::Escape, "\\{"));
        assert!(has(&spans, K::Escape, "\\\\"));
    }

    #[test]
    fn template_literals_in_props() {
        let spans = hl("@a[s: `x${y}z`]{t}\n");
        assert!(has(&spans, K::JsString, "`x${"));
        assert!(has(&spans, K::JsString, "}z`"));
        assert!(!has(&spans, K::JsString, "y"));
    }

    #[test]
    fn kind_all_is_in_discriminant_order() {
        for (i, kind) in super::NotaHighlightKind::ALL.iter().enumerate() {
            assert_eq!(*kind as usize, i, "ALL[{i}] = {kind:?} out of discriminant order");
        }
    }

    #[test]
    fn spans_are_sorted_outer_first() {
        let src = "# H *b*\n\n%let x = 1\n@em{y}\n";
        let allocator = Allocator::default();
        let spans =
            Parser::new(&allocator, src, SourceType::nota()).parse_nota_highlights().unwrap();
        for pair in spans.windows(2) {
            assert!(
                pair[0].start < pair[1].start
                    || (pair[0].start == pair[1].start && pair[0].end >= pair[1].end),
                "not sorted outer-first: {pair:?}"
            );
        }
    }
}
