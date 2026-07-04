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
//!
//! Semantic pin: `Doc` and the nested-`%` IIFE are always emitted **synchronous** — the presence of
//! `await` does not auto-`async`ify them. Top-level `await` therefore emits JS that does not parse,
//! by design (not a silent rewrite).

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
/// `Tex`, not `Math` (contract R14): the ambient identifier must not capture the JS `Math` global —
/// the integrator's prelude inject rewrites *free* references, so `% Math.floor(x)` would break.
const MATH: &str = "Tex";

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

/// Is `init` a call to a component constructor (`inlineComponent`/`blockComponent`)? Such a
/// binding is F1-hoistable.
fn is_f1_constructor(init: &Expression<'_>) -> bool {
    let Expression::CallExpression(call) = init else { return false };
    let Expression::Identifier(callee) = &call.callee else { return false };
    matches!(callee.name.as_str(), INLINE_COMPONENT | BLOCK_COMPONENT)
}
