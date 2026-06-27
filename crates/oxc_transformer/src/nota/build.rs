//! Hyperscript emit primitives + document/F1 assembly for [`super::lower::NotaLowering`].
//!
//! These build the lowered `h`/`Fragment`/`decode`/`String.raw` `Expression`s and the document
//! `Program` (Doc skeleton, `%`-statement routing, F1 component hoist+export, decode-wraps). They are
//! the leaf builders the [`NotaLowering`] methods call — moved here from the parser (they no longer
//! depend on parse state, only on the lowering's [`oxc_ast::AstBuilder`] + mapping accumulator).

use oxc_allocator::Vec as ArenaVec;
use oxc_ast::{NONE, ast::*};
use oxc_diagnostics::OxcDiagnostic;
use oxc_span::{GetSpan, SourceType, Span};

use super::lower::NotaLowering;
use super::mapping::NotaMappingKind;
use super::{
    BLOCK_COMPONENT, DECODE, DOC, DYNAMIC_TAG_BINDING, FOR_KEY_PARAM, FRAGMENT, H,
    INLINE_COMPONENT, f1_constructor_name, is_markup_call,
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
        let ast = self.ast;
        let callee = ast.expression_identifier(Span::empty(span.start), H);
        let props_obj = ast.expression_object(Span::empty(span.start), props);
        let children_arr = self.children_array(span, children);
        let mut arguments = ast.vec_with_capacity(3);
        arguments.push(Argument::from(tag));
        arguments.push(Argument::from(props_obj));
        arguments.push(Argument::from(children_arr));
        ast.expression_call(span, callee, NONE, arguments, false)
    }

    /// `Fragment(...children)` — variadic call (no props, no array wrap).
    pub(super) fn build_fragment(
        &self,
        span: Span,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let ast = self.ast;
        let callee = ast.expression_identifier(Span::empty(span.start), FRAGMENT);
        let mut arguments = ast.vec_with_capacity(children.len());
        for child in children {
            arguments.push(Argument::from(child));
        }
        ast.expression_call(span, callee, NONE, arguments, false)
    }

    /// `[children]` array-expression for the third `h(...)` argument.
    fn children_array(&self, span: Span, children: ArenaVec<'a, Expression<'a>>) -> Expression<'a> {
        let ast = self.ast;
        let mut elements = ast.vec_with_capacity(children.len());
        for child in children {
            elements.push(ArrayExpressionElement::from(child));
        }
        ast.expression_array(Span::empty(span.end), elements)
    }

    /// Pick a fresh identifier name for a reader-injected binding (`_i`, `_Tag`) that cannot collide
    /// with a user identifier in the construct at `span`. Returns `candidate` unless it appears as a
    /// whole word in the construct's source (`@for(_i of …)`, `@(_Tag)`), in which case a numeric
    /// suffix is appended until free. (Scanning the source over-approximates — a name in a string or
    /// comment also bumps — which only ever yields a *more* distinct name, never a colliding one.)
    fn fresh_name(&self, candidate: &'static str, span: Span) -> &'a str {
        fn contains_word(haystack: &str, needle: &str) -> bool {
            let bytes = haystack.as_bytes();
            let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
            let mut from = 0;
            while let Some(pos) = haystack[from..].find(needle) {
                let start = from + pos;
                let end = start + needle.len();
                let before = start == 0 || !is_ident(bytes[start - 1]);
                let after = end >= bytes.len() || !is_ident(bytes[end]);
                if before && after {
                    return true;
                }
                from = start + 1;
            }
            false
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
        let ast = self.ast;
        let empty = Span::empty(span.start);
        let tag_binding = self.fresh_name(DYNAMIC_TAG_BINDING, span);

        // `const _Tag = <expr>;`
        let binding = ast.binding_pattern_binding_identifier(empty, tag_binding);
        let declarator = ast.variable_declarator(
            empty,
            VariableDeclarationKind::Const,
            binding,
            NONE,
            Some(tag_expr),
            false,
        );
        let decl = ast.declaration_variable(
            empty,
            VariableDeclarationKind::Const,
            ast.vec1(declarator),
            false,
        );
        let const_stmt = Statement::from(decl);

        // `return h(_Tag, { props }, [children]);`
        let tag_ref = ast.expression_identifier(empty, tag_binding);
        let h_call = self.build_h(span, tag_ref, props, children);
        let return_stmt = ast.statement_return(empty, Some(h_call));

        // `() => { … }`
        let body = ast.function_body(empty, ast.vec(), {
            let mut stmts = ast.vec_with_capacity(2);
            stmts.push(const_stmt);
            stmts.push(return_stmt);
            stmts
        });
        let arrow = ast.expression_arrow_function(
            empty,
            false, // not an expression body
            false, // not async
            NONE,
            ast.formal_parameters(
                empty,
                FormalParameterKind::ArrowFormalParameters,
                ast.vec(),
                NONE,
            ),
            NONE,
            body,
        );
        ast.expression_call(span, arrow, NONE, ast.vec(), false)
    }

    /// Build `iter.map((bind, _i) => Fragment({ key: _i }, ...children))`.
    pub(super) fn build_for_map(
        &self,
        span: Span,
        bind: BindingPattern<'a>,
        iter: Expression<'a>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let ast = self.ast;
        let empty = Span::empty(span.start);
        let index_name = self.fresh_name(FOR_KEY_PARAM, span);

        // The arrow's wrapping `Fragment({ key: _i }, ...children)`.
        let key_props = {
            let key_name = ast.expression_identifier(empty, index_name);
            let key = PropertyKey::StaticIdentifier(ast.alloc_identifier_name(empty, "key"));
            let prop = ast.alloc_object_property(
                empty,
                PropertyKind::Init,
                key,
                key_name,
                false,
                false,
                false,
            );
            ast.vec1(ObjectPropertyKind::ObjectProperty(prop))
        };
        let fragment = self.build_keyed_fragment(span, key_props, children);

        // `(bind, _i) => Fragment(...)`.
        let mut params = ast.vec_with_capacity(2);
        params.push(ast.formal_parameter(
            empty,
            ast.vec(),
            bind,
            NONE,
            NONE,
            false,
            None,
            false,
            false,
        ));
        let index_pat = ast.binding_pattern_binding_identifier(empty, index_name);
        params.push(ast.formal_parameter(
            empty,
            ast.vec(),
            index_pat,
            NONE,
            NONE,
            false,
            None,
            false,
            false,
        ));
        let body = ast.function_body(
            empty,
            ast.vec(),
            ast.vec1(ast.statement_expression(empty, fragment)),
        );
        let arrow = ast.expression_arrow_function(
            empty,
            true, // expression body
            false,
            NONE,
            ast.formal_parameters(empty, FormalParameterKind::ArrowFormalParameters, params, NONE),
            NONE,
            body,
        );

        // `iter.map(<arrow>)`.
        let map_member = Expression::StaticMemberExpression(ast.alloc_static_member_expression(
            empty,
            iter,
            ast.identifier_name(empty, "map"),
            false,
        ));
        let mut args = ast.vec_with_capacity(1);
        args.push(Argument::from(arrow));
        ast.expression_call(span, map_member, NONE, args, false)
    }

    /// `Fragment({ key: _i }, ...children)` — a `Fragment` with a leading props arg.
    fn build_keyed_fragment(
        &self,
        span: Span,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let ast = self.ast;
        let callee = ast.expression_identifier(Span::empty(span.start), FRAGMENT);
        let props_obj = ast.expression_object(Span::empty(span.start), props);
        let mut arguments = ast.vec_with_capacity(children.len() + 1);
        arguments.push(Argument::from(props_obj));
        for child in children {
            arguments.push(Argument::from(child));
        }
        ast.expression_call(span, callee, NONE, arguments, false)
    }

    /// `(() => { …stmts…; return Fragment(...rest); })()`. Always synchronous — the reader does not
    /// `async`ify the IIFE from the presence of `await` in `stmts`.
    pub(super) fn build_statement_iife(
        &self,
        mut stmts: ArenaVec<'a, Statement<'a>>,
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
            false, // never async
            NONE,
            ast.formal_parameters(
                empty,
                FormalParameterKind::ArrowFormalParameters,
                ast.vec(),
                NONE,
            ),
            NONE,
            body,
        );
        ast.expression_call(empty, arrow, NONE, ast.vec(), false)
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

    /// `String.raw\`q0${e0}q1${e1}…\`` — a tagged template with substitutions (math `@`-interp).
    pub(super) fn build_string_raw_interp(
        &self,
        span: Span,
        quasis_raw: Vec<&'a str>,
        exprs: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let ast = self.ast;
        debug_assert_eq!(quasis_raw.len(), exprs.len() + 1);
        let last = quasis_raw.len() - 1;
        let mut quasis = ast.vec_with_capacity(quasis_raw.len());
        for (i, q) in quasis_raw.into_iter().enumerate() {
            quasis.push(self.raw_quasi(span, q, i == last));
        }
        let quasi = ast.template_literal(span, quasis, exprs);
        self.tag_string_raw(span, quasi)
    }

    /// One template-literal quasi carrying `raw` as its **raw** value with `cooked: None` (so a
    /// `String.raw` tag reproduces `raw`: `\` and `{}` are NOT interpreted). We do **not** use
    /// codegen's `escape_raw` (which doubles every `\`); we escape only the two template-syntax
    /// breakers — a backtick and a `${` — by prefixing a `\`.
    fn raw_quasi(&self, span: Span, raw: &'a str, tail: bool) -> TemplateElement<'a> {
        let escaped = self.escape_raw_template_syntax(raw);
        let value = TemplateElementValue { raw: self.ast.str(escaped), cooked: None };
        self.ast.template_element(span, value, tail, false)
    }

    /// Does `raw` contain a template-syntax breaker — a backtick or a `${` — that a `String.raw`
    /// template cannot represent without a `\` that leaks at runtime?
    fn has_template_breaker(raw: &str) -> bool {
        let bytes = raw.as_bytes();
        bytes
            .iter()
            .enumerate()
            .any(|(i, &b)| b == b'`' || (b == b'$' && bytes.get(i + 1) == Some(&b'{')))
    }

    /// Prefix a `\` before each backtick and each `${` in `raw` (the only template-syntax breakers),
    /// returning the original slice unchanged when neither occurs (the common case — no allocation).
    fn escape_raw_template_syntax(&self, raw: &'a str) -> &'a str {
        let bytes = raw.as_bytes();
        if !Self::has_template_breaker(raw) {
            return raw;
        }
        let mut out = String::with_capacity(bytes.len() + 8);
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'`' || (b == b'$' && bytes.get(i + 1) == Some(&b'{')) {
                out.push('\\');
            }
            let ch_len =
                if b < 0x80 { 1 } else { raw[i..].chars().next().map_or(1, char::len_utf8) };
            out.push_str(&raw[i..i + ch_len]);
            i += ch_len;
        }
        self.ast.allocator.alloc_str(&out)
    }

    /// `String.raw` — the member-expression callee for the raw tagged template.
    fn tag_string_raw(&self, span: Span, quasi: TemplateLiteral<'a>) -> Expression<'a> {
        let ast = self.ast;
        let empty = Span::empty(span.start);
        let object = ast.expression_identifier(empty, "String");
        let property = ast.identifier_name(empty, "raw");
        let tag = Expression::StaticMemberExpression(
            ast.alloc_static_member_expression(empty, object, property, false),
        );
        ast.expression_tagged_template(span, tag, NONE, quasi)
    }

    /// Build the ambient-prelude element `h(<Name>, { <props> }, [<raw-children>])` for a code/math
    /// span (`CodeInline`/`CodeBlock`/`Math` — referenced as identifiers, no import emitted).
    pub(super) fn build_raw_element(
        &self,
        span: Span,
        name: &'a str,
        props: ArenaVec<'a, ObjectPropertyKind<'a>>,
        children: ArenaVec<'a, Expression<'a>>,
    ) -> Expression<'a> {
        let tag = self.ast.expression_identifier(Span::new(span.start, span.start), name);
        self.build_h(span, tag, props, children)
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
    fn check_reserved_collisions(&mut self, stmt: &Statement<'a>) {
        match stmt {
            Statement::ExportDefaultDeclaration(d) => self.error(duplicate_default_export(d.span)),
            Statement::ImportDeclaration(imp) => {
                let Some(specifiers) = &imp.specifiers else { return };
                for spec in specifiers {
                    let local = match spec {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => &s.local,
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => &s.local,
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => &s.local,
                    };
                    self.check_reserved_binding(local.name.as_str(), local.span);
                }
            }
            Statement::VariableDeclaration(v) => self.check_var_decl_bindings(v),
            Statement::FunctionDeclaration(f) => {
                if let Some(id) = &f.id {
                    self.check_reserved_binding(id.name.as_str(), id.span);
                }
            }
            Statement::ClassDeclaration(c) => {
                if let Some(id) = &c.id {
                    self.check_reserved_binding(id.name.as_str(), id.span);
                }
            }
            // `% export const Doc = …` / `% export function h() {}` — check the inner declaration.
            Statement::ExportNamedDeclaration(e) => match &e.declaration {
                Some(Declaration::VariableDeclaration(v)) => self.check_var_decl_bindings(v),
                Some(Declaration::FunctionDeclaration(f)) => {
                    if let Some(id) = &f.id {
                        self.check_reserved_binding(id.name.as_str(), id.span);
                    }
                }
                Some(Declaration::ClassDeclaration(c)) => {
                    if let Some(id) = &c.id {
                        self.check_reserved_binding(id.name.as_str(), id.span);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Check every binding identifier of a `let/const/var` declaration against the reserved set,
    /// walking the binding PATTERN so destructured names collide too (`%const { h } = lib`,
    /// `%const [Doc] = xs`, `%const { x: h } = …`, rest elements), not only a plain `%const h = …`.
    fn check_var_decl_bindings(&mut self, decl: &VariableDeclaration<'a>) {
        for d in &decl.declarations {
            for id in d.id.get_binding_identifiers() {
                self.check_reserved_binding(id.name.as_str(), id.span);
            }
        }
    }

    /// Emit a collision diagnostic iff `name` is a reserved emit-surface name.
    fn check_reserved_binding(&mut self, name: &str, span: Span) {
        if is_reserved_emit_name(name) {
            self.error(reserved_name_collision(name, span));
        }
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
        let ast = self.ast;
        let callee = ast.expression_identifier(Span::empty(span.start), DECODE);
        let mut args = ast.vec_with_capacity(1);
        args.push(Argument::from(expr));
        ast.expression_call(span, callee, NONE, args, false)
    }

    /// Is `decl` a single `let/const X = inlineComponent(...)|blockComponent(...)` binding?
    fn is_f1_component_decl(decl: &VariableDeclaration<'a>) -> bool {
        decl.declarations.len() == 1
            && decl.declarations[0].id.get_binding_identifier().is_some()
            && decl.declarations[0]
                .init
                .as_ref()
                .is_some_and(|init| f1_constructor_name(init).is_some())
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
