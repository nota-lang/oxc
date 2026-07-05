//! Nota markup → hyperscript lowering (the Nota transform).
//!
//! The Nota *reader* (in `oxc_parser`) parses `@`-markup into a faithful Nota AST: a `.nota` document
//! is a single `Expression::NotaMarkup(NotaMarkupKind::Document(..))` statement, and every embedded
//! `@`-form is an `Expression::NotaMarkup` left in place. This module *lowers* those nodes to the
//! hyperscript `h`/`Fragment`/`decode` `Expression` AST — the deferred-pass analog of how
//! `oxc_transformer` lowers JSX to `createElement`.
//!
//! Entry: [`NotaLowering`]. The document is rebuilt by [`NotaLowering::lower_document_program`] (Doc
//! skeleton, `%`-statement routing + component name-attach — contract R15: component bindings are
//! ordinary lexical statements, document-local, NOT hoisted/exported); embedded `@`-forms are then
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
/// The fresh map-index parameter injected as the `@for` body's `Fragment` key.
const FOR_KEY_PARAM: &str = "_i";
/// The default-export document component name.
const DOC: &str = "Doc";
/// The component constructors. A top-level `%const X = inlineComponent(...)` binding stays
/// document-local (contract R15 — no hoist/export; `%export` is the author's opt-in); the reader
/// only attaches the binding name as the constructor's 2nd argument.
const INLINE_COMPONENT: &str = "inlineComponent";
const BLOCK_COMPONENT: &str = "blockComponent";
/// Ambient-prelude tags for code/math spans (referenced as identifiers — no import emitted).
const CODE_INLINE: &str = "CodeInline";
const CODE_BLOCK: &str = "CodeBlock";
/// `Tex`, not `Math` (contract R14): the ambient identifier must not capture the JS `Math` global —
/// the integrator's prelude inject rewrites *free* references, so `% Math.floor(x)` would break.
const MATH: &str = "Tex";
/// Ambient-prelude heading slot (contract R18f): `#` heading *sugar* lowers to
/// `h(Heading, { rank: N }, […])` — a free identifier reference (like `Tex`/`CodeInline`, no import
/// emitted). Raw `@hN{…}` element forms stay plain host tags (the unnumbered/un-Toc'd escape hatch).
const HEADING: &str = "Heading";
/// Ambient-prelude doc-state slots (contract R20a): the four inline sugars lower to free
/// identifier references, exactly the `HEADING` pattern — `<x>` → `h(Label, { id: "x" }, [])`,
/// `&x` → `h(Ref, { id: "x" }, [])`, `[^x]` → `h(FootnoteMark, { label: "x" }, [])`, line-start
/// `[^x]: body` → `h(FootnoteText, { label: "x" }, [body…])`.
const LABEL: &str = "Label";
const REF: &str = "Ref";
const FOOTNOTE_MARK: &str = "FootnoteMark";
const FOOTNOTE_TEXT: &str = "FootnoteText";

/// Is `init` a call to a component constructor (`inlineComponent`/`blockComponent`)? Such a
/// top-level binding gets the name attach (constructor 2nd argument — contract R15/F1: the
/// returned function cannot otherwise recover its authored name for the debug manifest).
fn is_component_constructor(init: &Expression<'_>) -> bool {
    let Expression::CallExpression(call) = init else { return false };
    let Expression::Identifier(callee) = &call.callee else { return false };
    matches!(callee.name.as_str(), INLINE_COMPONENT | BLOCK_COMPONENT)
}
