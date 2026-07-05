//! Nota markup AST nodes.
//!
//! Nota is a document language whose `@`-syntax markup is parsed into these AST nodes and then
//! *lowered* (in a separate pass) to hyperscript `h`/`Fragment`/`decode` calls — the same
//! parse-then-lower shape oxc uses for JSX (see [`super::jsx`]). The reader lives in
//! `crates/oxc_parser/src/nota/`; the cross-team spec is `design/contract.md` and the
//! implementation memory is `NOTA_READER.md`.
//!
//! NB: `#[ast]`, `#[generate_derive(...)]`, `#[estree(...)]` and friends are markers consumed by
//! `tasks/ast_tools`; they do not affect the code directly. Run `just ast` after editing this file.
//!
//! ## Shape (faithful surface tree; Scribble whitespace + all Nota→JS lowering run in a later pass)
//!
//! [`NotaMarkup`] is the single umbrella that hangs off `Expression::NotaMarkup` (discriminant 40).
//! The eight `@`-forms live in the [`NotaForm`] sub-enum, whose variants are *inherited* (via
//! `inherit_variants!` — the `Statement`/`Declaration` pattern) by every position that can hold
//! any form: [`NotaMarkupKind`] (forms + the document root), the markup-body child list
//! [`NotaChild`], markup-valued props ([`NotaPropValue`]), and verbatim `|@` re-entries
//! ([`NotaVerbatimPart`]). Positions other than `NotaMarkupKind` cannot hold a document — by
//! construction, not by `unreachable!`. Embedded JS
//! ([`Expression`]/[`Statement`]/[`IdentifierReference`]/[`BindingPattern`]) sits verbatim at the
//! leaves with real source spans.

use std::cell::Cell;

use oxc_allocator::{Box, CloneIn, Dummy, GetAddress, TakeIn, UnstableAddress, Vec};
use oxc_ast_macros::ast;
use oxc_estree::ESTree;
use oxc_span::{ContentEq, GetSpan, GetSpanMut, Span};
use oxc_str::Str;
use oxc_syntax::node::NodeId;

use super::js::{BindingPattern, Expression, IdentifierReference, Statement};
use super::macros::inherit_variants;

// ===============================================================================================
// Umbrella
// ===============================================================================================

/// A Nota markup form in expression position — the payload of `Expression::NotaMarkup`.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaMarkup<'a> {
    /// Unique identifier for this AST node.
    pub node_id: Cell<NodeId>,
    /// Node location in source code.
    pub span: Span,
    /// Which markup form this is.
    pub kind: NotaMarkupKind<'a>,
}

/// One `@`-form — the eight markup forms sharable across every position that can hold a form.
///
/// [`NotaMarkupKind`], [`NotaChild`], [`NotaPropValue`], and [`NotaVerbatimPart`] all inherit
/// these variants via `inherit_variants!`, so a parsed form converts to any of those positions
/// with the zero-cost `From`/`to_nota_form` conversions (`shared_enum_variants!`).
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaForm<'a> {
    /// `@p[..]{..}` / `@Aside{..}` / `@(expr){..}` — an element.
    Element(Box<'a, NotaElement<'a>>) = 0,
    /// `@{..}` — an anonymous fragment.
    Fragment(Box<'a, NotaFragment<'a>>) = 1,
    /// `@name` / `@(expr)` — an interpolated JS expression.
    Interpolation(Box<'a, NotaInterpolation<'a>>) = 2,
    /// `@if (c) {..} else {..}`.
    If(Box<'a, NotaIf<'a>>) = 3,
    /// `@for (x of xs) {..}`.
    For(Box<'a, NotaFor<'a>>) = 4,
    /// `` `code` `` / fenced ```` ```lang ```` — inline or block code.
    Code(Box<'a, NotaCode<'a>>) = 5,
    /// `$math$` / `$$display$$`.
    Math(Box<'a, NotaMath<'a>>) = 6,
    /// `@tag|{ raw }|` — a verbatim body.
    Verbatim(Box<'a, NotaVerbatim<'a>>) = 7,
}

/// Macro for matching `NotaForm`'s variants on an enum that inherits them.
#[macro_export]
macro_rules! match_nota_form {
    ($ty:ident) => {
        $ty::Element(_)
            | $ty::Fragment(_)
            | $ty::Interpolation(_)
            | $ty::If(_)
            | $ty::For(_)
            | $ty::Code(_)
            | $ty::Math(_)
            | $ty::Verbatim(_)
    };
}
pub use match_nota_form;

inherit_variants! {
/// The markup forms reachable in expression position, plus the document root.
///
/// Inherits variants from [`NotaForm`]. See [`ast` module docs] for explanation of inheritance.
///
/// [`ast` module docs]: `super`
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaMarkupKind<'a> {
    /// The whole `.nota` document (top-level form; full-document deferral).
    Document(Box<'a, NotaDocument<'a>>) = 8,
    // `NotaForm` variants added here by `inherit_variants!` macro
    @inherit NotaForm
}
}

// ===============================================================================================
// Document
// ===============================================================================================

/// A whole `.nota` document: a source-ordered run of top-level items (markup + `%`/`%%%` statements).
/// Lowering builds the `Doc` skeleton, hoists/routes statements (F1, imports), and wraps in `decode`.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaDocument<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    /// Top-level items in source order ([`NotaChild::Statement`] carries `%`/`%%%` lines).
    pub items: Vec<'a, NotaChild<'a>>,
}

// ===============================================================================================
// Children (markup-body items)
// ===============================================================================================

inherit_variants! {
/// One item in a markup body: a form, an embedded statement, or line/inline sugar.
///
/// Inherits variants from [`NotaForm`]. See [`ast` module docs] for explanation of inheritance.
///
/// [`ast` module docs]: `super`
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaChild<'a> {
    /// A raw literal text run (whitespace unprocessed; Scribble runs in lowering).
    Text(Box<'a, NotaText<'a>>) = 8,
    /// A `%`/`%%%` embedded JS statement.
    Statement(Box<'a, NotaStatement<'a>>) = 9,
    /// `*strong*` / `_em_` emphasis.
    Emphasis(Box<'a, NotaEmphasis<'a>>) = 10,
    /// `#`..`######` heading.
    Heading(Box<'a, NotaHeading<'a>>) = 11,
    /// `-`/`+`/`N.` list item (per-line; the runtime coalesces runs).
    ListItem(Box<'a, NotaListItem<'a>>) = 12,
    // `NotaForm` variants added here by `inherit_variants!` macro
    @inherit NotaForm
}
}

/// A raw literal text run — the source slice, whitespace not yet processed.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaText<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    /// The raw source text (Scribble whitespace processing is deferred to lowering).
    pub value: Str<'a>,
}

/// A `%`/`%%%` line: an embedded JS statement, positioned in the markup stream.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaStatement<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub statement: Statement<'a>,
}

// ===============================================================================================
// Element
// ===============================================================================================

/// `@tag[props]{children}` — a Nota element.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaElement<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub tag: NotaTag<'a>,
    pub props: Vec<'a, NotaProp<'a>>,
    pub children: Vec<'a, NotaChild<'a>>,
    /// `true` when the body came from `@tag:` colon/block sugar (vs `@tag{…}` braces). The two
    /// syntaxes carry *different* whitespace contracts — a brace body's surrounding spaces between
    /// `{`/`}` and text are content, while a colon body trims its edges like a document/block — so
    /// the lowering must thread this to the Scribble pass (it cannot be recovered post-parse).
    pub is_colon: bool,
}

/// An element's tag: a host string, a component identifier, or a dynamic `@(expr)` head.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaTag<'a> {
    /// Lowercase host tag, e.g. `p` → the string `"p"`.
    Host(Box<'a, NotaHostName<'a>>) = 0,
    /// Capitalized component tag, e.g. `Aside` → the identifier `Aside`.
    Component(Box<'a, IdentifierReference<'a>>) = 1,
    /// `@(expr)` — a dynamic head; lowering decides direct-vs-IIFE.
    Dynamic(Box<'a, NotaDynamicTag<'a>>) = 2,
}

/// A host tag name (lowered to a string literal).
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaHostName<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub name: Str<'a>,
}

/// A `@(expr)` dynamic tag head.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaDynamicTag<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub expression: Expression<'a>,
}

// ===============================================================================================
// Props
// ===============================================================================================

/// One `[..]` prop entry: `key: value`, `shorthand`, or `...spread`.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaProp<'a> {
    /// `key: value`.
    Field(Box<'a, NotaFieldProp<'a>>) = 0,
    /// `name` — shorthand for `name: name`.
    Shorthand(Box<'a, NotaShorthandProp<'a>>) = 1,
    /// `...rest`.
    Spread(Box<'a, NotaSpreadProp<'a>>) = 2,
}

/// `key: value` prop.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaFieldProp<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub name: NotaPropName<'a>,
    pub value: NotaPropValue<'a>,
}

/// A prop key.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaPropName<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub name: Str<'a>,
}

inherit_variants! {
/// A prop value: a JS expression, or nested markup (a markup-valued prop, `key: @em{..}` —
/// the inherited [`NotaForm`] variants).
///
/// Inherits variants from [`NotaForm`]. See [`ast` module docs] for explanation of inheritance.
///
/// [`ast` module docs]: `super`
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaPropValue<'a> {
    /// `key: expr` (incl. string literals — they parse as `Expression::StringLiteral`).
    Expression(Box<'a, NotaPropExpr<'a>>) = 8,
    // `NotaForm` variants added here by `inherit_variants!` macro
    @inherit NotaForm
}
}

/// A JS-expression prop value.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaPropExpr<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub expression: Expression<'a>,
}

/// `name` shorthand prop (→ `{ name }`).
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaShorthandProp<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub name: IdentifierReference<'a>,
}

/// `...argument` spread prop.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaSpreadProp<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub argument: Expression<'a>,
}

// ===============================================================================================
// Fragment / interpolation / control flow
// ===============================================================================================

/// `@{children}` — an anonymous fragment (also used for control-flow branch/loop bodies).
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaFragment<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub children: Vec<'a, NotaChild<'a>>,
}

/// `@name` / `@(expr)` — an interpolated JS expression spliced as a child.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaInterpolation<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub expression: Expression<'a>,
}

/// `@if (test) {consequent} [else ..]`.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaIf<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub test: Expression<'a>,
    pub consequent: Box<'a, NotaFragment<'a>>,
    pub alternate: Option<NotaElse<'a>>,
}

/// The `else` / `else if` continuation of a [`NotaIf`].
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaElse<'a> {
    /// `else if (..) {..}`.
    ElseIf(Box<'a, NotaIf<'a>>) = 0,
    /// `else {..}`.
    Else(Box<'a, NotaFragment<'a>>) = 1,
}

/// `@for (binding of iterable) {body}`.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaFor<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub binding: BindingPattern<'a>,
    pub iterable: Expression<'a>,
    pub body: Box<'a, NotaFragment<'a>>,
}

// ===============================================================================================
// Verbatim / code / math
// ===============================================================================================

/// `` `inline` `` / fenced ```` ```lang⏎…⏎``` ```` code.
///
/// Raw text runs interleaved with `|@`-armed `@`-forms (the unified raw-span content model; only
/// `|@` re-enters Nota, a bare `@` is literal). Shares [`NotaVerbatimPart`] with verbatim and math.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaCode<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    /// Fence language tag, if any (block code only).
    pub lang: Option<Str<'a>>,
    /// `true` for a fenced block, `false` for inline.
    pub block: bool,
    /// Raw runs interleaved with `|@`-armed `@`-forms.
    pub parts: Vec<'a, NotaVerbatimPart<'a>>,
}

/// `$inline$` / `$$⏎display⏎$$` math.
///
/// Raw text runs interleaved with `|@`-armed `@`-forms (the unified raw-span content model; only
/// `|@` re-enters Nota, a bare `@` is literal). Shares [`NotaVerbatimPart`] with verbatim and code.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaMath<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    /// `true` for the `$$…$$` fenced (display) form; `false` for an inline `$…$` span. Mirrors
    /// [`NotaCode::block`] (the runtime prop stays named `display`).
    pub block: bool,
    /// Raw runs interleaved with `|@`-armed `@`-forms.
    pub parts: Vec<'a, NotaVerbatimPart<'a>>,
}

/// `@tag[props]|{ raw … |@form… }|` — a verbatim body (raw runs interleaved with re-entered
/// `@`-forms).
///
/// `[props]` groups compose with the verbatim body exactly as they do with a braced element body
/// (contract R19): they accumulate the same way, ahead of the same `|{…}|` delimiter that would
/// otherwise open directly against the head.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaVerbatim<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub tag: NotaTag<'a>,
    pub props: Vec<'a, NotaProp<'a>>,
    pub parts: Vec<'a, NotaVerbatimPart<'a>>,
}

inherit_variants! {
/// One piece of a [`NotaVerbatim`] body: a raw text run, or a `|@`-re-entered Nota form (a
/// sibling child — the inherited [`NotaForm`] variants).
///
/// Inherits variants from [`NotaForm`]. See [`ast` module docs] for explanation of inheritance.
///
/// [`ast` module docs]: `super`
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, GetAddress, ContentEq, ESTree)]
pub enum NotaVerbatimPart<'a> {
    /// A raw text run.
    Raw(Box<'a, NotaText<'a>>) = 8,
    // `NotaForm` variants added here by `inherit_variants!` macro
    @inherit NotaForm
}
}

// ===============================================================================================
// Surface sugar (faithful; lowered to host elements)
// ===============================================================================================

/// `*strong*` / `_em_` emphasis.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaEmphasis<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub marker: NotaEmphasisMarker,
    pub children: Vec<'a, NotaChild<'a>>,
}

/// Which emphasis marker was used.
#[ast]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[generate_derive(CloneIn, Dummy, ContentEq, ESTree)]
pub enum NotaEmphasisMarker {
    /// `*…*` → `<strong>`.
    Strong = 0,
    /// `_…_` → `<em>`.
    Em = 1,
}

/// `#`..`######` heading.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaHeading<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    /// Heading level 1–6 (number of `#`).
    pub level: u8,
    pub children: Vec<'a, NotaChild<'a>>,
}

/// A `-`/`+`/`N.` list item (one per line; the runtime coalesces runs into `<ul>`/`<ol>`).
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaListItem<'a> {
    pub node_id: Cell<NodeId>,
    pub span: Span,
    pub kind: NotaListKind,
    pub children: Vec<'a, NotaChild<'a>>,
}

/// Whether a list item is unordered (`-` → `nota-ul-li`) or ordered (`+`/`N.` → `nota-ol-li`).
#[ast]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[generate_derive(CloneIn, Dummy, ContentEq, ESTree)]
pub enum NotaListKind {
    /// `-`.
    Unordered = 0,
    /// `+` or `N.`.
    Ordered = 1,
}

#[cfg(test)]
mod tests {
    use oxc_allocator::{Allocator, CloneIn};
    use oxc_span::{GetSpan, Span};

    use crate::{AstBuilder, AstKind, ast::*};

    /// Build `@p{Hello}` as a [`NotaElement`] wrapped in `Expression::NotaMarkup`, exercising the
    /// generated plumbing: construction via [`AstBuilder`], `GetSpan`, `CloneIn`, and `AstKind`.
    #[test]
    fn build_nota_element_clone_and_kind() {
        let allocator = Allocator::default();
        let ast = AstBuilder::new(&allocator);

        let outer = Span::new(0, 9);
        let element = ast.nota_element(
            outer,
            ast.nota_tag_host(Span::new(1, 2), "p"),
            ast.vec(),
            ast.vec1(ast.nota_child_text(Span::new(3, 8), "Hello")),
            false,
        );
        let expr = ast.expression_nota_markup(outer, NotaMarkupKind::Element(ast.alloc(element)));

        // It is an expression; the umbrella carries the outer span; AstKind resolves.
        assert!(matches!(expr, Expression::NotaMarkup(_)));
        assert_eq!(expr.span(), outer);
        assert!(matches!(AstKind::from_expression(&expr), AstKind::NotaMarkup(_)));

        // `CloneIn` into a fresh arena preserves the structure and spans.
        let arena2 = Allocator::default();
        let cloned = expr.clone_in(&arena2);
        assert_eq!(cloned.span(), outer);
        let Expression::NotaMarkup(m) = &cloned else { panic!("expected NotaMarkup") };
        let NotaMarkupKind::Element(el) = &m.kind else { panic!("expected Element") };
        let NotaTag::Host(host) = &el.tag else { panic!("expected Host tag") };
        assert_eq!(host.name.as_str(), "p");
        assert_eq!(el.children.len(), 1);
    }
}
