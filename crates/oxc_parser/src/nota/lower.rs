//! Nota AST → hyperscript lowering.
//!
//! The parser ([`super`]) builds a faithful Nota AST ([`oxc_ast::ast::NotaMarkup`] & friends); this
//! module *lowers* those nodes to the hyperscript `h()` / `Fragment()` / `decode()` `Expression`
//! AST — the same output the reader used to build inline. Lowering **consumes** owned Nota values
//! (`unbox`-ing boxed children to descend), exactly the ownership model the eventual standalone
//! transform pass (P3, via `take_in`) will use. The leaf emit-primitives (`build_h`,
//! `build_fragment`, `build_for_map`, `build_string_raw`, `build_document`, …) and the routing
//! helpers (`route_statement`, …) live in [`super`] and are reused here unchanged.
//!
//! Phase 2: lowering is invoked *inline* by the parser entry points (so `compile()` output stays
//! byte-identical and the fixtures remain a complete oracle). Phase 3 moves this behind a single
//! `Expression::NotaMarkup` traverse pass.

use oxc_allocator::Vec as ArenaVec;
use oxc_ast::{NONE, ast::*};
use oxc_span::{GetSpan, Span};

use super::{
    CODE_BLOCK, CODE_INLINE, Config, MATH, is_valid_tag_expr, scribble, statement_uses_await,
};
use crate::{ParserImpl, nota::mapping::NotaMappingKind};

impl<'a, C: Config> ParserImpl<'a, C> {
    // ===========================================================================================
    // Umbrella dispatch
    // ===========================================================================================

    /// Lower a markup form in expression position to its hyperscript `Expression`.
    pub(crate) fn lower_markup(&mut self, markup: NotaMarkup<'a>) -> Expression<'a> {
        let span = markup.span;
        match markup.kind {
            NotaMarkupKind::Element(e) => self.lower_element(e.unbox()),
            NotaMarkupKind::Fragment(f) => self.lower_fragment(f.unbox()),
            NotaMarkupKind::Interpolation(i) => self.lower_interpolation(i.unbox()),
            NotaMarkupKind::If(n) => self.lower_if(n.unbox()),
            NotaMarkupKind::For(n) => self.lower_for(n.unbox()),
            NotaMarkupKind::Code(c) => self.lower_code(c.unbox()),
            NotaMarkupKind::Math(m) => self.lower_math(m.unbox()),
            NotaMarkupKind::Verbatim(v) => self.lower_verbatim(v.unbox()),
            // The document lowers to a `Program` via `lower_document`, never as an expression. The
            // only way a `Document` reaches here is the `Dummy` placeholder a parse error returns
            // (its `kind` defaults to the first variant) — emit an inert placeholder; the fatal
            // error already set means the result is discarded.
            NotaMarkupKind::Document(_) => self.ast.expression_null_literal(Span::empty(span.start)),
        }
    }

    /// Lower a single body child (never `Text`/`Statement` — those are handled by the whitespace /
    /// statement machinery in [`Self::lower_children`]).
    fn lower_child(&mut self, child: NotaChild<'a>) -> Expression<'a> {
        match child {
            NotaChild::Element(e) => self.lower_element(e.unbox()),
            NotaChild::Fragment(f) => self.lower_fragment(f.unbox()),
            NotaChild::Interpolation(i) => self.lower_interpolation(i.unbox()),
            NotaChild::If(n) => self.lower_if(n.unbox()),
            NotaChild::For(n) => self.lower_for(n.unbox()),
            NotaChild::Code(c) => self.lower_code(c.unbox()),
            NotaChild::Math(m) => self.lower_math(m.unbox()),
            NotaChild::Verbatim(v) => self.lower_verbatim(v.unbox()),
            NotaChild::Emphasis(e) => self.lower_emphasis(e.unbox()),
            NotaChild::Heading(h) => self.lower_heading(h.unbox()),
            NotaChild::ListItem(li) => self.lower_list_item(li.unbox()),
            NotaChild::Text(_) | NotaChild::Statement(_) => {
                unreachable!("Text/Statement handled by lower_children")
            }
        }
    }

    // ===========================================================================================
    // Children + the Scribble whitespace bridge
    // ===========================================================================================

    /// Lower a body's children to the hyperscript child expressions, applying the Scribble
    /// whitespace algorithm. A `%`/`%%%` statement child scopes the *remaining* siblings into an
    /// IIFE (`(() => { …stmts…; return Fragment(...rest); })()`), exactly as the old
    /// `nested_statement_iife`.
    fn lower_children(&mut self, items: Vec<NotaChild<'a>>) -> ArenaVec<'a, Expression<'a>> {
        match items.iter().position(|c| matches!(c, NotaChild::Statement(_))) {
            None => {
                let (segs, elems) = self.children_to_segs(items);
                self.scribble_emit(&segs, elems)
            }
            Some(i) => {
                let mut items = items;
                let rest = items.split_off(i); // rest[0..] starts with the statement run
                let (mut segs, mut elems) = self.children_to_segs(items); // the prefix

                // Peel the leading consecutive statements; the remainder is the IIFE's body.
                let mut stmts = self.ast.vec();
                let mut is_async = false;
                let mut after: Vec<NotaChild<'a>> = Vec::new();
                let mut still_stmts = true;
                for c in rest {
                    if still_stmts {
                        if let NotaChild::Statement(s) = c {
                            let s = s.unbox();
                            if statement_uses_await(&s.statement) {
                                is_async = true;
                            }
                            stmts.push(s.statement);
                            continue;
                        }
                        still_stmts = false;
                    }
                    after.push(c);
                }

                let rest_children = self.lower_children(after);
                let iife = self.build_statement_iife(stmts, is_async, rest_children);
                segs.push(scribble::Seg::Elem(elems.len()));
                elems.push(Some(iife));
                self.scribble_emit(&segs, elems)
            }
        }
    }

    /// Split body children (no statements) into Scribble segments + the lowered element expressions.
    fn children_to_segs(
        &mut self,
        items: Vec<NotaChild<'a>>,
    ) -> (Vec<scribble::Seg<'a>>, Vec<Option<Expression<'a>>>) {
        let mut segs = Vec::with_capacity(items.len());
        let mut elems: Vec<Option<Expression<'a>>> = Vec::new();
        for c in items {
            match c {
                NotaChild::Text(t) => segs.push(scribble::Seg::Text(t.unbox().value.as_str())),
                NotaChild::Statement(_) => {
                    unreachable!("statements peeled by lower_children before segmenting")
                }
                other => {
                    segs.push(scribble::Seg::Elem(elems.len()));
                    let e = self.lower_child(other);
                    elems.push(Some(e));
                }
            }
        }
        (segs, elems)
    }

    /// Run the Scribble algorithm over the segments and materialize the child expressions: processed
    /// text → string literals (one `"\n"` per interior newline, never coalesced); elements moved out
    /// of `elems` exactly once. (The old `apply_whitespace` tail, shared by children + document.)
    fn scribble_emit(
        &self,
        segs: &[scribble::Seg<'a>],
        mut elems: Vec<Option<Expression<'a>>>,
    ) -> ArenaVec<'a, Expression<'a>> {
        let spec = scribble::lower(segs);
        let mut out = self.ast.vec_with_capacity(spec.len());
        for child in spec {
            out.push(match child {
                scribble::ChildSpec::Text(s) => {
                    let value: &'a str = self.ast.allocator.alloc_str(&s);
                    self.ast.expression_string_literal(Span::empty(0), value, None)
                }
                scribble::ChildSpec::Elem(idx) => {
                    elems[idx].take().expect("each element emitted exactly once")
                }
            });
        }
        out
    }

    /// `(() => { …stmts…; return Fragment(...rest); })()` (async iff a statement used `await`).
    fn build_statement_iife(
        &self,
        mut stmts: ArenaVec<'a, Statement<'a>>,
        is_async: bool,
        rest: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let ast = self.ast;
        let empty = Span::empty(0);
        let fragment = self.build_fragment(empty, rest);
        stmts.push(ast.statement_return(empty, Some(fragment)));
        let body = ast.function_body(empty, ast.vec(), stmts);
        let arrow = ast.expression_arrow_function(
            empty,
            false,
            is_async,
            NONE,
            ast.formal_parameters(empty, FormalParameterKind::ArrowFormalParameters, ast.vec(), NONE),
            NONE,
            body,
        );
        ast.expression_call(empty, arrow, NONE, ast.vec(), false)
    }

    // ===========================================================================================
    // Element + props
    // ===========================================================================================

    fn lower_element(&mut self, el: NotaElement<'a>) -> Expression<'a> {
        let NotaElement { span, tag, props, children, .. } = el;
        let props = self.lower_props(props);
        let children = self.lower_children(children.into_iter().collect());
        self.lower_tagged(span, tag, props, children)
    }

    /// Shared host/component/dynamic tag dispatch (used by elements and verbatim bodies): build
    /// `h(tag, { props }, [children])`, or the dynamic-tag IIFE for a non-trivial `@(expr)` head.
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
                // A component tag is a navigation/hover range (a TS symbol).
                self.record_nota_mapping(id.span, NotaMappingKind::ComponentIdentifier);
                let tag = Expression::Identifier(self.ast.alloc(id));
                self.build_h(span, tag, props, children)
            }
            NotaTag::Dynamic(d) => {
                let expr = d.unbox().expression;
                if is_valid_tag_expr(&expr) {
                    self.record_nota_mapping(expr.span(), NotaMappingKind::ComponentIdentifier);
                    self.build_h(span, expr, props, children)
                } else {
                    self.record_nota_mapping(expr.span(), NotaMappingKind::EmbeddedJs);
                    self.build_dynamic_iife(span, expr, props, children)
                }
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
                    let key = PropertyKey::StaticIdentifier(
                        self.ast.alloc_identifier_name(name.span, name.name.as_str()),
                    );
                    let value = match value {
                        NotaPropValue::Expression(e) => {
                            let expr = e.unbox().expression;
                            self.record_nota_mapping(expr.span(), NotaMappingKind::EmbeddedJs);
                            expr
                        }
                        NotaPropValue::Markup(m) => self.lower_markup(m.unbox()),
                    };
                    ObjectPropertyKind::ObjectProperty(self.ast.alloc_object_property(
                        span,
                        PropertyKind::Init,
                        key,
                        value,
                        false,
                        false,
                        false,
                    ))
                }
                NotaProp::Shorthand(s) => {
                    let id = s.unbox().name;
                    let (name, key_span) = (id.name.as_str(), id.span);
                    self.record_nota_mapping(key_span, NotaMappingKind::EmbeddedJs);
                    let key = PropertyKey::StaticIdentifier(
                        self.ast.alloc_identifier_name(key_span, name),
                    );
                    let value = Expression::Identifier(self.ast.alloc(id));
                    ObjectPropertyKind::ObjectProperty(self.ast.alloc_object_property(
                        key_span, PropertyKind::Init, key, value, false, true, false,
                    ))
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

    fn lower_fragment(&mut self, f: NotaFragment<'a>) -> Expression<'a> {
        let children = self.lower_children(f.children.into_iter().collect());
        self.build_fragment(f.span, children)
    }

    fn lower_interpolation(&mut self, i: NotaInterpolation<'a>) -> Expression<'a> {
        self.record_nota_mapping(i.expression.span(), NotaMappingKind::EmbeddedJs);
        i.expression
    }

    fn lower_if(&mut self, n: NotaIf<'a>) -> Expression<'a> {
        let NotaIf { span, test, consequent, alternate, .. } = n;
        self.record_nota_mapping(test.span(), NotaMappingKind::EmbeddedJs);
        let cons = self.lower_fragment_node(consequent.unbox());
        let alt = match alternate {
            None => self.ast.expression_null_literal(Span::empty(span.end)),
            Some(NotaElse::ElseIf(b)) => self.lower_if(b.unbox()),
            Some(NotaElse::Else(b)) => self.lower_fragment_node(b.unbox()),
        };
        self.ast.expression_conditional(span, test, cons, alt)
    }

    /// Lower a `NotaFragment` to a (keyless) `Fragment(...children)` — for `@if`/`else` branch bodies.
    fn lower_fragment_node(&mut self, f: NotaFragment<'a>) -> Expression<'a> {
        let span = f.span;
        let children = self.lower_children(f.children.into_iter().collect());
        self.build_fragment(span, children)
    }

    fn lower_for(&mut self, n: NotaFor<'a>) -> Expression<'a> {
        let NotaFor { span, binding, iterable, body, .. } = n;
        self.record_nota_mapping(binding.span(), NotaMappingKind::EmbeddedJs);
        self.record_nota_mapping(iterable.span(), NotaMappingKind::EmbeddedJs);
        let children = self.lower_children(body.unbox().children.into_iter().collect());
        self.build_for_map(span, binding, iterable, children)
    }

    // ===========================================================================================
    // Verbatim / code / math
    // ===========================================================================================

    fn lower_code(&mut self, c: NotaCode<'a>) -> Expression<'a> {
        let NotaCode { span, language, value, block, .. } = c;
        let raw_child = self.build_string_raw(span, value.as_str());
        let children = self.ast.vec1(raw_child);
        if block {
            let mut props = self.ast.vec();
            if let Some(lang) = language {
                let key = PropertyKey::StaticIdentifier(
                    self.ast.alloc_identifier_name(Span::empty(0), "lang"),
                );
                let val = self.ast.expression_string_literal(Span::empty(0), lang.as_str(), None);
                props.push(ObjectPropertyKind::ObjectProperty(self.ast.alloc_object_property(
                    Span::empty(0),
                    PropertyKind::Init,
                    key,
                    val,
                    false,
                    false,
                    false,
                )));
            }
            self.build_raw_element(span, CODE_BLOCK, props, children)
        } else {
            self.build_raw_element(span, CODE_INLINE, self.ast.vec(), children)
        }
    }

    fn lower_math(&mut self, m: NotaMath<'a>) -> Expression<'a> {
        let NotaMath { span, display, parts, .. } = m;

        // Reconstruct the `String.raw` quasis (raw chunks) + substitution expressions from the
        // alternating parts (canonical form starts with a raw run; pad empty quasis as needed so
        // `quasis.len() == exprs.len() + 1`).
        let mut quasis: Vec<&'a str> = Vec::new();
        let mut exprs = self.ast.vec();
        for part in parts {
            match part {
                NotaMathPart::Raw(t) => quasis.push(t.unbox().value.as_str()),
                NotaMathPart::Interpolation(i) => {
                    if quasis.len() == exprs.len() {
                        quasis.push(""); // no raw immediately before this substitution
                    }
                    let expr = i.unbox().expression;
                    self.record_nota_mapping(expr.span(), NotaMappingKind::EmbeddedJs);
                    exprs.push(expr);
                }
            }
        }
        while quasis.len() < exprs.len() + 1 {
            quasis.push("");
        }

        let raw_child = if exprs.is_empty() {
            self.build_string_raw(span, quasis.first().copied().unwrap_or(""))
        } else {
            self.build_string_raw_interp(span, quasis, exprs)
        };

        let mut props = self.ast.vec();
        if display {
            let key = PropertyKey::StaticIdentifier(
                self.ast.alloc_identifier_name(Span::empty(0), "display"),
            );
            let val = self.ast.expression_boolean_literal(Span::empty(0), true);
            props.push(ObjectPropertyKind::ObjectProperty(self.ast.alloc_object_property(
                Span::empty(0),
                PropertyKind::Init,
                key,
                val,
                false,
                false,
                false,
            )));
        }
        self.build_raw_element(span, MATH, props, self.ast.vec1(raw_child))
    }

    fn lower_verbatim(&mut self, v: NotaVerbatim<'a>) -> Expression<'a> {
        let NotaVerbatim { span, tag, parts, .. } = v;
        let mut children = self.ast.vec_with_capacity(parts.len());
        for part in parts {
            children.push(match part {
                NotaVerbatimPart::Raw(t) => {
                    let t = t.unbox();
                    self.build_string_raw(t.span, t.value.as_str())
                }
                NotaVerbatimPart::Child(m) => self.lower_markup(m.unbox()),
            });
        }
        self.lower_tagged(span, tag, self.ast.vec(), children)
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
        let children = self.lower_children(children.into_iter().collect());
        let tag = self.ast.expression_string_literal(Span::empty(span.start), tag_name, None);
        self.build_h(span, tag, self.ast.vec(), children)
    }

    fn lower_heading(&mut self, h: NotaHeading<'a>) -> Expression<'a> {
        let NotaHeading { span, level, children, .. } = h;
        let tag_name: &'a str = self.ast.allocator.alloc_str(&format!("h{level}"));
        let children = self.lower_children(children.into_iter().collect());
        let tag = self.ast.expression_string_literal(Span::empty(span.start), tag_name, None);
        self.build_h(span, tag, self.ast.vec(), children)
    }

    fn lower_list_item(&mut self, li: NotaListItem<'a>) -> Expression<'a> {
        let NotaListItem { span, kind, children, .. } = li;
        let tag_name = match kind {
            NotaListKind::Unordered => "nota-ul-li",
            NotaListKind::Ordered => "nota-ol-li",
        };
        let children = self.lower_children(children.into_iter().collect());
        let tag = self.ast.expression_string_literal(Span::empty(span.start), tag_name, None);
        self.build_h(span, tag, self.ast.vec(), children)
    }

    // ===========================================================================================
    // Document
    // ===========================================================================================

    /// Lower a whole `.nota` document to its `Program`: route `%`/`%%%` statements (import/export +
    /// F1 component hoist+export → module scope; others → `Doc` prelude), Scribble the markup
    /// siblings, and assemble `export default function Doc() { …; return decode(Fragment(...)); }`.
    pub(super) fn lower_document(&mut self, doc: NotaDocument<'a>) -> Program<'a> {
        let mut module_items = self.ast.vec();
        let mut doc_prelude = self.ast.vec();
        let mut is_async = false;
        let items: Vec<NotaChild<'a>> = doc.items.into_iter().collect();
        let siblings =
            self.lower_document_items(items, &mut module_items, &mut doc_prelude, &mut is_async);
        self.build_document(siblings, module_items, doc_prelude, is_async)
    }

    /// Like [`Self::lower_children`] but for the document body: a `%`/`%%%` statement is *routed*
    /// (hoisted / preluded) rather than scoping the rest into an IIFE.
    fn lower_document_items(
        &mut self,
        items: Vec<NotaChild<'a>>,
        module_items: &mut ArenaVec<'a, Statement<'a>>,
        doc_prelude: &mut ArenaVec<'a, Statement<'a>>,
        is_async: &mut bool,
    ) -> ArenaVec<'a, Expression<'a>> {
        let mut segs = Vec::with_capacity(items.len());
        let mut elems: Vec<Option<Expression<'a>>> = Vec::new();
        for c in items {
            match c {
                NotaChild::Text(t) => segs.push(scribble::Seg::Text(t.unbox().value.as_str())),
                NotaChild::Statement(s) => {
                    self.route_statement(s.unbox().statement, module_items, doc_prelude, is_async);
                }
                other => {
                    segs.push(scribble::Seg::Elem(elems.len()));
                    let e = self.lower_child(other);
                    elems.push(Some(e));
                }
            }
        }
        self.scribble_emit(&segs, elems)
    }
}
