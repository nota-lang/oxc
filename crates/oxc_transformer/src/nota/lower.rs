//! Nota AST → Solid JSX lowering — a standalone pass over the parsed `Program`.
//!
//! The reader ([`oxc_parser`]'s nota module) leaves a document as a single
//! `Expression::NotaMarkup(Document)` statement and every embedded `@`-form in place as
//! `Expression::NotaMarkup`. This pass lowers those to **JSX** `Expression` AST (design/solid.md
//! §The pipeline): [`NotaLowering::lower_document_program`] rebuilds the document `Program`
//! (Doc skeleton, `%` routing), then a [`VisitMut`] walk replaces
//! each remaining embedded `NotaMarkup` bottom-up (the lowered result is re-walked, so a `@`-form
//! nested inside embedded JS inside another `@`-form lowers too). Lowering *consumes* owned Nota
//! nodes via `unbox()`. The emit primitives live in [`super::build`]; the whitespace algorithm in
//! [`super::scribble`].

use itertools::Itertools;
use oxc_allocator::{Allocator, Vec as ArenaVec};
use oxc_ast::{AstBuilder, ast::*};
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_diagnostics::OxcDiagnostic;
use oxc_span::{GetSpan, Span};

use super::build;
use super::mapping::{NotaMappingKind, NotaMappingMark};
use super::scribble;

/// The result of a Nota lowering pass: Volar mapping marks + lowering diagnostics.
///
/// The diagnostics are semantic facts about the *lowered* module the reader cannot see at parse
/// time: a user binding colliding with a reader-injected emit-surface name (`Doc`, the runtime
/// imports), or a second `export default`.
#[derive(Debug)]
pub struct NotaLoweringReturn {
    /// Source⇄generated mapping marks (ascending source offset).
    pub mappings: Vec<NotaMappingMark>,
    /// Name-collision / duplicate-default-export diagnostics (empty on a clean document).
    pub diagnostics: Vec<OxcDiagnostic>,
}

/// The Nota lowering pass. `collect_mappings` gates Volar CodeMapping mark collection (off for the
/// plain build path → allocation-free).
pub struct NotaLowering<'a> {
    pub(super) ast: AstBuilder<'a>,
    pub(super) source_text: &'a str,
    mappings: Vec<NotaMappingMark>,
    diagnostics: Vec<OxcDiagnostic>,
    collect_mappings: bool,
}

impl<'a> NotaLowering<'a> {
    pub fn new(allocator: &'a Allocator, source_text: &'a str, collect_mappings: bool) -> Self {
        Self {
            ast: AstBuilder::new(allocator),
            source_text,
            mappings: Vec::new(),
            diagnostics: Vec::new(),
            collect_mappings,
        }
    }

    /// Lower a parsed document `Program` in place.
    pub fn lower_document_program(mut self, program: &mut Program<'a>) -> NotaLoweringReturn {
        if let Some(document) = self.take_document(program) {
            *program = self.lower_document(document);
        }
        self.visit_program(program);
        self.finish()
    }

    /// Lower a parsed expression-mode form in place.
    pub fn lower_expression(mut self, expr: &mut Expression<'a>) -> NotaLoweringReturn {
        self.visit_expression(expr);
        self.finish()
    }

    /// Record a lowering diagnostic (reserved-name collision / duplicate default export).
    pub(super) fn error(&mut self, diagnostic: OxcDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn finish(self) -> NotaLoweringReturn {
        let mut marks = self.mappings;
        // Volar wants ascending source offsets (the walk visits children before some siblings).
        marks.sort_by_key(|m| (m.span.start, m.span.end));
        NotaLoweringReturn { mappings: marks, diagnostics: self.diagnostics }
    }

    /// Record a source→generated mapping mark, iff collection is on. Empty spans (synthesized
    /// boilerplate) carry no source and are dropped.
    pub(super) fn record_nota_mapping(&mut self, span: Span, kind: NotaMappingKind) {
        if self.collect_mappings && !span.is_empty() {
            self.mappings.push(NotaMappingMark::new(span, kind));
        }
    }

    /// Extract the `NotaDocument` from the reader's wrapper program
    /// (`[ExpressionStatement(NotaMarkup(Document))]`); `None` if the program is not that shape.
    fn take_document(&self, program: &mut Program<'a>) -> Option<NotaDocument<'a>> {
        if program.body.len() != 1 {
            return None;
        }
        let Statement::ExpressionStatement(es) = &mut program.body[0] else { return None };
        let is_document = matches!(
            &es.expression,
            Expression::NotaMarkup(markup) if matches!(markup.kind, NotaMarkupKind::Document(_))
        );
        if !is_document {
            return None;
        }
        let taken =
            std::mem::replace(&mut es.expression, self.ast.expression_null_literal(Span::empty(0)));
        let Expression::NotaMarkup(markup) = taken else { unreachable!() };
        let NotaMarkupKind::Document(document) = markup.unbox().kind else { unreachable!() };
        Some(document.unbox())
    }

    // ===========================================================================================
    // Dispatch
    // ===========================================================================================

    /// Lower a markup form in expression position.
    fn lower_markup(&mut self, markup: NotaMarkup<'a>) -> Expression<'a> {
        match markup.kind {
            // A document lowers via `lower_document`; one can only reach here from a parse-error
            // placeholder (the result is discarded).
            NotaMarkupKind::Document(_) => {
                self.ast.expression_null_literal(Span::empty(markup.span.start))
            }
            kind => self.lower_form(kind.into_nota_form()),
        }
    }

    /// Lower one `@`-form — the single dispatch every form-holding position
    /// (`NotaMarkupKind`/`NotaChild`/`NotaPropValue`/`NotaVerbatimPart`) funnels into.
    fn lower_form(&mut self, form: NotaForm<'a>) -> Expression<'a> {
        match form {
            NotaForm::Element(e) => self.lower_element(e.unbox()),
            NotaForm::Fragment(f) => self.lower_fragment(f.unbox(), true),
            NotaForm::Interpolation(i) => self.lower_interpolation(i.unbox()),
            NotaForm::If(n) => self.lower_if(n.unbox()),
            NotaForm::For(n) => self.lower_for(n.unbox()),
            NotaForm::Code(c) => self.lower_code(c.unbox()),
            NotaForm::Math(m) => self.lower_math(m.unbox()),
            NotaForm::Verbatim(v) => self.lower_verbatim(v.unbox()),
        }
    }

    /// Lower a single body child (never `Text`/`Statement` — those belong to the
    /// whitespace/statement machinery in [`Self::lower_children`]).
    fn lower_child(&mut self, child: NotaChild<'a>) -> Expression<'a> {
        match child {
            NotaChild::Emphasis(e) => self.lower_emphasis(e.unbox()),
            NotaChild::Heading(h) => self.lower_heading(h.unbox()),
            NotaChild::ListItem(li) => self.lower_list_item(li.unbox()),
            NotaChild::DocState(d) => self.lower_doc_state(d.unbox()),
            NotaChild::ThematicBreak(t) => self.lower_thematic_break(&t),
            NotaChild::Link(l) => self.lower_link(l.unbox()),
            NotaChild::Image(i) => self.lower_image(&i),
            NotaChild::Attrs(a) => self.lower_attrs_marker(a.unbox()),
            NotaChild::Text(_) | NotaChild::Statement(_) => {
                unreachable!("Text/Statement handled by lower_children")
            }
            form @ (match_nota_form!(NotaChild)) => self.lower_form(form.into_nota_form()),
        }
    }

    // ===========================================================================================
    // Children (the Scribble whitespace bridge + `%`-statement scoping)
    // ===========================================================================================

    /// Lower a body's children, applying the Scribble whitespace algorithm. A `%`/`%%%` statement
    /// child scopes the *remaining* siblings into an IIFE:
    /// `(() => { …stmts…; return Fragment(...rest); })()`.
    fn lower_children(
        &mut self,
        items: ArenaVec<'a, NotaChild<'a>>,
        is_brace: bool,
    ) -> ArenaVec<'a, Expression<'a>> {
        let mut segs: Vec<scribble::Seg<'a, Expression<'a>>> = Vec::with_capacity(items.len());
        let mut iter = items.into_iter().peekable();
        while let Some(child) = iter.next() {
            match child {
                NotaChild::Text(t) => segs.push(scribble::Seg::Text(t.unbox().value.as_str())),
                NotaChild::Statement(first) => {
                    // Peel the statement run; the remaining siblings lower recursively into the
                    // IIFE's returned Fragment.
                    let mut stmts = self.ast.vec1(first.unbox().statement);
                    stmts.extend(
                        iter.peeking_take_while(|c| matches!(c, NotaChild::Statement(_))).map(
                            |c| {
                                let NotaChild::Statement(s) = c else { unreachable!() };
                                s.unbox().statement
                            },
                        ),
                    );
                    let tail = self.ast.vec_from_iter(iter.by_ref());
                    let rest = self.lower_children(tail, is_brace);
                    segs.push(scribble::Seg::Elem(self.build_statement_iife(stmts, rest)));
                    break; // `iter` was drained into the IIFE
                }
                other => segs.push(scribble::Seg::Elem(self.lower_child(other))),
            }
        }
        self.scribble_emit(segs, is_brace)
    }

    /// Run the Scribble algorithm over the segments and materialize the child expressions.
    fn scribble_emit(
        &self,
        segs: Vec<scribble::Seg<'a, Expression<'a>>>,
        is_brace: bool,
    ) -> ArenaVec<'a, Expression<'a>> {
        let children = scribble::lower(segs, is_brace);
        let mut out = self.ast.vec_with_capacity(children.len());
        for child in children {
            out.push(match child {
                scribble::Child::Text(s) => {
                    let value: &'a str = self.ast.allocator.alloc_str(&s);
                    self.ast.expression_string_literal(Span::empty(0), value, None)
                }
                scribble::Child::Elem(e) => e,
            });
        }
        out
    }

    // ===========================================================================================
    // Element + props
    // ===========================================================================================

    fn lower_element(&mut self, el: NotaElement<'a>) -> Expression<'a> {
        let NotaElement { span, tag, props, children, is_colon, props_recovery, .. } = el;
        let props = self.lower_attrs(props);
        // The whitespace regime follows the body syntax: a brace body keeps the spaces between
        // `{`/`}` and text as content; a colon body trims its edges like a document/block.
        let children = self.lower_children(children, !is_colon);
        // EOF error-recovery completion anchor: for an unclosed `[props]` group, give the JSX
        // opening element a *real* span (the source `[`) so codegen logs its position, and record
        // a `PropsAnchor` mark the join turns into a zero-width attribute-completion anchor just
        // inside the opening tag. Well-formed elements keep the unmapped `Span::empty` opening.
        let recovery_span = props_recovery.inspect(|bracket| {
            self.record_nota_mapping(*bracket, NotaMappingKind::PropsAnchor);
        });
        self.lower_tagged(span, tag, props, children, recovery_span)
    }

    /// Shared host/component/dynamic tag dispatch into [`super::build::JsxTag`] — a host tag is a
    /// lowercase intrinsic JSX name, a component tag an identifier reference, and a dynamic head
    /// rides on `<Dynamic component={expr}>`. `recovery_span` is the unclosed `[`'s span on the
    /// EOF-recovery path (the prop-completion anchor), else `None`.
    fn lower_tagged(
        &mut self,
        span: Span,
        tag: NotaTag<'a>,
        props: ArenaVec<'a, JSXAttributeItem<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
        recovery_span: Option<Span>,
    ) -> Expression<'a> {
        let tag = match tag {
            NotaTag::Host(h) => {
                let h = h.unbox();
                build::JsxTag::Host { name: h.name.as_str(), span: h.span }
            }
            NotaTag::Component(id) => {
                let id = id.unbox();
                self.record_nota_mapping(id.span, NotaMappingKind::ComponentIdentifier);
                build::JsxTag::Component(id)
            }
            NotaTag::Dynamic(d) => {
                let expr = d.unbox().expression;
                self.record_nota_mapping(expr.span(), NotaMappingKind::EmbeddedJs);
                build::JsxTag::Dynamic(expr)
            }
        };
        self.build_element(span, tag, props, children, recovery_span)
    }

    fn lower_attrs(
        &mut self,
        props: ArenaVec<'a, NotaProp<'a>>,
    ) -> ArenaVec<'a, JSXAttributeItem<'a>> {
        let mut out = self.ast.vec_with_capacity(props.len());
        for prop in props {
            out.push(match prop {
                NotaProp::Field(f) => {
                    let NotaFieldProp { span, name, value, .. } = f.unbox();
                    let value = match value {
                        NotaPropValue::Expression(e) => {
                            let expr = e.unbox().expression;
                            self.record_nota_mapping(expr.span(), NotaMappingKind::EmbeddedJs);
                            expr
                        }
                        // A markup-valued prop (`key: @em{..}`) — the inherited form variants.
                        form => self.lower_form(form.into_nota_form()),
                    };
                    self.jsx_attr(span, name.span, name.name.as_str(), Some(value))
                }
                NotaProp::Shorthand(s) => {
                    // `[foo]` shorthand: the prop takes the in-scope binding — `foo={foo}` (NOT a
                    // bare JSX boolean attribute, which would mean `foo={true}`).
                    let id = s.unbox().name;
                    let (name, key_span) = (id.name.as_str(), id.span);
                    self.record_nota_mapping(key_span, NotaMappingKind::EmbeddedJs);
                    let value = Expression::Identifier(self.ast.alloc(id));
                    self.jsx_attr(key_span, key_span, name, Some(value))
                }
                NotaProp::Spread(sp) => {
                    let NotaSpreadProp { span, argument, .. } = sp.unbox();
                    self.record_nota_mapping(argument.span(), NotaMappingKind::EmbeddedJs);
                    self.jsx_spread_attr(span, argument)
                }
            });
        }
        out
    }

    // ===========================================================================================
    // Fragment / interpolation / control flow
    // ===========================================================================================

    fn lower_fragment(&mut self, f: NotaFragment<'a>, is_brace: bool) -> Expression<'a> {
        let span = f.span;
        let children = self.lower_children(f.children, is_brace);
        self.build_fragment(span, children)
    }

    fn lower_interpolation(&mut self, i: NotaInterpolation<'a>) -> Expression<'a> {
        self.record_nota_mapping(i.expression.span(), NotaMappingKind::EmbeddedJs);
        i.expression
    }

    /// `@if` → `<Show when={test}>` (design/solid.md), Solid's native conditional — `else` becomes
    /// the `fallback` prop and `else if` nests another `<Show>` inside it, exactly where the
    /// nested ternary this replaces used to go. No branch to fall back to ⇒ no `fallback` prop.
    ///
    /// `<Show>` over a ternary because Solid's JSX compiler treats the two differently: an
    /// interpolated ternary is one memo over the whole conditional, so *any* change to `test`
    /// re-runs it, while `<Show>` only tears down and rebuilds when `when` crosses truthiness.
    /// Branches stay keyless — a single branch needs no list reconciliation, and unkeyed `<Show>`
    /// is the ternary-matching semantics (`keyed` would rebuild on every distinct truthy value).
    fn lower_if(&mut self, n: NotaIf<'a>) -> Expression<'a> {
        let NotaIf { span, test, consequent, alternate, .. } = n;
        self.record_nota_mapping(test.span(), NotaMappingKind::EmbeddedJs);
        let cons = self.lower_fragment(consequent.unbox(), false);
        let alt = match alternate {
            None => None,
            Some(NotaElse::ElseIf(b)) => Some(self.lower_if(b.unbox())),
            Some(NotaElse::Else(b)) => Some(self.lower_fragment(b.unbox(), false)),
        };
        self.build_show(span, test, cons, alt)
    }

    /// `@for (bind of iter) {body}` → `<For each={iter}>{(bind) => <>…body…</>}</For>` — Solid's
    /// keyed list rendering (design/solid.md; the old map-index Fragment key is gone: Solid has
    /// no `key` prop, `<For>` reconciles by item identity).
    fn lower_for(&mut self, n: NotaFor<'a>) -> Expression<'a> {
        let NotaFor { span, binding, iterable, body, .. } = n;
        self.record_nota_mapping(binding.span(), NotaMappingKind::EmbeddedJs);
        self.record_nota_mapping(iterable.span(), NotaMappingKind::EmbeddedJs);
        let children = self.lower_children(body.unbox().children, false);
        self.build_for(span, binding, iterable, children)
    }

    // ===========================================================================================
    // Verbatim / code / math (→ `String.raw` templates + ambient prelude tags)
    // ===========================================================================================

    /// Lower the shared raw-span parts (verbatim / code / math): each raw run → a `String.raw`
    /// child (`build_string_raw` handles template escaping / the cooked-literal fallback), each
    /// `|@`-armed form → its lowered sibling. One lowering for the unified content model.
    fn lower_raw_parts(
        &mut self,
        parts: ArenaVec<'a, NotaVerbatimPart<'a>>,
    ) -> ArenaVec<'a, Expression<'a>> {
        let mut children = self.ast.vec_with_capacity(parts.len());
        for part in parts {
            children.push(match part {
                NotaVerbatimPart::Raw(t) => {
                    let t = t.unbox();
                    self.build_string_raw(t.span, t.value.as_str())
                }
                // A `|@`-re-entered form — the inherited form variants.
                form => self.lower_form(form.into_nota_form()),
            });
        }
        children
    }

    fn lower_code(&mut self, c: NotaCode<'a>) -> Expression<'a> {
        let NotaCode { span, lang, block, parts, .. } = c;
        let children = self.lower_raw_parts(parts);
        if block {
            let mut props = self.ast.vec();
            if let Some(lang) = lang {
                let val = self.ast.expression_string_literal(Span::empty(0), lang.as_str(), None);
                props.push(self.jsx_attr(Span::empty(0), Span::empty(0), "lang", Some(val)));
            }
            self.build_named_element(span, super::CODE_BLOCK, props, children)
        } else {
            self.build_named_element(span, super::CODE_INLINE, self.ast.vec(), children)
        }
    }

    fn lower_math(&mut self, m: NotaMath<'a>) -> Expression<'a> {
        let NotaMath { span, block, parts, .. } = m;
        let children = self.lower_raw_parts(parts);
        // The runtime prop is `display` (the AST field renamed to `block` to mirror `NotaCode`);
        // a bare JSX attribute is `display={true}` — exactly the `$$` fence's meaning.
        let mut props = self.ast.vec();
        if block {
            props.push(self.jsx_attr(Span::empty(0), Span::empty(0), "display", None));
        }
        self.build_named_element(span, super::MATH, props, children)
    }

    fn lower_verbatim(&mut self, v: NotaVerbatim<'a>) -> Expression<'a> {
        let NotaVerbatim { span, tag, props, parts, .. } = v;
        let props = self.lower_attrs(props);
        let children = self.lower_raw_parts(parts);
        // A verbatim body cannot leave an unclosed `[props]` group, so no recovery anchor.
        self.lower_tagged(span, tag, props, children, None)
    }

    // ===========================================================================================
    // Surface sugar → host elements
    // ===========================================================================================

    fn lower_emphasis(&mut self, e: NotaEmphasis<'a>) -> Expression<'a> {
        let NotaEmphasis { span, marker, children, .. } = e;
        let tag_name = match marker {
            NotaEmphasisMarker::Strong => "strong",
            NotaEmphasisMarker::Em => "em",
            NotaEmphasisMarker::Strike => "s",
        };
        let children = self.lower_children(children, true);
        self.build_element(
            span,
            build::JsxTag::Host { name: tag_name, span: Span::empty(span.start) },
            self.ast.vec(),
            children,
            None,
        )
    }

    /// A flow-position attrs group (notation.md §Attrs) → the ambient `<Attrs …/>` marker, which
    /// the runtime's Reforest pass strips and applies to the paragraph it is forming. (Trailing
    /// groups in heading/list-item bodies never reach here — [`Self::take_trailing_attrs`] hoists
    /// them onto the construct.)
    fn lower_attrs_marker(&mut self, a: NotaAttrs<'a>) -> Expression<'a> {
        let props = self.lower_attrs(a.props);
        self.build_named_element(a.span, super::ATTRS, props, self.ast.vec())
    }

    /// Detach a **trailing** attrs child (the last child modulo whitespace-only text) from a
    /// sugar construct's body, for hoisting onto the construct's own element.
    fn take_trailing_attrs(
        &self,
        children: &mut ArenaVec<'a, NotaChild<'a>>,
    ) -> Option<oxc_allocator::Box<'a, NotaAttrs<'a>>> {
        let idx = children.iter().rposition(|c| match c {
            NotaChild::Text(t) => !t.value.as_str().trim().is_empty(),
            _ => true,
        })?;
        if matches!(children[idx], NotaChild::Attrs(_)) {
            let NotaChild::Attrs(a) = children.remove(idx) else { unreachable!() };
            Some(a)
        } else {
            None
        }
    }

    /// `#` heading *sugar* → `<Heading rank={N}>…</Heading>`: `Heading` is an ambient-prelude
    /// component referenced as a free identifier (mirroring `Tex`/`CodeInline`), `rank` the level
    /// as a numeric literal. Raw `@hN{…}` element forms lower via [`Self::lower_element`] and stay
    /// plain host tags — the unnumbered/un-Toc'd escape hatch. A trailing attrs group
    /// (`# Title [id: "intro"]`) hoists onto the `<Heading>` call after `rank` — the prelude
    /// forwards `id` and spreads the rest onto the rendered `<hN>`.
    fn lower_heading(&mut self, h: NotaHeading<'a>) -> Expression<'a> {
        let NotaHeading { span, level, mut children, .. } = h;
        let attrs = self.take_trailing_attrs(&mut children);
        let children = self.lower_children(children, false);
        let rank = self.ast.expression_numeric_literal(
            Span::empty(span.start),
            f64::from(level),
            None,
            NumberBase::Decimal,
        );
        let mut props = self.ast.vec1(self.jsx_attr(
            Span::empty(span.start),
            Span::empty(span.start),
            "rank",
            Some(rank),
        ));
        if let Some(attrs) = attrs {
            props.extend(self.lower_attrs(attrs.unbox().props));
        }
        self.build_named_element(span, super::HEADING, props, children)
    }

    /// Doc-state sugar (notation.md §Doc-state references) → `<Slot key="label">children</Slot>`,
    /// the same ambient pattern as `Heading`: the component is an ambient-prelude free identifier
    /// (no import emitted), the `id`/`label` attribute reader-synthesized boilerplate (empty spans
    /// — unmapped). Only `FootnoteText` has children (its colon body); the three leaf sugars
    /// self-close.
    fn lower_doc_state(&mut self, d: NotaDocState<'a>) -> Expression<'a> {
        let NotaDocState { span, kind, label, children, .. } = d;
        let (slot, key) = match kind {
            NotaDocStateKind::Label => (super::LABEL, "id"),
            NotaDocStateKind::Ref => (super::REF, "id"),
            NotaDocStateKind::FootnoteMark => (super::FOOTNOTE_MARK, "label"),
            NotaDocStateKind::FootnoteText => (super::FOOTNOTE_TEXT, "label"),
        };
        // A footnote-text body is a colon body (non-brace whitespace regime); leaves are empty.
        let children = self.lower_children(children, false);
        let value =
            self.ast.expression_string_literal(Span::empty(span.start), label.as_str(), None);
        let props = self.ast.vec1(self.jsx_attr(
            Span::empty(span.start),
            Span::empty(span.start),
            key,
            Some(value),
        ));
        self.build_named_element(span, slot, props, children)
    }

    /// Cook a raw link-target / alt slice (notation.md §Links): process `\<c>` escapes (the `\`
    /// dropped, the `<c>` kept; a trailing lone `\` stays). Escape-free slices pass through
    /// without allocation.
    fn cook_link_slice(&self, raw: &'a str) -> &'a str {
        if !raw.contains('\\') {
            return raw;
        }
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.push(chars.next().unwrap_or('\\')),
                c => out.push(c),
            }
        }
        self.ast.allocator.alloc_str(&out)
    }

    /// `[text](url)` link sugar → `<a href="url">text…</a>` — a plain host element (phrasing, so
    /// it flows inside paragraphs). The href is the raw slice trimmed of surrounding whitespace
    /// (`[x]( /y )` → `/y`), with `\<c>` escapes cooked.
    fn lower_link(&mut self, l: NotaLink<'a>) -> Expression<'a> {
        let NotaLink { span, url, children, .. } = l;
        let empty = Span::empty(span.start);
        let href = self.cook_link_slice(url.as_str().trim_matches([' ', '\t']));
        let value = self.ast.expression_string_literal(empty, href, None);
        let props = self.ast.vec1(self.jsx_attr(empty, empty, "href", Some(value)));
        // The text is an ordinary (brace-regime) markup body.
        let children = self.lower_children(children, true);
        self.build_element(
            span,
            build::JsxTag::Host { name: "a", span: empty },
            props,
            children,
            None,
        )
    }

    /// `![alt](src)` image sugar → `<img src="src" alt="alt" />`. Both attributes always emit
    /// (an empty `alt=""` is the accessible marker for a decorative image); the src is trimmed
    /// like a link href, the alt cooked verbatim (plain text — no markup, no trim).
    fn lower_image(&mut self, i: &NotaImage<'a>) -> Expression<'a> {
        let empty = Span::empty(i.span.start);
        let src = self.cook_link_slice(i.src.as_str().trim_matches([' ', '\t']));
        let alt = self.cook_link_slice(i.alt.as_str());
        let src_value = self.ast.expression_string_literal(empty, src, None);
        let alt_value = self.ast.expression_string_literal(empty, alt, None);
        let mut props = self.ast.vec_with_capacity(2);
        props.push(self.jsx_attr(empty, empty, "src", Some(src_value)));
        props.push(self.jsx_attr(empty, empty, "alt", Some(alt_value)));
        self.build_element(
            i.span,
            build::JsxTag::Host { name: "img", span: empty },
            props,
            self.ast.vec(),
            None,
        )
    }

    /// `---` thematic-break sugar → `<hr />` — a plain host element (a block, so the runtime's
    /// Reforest pass breaks paragraphs around it).
    fn lower_thematic_break(&mut self, t: &NotaThematicBreak) -> Expression<'a> {
        self.build_element(
            t.span,
            build::JsxTag::Host { name: "hr", span: Span::empty(t.span.start) },
            self.ast.vec(),
            self.ast.vec(),
            None,
        )
    }

    /// One `<UlLi>`/`<OlLi>` item per marker — runs coalesce into `<ul>`/`<ol>` in the runtime's
    /// Reforest pass (design/solid.md). A trailing attrs group (`- item [class: "hot"]`) hoists
    /// onto the item's element — `UlLi`/`OlLi` spread it onto the `<li>` they render.
    fn lower_list_item(&mut self, li: NotaListItem<'a>) -> Expression<'a> {
        let NotaListItem { span, kind, mut children, .. } = li;
        let tag_name = match kind {
            NotaListKind::Unordered => "nota-ul-li",
            NotaListKind::Ordered => "nota-ol-li",
        };
        let attrs = self.take_trailing_attrs(&mut children);
        let props = match attrs {
            Some(attrs) => self.lower_attrs(attrs.unbox().props),
            None => self.ast.vec(),
        };
        let children = self.lower_children(children, false);
        self.build_element(
            span,
            build::JsxTag::Host { name: tag_name, span: Span::empty(span.start) },
            props,
            children,
            None,
        )
    }

    // ===========================================================================================
    // Document
    // ===========================================================================================

    /// Lower a whole document: route `%`/`%%%` statements (`import`/`export` hoist; everything
    /// else prepends into Doc), Scribble the markup siblings, and assemble
    /// `export default function Doc() { …prelude…; return <NotaDoc>…</NotaDoc>; }`.
    fn lower_document(&mut self, doc: NotaDocument<'a>) -> Program<'a> {
        let mut module_items = self.ast.vec();
        let mut doc_prelude = self.ast.vec();
        let mut segs: Vec<scribble::Seg<'a, Expression<'a>>> = Vec::with_capacity(doc.items.len());
        for child in doc.items {
            match child {
                NotaChild::Text(t) => segs.push(scribble::Seg::Text(t.unbox().value.as_str())),
                NotaChild::Statement(s) => {
                    self.route_statement(s.unbox().statement, &mut module_items, &mut doc_prelude);
                }
                other => segs.push(scribble::Seg::Elem(self.lower_child(other))),
            }
        }
        let siblings = self.scribble_emit(segs, false);
        self.build_document(siblings, module_items, doc_prelude)
    }
}

impl<'a> VisitMut<'a> for NotaLowering<'a> {
    /// Lower an embedded `Expression::NotaMarkup` in place, then walk the lowered result so a
    /// `@`-form nested inside its embedded JS lowers too (bottom-up).
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if matches!(expr, Expression::NotaMarkup(_)) {
            let taken = std::mem::replace(expr, self.ast.expression_null_literal(Span::empty(0)));
            let Expression::NotaMarkup(markup) = taken else { unreachable!() };
            *expr = self.lower_markup(markup.unbox());
        }
        walk_mut::walk_expression(self, expr);
    }
}
