//! Nota compiler entry — the `nota source → { code, map }` seam.
//!
//! This is the surface that `@nota-lang/compiler` (the wasm/napi wrapper) builds on: the three
//! compile entries. It lives in the `oxc` umbrella crate because that is the only place with *all
//! three* stages on the Nota path available together: the reader (`oxc_parser`, document mode → a
//! faithful Nota AST), the lowering ([`oxc_transformer::NotaLowering`], Nota AST → hyperscript),
//! and `oxc_codegen`. The lowering is the deferred-pass analog of how `oxc_transformer` lowers
//! JSX. (The parse-stage *views* — the playground's `parseAst` document parse and the
//! `parse_nota_highlights` editor spans — are `Parser` entries consumed directly by the wasm
//! bindings; they never reach the lowering, so they don't belong to this compile seam.)
//!
//! The runtime import (`import { h, decode, Fragment, inlineComponent, blockComponent } from
//! "@nota-lang/runtime"`) is *not* emitted here; the wrapper prepends it.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{
    NotaLowering, NotaMappingKind, NotaMappingMark, TransformOptions, Transformer,
    TypeScriptOptions,
};

/// The result of compiling a `.nota` source string.
pub struct NotaCompiled {
    /// The emitted JS module source (document mode: `export default function Doc() { … }`).
    pub code: String,
    /// The source map, if `source_map_path` was provided.
    pub map: Option<oxc_sourcemap::SourceMap>,
}

// ===================================================================================================
// Volar structured code mappings.
// ===================================================================================================

/// Volar `@volar/language-core` `CodeInformation` capability flags for a mapped range.
///
/// Each boolean enables a class of IDE feature for the range when the TS service result is mapped
/// back to the `.nota` source:
/// * `completion` — autocomplete is offered when the cursor is in this range.
/// * `format`     — the range participates in formatting/document edits.
/// * `navigation` — go-to-definition / find-references / rename cross this range.
/// * `semantic`   — semantic tokens + **hover** are reported for this range.
/// * `structure`  — the range contributes to the document outline / folding.
/// * `verification` — **diagnostics** (type errors, `@Unknown{}` "Cannot find name") surface here.
///
/// Presets: [`MappingCapabilities::full`] (embedded JS/TS) and
/// [`MappingCapabilities::navigation_hover`] (component identifiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingCapabilities {
    /// Autocomplete in this range.
    pub completion: bool,
    /// Formatting / document edits cross this range.
    pub format: bool,
    /// Go-to-definition / find-references / rename cross this range.
    pub navigation: bool,
    /// Semantic tokens + hover for this range.
    pub semantic: bool,
    /// Document outline / folding contribution.
    pub structure: bool,
    /// Diagnostics surface in this range.
    pub verification: bool,
}

impl MappingCapabilities {
    /// Full capabilities — for an **embedded-JS/TS** range (prop value, `@(expr)`/`@name`
    /// interpolation, `%`/`%%%` body, math interpolation, `@if`/`@for` head). Every TS feature
    /// applies to embedded-JS ranges.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            completion: true,
            format: true,
            navigation: true,
            semantic: true,
            structure: true,
            verification: true,
        }
    }

    /// Navigation + hover only — for a **component-identifier** range (`@Aside` → `h(Aside, …)`).
    /// The TS service resolves it like any identifier reference (hover, go-to-def, find-references,
    /// rename, and the `@Unknown{}` "Cannot find name" diagnostic), but it is not a completion-,
    /// formatting-, or structure-region. `verification` stays on so the scope error is reported at
    /// the tag.
    #[must_use]
    pub const fn navigation_hover() -> Self {
        Self {
            completion: false,
            format: false,
            navigation: true,
            semantic: true,
            structure: false,
            verification: true,
        }
    }

    fn from_kind(kind: NotaMappingKind) -> Self {
        match kind {
            NotaMappingKind::EmbeddedJs => Self::full(),
            NotaMappingKind::ComponentIdentifier => Self::navigation_hover(),
        }
    }
}

/// One Volar `CodeMapping` — a source⇄generated range correspondence with capability flags.
///
/// Mirrors the `@volar/language-core` `CodeMapping` shape: parallel `source_offsets` /
/// `generated_offsets` / `lengths` arrays (one *segment* each), an optional `generated_lengths` (set
/// only when a segment's generated text differs in length from its source, e.g. codegen normalised
/// `a+b`→`a + b`), and `data` = the capability flags. The Nota reader produces one segment per
/// mapping here (1-element arrays); the language-server `LanguagePlugin` can pass them straight to
/// Volar or coalesce them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeMapping {
    /// Source byte offsets (one per segment).
    pub source_offsets: Vec<u32>,
    /// Generated byte offsets (one per segment), parallel to `source_offsets`.
    pub generated_offsets: Vec<u32>,
    /// Segment lengths in the **source** (one per segment).
    pub lengths: Vec<u32>,
    /// Segment lengths in the **generated** output, when they differ from `lengths`. `None` ⇒ the
    /// generated length equals the source length for every segment (the common case — identifiers
    /// and atoms round-trip byte-for-byte).
    pub generated_lengths: Option<Vec<u32>>,
    /// The capability flags for this mapping's range(s).
    pub data: MappingCapabilities,
}

/// The result of [`compile_with_mappings`] — JS + sourcemap + structured Volar CodeMappings.
pub struct NotaCompiledWithMappings {
    /// The emitted JS module source.
    pub code: String,
    /// The source map, if `source_map_path` was provided.
    pub map: Option<oxc_sourcemap::SourceMap>,
    /// The Volar `CodeMapping`s: source⇄generated ranges with capability flags.
    pub mappings: Vec<CodeMapping>,
}

/// The result of [`compile_virtual`] — the type-preserving virtual `.tsx` emit + code mappings.
pub struct NotaVirtualCompiled {
    /// The emitted **virtual TypeScript** (`.tsx`) module source — TS types preserved, for the
    /// language server's TS service.
    pub code: String,
    /// The Volar `CodeMapping`s for the virtual `.tsx`.
    pub mappings: Vec<CodeMapping>,
}

/// Per-call configuration for the one shared Nota compile pipeline ([`compile_internal`]). The three
/// public entries are thin wrappers that differ only in these knobs.
struct CompileConfig {
    /// Strip embedded TypeScript to plain JS (the build path, contract H2). Mutually exclusive with
    /// `collect_mappings` — stripping shifts codegen offsets, so it never runs on a mapping path.
    strip_ts: bool,
    /// Collect Volar `CodeMapping`s (the mapping / virtual paths) — also enables codegen's offset log.
    collect_mappings: bool,
    /// Tolerate lowering diagnostics (reserved-name collisions) instead of failing: the language
    /// server's virtual `.tsx` path still emits a best-effort file so the editor degrades gracefully
    /// (it surfaces the collision through its own diagnostic channel). The build paths stay strict.
    lenient_diagnostics: bool,
    /// Source-map path (names the source in the emitted map); `None` skips map generation.
    source_map_path: Option<PathBuf>,
}

/// The output of [`compile_internal`]; each public wrapper takes the fields it exposes.
struct CompileOutput {
    code: String,
    map: Option<oxc_sourcemap::SourceMap>,
    mappings: Vec<CodeMapping>,
}

/// The one Nota compile pipeline: parse (TS-aware) → Nota-lower → optionally strip TS → codegen,
/// joining mapping marks with the codegen offset log when requested. The public [`compile`],
/// [`compile_with_mappings`], and [`compile_virtual`] are wrappers over this with different
/// [`CompileConfig`]s — keeping the parse mode, the lowering, and the mapping assembly in one place.
///
/// The canonical Nota parse is `SourceType::tsx` (contract H2): embedded TypeScript in `%`/`[props]`/
/// `@(expr)`/`@for` heads is admitted into the AST. The build path then *strips* the types (plain-JS
/// emit); the mapping/virtual paths *preserve* them (the language server's TS service types them).
fn compile_internal(
    source_text: &str,
    config: CompileConfig,
) -> Result<CompileOutput, Vec<OxcDiagnostic>> {
    let allocator = Allocator::default();
    let mut program =
        Parser::new(&allocator, source_text, SourceType::nota()).parse_nota_document()?;

    let lowered = NotaLowering::new(&allocator, source_text, config.collect_mappings)
        .lower_document_program(&mut program);
    if !config.lenient_diagnostics && !lowered.diagnostics.is_empty() {
        return Err(lowered.diagnostics);
    }

    if config.strip_ts {
        strip_typescript(&allocator, &mut program)?;
    }

    let options = CodegenOptions { source_map_path: config.source_map_path, ..Default::default() };
    let mut codegen = Codegen::new().with_options(options);
    if config.collect_mappings {
        codegen = codegen.with_nota_offset_log();
    }
    let CodegenReturn { code, map, nota_offset_log, .. } = codegen.build(&program);

    let mappings = if config.collect_mappings {
        build_code_mappings(source_text, &code, &lowered.mappings, &nota_offset_log)
    } else {
        Vec::new()
    };
    Ok(CompileOutput { code, map, mappings })
}

/// Strip embedded TypeScript from the (already Nota-lowered) plain-JS/TS `program` in place, leaving
/// plain JS. Runs `oxc_transformer`'s TypeScript transform only — `EnvOptions::default()` leaves all
/// non-TS JS byte-identical (no arrow/class/etc. lowering), so an all-JS document is unchanged. The
/// transform needs scoping, so a `SemanticBuilder` pass runs first over the lowered program.
fn strip_typescript<'a>(
    allocator: &'a Allocator,
    program: &mut oxc_ast::ast::Program<'a>,
) -> Result<(), Vec<OxcDiagnostic>> {
    // The Nota lowering rebuilds the document `Program` with a plain-JS `SourceType`, so the
    // transformer would skip the TS pass (it only strips when the source type is TS-flagged). Mark it
    // TypeScript (keeping module-ness) so the embedded TS nodes — already in the AST from the tsx
    // parse — get stripped. There is no JSX in the lowered hyperscript, so `ts` (not `tsx`) suffices.
    program.source_type = program.source_type.with_typescript(true);
    let scoping = SemanticBuilder::new().build(program).semantic.into_scoping();
    let options =
        TransformOptions { typescript: TypeScriptOptions::default(), ..Default::default() };
    let ret = Transformer::new(allocator, Path::new("doc.nota"), &options)
        .build_with_scoping(scoping, program);
    if ret.errors.is_empty() { Ok(()) } else { Err(ret.errors) }
}

/// Compile a `.nota` source string to a JS module (+ optional source map).
///
/// The build path: parses the whole file in Nota *document mode* (markup at the top level → `Doc`),
/// lowers, **strips embedded TypeScript** to plain JS (contract H2), and runs `oxc_codegen`. On a
/// parse error or a name-collision diagnostic, returns the collected diagnostics (`Err`).
///
/// `source_map_path` controls whether a source map is generated (it names the source in the map);
/// pass `None` to skip map generation (faster).
///
/// # Errors
/// If the source is not well-formed Nota, or a `%` binding collides with a reserved emit name.
pub fn compile(
    source_text: &str,
    source_map_path: Option<PathBuf>,
) -> Result<NotaCompiled, Vec<OxcDiagnostic>> {
    let out = compile_internal(
        source_text,
        CompileConfig {
            strip_ts: true,
            collect_mappings: false,
            lenient_diagnostics: false,
            source_map_path,
        },
    )?;
    Ok(NotaCompiled { code: out.code, map: out.map })
}

/// Compile a `.nota` source to JS **plus** structured Volar [`CodeMapping`]s.
///
/// The mapping companion to [`compile`] that exposes the per-range source⇄generated code mappings the
/// language server consumes. Parses TS-aware; codegen **preserves** TS types verbatim (mappings stay
/// byte-exact — it does not strip, unlike the build [`compile`]).
///
/// # Errors
/// If the source is not well-formed Nota, or a `%` binding collides with a reserved emit name.
pub fn compile_with_mappings(
    source_text: &str,
    source_map_path: Option<PathBuf>,
) -> Result<NotaCompiledWithMappings, Vec<OxcDiagnostic>> {
    let out = compile_internal(
        source_text,
        CompileConfig {
            strip_ts: false,
            collect_mappings: true,
            lenient_diagnostics: false,
            source_map_path,
        },
    )?;
    Ok(NotaCompiledWithMappings { code: out.code, map: out.map, mappings: out.mappings })
}

/// Compile a `.nota` source to the **type-preserving virtual `.tsx`** emit + code mappings.
///
/// The language-server emit: TS-aware parse, and codegen **preserves** the TS type annotations
/// verbatim (no strip step) so the TS service can type the virtual `.tsx`. Returns the virtual code +
/// the [`CodeMapping`]s mapping `.tsx` offsets back to `.nota` offsets.
///
/// **For the Volar `LanguagePlugin`:** like the build path, the runtime `import { h, decode,
/// Fragment, … } from "@nota-lang/runtime"` and the ambient `CodeInline`/`CodeBlock`/`Math`
/// declarations are *not* emitted here — the plugin prepends that typing preamble to the virtual
/// `.tsx` so `h`/`decode`/component refs type-check. When it does, it must shift every mapping's
/// `generated_offsets` by the prepended prefix length (the `source_offsets` are unchanged — they
/// index the `.nota`).
///
/// # Errors
/// If the source is not well-formed Nota, or a `%` binding collides with a reserved emit name.
pub fn compile_virtual(source_text: &str) -> Result<NotaVirtualCompiled, Vec<OxcDiagnostic>> {
    let out = compile_internal(
        source_text,
        CompileConfig {
            strip_ts: false,
            collect_mappings: true,
            lenient_diagnostics: true,
            source_map_path: None,
        },
    )?;
    Ok(NotaVirtualCompiled { code: out.code, mappings: out.mappings })
}

/// Join the reader's [`NotaMappingMark`]s (source ranges + kinds) with codegen's offset log into
/// Volar [`CodeMapping`]s.
///
/// Codegen logs *every* mapped AST node — both composite nodes (a `VariableDeclaration`, a
/// `CallExpression`) and their leaves (identifiers, literals). Only the **innermost leaves** are
/// byte-exact between source and generated: codegen reformats *inter-token* whitespace (`a+b`→`a +
/// b`, `const   x`→`const x`), which inflates a composite node's generated length, but it never
/// changes an identifier's or number's text — so a leaf maps `[leaf_src, +len) ⇄ [leaf_gen, +len)`
/// exactly (`generated_length == source_length`). We therefore (1) drop any log entry that strictly
/// contains another (the composites), keeping the leaves; (2) for each mark covering `[s, e)`, emit
/// one segment per leaf inside `[s, e)`; and (3) keep only segments whose source slice **equals** the
/// generated slice — this drops the rare non-verbatim leaf (a host-tag `"span"` reinterpreted from
/// the bare source `span`, or a quote-normalised string) that a statement-level mark on a
/// markup-containing component body would otherwise sweep in. The mark's [`NotaMappingKind`] sets the
/// segment's capability flags. A mark matching *no* byte-exact leaf (a body that produced only
/// generated boilerplate) yields no mapping — boilerplate stays unmapped.
fn build_code_mappings(
    source: &str,
    code: &str,
    marks: &[NotaMappingMark],
    offset_log: &[(u32, u32, u32)],
) -> Vec<CodeMapping> {
    // Sort + dedup the log (emit order → range order). Drop zero-length entries. The order is
    // (start ASC, end DESC, gen): a composite node sorts before everything it contains.
    let mut log: Vec<(u32, u32, u32)> =
        offset_log.iter().copied().filter(|&(src_start, src_end, _)| src_end > src_start).collect();
    log.sort_unstable_by_key(|&(start, end, gen_start)| (start, std::cmp::Reverse(end), gen_start));
    log.dedup();

    // Keep only innermost leaves: drop an entry that *strictly* contains another entry's source
    // range (those are composite nodes whose generated text was reformatted, hence not byte-exact).
    // AST spans nest or are disjoint — never partially overlap — so under the sort above an
    // entry's contained entries immediately follow its run of same-span duplicates: entry `k` is
    // a composite iff the next different-span entry starts before `k` ends. One forward pass.
    let leaves: Vec<(u32, u32, u32)> = log
        .iter()
        .enumerate()
        .filter(|&(k, &(start, end, _))| {
            let next_different = log[k + 1..].iter().find(|&&(s2, e2, _)| (s2, e2) != (start, end));
            next_different.is_none_or(|&(s2, _, _)| s2 >= end)
        })
        .map(|(_, &entry)| entry)
        .collect();

    // A leaf segment is valid iff its source slice and generated slice are byte-identical (the
    // verbatim-splice premise). Guards against host-tag string reinterpretation and out-of-range.
    let byte_exact = |src_start: u32, src_end: u32, gen_start: u32| -> bool {
        let (ss, se, gs) = (src_start as usize, src_end as usize, gen_start as usize);
        let len = se - ss;
        se <= source.len()
            && gs + len <= code.len()
            && source.as_bytes()[ss..se] == code.as_bytes()[gs..gs + len]
    };

    let mut mappings = Vec::with_capacity(marks.len());
    for mark in marks {
        let (s, e) = (mark.span.start, mark.span.end);
        let data = MappingCapabilities::from_kind(mark.kind);

        // One segment per byte-exact leaf whose source range is contained in this mark's `[s, e)`;
        // leaves are start-sorted, so the candidates are one binary-searched run.
        let lo = leaves.partition_point(|&(src_start, ..)| src_start < s);
        let mut source_offsets = Vec::new();
        let mut generated_offsets = Vec::new();
        let mut lengths = Vec::new();
        for &(src_start, src_end, gen_start) in
            leaves[lo..].iter().take_while(|&&(src_start, ..)| src_start < e)
        {
            if src_end <= e && byte_exact(src_start, src_end, gen_start) {
                source_offsets.push(src_start);
                generated_offsets.push(gen_start);
                lengths.push(src_end - src_start);
            }
        }

        if source_offsets.is_empty() {
            // The marked region produced no byte-exact leaf (only generated boilerplate). Unmapped.
            continue;
        }

        mappings.push(CodeMapping {
            source_offsets,
            generated_offsets,
            lengths,
            generated_lengths: None,
            data,
        });
    }
    mappings
}

#[cfg(test)]
mod tests {
    use super::compile;

    #[test]
    fn compile_basic_document() {
        let out = compile("@h1{Hello}\n", None).expect("compiles");
        assert!(out.code.contains("export default function Doc()"), "{}", out.code);
        assert!(out.code.contains(r#"h("h1", {}, ["Hello"])"#), "{}", out.code);
        assert!(out.map.is_none());
    }

    #[test]
    fn compile_emits_source_map_when_requested() {
        let out = compile("@p{hi}\n", Some("doc.nota".into())).expect("compiles");
        assert!(out.map.is_some(), "source map present");
    }

    #[test]
    fn compile_reports_errors() {
        match compile("@p{unterminated", None) {
            Ok(_) => panic!("expected a diagnostic for an unterminated body"),
            Err(errors) => assert!(!errors.is_empty()),
        }
    }

    #[test]
    fn compile_accepts_and_strips_embedded_typescript() {
        // The build path parses TS-aware (contract H2) and strips the types → plain, runnable JS.
        let out = compile("% const n: number = 1\n@p{@(n)}\n", None).expect("compiles");
        assert!(
            !out.code.contains(": number"),
            "TS annotation stripped on the build path:\n{}",
            out.code
        );
        assert!(out.code.contains("const n = 1"), "the value survives the strip:\n{}", out.code);
    }

    #[test]
    fn compile_strips_generic_call_type_arguments() {
        // A generic call `f<Foo>(x)` must parse as a call (not `f < Foo > x`), then strip to `f(x)`.
        let out = compile("@p{@(f<Foo>(x))}\n", None).expect("compiles");
        assert!(
            !out.code.contains("f < Foo"),
            "generic not mis-parsed as comparison:\n{}",
            out.code
        );
        assert!(out.code.contains("f(x)"), "type args stripped to a plain call:\n{}", out.code);
    }

    #[test]
    fn compile_reports_reserved_name_collision() {
        // A `%` binding that shadows a reserved emit name (`h`/`Doc`/…) is a lowering diagnostic.
        match compile("%let h = 1\n@p{x}\n", None) {
            Ok(out) => panic!("expected a collision diagnostic, got:\n{}", out.code),
            Err(errors) => assert!(!errors.is_empty()),
        }
    }
}

// ===============================================================================================
// Code-mapping + type-preserving virtual emit tests.
// ===============================================================================================
#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "test fixtures: substring offsets/lengths fit in u32 (oxc's Span model)"
)]
mod h1_h2 {
    use super::{CodeMapping, MappingCapabilities, compile_virtual, compile_with_mappings};

    /// Byte offset of the (unique) substring `needle` in `hay`.
    #[track_caller]
    fn offset_of(hay: &str, needle: &str) -> u32 {
        let i = hay.find(needle).unwrap_or_else(|| panic!("{needle:?} not found in {hay:?}"));
        assert!(!hay[i + needle.len()..].contains(needle), "{needle:?} is not unique in {hay:?}");
        u32::try_from(i).unwrap()
    }

    /// Find the mapping segment whose source offset is `src_off`, returning `(generated_offset,
    /// length, capabilities)`. Asserts exactly one such segment exists.
    #[track_caller]
    fn segment_at(mappings: &[CodeMapping], src_off: u32) -> (u32, u32, MappingCapabilities) {
        let mut found = None;
        for m in mappings {
            for k in 0..m.source_offsets.len() {
                if m.source_offsets[k] == src_off {
                    assert!(
                        found.is_none(),
                        "duplicate mapping segment for source offset {src_off}"
                    );
                    found = Some((m.generated_offsets[k], m.lengths[k], m.data));
                }
            }
        }
        found.unwrap_or_else(|| panic!("no mapping segment for source offset {src_off}"))
    }

    /// Is `src_off` covered by *any* mapping segment?
    fn is_mapped(mappings: &[CodeMapping], src_off: u32) -> bool {
        mappings.iter().any(|m| m.source_offsets.contains(&src_off))
    }

    /// Every mapping segment must round-trip byte-for-byte: the source slice equals the generated
    /// slice (the core invariant — leaves are spliced verbatim).
    #[track_caller]
    fn assert_segments_byte_exact(src: &str, code: &str, mappings: &[CodeMapping]) {
        for m in mappings {
            for k in 0..m.source_offsets.len() {
                let so = m.source_offsets[k] as usize;
                let go = m.generated_offsets[k] as usize;
                let len = m.lengths[k] as usize;
                assert!(so + len <= src.len() && go + len <= code.len());
                assert_eq!(
                    &src[so..so + len],
                    &code[go..go + len],
                    "segment src@{so}+{len} != g@{go}",
                );
            }
        }
    }

    #[test]
    fn embedded_js_in_percent_statement_maps_with_full_caps() {
        // A `%` statement with embedded TS: the identifiers `n` and `count` are byte-exact leaves
        // with full caps; the TS annotation `: number` survives in the emit.
        let src = "% const n: number = count();\n@p{hi}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // The TS type annotation is preserved (codegen does not strip — see compile_virtual).
        assert!(out.code.contains(": number"), "type annotation preserved:\n{}", out.code);

        // `count` (a unique identifier) maps to its emitted location with full capabilities.
        let count_src = offset_of(src, "count");
        let (g, len, caps) = segment_at(&out.mappings, count_src);
        assert_eq!(len, 5, "length of `count`");
        assert_eq!(&out.code[g as usize..g as usize + 5], "count", "round-trips to `count`");
        assert_eq!(caps, MappingCapabilities::full(), "embedded JS → full caps");

        assert_segments_byte_exact(src, &out.code, &out.mappings);
    }

    #[test]
    fn prop_expr_and_interpolation_map_with_full_caps() {
        let src = "@p[id: theId]{@(user) world}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // Prop value expression `theId` — full caps, byte-exact.
        let (g, len, caps) = segment_at(&out.mappings, offset_of(src, "theId"));
        assert_eq!(len, "theId".len() as u32);
        assert_eq!(&out.code[g as usize..g as usize + len as usize], "theId");
        assert_eq!(caps, MappingCapabilities::full());

        // `@(user)` interpolation — full caps, byte-exact.
        let (g2, _, caps2) = segment_at(&out.mappings, offset_of(src, "user"));
        assert_eq!(&out.code[g2 as usize..g2 as usize + 4], "user");
        assert_eq!(caps2, MappingCapabilities::full());

        assert_segments_byte_exact(src, &out.code, &out.mappings);
    }

    #[test]
    fn component_identifier_maps_with_navigation_hover_only() {
        // `@Aside` → `h(Aside, …)`: navigation + hover, NOT a completion/format/structure region.
        let src = "@Aside{hi}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        let (g, len, caps) = segment_at(&out.mappings, offset_of(src, "Aside"));
        assert_eq!(len, "Aside".len() as u32);
        assert_eq!(&out.code[g as usize..g as usize + len as usize], "Aside");
        assert_eq!(caps, MappingCapabilities::navigation_hover());
        assert!(caps.navigation && caps.semantic && caps.verification);
        assert!(!caps.completion && !caps.format && !caps.structure);
    }

    #[test]
    fn host_tag_and_generated_boilerplate_are_unmapped() {
        // `@p` is a host tag (emitted as the string `"p"`), NOT a TS symbol → unmapped. The
        // generated `h(`, `{}`, `[`, `decode`, `Fragment` boilerplate is unmapped too.
        let src = "@p{@(x)}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // The host tag name `p` in the source is not mapped (its source offset 1).
        let p_src = 1u32; // `@p` → the `p`
        assert_eq!(&src[p_src as usize..=p_src as usize], "p");
        assert!(!is_mapped(&out.mappings, p_src), "host tag `p` must be unmapped");

        // The only mapped source offset is the embedded `x`.
        let x_src = offset_of(src, "x");
        assert!(is_mapped(&out.mappings, x_src), "embedded `x` is mapped");
        let mapped_count: usize = out.mappings.iter().map(|m| m.source_offsets.len()).sum();
        assert_eq!(mapped_count, 1, "only the embedded `x` maps; boilerplate is unmapped");

        assert_segments_byte_exact(src, &out.code, &out.mappings);
    }

    #[test]
    fn virtual_emit_preserves_ts_annotation_and_frames_tsx() {
        // The virtual emit keeps the TS type annotation `: number` (no strip step) and the
        // `@for` head `as` cast, ready for the language server's `.tsx` TS service.
        let src = "% const n: number = count();\n@for (x of xs as string[]) {@x}\n";
        let out = compile_virtual(src).expect("compiles");

        assert!(out.code.contains(": number"), "`: number` preserved:\n{}", out.code);
        assert!(out.code.contains("as string[]"), "`as string[]` preserved:\n{}", out.code);

        // The mappings still resolve embedded identifiers byte-exactly in the virtual `.tsx`.
        let (g, _, caps) = segment_at(&out.mappings, offset_of(src, "count"));
        assert_eq!(&out.code[g as usize..g as usize + 5], "count");
        assert_eq!(caps, MappingCapabilities::full());
    }

    #[test]
    fn round_trip_source_offset_inside_embedded_js() {
        // The headline invariant: a source offset *inside* an embedded-JS span maps to the correct
        // generated offset, and back.
        let src = "@p[onClick: () => go()]{hi}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // `go` is inside the embedded prop arrow body; it maps to the `go` in the generated code.
        let go_src = offset_of(src, "go()");
        let (g, _, caps) = segment_at(&out.mappings, go_src);
        assert_eq!(&out.code[g as usize..g as usize + 2], "go", "round-trips to `go`");
        assert_eq!(caps, MappingCapabilities::full());
        assert_eq!(out.code.as_bytes()[g as usize], b'g');
    }

    #[test]
    fn build_and_virtual_share_mappings_modulo_code() {
        // Same parse, two tails: `compile_with_mappings` (build) and `compile_virtual` (.tsx)
        // produce the same mapping structure over the same source ranges.
        let src = "@p[id: theId]{@(user)}\n";
        let build = compile_with_mappings(src, None).expect("compiles");
        let virt = compile_virtual(src).expect("compiles");

        let build_srcs: Vec<u32> =
            build.mappings.iter().flat_map(|m| m.source_offsets.iter().copied()).collect();
        let virt_srcs: Vec<u32> =
            virt.mappings.iter().flat_map(|m| m.source_offsets.iter().copied()).collect();
        assert_eq!(build_srcs, virt_srcs, "same source ranges mapped in both emits");
    }

    /// The canonical golden, exercising the code mappings: the component binding, the `@Colorized`
    /// tag reference, the `@for` iterable + binding, the `@x`/`@children` interps — all map
    /// byte-exactly, with the right capabilities, and no boilerplate leaks in.
    const CANONICAL_NOTA: &str = "%let Colorized = inlineComponent((children) => {\n  let [color, setColor] = useState(\"red\");\n  return @span[onClick: () => setColor(\"green\")][style: {color}]{@children};\n})\n\n@for (x of items) {\n  - @Colorized{@x}\n}\n";

    #[test]
    fn canonical_golden_mappings_are_byte_exact() {
        let out = compile_with_mappings(CANONICAL_NOTA, None).expect("compiles");

        // Every segment round-trips byte-for-byte (the core invariant).
        assert_segments_byte_exact(CANONICAL_NOTA, &out.code, &out.mappings);

        // The `@Colorized{…}` tag reference (in the `@for` body) maps as a component identifier.
        // (`offset_of` requires uniqueness; `@Colorized` is the 2nd `Colorized` — locate it via the
        // `@` sigil prefix, which is unique.)
        let colorized_tag = (CANONICAL_NOTA.find("@Colorized").unwrap() + 1) as u32;
        let (g, len, caps) = segment_at(&out.mappings, colorized_tag);
        assert_eq!(len, "Colorized".len() as u32);
        assert_eq!(&out.code[g as usize..g as usize + len as usize], "Colorized");
        assert_eq!(caps, MappingCapabilities::navigation_hover(), "tag ref → navigation/hover");

        // The `@for` iterable `items` maps with full caps.
        let (gi, _, capsi) = segment_at(&out.mappings, offset_of(CANONICAL_NOTA, "items"));
        assert_eq!(&out.code[gi as usize..gi as usize + 5], "items");
        assert_eq!(capsi, MappingCapabilities::full());

        // The `%let` binding name `Colorized` (its *declaration*, the 1st occurrence) is part of
        // the hoisted statement → embedded JS, full caps.
        let colorized_decl = offset_of(CANONICAL_NOTA, "Colorized = ") as usize; // unique form
        let (gd, _, capsd) = segment_at(&out.mappings, colorized_decl as u32);
        assert_eq!(&out.code[gd as usize..gd as usize + "Colorized".len()], "Colorized");
        assert_eq!(capsd, MappingCapabilities::full());
    }
}
