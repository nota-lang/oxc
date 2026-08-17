//! Nota markup → Solid JSX lowering (the Nota transform).
//!
//! The Nota *reader* (in `oxc_parser`) parses `@`-markup into a faithful Nota AST: a `.nota`
//! document is a single `Expression::NotaMarkup(NotaMarkupKind::Document(..))` statement, and
//! every embedded `@`-form is an `Expression::NotaMarkup` left in place. This module *lowers*
//! those nodes to **Solid JSX** (design/solid.md §The pipeline): the document becomes
//! `export default function Doc() { …; return <NotaDoc>…</NotaDoc>; }`, list markers become
//! `<UlLi>`/`<OlLi>`, flow-container host tags get a `<Reforest>` interior, and `@for` lowers to
//! Solid's `<For>`. The consumer compiles the JSX per target with vite-plugin-solid.
//!
//! Entry: [`NotaLowering`]. The document is rebuilt by [`NotaLowering::lower_document_program`]
//! (Doc skeleton, `%`-statement routing — component bindings are ordinary lexical statements,
//! document-local, NOT hoisted/exported); embedded `@`-forms are then replaced by a
//! [`oxc_ast_visit::VisitMut`] walk. Optionally collects Volar [`NotaMappingMark`]s.
//!
//! Semantic pin: `Doc` and the nested-`%` IIFE are always emitted **synchronous** — the presence
//! of `await` does not auto-`async`ify them. Top-level `await` therefore emits JS that does not
//! parse, by design (not a silent rewrite).

mod build;
mod lower;
mod mapping;
mod scribble;

pub use build::FLOW_TAGS;
pub use lower::{NotaLowering, NotaLoweringReturn};
pub use mapping::{NotaMappingKind, NotaMappingMark};

/// The default-export document component name.
const DOC: &str = "Doc";
/// The `@nota-lang/core` structural names the emit references free (the shim binds them):
/// the document wrapper, the flow-interior restructurer, and the list-item sentinels.
const NOTA_DOC: &str = "NotaDoc";
const REFOREST: &str = "Reforest";
const UL_LI: &str = "UlLi";
const OL_LI: &str = "OlLi";
/// Solid's keyed list component (`@for` lowers to `<For each={…}>`), bound from `"solid-js"`.
const FOR: &str = "For";
/// Solid's conditional component (`@if` lowers to `<Show when={…}>`), bound from `"solid-js"`.
const SHOW: &str = "Show";
/// Solid's dynamic-tag component (`@(expr)[…]{…}` heads), bound from `"solid-js/web"`.
const DYNAMIC: &str = "Dynamic";
/// Ambient-prelude tags for code/math spans (referenced as identifiers — no import emitted).
const CODE_INLINE: &str = "CodeInline";
const CODE_BLOCK: &str = "CodeBlock";
/// `Tex`, not `Math`: the ambient identifier must not capture the JS `Math` global — the
/// integrator's prelude inject rewrites *free* references, so `% Math.floor(x)` would break.
const MATH: &str = "Tex";
/// Ambient-prelude heading component: `#` heading *sugar* lowers to `<Heading rank={N}>…` — a
/// free identifier reference (like `Tex`/`CodeInline`, no import emitted). Raw `@hN{…}` element
/// forms stay plain host tags (the unnumbered/un-Toc'd escape hatch).
const HEADING: &str = "Heading";
/// Ambient-prelude doc-state components (notation.md §Doc-state references,
/// design/references.md): the two inline sugars lower to free identifier references, exactly the
/// `HEADING` pattern — `<x>` → `<Label id="x" />`, `&x` → `<Ref id="x" …props>body…</Ref>` (the
/// props/body from the ref's glued postfix groups; footnote uses are refs, and footnote
/// definitions are the plain `@Footnote[id]: …` element form — nothing reader-privileged).
const LABEL: &str = "Label";
const REF: &str = "Ref";
/// The `@nota-lang/core` attrs marker (notation.md §Attrs): a flow-position attrs group lowers
/// to `<Attrs …/>`, which the Reforest pass strips and applies to the paragraph it is forming.
const ATTRS: &str = "Attrs";

// ===================================================================================================
// The emit surface, grouped — the introspectable source of truth.
//
// These arrays ARE the constants above, grouped by binding module. They cross the wasm boundary
// as `emitSurface()` (napi/nota), where `@nota-lang/compiler` derives its name lists from them —
// the TS side holds no hand-copied mirror. Extend the emit here and every downstream list,
// reservation diagnostic, and coverage test follows.
// ===================================================================================================

/// The `@nota-lang/core` structural names the emit references free.
pub const STRUCTURAL_EMIT_NAMES: &[&str] = &[NOTA_DOC, REFOREST, UL_LI, OL_LI, ATTRS];
/// The `solid-js` names the lowering itself emits (`@for` → `<For>`, `@if` → `<Show>`).
pub const SOLID_EMIT_NAMES: &[&str] = &[FOR, SHOW];
/// The `solid-js/web` names the lowering emits (`@(expr)` dynamic tags).
pub const SOLID_WEB_EMIT_NAMES: &[&str] = &[DYNAMIC];
/// The ambient-prelude names the lowering emits free: code/math spans, heading sugar, and the
/// doc-state sugars.
pub const PRELUDE_EMIT_NAMES: &[&str] = &[CODE_INLINE, CODE_BLOCK, MATH, HEADING, LABEL, REF];

/// Is `name` part of the emit surface — declared (`Doc`) or referenced free by lowered markup —
/// such that a user module binding of it must be diagnosed rather than silently shadow the emit?
/// Covers all four groups: `%let Tex = 1` breaks `$…$` exactly as `%let NotaDoc = …` breaks the
/// document wrapper.
pub fn is_reserved_emit_name(name: &str) -> bool {
    name == DOC
        || STRUCTURAL_EMIT_NAMES.contains(&name)
        || SOLID_EMIT_NAMES.contains(&name)
        || SOLID_WEB_EMIT_NAMES.contains(&name)
        || PRELUDE_EMIT_NAMES.contains(&name)
}

/// Every reserved emit name, `Doc` first — for diagnostics and the wasm introspection surface.
pub fn reserved_emit_names() -> Vec<&'static str> {
    let mut names = vec![DOC];
    names.extend_from_slice(STRUCTURAL_EMIT_NAMES);
    names.extend_from_slice(SOLID_EMIT_NAMES);
    names.extend_from_slice(SOLID_WEB_EMIT_NAMES);
    names.extend_from_slice(PRELUDE_EMIT_NAMES);
    names
}
