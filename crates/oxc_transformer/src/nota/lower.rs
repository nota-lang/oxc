//! Nota AST → hyperscript lowering — a standalone pass over the parsed `Program`.
//!
//! The reader ([`oxc_parser`]'s nota module) leaves a document as a single
//! `Expression::NotaMarkup(Document)` statement and every embedded `@`-form in place as
//! `Expression::NotaMarkup`. This pass lowers those to the hyperscript `h`/`Fragment`/`decode`
//! `Expression` AST: [`NotaLowering::lower_document_program`] rebuilds the document `Program`
//! (Doc skeleton, `%` routing + component name-attach — R15), then a [`VisitMut`] walk replaces
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

/// The Nota lowering pass. `collect_mappings` gates H1/H2 mark collection (off for the plain
/// build path → allocation-free).
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
        let NotaElement { span, tag, props, children, is_colon, .. } = el;
        let props = self.lower_props(props);
        // The whitespace regime follows the body syntax: a brace body keeps the spaces between
        // `{`/`}` and text as content; a colon body trims its edges like a document/block.
        let children = self.lower_children(children, !is_colon);
        self.lower_tagged(span, tag, props, children)
    }

    /// Shared host/component/dynamic tag dispatch: `h(tag, { props }, [children])` — `tag` is a
    /// string literal, a component identifier, or (dynamic) the head expression verbatim. `h` is a
    /// plain function, so any expression is valid in argument position; no binding is needed.
    fn lower_tagged(
        &mut self,
        span: Span,
        tag: NotaTag<'a>,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        match tag {
            NotaTag::Host(h) => {
                let h = h.unbox();
                let tag = self.ast.expression_string_literal(h.span, h.name.as_str(), None);
                self.build_h(span, tag, props, children)
            }
            NotaTag::Component(id) => {
                let id = id.unbox();
                self.record_nota_mapping(id.span, NotaMappingKind::ComponentIdentifier);
                let tag = Expression::Identifier(self.ast.alloc(id));
                self.build_h(span, tag, props, children)
            }
            NotaTag::Dynamic(d) => {
                let expr = d.unbox().expression;
                self.record_nota_mapping(expr.span(), NotaMappingKind::EmbeddedJs);
                self.build_h(span, expr, props, children)
            }
        }
    }

    fn lower_props(
        &mut self,
        props: ArenaVec<'a, NotaProp<'a>>,
    ) -> ArenaVec<'a, ObjectPropertyKind<'a>> {
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
                    // `obj_prop` picks a bare vs string-literal key (`data-x`) by ident validity.
                    self.obj_prop(span, name.span, name.name.as_str(), value, false)
                }
                NotaProp::Shorthand(s) => {
                    let id = s.unbox().name;
                    let (name, key_span) = (id.name.as_str(), id.span);
                    self.record_nota_mapping(key_span, NotaMappingKind::EmbeddedJs);
                    let value = Expression::Identifier(self.ast.alloc(id));
                    self.obj_prop(key_span, key_span, name, value, true)
                }
                NotaProp::Spread(sp) => {
                    let NotaSpreadProp { span, argument, .. } = sp.unbox();
                    self.record_nota_mapping(argument.span(), NotaMappingKind::EmbeddedJs);
                    let spread = self.ast.spread_element(span, argument);
                    ObjectPropertyKind::SpreadProperty(self.ast.alloc(spread))
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

    /// `@if` → a (nested) ternary; `null` when no branch matches. Branches are keyless (a single
    /// branch needs no list reconciliation).
    fn lower_if(&mut self, n: NotaIf<'a>) -> Expression<'a> {
        let NotaIf { span, test, consequent, alternate, .. } = n;
        self.record_nota_mapping(test.span(), NotaMappingKind::EmbeddedJs);
        let cons = self.lower_fragment(consequent.unbox(), false);
        let alt = match alternate {
            None => self.ast.expression_null_literal(Span::empty(span.end)),
            Some(NotaElse::ElseIf(b)) => self.lower_if(b.unbox()),
            Some(NotaElse::Else(b)) => self.lower_fragment(b.unbox(), false),
        };
        self.ast.expression_conditional(span, test, cons, alt)
    }

    /// `@for (bind of iter) {body}` → `iter.map((bind, _i) => Fragment({ key: _i }, ...body))` —
    /// the reader injects the map index as the wrapping Fragment's key (contract E5).
    fn lower_for(&mut self, n: NotaFor<'a>) -> Expression<'a> {
        let NotaFor { span, binding, iterable, body, .. } = n;
        self.record_nota_mapping(binding.span(), NotaMappingKind::EmbeddedJs);
        self.record_nota_mapping(iterable.span(), NotaMappingKind::EmbeddedJs);
        let children = self.lower_children(body.unbox().children, false);
        self.build_for_map(span, binding, iterable, children)
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
                props.push(self.obj_prop(Span::empty(0), Span::empty(0), "lang", val, false));
            }
            self.build_raw_element(span, super::CODE_BLOCK, props, children)
        } else {
            self.build_raw_element(span, super::CODE_INLINE, self.ast.vec(), children)
        }
    }

    fn lower_math(&mut self, m: NotaMath<'a>) -> Expression<'a> {
        let NotaMath { span, block, parts, .. } = m;
        let children = self.lower_raw_parts(parts);
        // The runtime prop is `display` (the AST field renamed to `block` to mirror `NotaCode`).
        let mut props = self.ast.vec();
        if block {
            let val = self.ast.expression_boolean_literal(Span::empty(0), true);
            props.push(self.obj_prop(Span::empty(0), Span::empty(0), "display", val, false));
        }
        self.build_raw_element(span, super::MATH, props, children)
    }

    fn lower_verbatim(&mut self, v: NotaVerbatim<'a>) -> Expression<'a> {
        let NotaVerbatim { span, tag, props, parts, .. } = v;
        let props = self.lower_props(props);
        let children = self.lower_raw_parts(parts);
        self.lower_tagged(span, tag, props, children)
    }

    // ===========================================================================================
    // Surface sugar → host elements
    // ===========================================================================================

    fn lower_emphasis(&mut self, e: NotaEmphasis<'a>) -> Expression<'a> {
        let NotaEmphasis { span, marker, children, .. } = e;
        let tag_name = match marker {
            NotaEmphasisMarker::Strong => "strong",
            NotaEmphasisMarker::Em => "em",
        };
        let children = self.lower_children(children, true);
        let tag = self.ast.expression_string_literal(Span::empty(span.start), tag_name, None);
        self.build_h(span, tag, self.ast.vec(), children)
    }

    /// `#` heading *sugar* → `h(Heading, { rank: N }, [children])` (contract R18f): `Heading` is an
    /// ambient-prelude slot referenced as a free identifier (mirroring `Tex`/`CodeInline`), `rank`
    /// the level as a numeric literal. The default `Heading` marks + queries the concrete `hN` at
    /// decode time. Raw `@hN{…}` element forms lower via [`Self::lower_element`] and stay plain host
    /// tags — the unnumbered/un-Toc'd escape hatch.
    fn lower_heading(&mut self, h: NotaHeading<'a>) -> Expression<'a> {
        let NotaHeading { span, level, children, .. } = h;
        let children = self.lower_children(children, false);
        let rank = self.ast.expression_numeric_literal(
            Span::empty(span.start),
            f64::from(level),
            None,
            NumberBase::Decimal,
        );
        let props = self.ast.vec1(self.obj_prop(
            Span::empty(span.start),
            Span::empty(span.start),
            "rank",
            rank,
            false,
        ));
        self.build_raw_element(span, super::HEADING, props, children)
    }

    /// One `nota-ul-li`/`nota-ol-li` sentinel per item — the runtime `struct` pass coalesces runs
    /// into `<ul>`/`<ol>` (contract §7).
    fn lower_list_item(&mut self, li: NotaListItem<'a>) -> Expression<'a> {
        let NotaListItem { span, kind, children, .. } = li;
        let tag_name = match kind {
            NotaListKind::Unordered => "nota-ul-li",
            NotaListKind::Ordered => "nota-ol-li",
        };
        let children = self.lower_children(children, false);
        let tag = self.ast.expression_string_literal(Span::empty(span.start), tag_name, None);
        self.build_h(span, tag, self.ast.vec(), children)
    }

    // ===========================================================================================
    // Document
    // ===========================================================================================

    /// Lower a whole document: route `%`/`%%%` statements (`import`/`export` hoist; everything
    /// else — component bindings included, R15 — prepends into Doc), Scribble the
    /// markup siblings, and assemble
    /// `export default function Doc() { …prelude…; return decode(Fragment(...)); }`.
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
