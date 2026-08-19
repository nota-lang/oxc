//! Solid JSX emit primitives + document assembly for [`super::lower::NotaLowering`]: the
//! JSX element/fragment `Expression` builders and the document `Program` assembly (Doc skeleton,
//! `%`-statement routing — design/solid.md §The pipeline).
//!
//! The emit targets `@nota-lang/core`'s runtime surface: the document body is wrapped in
//! `<NotaDoc>`, list markers become `<UlLi>`/`<OlLi>`, flow-container host tags get a
//! `<Reforest>` interior, `@for` lowers to Solid's `<For>`, and a dynamic tag rides on
//! `<Dynamic component={…}>`. Text children are emitted as `{"…"}` expression containers
//! (never `JSXText` — whitespace is the reader's Scribble contract, not JSX's), with adjacent
//! runs coalesced so a blank source line surfaces as `"\n\n"` inside one string (Reforest's
//! paragraph-break marker).

use oxc_allocator::Vec as ArenaVec;
use oxc_ast::{NONE, ast::*};
use oxc_diagnostics::OxcDiagnostic;
use oxc_ecmascript::BoundNames;
use oxc_span::{GetSpan, SourceType, Span};

use super::lower::NotaLowering;
use super::{DOC, DYNAMIC, FOR, NOTA_DOC, REFOREST, SHOW, is_reserved_emit_name};

/// decode.md's HOST_FLOW_TAGS, now an **emit policy** (design/solid.md): the host containers
/// whose interior decodes as flow, realized by wrapping their children in `<Reforest>` at emit
/// time (the tag is statically known here; a rendered element cannot be restructured from
/// outside). `pub`: crosses the wasm boundary via `emitSurface()` so the runtime's categorizer
/// can be checked disjoint against it, and the integration suite loops the real list.
pub const FLOW_TAGS: &[&str] = &[
    "section",
    "article",
    "aside",
    "nav",
    "header",
    "footer",
    "main",
    "div",
    "blockquote",
    "figure",
    "td",
    "th",
];

/// Diagnostic for a user module binding that shadows a reserved emit-surface name
/// ([`is_reserved_emit_name`]).
fn reserved_name_collision(name: &str, span: Span) -> OxcDiagnostic {
    OxcDiagnostic::error(format!(
        "`{name}` collides with a name the Nota emit declares or references: `Doc` (the \
         default-export document component), the structural components \
         (`NotaDoc`/`Reforest`/…), and the ambient prelude the markup lowers to \
         (`Tex`/`Heading`/…). A module binding of the same name shadows it and breaks the emit. \
         Rename the binding."
    ))
    .with_label(span)
}

/// Diagnostic for a user `export default` (the reader already emits `export default function Doc`).
fn duplicate_default_export(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::error(
        "a Nota document already emits `export default function Doc(…)`, so this `% export default` \
         would be a second default export (a module may have only one). Remove it.",
    )
    .with_label(span)
}

/// A lowered element tag, as [`NotaLowering::lower_tagged`] dispatches it.
pub(super) enum JsxTag<'a> {
    /// A host tag (`@p`, the emphasis/list sugar targets): lowercase JSX name, not a reference.
    Host { name: &'a str, span: Span },
    /// A component tag (`@Aside`): an identifier **reference** (free-name analysis + mappings).
    Component(IdentifierReference<'a>),
    /// A dynamic tag (`@(expr)…` head): `<Dynamic component={expr} …>`.
    Dynamic(Expression<'a>),
}

impl<'a> NotaLowering<'a> {
    // ===========================================================================================
    // Synthesized-node shorthands
    //
    // Every node the lowering fabricates carries `Span::empty(at)` — *anchored* at the source
    // construct (sourcemap entries for scaffolding point into it) but *empty* (the CodeMapping
    // offset log never treats scaffolding as a mapped source range).
    // ===========================================================================================

    /// A synthesized identifier reference.
    fn ident(&self, at: u32, name: &'a str) -> Expression<'a> {
        self.ast.expression_identifier(Span::empty(at), name)
    }

    /// `<callee>(<args>)` — the call node carries `span`.
    fn call(
        &self,
        span: Span,
        callee: Expression<'a>,
        args: impl IntoIterator<Item = Expression<'a>>,
    ) -> Expression<'a> {
        let args = self.ast.vec_from_iter(args.into_iter().map(Argument::from));
        self.ast.expression_call(span, callee, NONE, args, false)
    }

    /// `<object>.<property>` — a synthesized static member access.
    fn member(&self, at: u32, object: Expression<'a>, property: &'a str) -> Expression<'a> {
        let empty = Span::empty(at);
        Expression::StaticMemberExpression(self.ast.alloc_static_member_expression(
            empty,
            object,
            self.ast.identifier_name(empty, property),
            false,
        ))
    }

    /// `(<params>) => { <stmts> }` — or `(<params>) => <expr>` when `expression` (then `stmts` is
    /// the single wrapped expression statement). Plain params; never `async` (the sync pin).
    fn arrow(
        &self,
        at: u32,
        params: impl IntoIterator<Item = BindingPattern<'a>>,
        expression: bool,
        stmts: ArenaVec<'a, Statement<'a>>,
    ) -> Expression<'a> {
        let empty = Span::empty(at);
        let params = self.ast.vec_from_iter(params.into_iter().map(|pat| {
            self.ast.formal_parameter(
                empty,
                self.ast.vec(),
                pat,
                NONE,
                NONE,
                false,
                None,
                false,
                false,
            )
        }));
        let params = self.ast.formal_parameters(
            empty,
            FormalParameterKind::ArrowFormalParameters,
            params,
            NONE,
        );
        let body = self.ast.function_body(empty, self.ast.vec(), stmts);
        self.ast.expression_arrow_function(empty, expression, false, NONE, params, NONE, body)
    }

    /// `(() => { <stmts>; return <ret>; })()` — the call node carries `span`. Never `async`.
    fn iife(
        &self,
        span: Span,
        mut stmts: ArenaVec<'a, Statement<'a>>,
        ret: Expression<'a>,
    ) -> Expression<'a> {
        stmts.push(self.ast.statement_return(Span::empty(span.start), Some(ret)));
        self.call(
            span,
            self.arrow(span.start, std::iter::empty(), false, stmts),
            std::iter::empty(),
        )
    }

    // ===========================================================================================
    // JSX emit primitives
    // ===========================================================================================

    /// A synthesized **reference** element name (`NotaDoc`/`Reforest`/`UlLi`/… and component
    /// tags): participates in scoping, so it surfaces as a free name and maps as an identifier.
    fn jsx_ref_name(&self, span: Span, name: &'a str) -> JSXElementName<'a> {
        self.ast.jsx_element_name_identifier_reference(span, name)
    }

    /// A host-tag element name (`p`, `em`, …): a plain `JSXIdentifier` — intrinsic, NOT a
    /// reference (a lowercase JSX name resolves to the host vocabulary, not a binding).
    fn jsx_host_name(&self, span: Span, name: &'a str) -> JSXElementName<'a> {
        self.ast.jsx_element_name_identifier(span, name)
    }

    /// `{<expr>}` — a JSX expression container child.
    fn jsx_container(&self, at: u32, expr: Expression<'a>) -> JSXChild<'a> {
        self.ast.jsx_child_expression_container(Span::empty(at), JSXExpression::from(expr))
    }

    /// Convert lowered child expressions to JSX children: an emitted JSX element/fragment nests
    /// directly; **adjacent string literals coalesce** into one `{"…"}` container (a blank source
    /// line surfaces as `"\n\n"` within one string — Reforest's paragraph-break contract); any
    /// other expression rides in a container.
    pub(super) fn jsx_children(
        &self,
        exprs: ArenaVec<'a, Expression<'a>>,
    ) -> ArenaVec<'a, JSXChild<'a>> {
        let mut out = self.ast.vec_with_capacity(exprs.len());
        let mut text: Option<(u32, String)> = None;
        macro_rules! flush_text {
            () => {
                if let Some((at, s)) = text.take() {
                    let value: &'a str = self.ast.allocator.alloc_str(&s);
                    out.push(self.jsx_container(
                        at,
                        self.ast.expression_string_literal(Span::empty(at), value, None),
                    ));
                }
            };
        }
        for expr in exprs {
            match expr {
                Expression::StringLiteral(lit) => match &mut text {
                    Some((_, s)) => s.push_str(lit.value.as_str()),
                    None => text = Some((lit.span.start, lit.value.as_str().to_string())),
                },
                Expression::JSXElement(el) => {
                    flush_text!();
                    out.push(JSXChild::Element(el));
                }
                Expression::JSXFragment(frag) => {
                    flush_text!();
                    out.push(JSXChild::Fragment(frag));
                }
                other => {
                    flush_text!();
                    out.push(self.jsx_container(other.span().start, other));
                }
            }
        }
        flush_text!();
        out
    }

    /// `<name attrs>children</name>` (self-closing when childless). `opening_span` is
    /// `Span::empty` boilerplate except on the EOF props-recovery path, where it carries the
    /// unclosed `[`'s span so codegen logs the opening element's generated position (the
    /// prop-completion anchor — see the join in `oxc::nota`).
    fn jsx_element(
        &self,
        span: Span,
        name: JSXElementName<'a>,
        attrs: ArenaVec<'a, JSXAttributeItem<'a>>,
        children: ArenaVec<'a, JSXChild<'a>>,
        opening_span: Span,
    ) -> Expression<'a> {
        let closing = if children.is_empty() {
            None
        } else {
            Some(
                self.ast
                    .alloc_jsx_closing_element(Span::empty(span.end), name.clone_in_name(self.ast)),
            )
        };
        let opening = self.ast.alloc_jsx_opening_element(opening_span, name, NONE, attrs);
        self.ast.expression_jsx_element(span, opening, children, closing)
    }

    /// A `name={value}` / `name="value"` attribute. A reader-synthesized string prop prints as a
    /// JSX string attribute only when its text is inert under JSX attribute-string rules (no
    /// quote, no HTML-entity ampersand — JSX attribute strings are escape-less and
    /// entity-decoded); anything else rides in an expression container, whose JS string escaping
    /// is exact.
    pub(super) fn jsx_attr(
        &self,
        span: Span,
        key_span: Span,
        name: &'a str,
        value: Option<Expression<'a>>,
    ) -> JSXAttributeItem<'a> {
        let attr_name = self.ast.jsx_attribute_name_identifier(key_span, name);
        let attr_value = value.map(|expr| match expr {
            Expression::StringLiteral(lit) if is_jsx_attr_inert(lit.value.as_str()) => {
                JSXAttributeValue::StringLiteral(lit)
            }
            other => self.ast.jsx_attribute_value_expression_container(
                Span::empty(span.start),
                JSXExpression::from(other),
            ),
        });
        JSXAttributeItem::Attribute(self.ast.alloc_jsx_attribute(span, attr_name, attr_value))
    }

    /// `{...argument}` — a JSX spread attribute.
    pub(super) fn jsx_spread_attr(
        &self,
        span: Span,
        argument: Expression<'a>,
    ) -> JSXAttributeItem<'a> {
        JSXAttributeItem::SpreadAttribute(self.ast.alloc_jsx_spread_attribute(span, argument))
    }

    /// The tagged-element builder — the one funnel for host/component/dynamic tags
    /// (design/solid.md §The pipeline):
    ///
    /// * host flow containers ([`FLOW_TAGS`]) → `<tag …><Reforest>children</Reforest></tag>`;
    /// * other host tags → plain intrinsic elements;
    /// * components → reference-named elements;
    /// * dynamic tags → `<Dynamic component={expr} …>`.
    ///
    /// List items (`<UlLi>`/`<OlLi>`) do NOT go through here — they are a lowering-internal
    /// construct, not a host tag a document can spell, so [`NotaLowering::lower_list_item`] calls
    /// [`Self::build_named_element`] directly. A literal user tag named `@nota-ul-li`/`@nota-ol-li`
    /// is an ordinary host element and must fall through the plain-host-tag arm below unchanged.
    pub(super) fn build_element(
        &self,
        span: Span,
        tag: JsxTag<'a>,
        mut attrs: ArenaVec<'a, JSXAttributeItem<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
        recovery_span: Option<Span>,
    ) -> Expression<'a> {
        let opening_span = recovery_span.unwrap_or_else(|| Span::empty(span.start));
        let children = self.jsx_children(children);
        match tag {
            JsxTag::Host { name, span: tag_span } => {
                let children = if FLOW_TAGS.contains(&name) && !children.is_empty() {
                    let reforest = self.jsx_element(
                        Span::empty(span.start),
                        self.jsx_ref_name(Span::empty(span.start), REFOREST),
                        self.ast.vec(),
                        children,
                        Span::empty(span.start),
                    );
                    let Expression::JSXElement(el) = reforest else { unreachable!() };
                    self.ast.vec1(JSXChild::Element(el))
                } else {
                    children
                };
                self.jsx_element(
                    span,
                    self.jsx_host_name(tag_span, name),
                    attrs,
                    children,
                    opening_span,
                )
            }
            JsxTag::Component(ident) => {
                let name = JSXElementName::IdentifierReference(self.ast.alloc(ident));
                self.jsx_element(span, name, attrs, children, opening_span)
            }
            JsxTag::Dynamic(expr) => {
                let component_attr = self.jsx_attr(
                    Span::empty(span.start),
                    Span::empty(span.start),
                    "component",
                    Some(expr),
                );
                attrs.insert(0, component_attr);
                self.jsx_element(
                    span,
                    self.jsx_ref_name(Span::empty(span.start), DYNAMIC),
                    attrs,
                    children,
                    opening_span,
                )
            }
        }
    }

    /// A runtime/ambient-named element (`<Tex …>`, `<Heading …>`, `<NotaDoc>`, `<Reforest>`):
    /// reference-named, so the shim's free-name binding reaches it.
    pub(super) fn build_named_element(
        &self,
        span: Span,
        name: &'a str,
        attrs: ArenaVec<'a, JSXAttributeItem<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let children = self.jsx_children(children);
        self.jsx_element(
            span,
            self.jsx_ref_name(Span::empty(span.start), name),
            attrs,
            children,
            Span::empty(span.start),
        )
    }

    /// `<>children</>`.
    pub(super) fn build_fragment(
        &self,
        span: Span,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let children = self.jsx_children(children);
        self.ast.expression_jsx_fragment(
            span,
            self.ast.jsx_opening_fragment(Span::empty(span.start)),
            children,
            self.ast.jsx_closing_fragment(Span::empty(span.end)),
        )
    }

    /// `<Show when={test} fallback={alt}>{cons}</Show>` — Solid's conditional. `fallback` is
    /// omitted entirely when there is no `else` branch (Solid renders nothing by default), so the
    /// no-else emit carries no `null`.
    ///
    /// Both branches arrive already **fragment-wrapped** and the consequent nests as a fragment
    /// child rather than having its children spliced in: `<Show>` reads a lone *function* child as
    /// its keyed accessor callback, so splicing would make `@if (c) {@(f)}` silently mean
    /// something else. The fragment costs nothing in Solid's output and closes that hole.
    pub(super) fn build_show(
        &self,
        span: Span,
        test: Expression<'a>,
        cons: Expression<'a>,
        alt: Option<Expression<'a>>,
    ) -> Expression<'a> {
        let empty = Span::empty(span.start);
        let mut attrs = self.ast.vec_with_capacity(2);
        attrs.push(self.jsx_attr(empty, empty, "when", Some(test)));
        if let Some(alt) = alt {
            attrs.push(self.jsx_attr(empty, empty, "fallback", Some(alt)));
        }
        let children = self.jsx_children(self.ast.vec1(cons));
        self.jsx_element(span, self.jsx_ref_name(empty, SHOW), attrs, children, empty)
    }

    /// `<For each={iter}>{(bind) => <>children</>}</For>` — Solid's keyed list rendering; the old
    /// map-index Fragment key is gone (Solid has no `key`; `<For>` reconciles by item).
    pub(super) fn build_for(
        &self,
        span: Span,
        bind: BindingPattern<'a>,
        iter: Expression<'a>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let empty = Span::empty(span.start);
        let body = self.build_fragment(span, children);
        let arrow = self.arrow(
            span.start,
            [bind],
            true,
            self.ast.vec1(self.ast.statement_expression(empty, body)),
        );
        let each = self.jsx_attr(empty, empty, "each", Some(iter));
        let attrs = self.ast.vec1(each);
        let callback = self.ast.vec1(self.jsx_container(span.start, arrow));
        self.jsx_element(span, self.jsx_ref_name(empty, FOR), attrs, callback, empty)
    }

    /// `(() => { …stmts…; return <>...rest</>; })()`. Always synchronous — the reader does not
    /// `async`ify the IIFE from the presence of `await` in `stmts`.
    pub(super) fn build_statement_iife(
        &self,
        at: u32,
        stmts: ArenaVec<'a, Statement<'a>>,
        rest: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let fragment = self.build_fragment(Span::empty(at), rest);
        self.iife(Span::empty(at), stmts, fragment)
    }

    // ===========================================================================================
    // `String.raw` tagged-template builders (code/math raw runs)
    // ===========================================================================================

    /// `String.raw\`<raw>\`` — a tagged template over a single raw quasi (no substitutions).
    ///
    /// `String.raw` cannot faithfully carry a backtick or a literal `${`: a backtick closes the
    /// template, `${` opens a substitution, and `String.raw` does NOT process a `\` escape — so any
    /// `\` added to neutralize them leaks into the runtime string. For content with either breaker we
    /// emit a **cooked** string literal instead, whose codegen escaping (`\\`, control chars, the
    /// closing quote) reproduces `raw` exactly. Breaker-free content keeps the readable `String.raw`
    /// form (notation.md §Emit reference).
    pub(super) fn build_string_raw(&self, span: Span, raw: &'a str) -> Expression<'a> {
        if Self::has_template_breaker(raw) {
            return self.ast.expression_string_literal(span, raw, None);
        }
        let ast = self.ast;
        let mut quasis = ast.vec_with_capacity(1);
        quasis.push(self.raw_quasi(span, raw));
        let quasi = ast.template_literal(span, quasis, ast.vec());
        self.tag_string_raw(span, quasi)
    }

    /// One template-literal quasi carrying `raw` **verbatim** as its raw value, `cooked: None` (a
    /// `String.raw` tag reads only the raw text: `\` and `{}` are NOT interpreted). We do **not**
    /// use codegen's `escape_raw` (which doubles every `\`, wrong for `String.raw`); the caller
    /// guarantees `raw` is breaker-free.
    fn raw_quasi(&self, span: Span, raw: &'a str) -> TemplateElement<'a> {
        debug_assert!(!Self::has_template_breaker(raw));
        let value = TemplateElementValue { raw: self.ast.str(raw), cooked: None };
        self.ast.template_element(span, value, true, false)
    }

    /// Does `raw` contain a template-syntax breaker — a backtick or a `${` — that a `String.raw`
    /// template cannot represent without a `\` that leaks at runtime? Such content falls back to a
    /// cooked string literal ([`Self::build_string_raw`]).
    fn has_template_breaker(raw: &str) -> bool {
        let bytes = raw.as_bytes();
        bytes
            .iter()
            .enumerate()
            .any(|(i, &b)| b == b'`' || (b == b'$' && bytes.get(i + 1) == Some(&b'{')))
    }

    /// `String.raw` — the member-expression callee for the raw tagged template.
    fn tag_string_raw(&self, span: Span, quasi: TemplateLiteral<'a>) -> Expression<'a> {
        let tag = self.member(span.start, self.ident(span.start, "String"), "raw");
        self.ast.expression_tagged_template(span, tag, NONE, quasi)
    }

    // ===========================================================================================
    // Document assembly + `%`-statement routing
    // ===========================================================================================

    /// Route a parsed top-level statement: `import`/`export` hoist to module scope; everything
    /// else prepends into `Doc` as an ordinary lexical statement (document-local — a component
    /// binding may close over document state; the document hydrates as one Solid app, so the
    /// closure is just the program's own).
    pub(super) fn route_statement(
        &mut self,
        stmt: Statement<'a>,
        module_items: &mut ArenaVec<'a, Statement<'a>>,
        doc_prelude: &mut ArenaVec<'a, Statement<'a>>,
    ) {
        // Diagnose a binding / default-export that would collide with the reader's emit surface
        // (`Doc`, the structural references) before routing it — the oxc parser cannot catch
        // these (the collision is with names the *lowering* injects, not anything in the source).
        self.check_reserved_collisions(&stmt);
        // A `%`/`%%%` statement body is embedded JS/TS spliced verbatim (full capabilities).
        self.record_nota_mapping(stmt.span(), super::mapping::NotaMappingKind::EmbeddedJs);
        match stmt {
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportAllDeclaration(_) => module_items.push(stmt),
            other => doc_prelude.push(other),
        }
    }

    /// Diagnose a routed top-level statement that collides with the reader's emit surface: a module
    /// binding named like a reserved emit name ([`is_reserved_emit_name`]), or a `% export default`
    /// (a second default export beside `export default function Doc`). The statement is still routed
    /// as usual — the diagnostic is advisory (the emit would be broken/ambiguous JS otherwise).
    ///
    /// Bindings come from ECMA's `BoundNames` ([`oxc_ecmascript::BoundNames`]), so destructured
    /// names collide too (`%const { NotaDoc } = lib`, rest elements), as do import locals and
    /// `% export`-wrapped declarations.
    fn check_reserved_collisions(&mut self, stmt: &Statement<'a>) {
        match stmt {
            Statement::ExportDefaultDeclaration(d) => self.error(duplicate_default_export(d.span)),
            Statement::ImportDeclaration(d) => self.check_bound_names(&**d),
            Statement::VariableDeclaration(d) => self.check_bound_names(&**d),
            Statement::FunctionDeclaration(d) => self.check_bound_names(&**d),
            Statement::ClassDeclaration(d) => self.check_bound_names(&**d),
            Statement::ExportNamedDeclaration(d) => self.check_bound_names(&**d),
            _ => {}
        }
    }

    /// Emit a collision diagnostic for every bound name of `decl` that is reserved.
    fn check_bound_names(&mut self, decl: &impl BoundNames<'a>) {
        decl.bound_names(&mut |id| {
            if is_reserved_emit_name(id.name.as_str()) {
                self.error(reserved_name_collision(id.name.as_str(), id.span));
            }
        });
    }

    /// Build the `export default function Doc() { …prelude…; return <NotaDoc>…</NotaDoc>; }`
    /// module. `Doc` is always synchronous — the reader does not `async`ify it from `await` in
    /// the prelude.
    pub(super) fn build_document(
        &self,
        siblings: ArenaVec<'a, Expression<'a>>,
        module_items: ArenaVec<'a, Statement<'a>>,
        doc_prelude: ArenaVec<'a, Statement<'a>>,
    ) -> Program<'a> {
        let ast = self.ast;
        let empty = Span::empty(0);

        let doc_body = self.build_named_element(empty, NOTA_DOC, ast.vec(), siblings);
        let return_stmt = ast.statement_return(empty, Some(doc_body));

        let mut body_stmts = doc_prelude;
        body_stmts.push(return_stmt);
        let body = ast.function_body(empty, ast.vec(), body_stmts);

        let func = ast.function(
            empty,
            FunctionType::FunctionDeclaration,
            Some(ast.binding_identifier(empty, DOC)),
            false,
            false, // never async
            false,
            NONE,
            NONE,
            ast.formal_parameters(empty, FormalParameterKind::FormalParameter, ast.vec(), NONE),
            NONE,
            Some(body),
        );
        let default_decl = ast.module_declaration_export_default_declaration(
            empty,
            ExportDefaultDeclarationKind::FunctionDeclaration(ast.alloc(func)),
        );
        let doc_stmt = Statement::from(default_decl);

        let mut program_body = module_items;
        program_body.push(doc_stmt);

        ast.program(
            empty,
            SourceType::default().with_module(true).with_jsx(true),
            self.source_text,
            ast.vec(),
            None,
            ast.vec(),
            program_body,
        )
    }
}

/// Is `s` inert as a JSX attribute string? JSX attribute strings have **no escapes** (the value
/// runs to the matching quote) and **decode HTML entities** — so a value with a `"` or an `&`
/// cannot round-trip and must ride in an expression container instead. `<`/`>`/newlines are
/// legal in attribute strings, but `<` is kept out conservatively (some downstream tooling
/// chokes); everything the reader synthesizes (ids, labels, langs) passes.
fn is_jsx_attr_inert(s: &str) -> bool {
    !s.contains(['"', '&', '<', '>'])
}

/// Clone-a-name helper: `JSXElementName` is consumed by the opening element, but the closing
/// element repeats it. Only the variants the lowering synthesizes are supported.
trait CloneInName<'a> {
    fn clone_in_name(&self, ast: oxc_ast::AstBuilder<'a>) -> JSXElementName<'a>;
}

impl<'a> CloneInName<'a> for JSXElementName<'a> {
    fn clone_in_name(&self, ast: oxc_ast::AstBuilder<'a>) -> JSXElementName<'a> {
        match self {
            JSXElementName::Identifier(id) => {
                ast.jsx_element_name_identifier(Span::empty(id.span.start), id.name.as_str())
            }
            JSXElementName::IdentifierReference(id) => {
                ast.jsx_element_name_identifier(Span::empty(id.span.start), id.name.as_str())
            }
            _ => unreachable!("the Nota lowering synthesizes only identifier element names"),
        }
    }
}
