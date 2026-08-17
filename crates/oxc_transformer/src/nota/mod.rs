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
/// Ambient-prelude doc-state components (notation.md §Doc-state references): the four inline
/// sugars lower to free identifier references, exactly the `HEADING` pattern — `<x>` →
/// `<Label id="x" />`, `&x` → `<Ref id="x" />`, `[^x]` → `<FootnoteMark label="x" />`,
/// line-start `[^x]: body` → `<FootnoteText label="x">body…</FootnoteText>`.
const LABEL: &str = "Label";
const REF: &str = "Ref";
const FOOTNOTE_MARK: &str = "FootnoteMark";
const FOOTNOTE_TEXT: &str = "FootnoteText";
/// The `@nota-lang/core` attrs marker (notation.md §Attrs): a flow-position attrs group lowers
/// to `<Attrs …/>`, which the Reforest pass strips and applies to the paragraph it is forming.
const ATTRS: &str = "Attrs";
