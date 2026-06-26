//! Nota markup → hyperscript lowering (the Nota transform).
//!
//! The Nota *reader* (in `oxc_parser`) parses `@`-markup into a faithful Nota AST: a `.nota` document
//! is a single `Expression::NotaMarkup(NotaMarkupKind::Document(..))` statement, and every embedded
//! `@`-form is an `Expression::NotaMarkup` left in place. This module *lowers* those nodes to the
//! hyperscript `h`/`Fragment`/`decode` `Expression` AST — the deferred-pass analog of how
//! `oxc_transformer` lowers JSX to `createElement`.
//!
//! Entry: [`NotaLowering`]. The document is rebuilt by [`NotaLowering::lower_document_program`] (Doc
//! skeleton, `%`-statement routing / F1 hoist+export, decode-wraps); embedded `@`-forms are then
//! replaced by a [`oxc_ast_visit::VisitMut`] walk. Optionally collects Volar [`NotaMappingMark`]s.

use oxc_ast::ast::*;

mod build;
mod lower;
mod mapping;
mod scribble;

pub use lower::{NotaLowering, NotaLoweringReturn};
pub use mapping::{NotaMappingKind, NotaMappingMark};

/// Runtime hyperscript names (`import { h, Fragment, decode, ... } from "@nota-lang/runtime"`).
const H: &str = "h";
const FRAGMENT: &str = "Fragment";
const DECODE: &str = "decode";
/// The fresh component-cased binding for a dynamic-tag IIFE (`@(getTag()){…}`).
const DYNAMIC_TAG_BINDING: &str = "_Tag";
/// The fresh map-index parameter injected as the `@for` body's `Fragment` key.
const FOR_KEY_PARAM: &str = "_i";
/// The default-export document component name.
const DOC: &str = "Doc";
/// The component constructors (their `%const X = inlineComponent(...)` bindings hoist+export).
const INLINE_COMPONENT: &str = "inlineComponent";
const BLOCK_COMPONENT: &str = "blockComponent";
/// Ambient-prelude tags for code/math spans (referenced as identifiers — no import emitted).
const CODE_INLINE: &str = "CodeInline";
const CODE_BLOCK: &str = "CodeBlock";
const MATH: &str = "Math";

/// A tag name is a *component* (identifier) iff it starts with an uppercase ASCII letter; otherwise
/// it is a *host* element (string tag).
fn is_component_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}

/// A dynamic-tag head expression is "already a valid tag" (emit directly, no `_Tag` binding) iff it
/// is a Capitalized identifier or a *static* member expression — a name JSX would also accept as a
/// tag (`@(Box)` → `h(Box,…)`, `@(ui.Card)` → `h(ui.Card,…)`). A *computed* member (`@(comps[k])`) or
/// any other expression goes through the `_Tag` IIFE.
fn is_valid_tag_expr(expr: &Expression) -> bool {
    match expr {
        Expression::Identifier(id) => is_component_name(&id.name),
        // Static member chains only (`a.b.c`); the object side may be anything name-like.
        Expression::StaticMemberExpression(_) => true,
        _ => false,
    }
}

/// Is `expr` an *unwrapped* markup call — `h(...)` or `Fragment(...)` (NOT already `decode(...)`)?
/// Used to decide whether a component body's return value needs a `decode(...)` wrap.
fn is_markup_call(expr: &Expression) -> bool {
    // An un-lowered `@`-form (a component body, before the lowering walk reaches it) is markup, as is
    // a lowered `h(...)`/`Fragment(...)` call.
    if matches!(expr, Expression::NotaMarkup(_)) {
        return true;
    }
    let Expression::CallExpression(call) = expr else { return false };
    let Expression::Identifier(callee) = &call.callee else { return false };
    matches!(callee.name.as_str(), H | FRAGMENT)
}

/// The component constructor name if `init` is a call to `inlineComponent`/`blockComponent`, else
/// `None`.
fn f1_constructor_name<'a>(init: &Expression<'a>) -> Option<&'a str> {
    let Expression::CallExpression(call) = init else { return None };
    let Expression::Identifier(callee) = &call.callee else { return None };
    match callee.name.as_str() {
        INLINE_COMPONENT => Some(INLINE_COMPONENT),
        BLOCK_COMPONENT => Some(BLOCK_COMPONENT),
        _ => None,
    }
}

/// Does a top-level statement use top-level `await` (so its host `Doc` must be `async`)? We look for
/// an `AwaitExpression` in a variable-declaration initializer or an expression statement, without
/// descending into nested function/arrow bodies (whose `await` belongs to that function).
fn statement_uses_await(stmt: &Statement) -> bool {
    match stmt {
        Statement::VariableDeclaration(decl) => {
            decl.declarations.iter().any(|d| d.init.as_ref().is_some_and(expr_has_top_await))
        }
        Statement::ExpressionStatement(es) => expr_has_top_await(&es.expression),
        _ => false,
    }
}

/// Recursively check an expression for an `await` not under a nested function/arrow boundary. Used
/// both for `%` statements and (over the lowered document body) for `await` embedded in markup — a
/// prop value (`@p[x: await f()]`), an interpolation (`@(await f())`), or a `@for` iterable
/// (`@for(x of await xs)`) — all of which must make `Doc` `async`.
fn expr_has_top_await(expr: &Expression) -> bool {
    match expr {
        Expression::AwaitExpression(_) => true,
        Expression::ParenthesizedExpression(p) => expr_has_top_await(&p.expression),
        Expression::CallExpression(c) => {
            expr_has_top_await(&c.callee) || c.arguments.iter().any(arg_has_top_await)
        }
        Expression::SequenceExpression(s) => s.expressions.iter().any(expr_has_top_await),
        Expression::BinaryExpression(b) => {
            expr_has_top_await(&b.left) || expr_has_top_await(&b.right)
        }
        Expression::LogicalExpression(b) => {
            expr_has_top_await(&b.left) || expr_has_top_await(&b.right)
        }
        Expression::ConditionalExpression(c) => {
            expr_has_top_await(&c.test)
                || expr_has_top_await(&c.consequent)
                || expr_has_top_await(&c.alternate)
        }
        Expression::AssignmentExpression(a) => expr_has_top_await(&a.right),
        // Markup lowers to `h(tag, { …props }, [ …children ])`, so descend into object property
        // values, array elements, and member objects to catch await embedded in a prop / child /
        // iterable.
        Expression::ObjectExpression(o) => o.properties.iter().any(|p| match p {
            ObjectPropertyKind::ObjectProperty(prop) => expr_has_top_await(&prop.value),
            ObjectPropertyKind::SpreadProperty(s) => expr_has_top_await(&s.argument),
        }),
        Expression::ArrayExpression(a) => a.elements.iter().any(|e| match e {
            ArrayExpressionElement::SpreadElement(s) => expr_has_top_await(&s.argument),
            other => other.as_expression().is_some_and(expr_has_top_await),
        }),
        Expression::StaticMemberExpression(m) => expr_has_top_await(&m.object),
        Expression::ComputedMemberExpression(m) => {
            expr_has_top_await(&m.object) || expr_has_top_await(&m.expression)
        }
        // Do NOT descend into function/arrow bodies (their await is theirs).
        _ => false,
    }
}

fn arg_has_top_await(arg: &Argument) -> bool {
    match arg {
        Argument::SpreadElement(s) => expr_has_top_await(&s.argument),
        _ => arg.as_expression().is_some_and(expr_has_top_await),
    }
}
