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
/// time: a user binding colliding with a reserved emit-surface name (`Doc`, the structural/
/// solid-js/ambient-prelude names — [`super::reserved_emit_names`]), or a second `export default`.
#[derive(Debug)]
pub struct NotaLoweringReturn {
    /// Source⇄generated mapping marks (ascending source offset).
    pub mappings: Vec<NotaMappingMark>,
    /// Name-collision / duplicate-default-export diagnostics (empty on a clean document).
    pub diagnostics: Vec<OxcDiagnostic>,
    /// The language tags written on fenced code blocks (```rust), sorted and deduplicated.
    ///
    /// Highlighting grammars are opt-in and cost 50-190 KB each, so the integrator turns this
    /// into the imports and the `lstset` registration a document needs — see
    /// `@nota-lang/compiler`. Collected here because this is where a fence tag is known to *be*
    /// a fence tag; recovering it downstream would mean re-parsing the emit.
    pub fence_langs: Vec<String>,
}

/// The Nota lowering pass. `collect_mappings` gates Volar CodeMapping mark collection (off for the
/// plain build path → allocation-free).
pub struct NotaLowering<'a> {
    pub(super) ast: AstBuilder<'a>,
    pub(super) source_text: &'a str,
    mappings: Vec<NotaMappingMark>,
    diagnostics: Vec<OxcDiagnostic>,
    fence_langs: Vec<String>,
    collect_mappings: bool,
}

impl<'a> NotaLowering<'a> {
    pub fn new(allocator: &'a Allocator, source_text: &'a str, collect_mappings: bool) -> Self {
        Self {
            ast: AstBuilder::new(allocator),
            source_text,
            mappings: Vec::new(),
            diagnostics: Vec::new(),
            fence_langs: Vec::new(),
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
        let mut fence_langs = self.fence_langs;
        fence_langs.sort_unstable();
        fence_langs.dedup();
        NotaLoweringReturn { mappings: marks, diagnostics: self.diagnostics, fence_langs }
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
    /// `(() => { …stmts…; return <>…rest…</>; })()`.
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
                    // IIFE's returned Fragment. Anchored at the first statement's own span (build.rs's
                    // "anchored at the source construct" rule) — not the whole run's, since a
                    // multi-statement run's later members already carry their own real spans.
                    let at = first.span.start;
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
                    segs.push(scribble::Seg::Elem(self.build_statement_iife(at, stmts, rest)));
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
        let mut children = self.lower_children(children, !is_colon);
        // ...except where HTML forbids text entirely (see `FOSTER_PARENTING_TAGS`).
        if is_foster_parenting_tag(&tag) {
            children.retain(|child| !is_whitespace_text(child));
        }
        // `<table>` gets the `<tbody>` the parser would have inserted (see `wrap_bare_rows`).
        if is_host_tag(&tag, "table") {
            children = self.wrap_bare_rows(children);
        }
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
                let at = Span::empty(span.start);
                let val = self.ast.expression_string_literal(at, lang.as_str(), None);
                props.push(self.jsx_attr(at, at, "lang", Some(val)));
                self.fence_langs.push(lang.as_str().to_string());
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
            let at = Span::empty(span.start);
            props.push(self.jsx_attr(at, at, "display", None));
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

    /// Doc-state sugar (notation.md §Doc-state references, design/references.md) →
    /// `<Slot id="label" …props>children</Slot>`, the same ambient pattern as `Heading`: the
    /// component is an ambient-prelude free identifier (no import emitted), the `id` attribute
    /// reader-synthesized boilerplate (empty spans — unmapped). A ref's postfix `[props]` groups
    /// merge after the synthesized `id` (authored props — mapped like element props); its
    /// `{body}` becomes the children. `Label` and bare refs self-close.
    fn lower_doc_state(&mut self, d: NotaDocState<'a>) -> Expression<'a> {
        let NotaDocState { span, kind, label, props, children, .. } = d;
        let slot = match kind {
            NotaDocStateKind::Label => super::LABEL,
            NotaDocStateKind::Ref => super::REF,
        };
        let children = self.lower_children(children, false);
        let value =
            self.ast.expression_string_literal(Span::empty(span.start), label.as_str(), None);
        let mut jsx_props = self.ast.vec1(self.jsx_attr(
            Span::empty(span.start),
            Span::empty(span.start),
            "id",
            Some(value),
        ));
        jsx_props.extend(self.lower_attrs(props));
        self.build_named_element(span, slot, jsx_props, children)
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
    /// onto the item's element — `UlLi`/`OlLi` spread it onto the `<li>` they render. Built
    /// directly as a reference-named element via [`Self::build_named_element`] — NOT routed
    /// through [`build::JsxTag::Host`]/[`Self::build_element`], so a literal user tag spelled
    /// `@nota-ul-li`/`@nota-ol-li` (which *does* take that path, via [`Self::lower_tagged`])
    /// cannot collide with this construct and stays a plain host element.
    fn lower_list_item(&mut self, li: NotaListItem<'a>) -> Expression<'a> {
        let NotaListItem { span, kind, mut children, .. } = li;
        let name = match kind {
            NotaListKind::Unordered => super::UL_LI,
            NotaListKind::Ordered => super::OL_LI,
        };
        let attrs = self.take_trailing_attrs(&mut children);
        let props = match attrs {
            Some(attrs) => self.lower_attrs(attrs.unbox().props),
            None => self.ast.vec(),
        };
        let children = self.lower_children(children, false);
        self.build_named_element(span, name, props, children)
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

/// The HTML elements whose child *text* the parser refuses to keep in place.
///
/// Inside table structure, character data is not "in table text" — the parser **foster-parents**
/// it, re-inserting it in the DOM immediately *before* the table. Whitespace between rows is
/// content to Scribble (`@table{\n  @tr{…}\n  @tr{…}\n}` lowers with a `"\n"` between the rows),
/// so the server writes markup whose parsed DOM has fewer children than it emitted — and a client
/// walking `nextSibling` to claim those children runs off the end and the hydration tears the page
/// down. Dropping the whitespace at lowering time is the fix: the bytes and the DOM agree.
///
/// Only *whitespace-only* text is dropped, and only for a statically known host tag — a component
/// that happens to render a `<table>` is out of reach here, as is deliberate non-whitespace text
/// in table position (which is a document bug the browser will relocate either way).
const FOSTER_PARENTING_TAGS: [&str; 6] = ["table", "thead", "tbody", "tfoot", "tr", "colgroup"];

/// Whether `tag` is a host element from {@link FOSTER_PARENTING_TAGS}.
fn is_foster_parenting_tag(tag: &NotaTag<'_>) -> bool {
    matches!(tag, NotaTag::Host(h) if FOSTER_PARENTING_TAGS.contains(&h.name.as_str()))
}

/// Whether a lowered child is a text child holding only ASCII whitespace.
fn is_whitespace_text(child: &Expression<'_>) -> bool {
    matches!(child, Expression::StringLiteral(s)
        if !s.value.is_empty() && s.value.as_str().bytes().all(|b| b.is_ascii_whitespace()))
}

/// Whether `tag` is the named host element.
fn is_host_tag(tag: &NotaTag<'_>, name: &str) -> bool {
    matches!(tag, NotaTag::Host(h) if h.name.as_str() == name)
}

/// The host element name of a lowered child, if it is one.
fn lowered_host_name<'a>(child: &Expression<'a>) -> Option<&'a str> {
    let Expression::JSXElement(el) = child else { return None };
    match &el.opening_element.name {
        JSXElementName::Identifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

impl<'a> NotaLowering<'a> {
    /// Wrap each run of bare `<tr>` children of a `<table>` in the `<tbody>` the HTML parser
    /// would insert anyway.
    ///
    /// `@table{@tr{…} @tr{…}}` is the natural way to write a table, and it lowers to a `<table>`
    /// whose direct children are rows. The parser does not build that tree: rows outside a row
    /// group get an implicit `<tbody>`, so the DOM has a generation the emitted bytes never
    /// mentioned — `table.firstChild` is a `<tbody>`, the client's compiled template expects the
    /// first `<tr>`, and hydration dies on the mismatch. Emitting the `<tbody>` ourselves makes
    /// the bytes describe the tree they will actually parse into. Sibling row groups an author
    /// writes explicitly (`@thead{…}`) are left alone, and each run is wrapped separately so
    /// `@thead{…} @tr{…}` keeps its order.
    ///
    /// Out of reach here (statically invisible): a *component* child that renders rows.
    fn wrap_bare_rows(
        &self,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> ArenaVec<'a, Expression<'a>> {
        if !children.iter().any(|c| lowered_host_name(c) == Some("tr")) {
            return children;
        }
        let mut out = self.ast.vec_with_capacity(children.len());
        let mut run: ArenaVec<'a, Expression<'a>> = self.ast.vec();
        for child in children {
            if lowered_host_name(&child) == Some("tr") {
                run.push(child);
            } else {
                if !run.is_empty() {
                    let rows = std::mem::replace(&mut run, self.ast.vec());
                    out.push(self.build_tbody(rows));
                }
                out.push(child);
            }
        }
        if !run.is_empty() {
            out.push(self.build_tbody(run));
        }
        out
    }

    /// A synthesized `<tbody>` around `rows` (no props, no source span — it is not in the source).
    fn build_tbody(&self, rows: ArenaVec<'a, Expression<'a>>) -> Expression<'a> {
        self.build_element(
            Span::empty(0),
            build::JsxTag::Host { name: "tbody", span: Span::empty(0) },
            self.ast.vec(),
            rows,
            None,
        )
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
