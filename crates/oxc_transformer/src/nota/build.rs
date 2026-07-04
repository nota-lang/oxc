//! Hyperscript emit primitives + document/F1 assembly for [`super::lower::NotaLowering`]: the
//! `h`/`Fragment`/`decode`/`String.raw` `Expression` builders and the document `Program` assembly
//! (Doc skeleton, `%`-statement routing, F1 component hoist+export, decode-wraps).

use lazy_regex::{Regex, regex};
use oxc_allocator::Vec as ArenaVec;
use oxc_ast::{NONE, ast::*};
use oxc_diagnostics::OxcDiagnostic;
use oxc_ecmascript::BoundNames;
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::identifier::is_identifier_name;

use super::lower::NotaLowering;
use super::mapping::NotaMappingKind;
use super::{
    BLOCK_COMPONENT, DECODE, DOC, DYNAMIC_TAG_BINDING, FOR_KEY_PARAM, FRAGMENT, H,
    INLINE_COMPONENT, is_f1_constructor, is_markup_call,
};

/// Is `name` a reader-injected emit-surface name a user module binding must not shadow? The lowered
/// module references the default-export component `Doc` and the runtime imports the markup calls
/// (`h`/`Fragment`/`decode`/`inlineComponent`/`blockComponent`); these are pinned by the contract and
/// cannot be silently renamed, so a colliding binding is diagnosed rather than emitted.
fn is_reserved_emit_name(name: &str) -> bool {
    matches!(name, DOC | H | FRAGMENT | DECODE | INLINE_COMPONENT | BLOCK_COMPONENT)
}

/// Diagnostic for a user module binding that shadows a reader-injected emit-surface name.
fn reserved_name_collision(name: &str, span: Span) -> OxcDiagnostic {
    OxcDiagnostic::error(format!(
        "`{name}` collides with a Nota reader-injected name. The emitted module declares `Doc` (the \
         default-export document component) and imports `h`/`Fragment`/`decode`/`inlineComponent`/\
         `blockComponent` from the runtime, which the lowered markup calls; a module binding of the \
         same name shadows them and breaks the emit. Rename the binding."
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

impl<'a> NotaLowering<'a> {
    // ===========================================================================================
    // Synthesized-node shorthands
    //
    // Every node the lowering fabricates carries `Span::empty(at)` — *anchored* at the source
    // construct (sourcemap entries for scaffolding point into it) but *empty* (the H1 offset log
    // never treats scaffolding as a mapped source range).
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

    /// A `key: value` object property (or shorthand). The key is a bare identifier when `name` is
    /// a valid JS identifier (incl. keywords), a string literal otherwise (`data-x`, `aria-label`).
    pub(super) fn obj_prop(
        &self,
        span: Span,
        key_span: Span,
        name: &'a str,
        value: Expression<'a>,
        shorthand: bool,
    ) -> ObjectPropertyKind<'a> {
        let key = if is_identifier_name(name) {
            PropertyKey::StaticIdentifier(self.ast.alloc_identifier_name(key_span, name))
        } else {
            PropertyKey::StringLiteral(self.ast.alloc_string_literal(key_span, name, None))
        };
        ObjectPropertyKind::ObjectProperty(self.ast.alloc_object_property(
            span,
            PropertyKind::Init,
            key,
            value,
            false,
            shorthand,
            false,
        ))
    }

    // ===========================================================================================
    // Element / fragment emit primitives
    // ===========================================================================================

    /// `h(tag, { props }, [children])`.
    pub(super) fn build_h(
        &self,
        span: Span,
        tag: Expression<'a>,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let props_obj = self.ast.expression_object(Span::empty(span.start), props);
        let children_arr = self.ast.expression_array(
            Span::empty(span.end),
            self.ast.vec_from_iter(children.into_iter().map(ArrayExpressionElement::from)),
        );
        self.call(span, self.ident(span.start, H), [tag, props_obj, children_arr])
    }

    /// `Fragment(...children)` — variadic call (no props, no array wrap).
    pub(super) fn build_fragment(
        &self,
        span: Span,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        self.call(span, self.ident(span.start, FRAGMENT), children)
    }

    /// `Fragment({ key: _i }, ...children)` — a `Fragment` with a leading props arg.
    fn build_keyed_fragment(
        &self,
        span: Span,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let props_obj = self.ast.expression_object(Span::empty(span.start), props);
        self.call(
            span,
            self.ident(span.start, FRAGMENT),
            std::iter::once(props_obj).chain(children),
        )
    }

    /// Pick a fresh identifier name for a reader-injected binding (`_i`, `_Tag`) that cannot collide
    /// with a user identifier in the construct at `span`. Returns `candidate` unless it appears as a
    /// whole word in the construct's source (`@for(_i of …)`, `@(_Tag)`), in which case a numeric
    /// suffix is appended until free. (Scanning the source over-approximates — a name in a string or
    /// comment also bumps — which only ever yields a *more* distinct name, never a colliding one.)
    fn fresh_name(&self, candidate: &'static str, span: Span) -> &'a str {
        /// Does `needle` occur in `hay` as a whole word? Boundaries are JS-identifier chars in the
        /// ASCII class `[0-9A-Za-z_$]` — a multibyte char conservatively counts as a boundary
        /// (over-approximation is the safe direction, see above).
        fn contains_word(hay: &str, needle: &str) -> bool {
            let pattern =
                format!(r"(?:^|[^0-9A-Za-z_$]){}(?:[^0-9A-Za-z_$]|$)", regex::escape(needle));
            Regex::new(&pattern).expect("escaped word pattern is valid").is_match(hay)
        }
        let src = &self.source_text[span.start as usize..span.end as usize];
        if !contains_word(src, candidate) {
            return candidate;
        }
        let mut n = 2u32;
        loop {
            let cand = format!("{candidate}{n}");
            if !contains_word(src, &cand) {
                return self.ast.allocator.alloc_str(&cand);
            }
            n += 1;
        }
    }

    /// `(() => { const _Tag = <expr>; return h(_Tag, { props }, [children]); })()` — dynamic tag.
    pub(super) fn build_dynamic_iife(
        &self,
        span: Span,
        tag_expr: Expression<'a>,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let empty = Span::empty(span.start);
        let tag_binding = self.fresh_name(DYNAMIC_TAG_BINDING, span);

        // `const _Tag = <expr>;`
        let binding = self.ast.binding_pattern_binding_identifier(empty, tag_binding);
        let declarator = self.ast.variable_declarator(
            empty,
            VariableDeclarationKind::Const,
            binding,
            NONE,
            Some(tag_expr),
            false,
        );
        let decl = self.ast.declaration_variable(
            empty,
            VariableDeclarationKind::Const,
            self.ast.vec1(declarator),
            false,
        );

        let h_call = self.build_h(span, self.ident(span.start, tag_binding), props, children);
        self.iife(span, self.ast.vec1(Statement::from(decl)), h_call)
    }

    /// Build `iter.map((bind, _i) => Fragment({ key: _i }, ...children))`.
    pub(super) fn build_for_map(
        &self,
        span: Span,
        bind: BindingPattern<'a>,
        iter: Expression<'a>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let empty = Span::empty(span.start);
        let index_name = self.fresh_name(FOR_KEY_PARAM, span);

        // The arrow's expression body: `Fragment({ key: _i }, ...children)`.
        let key_props = self.ast.vec1(self.obj_prop(
            empty,
            empty,
            "key",
            self.ident(span.start, index_name),
            false,
        ));
        let fragment = self.build_keyed_fragment(span, key_props, children);

        // `iter.map((bind, _i) => Fragment(...))`.
        let index_pat = self.ast.binding_pattern_binding_identifier(empty, index_name);
        let arrow = self.arrow(
            span.start,
            [bind, index_pat],
            true,
            self.ast.vec1(self.ast.statement_expression(empty, fragment)),
        );
        self.call(span, self.member(span.start, iter, "map"), [arrow])
    }

    /// `(() => { …stmts…; return Fragment(...rest); })()`. Always synchronous — the reader does not
    /// `async`ify the IIFE from the presence of `await` in `stmts`.
    pub(super) fn build_statement_iife(
        &self,
        stmts: ArenaVec<'a, Statement<'a>>,
        rest: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let fragment = self.build_fragment(Span::empty(0), rest);
        self.iife(Span::empty(0), stmts, fragment)
    }

    // ===========================================================================================
    // `String.raw` tagged-template builders + code/math element
    // ===========================================================================================

    /// `String.raw\`<raw>\`` — a tagged template over a single raw quasi (no substitutions).
    ///
    /// `String.raw` cannot faithfully carry a backtick or a literal `${`: a backtick closes the
    /// template, `${` opens a substitution, and `String.raw` does NOT process a `\` escape — so any
    /// `\` added to neutralize them leaks into the runtime string. For content with either breaker we
    /// emit a **cooked** string literal instead, whose codegen escaping (`\\`, control chars, the
    /// closing quote) reproduces `raw` exactly. Breaker-free content keeps the readable `String.raw`
    /// form (contract §3).
    pub(super) fn build_string_raw(&self, span: Span, raw: &'a str) -> Expression<'a> {
        if Self::has_template_breaker(raw) {
            return self.ast.expression_string_literal(span, raw, None);
        }
        let ast = self.ast;
        let mut quasis = ast.vec_with_capacity(1);
        quasis.push(self.raw_quasi(span, raw, true));
        let quasi = ast.template_literal(span, quasis, ast.vec());
        self.tag_string_raw(span, quasi)
    }

    /// One template-literal quasi carrying `raw` **verbatim** as its raw value, `cooked: None` (a
    /// `String.raw` tag reads only the raw text: `\` and `{}` are NOT interpreted). We do **not**
    /// use codegen's `escape_raw` (which doubles every `\`, wrong for `String.raw`); the caller
    /// guarantees `raw` is breaker-free.
    fn raw_quasi(&self, span: Span, raw: &'a str, tail: bool) -> TemplateElement<'a> {
        debug_assert!(!Self::has_template_breaker(raw));
        let value = TemplateElementValue { raw: self.ast.str(raw), cooked: None };
        self.ast.template_element(span, value, tail, false)
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

    /// Build the ambient-prelude element `h(<Name>, { <props> }, [<raw-children>])` for a code/math
    /// span (`CodeInline`/`CodeBlock`/`Tex` — referenced as identifiers, no import emitted).
    pub(super) fn build_raw_element(
        &self,
        span: Span,
        name: &'a str,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        self.build_h(span, self.ident(span.start, name), props, children)
    }

    // ===========================================================================================
    // Document assembly + `%`-statement routing + F1 component hoisting
    // ===========================================================================================

    /// Route a parsed top-level statement: `import`/`export`/component bindings hoist to module
    /// scope (component bindings add `export` + the name argument); everything else prepends into
    /// `Doc`.
    pub(super) fn route_statement(
        &mut self,
        stmt: Statement<'a>,
        module_items: &mut ArenaVec<'a, Statement<'a>>,
        doc_prelude: &mut ArenaVec<'a, Statement<'a>>,
    ) {
        // Diagnose a binding / default-export that would collide with the reader's emit surface
        // (`Doc`, the runtime imports) before routing it — the oxc parser cannot catch these (the
        // collision is with names the *lowering* injects, not with anything in the source).
        self.check_reserved_collisions(&stmt);
        // A `%`/`%%%` statement body is embedded JS/TS spliced verbatim (full capabilities).
        self.record_nota_mapping(stmt.span(), NotaMappingKind::EmbeddedJs);
        match stmt {
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportAllDeclaration(_) => module_items.push(stmt),
            Statement::VariableDeclaration(mut decl) if Self::is_f1_component_decl(&decl) => {
                self.attach_f1_name(&mut decl);
                let export = self.make_export_named_decl(Declaration::VariableDeclaration(decl));
                module_items.push(export);
            }
            other => doc_prelude.push(other),
        }
    }

    /// Diagnose a routed top-level statement that collides with the reader's emit surface: a module
    /// binding named like a reserved emit name ([`is_reserved_emit_name`]), or a `% export default`
    /// (a second default export beside `export default function Doc`). The statement is still routed
    /// as usual — the diagnostic is advisory (the emit would be broken/ambiguous JS otherwise).
    ///
    /// Bindings come from ECMA's `BoundNames` ([`oxc_ecmascript::BoundNames`]), so destructured
    /// names collide too (`%const { h } = lib`, `%const [Doc] = xs`, rest elements), as do import
    /// locals and `% export`-wrapped declarations.
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

    /// Build the `export default function Doc() { …prelude…; return decode(Fragment(...)); }` module.
    /// `Doc` is always synchronous — the reader does not `async`ify it from `await` in the prelude.
    pub(super) fn build_document(
        &self,
        siblings: ArenaVec<'a, Expression<'a>>,
        module_items: ArenaVec<'a, Statement<'a>>,
        doc_prelude: ArenaVec<'a, Statement<'a>>,
    ) -> Program<'a> {
        let ast = self.ast;
        let empty = Span::empty(0);

        let fragment = self.build_fragment(empty, siblings);
        let decoded = self.build_decode(empty, fragment);
        let return_stmt = ast.statement_return(empty, Some(decoded));

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
            SourceType::default().with_module(true),
            self.source_text,
            ast.vec(),
            None,
            ast.vec(),
            program_body,
        )
    }

    /// `decode(<expr>)`.
    fn build_decode(&self, span: Span, expr: Expression<'a>) -> Expression<'a> {
        self.call(span, self.ident(span.start, DECODE), [expr])
    }

    /// Is `decl` a single `let/const X = inlineComponent(...)|blockComponent(...)` binding?
    fn is_f1_component_decl(decl: &VariableDeclaration<'a>) -> bool {
        decl.declarations.len() == 1
            && decl.declarations[0].id.get_binding_identifier().is_some()
            && decl.declarations[0].init.as_ref().is_some_and(is_f1_constructor)
    }

    /// Pass the binding name as the constructor's 2nd argument (`inlineComponent(fn, "Name")`), and
    /// wrap the component body's returned markup in `decode(...)`.
    fn attach_f1_name(&self, decl: &mut VariableDeclaration<'a>) {
        let declarator = &mut decl.declarations[0];
        let Some(name) = declarator.id.get_binding_identifier().map(|id| id.name) else {
            return;
        };
        if let Some(Expression::CallExpression(call)) = declarator.init.as_mut() {
            if let Some(arg0) = call.arguments.first_mut() {
                self.wrap_component_returns(arg0);
            }
            // F1: the component name passed to the constructor MUST be the binding name (so the
            // island manifest's `comp` matches the exported registry key) — override any user-
            // supplied 2nd argument rather than keeping it.
            let name_lit = self.ast.expression_string_literal(Span::empty(0), name, None);
            if call.arguments.len() >= 2 {
                call.arguments[1] = Argument::from(name_lit);
            } else {
                call.arguments.push(Argument::from(name_lit));
            }
        }
    }

    /// Wrap a component-constructor function argument's returned markup in `decode(...)`. The body
    /// markup is still an un-lowered `Expression::NotaMarkup` here (the walk lowers inside the
    /// `decode(...)` afterwards), so `is_markup_call` treats `NotaMarkup` as markup.
    fn wrap_component_returns(&self, arg: &mut Argument<'a>) {
        let Some(expr) = arg.as_expression_mut() else { return };
        match expr {
            Expression::ArrowFunctionExpression(arrow) => {
                if arrow.expression {
                    if let Some(Statement::ExpressionStatement(es)) =
                        arrow.body.statements.first_mut()
                    {
                        self.wrap_expr_in_decode(&mut es.expression);
                    }
                } else {
                    self.wrap_return_statements(&mut arrow.body.statements);
                }
            }
            Expression::FunctionExpression(func) => {
                if let Some(body) = func.body.as_mut() {
                    self.wrap_return_statements(&mut body.statements);
                }
            }
            _ => {}
        }
    }

    /// Wrap the argument of each top-level `return <markup>;` in `decode(...)`.
    fn wrap_return_statements(&self, stmts: &mut ArenaVec<'a, Statement<'a>>) {
        for stmt in stmts.iter_mut() {
            if let Statement::ReturnStatement(ret) = stmt
                && let Some(arg) = ret.argument.as_mut()
            {
                self.wrap_expr_in_decode(arg);
            }
        }
    }

    /// Replace `expr` with `decode(expr)` iff it is unwrapped markup (`h(...)`/`Fragment(...)`/a
    /// not-yet-lowered `NotaMarkup`).
    fn wrap_expr_in_decode(&self, expr: &mut Expression<'a>) {
        if !is_markup_call(expr) {
            return;
        }
        let taken = std::mem::replace(expr, self.ast.expression_null_literal(Span::empty(0)));
        *expr = self.build_decode(Span::empty(0), taken);
    }

    /// `export <decl>;` (named export of a declaration).
    fn make_export_named_decl(&self, decl: Declaration<'a>) -> Statement<'a> {
        let ast = self.ast;
        let export = ast.module_declaration_export_named_declaration(
            Span::empty(0),
            Some(decl),
            ast.vec(),
            None,
            ImportOrExportKind::Value,
            NONE,
        );
        Statement::from(export)
    }
}
