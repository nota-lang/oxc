//! Nota compiler entry — the `nota source → { code, map }` seam.
//!
//! This is the surface that `@nota-lang/compiler` (the wasm wrapper) builds on. It lives in the
//! `oxc` umbrella crate because that is the only place with all stages on the Nota path available
//! together: the reader (`oxc_parser`, document mode → a
//! faithful Nota AST), the lowering ([`oxc_transformer::NotaLowering`], Nota AST → Solid JSX), and
//! `oxc_codegen`. The lowering is the deferred-pass analog of how `oxc_transformer` lowers plain
//! JSX. [`analyze`] parses once and derives the editor's AST, highlights, virtual TSX, mappings,
//! diagnostics, and free names from that parse.
//!
//! The emit is **Solid JSX** (design/solid.md): no imports are emitted here — the structural
//! names, the ambient prelude, and the `solid-js` state surface are all *free names* the
//! `@nota-lang/compiler` wrapper binds. The authoritative name groups are `oxc_transformer`'s
//! `*_EMIT_NAMES` constants, introspectable downstream via the wasm `emitSurface()` entry.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::{Parser, nota_highlights_from_program};
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_span::SourceType;
use oxc_transformer::{
    JsxOptions, NotaLowering, NotaMappingKind, NotaMappingMark, TransformOptions, Transformer,
    TypeScriptOptions,
};
use serde::Serialize;
#[cfg(feature = "nota-wasm")]
use tsify::Tsify;

/// The output shared by strict compilation and recoverable editor analysis.
#[derive(Serialize)]
#[cfg_attr(feature = "nota-wasm", derive(Tsify))]
#[cfg_attr(feature = "nota-wasm", tsify(into_wasm_abi, missing_as_null))]
#[serde(rename_all = "camelCase")]
pub struct NotaOutput {
    pub code: String,
    #[serde(skip)]
    pub map: Option<oxc_sourcemap::SourceMap>,
    pub free_names: Vec<String>,
    /// Language tags on fenced code blocks, sorted and deduplicated. The integrator turns these
    /// into grammar imports + an `lstset` registration (grammars are opt-in and large).
    pub fence_langs: Vec<String>,
    pub mappings: Vec<CodeMapping>,
    pub errors: Vec<NotaDiagnostic>,
    pub ast: Option<String>,
    /// Flat `[start, end, kind]` triples over UTF-8 source bytes.
    pub highlights: Vec<u32>,
}

/// One recovered parser or lowering diagnostic.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "nota-wasm", derive(Tsify))]
#[cfg_attr(feature = "nota-wasm", tsify(missing_as_null))]
#[serde(rename_all = "camelCase")]
pub struct NotaDiagnostic {
    pub message: String,
    pub start: u32,
    pub len: u32,
}

impl From<&OxcDiagnostic> for NotaDiagnostic {
    #[expect(clippy::cast_possible_truncation)]
    fn from(error: &OxcDiagnostic) -> Self {
        let (start, len) = error
            .labels
            .as_ref()
            .and_then(|labels| labels.first())
            .map_or((0, 0), |label| (label.offset() as u32, label.len() as u32));
        Self { message: error.to_string(), start, len }
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "nota-wasm", derive(Tsify))]
#[cfg_attr(feature = "nota-wasm", tsify(missing_as_null))]
#[serde(rename_all = "camelCase")]
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

    /// Navigation + hover only — for a **component-identifier** range (`@Aside` → `<Aside>`).
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

    /// Completion only — for the zero-width **props-completion anchor** an EOF-recovered `@tag[|`
    /// leaves just inside the emitted JSX opening element's attribute position. It exists purely to
    /// route a completion request into the element's attributes type (so the TS service proposes
    /// prop names); it must not carry verification/semantic (there is no real text to diagnose or
    /// hover at a zero-width point).
    #[must_use]
    pub const fn props_anchor() -> Self {
        Self {
            completion: true,
            format: false,
            navigation: false,
            semantic: false,
            structure: false,
            verification: false,
        }
    }

    fn from_kind(kind: NotaMappingKind) -> Self {
        match kind {
            NotaMappingKind::EmbeddedJs => Self::full(),
            NotaMappingKind::ComponentIdentifier => Self::navigation_hover(),
            NotaMappingKind::PropsAnchor => Self::props_anchor(),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "nota-wasm", derive(Tsify))]
#[cfg_attr(feature = "nota-wasm", tsify(missing_as_null))]
#[serde(rename_all = "camelCase")]
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

enum CompileMode {
    Build { source_map_path: Option<PathBuf> },
    Analyze,
}

/// Parse, lower, optionally strip TypeScript, and generate code and mappings.
///
/// The canonical Nota parse is `SourceType::tsx` (NOTA_READER.md §Compiler entries): embedded
/// TypeScript in `%`/`[props]`/
/// `@(expr)`/`@for` heads is admitted into the AST. The build path then *strips* the types (plain-JS
/// emit); analysis preserves them for the language server's TS service.
fn compile_internal(
    source_text: &str,
    mode: CompileMode,
) -> Result<NotaOutput, Vec<OxcDiagnostic>> {
    let strip_ts = matches!(&mode, CompileMode::Build { .. });
    let recover = matches!(&mode, CompileMode::Analyze);
    let source_map_path = match mode {
        CompileMode::Build { source_map_path } => source_map_path,
        CompileMode::Analyze => None,
    };

    let allocator = Allocator::default();
    let (mut program, mut errors) = if recover {
        let recovered =
            Parser::new(&allocator, source_text, SourceType::nota()).parse_nota_document_recover();
        (recovered.program, recovered.errors)
    } else {
        let program =
            Parser::new(&allocator, source_text, SourceType::nota()).parse_nota_document()?;
        (program, Vec::new())
    };

    let (ast, highlights) = if recover {
        let ast = program.to_estree_js_json(true);
        let highlights = nota_highlights_from_program(&allocator, source_text, &program)
            .into_iter()
            .flat_map(|span| [span.start, span.end, u32::from(span.kind as u8)])
            .collect();
        (Some(ast), highlights)
    } else {
        (None, Vec::new())
    };

    let lowered =
        NotaLowering::new(&allocator, source_text, recover).lower_document_program(&mut program);
    if !lowered.diagnostics.is_empty() {
        if recover {
            errors.extend(lowered.diagnostics);
        } else {
            return Err(lowered.diagnostics);
        }
    }

    let free_names = if strip_ts {
        strip_typescript(&allocator, &mut program)?
    } else {
        free_names(SemanticBuilder::new().build(&program).semantic.scoping())
    };

    let options = CodegenOptions { source_map_path, ..Default::default() };
    let mut codegen = Codegen::new().with_options(options);
    if recover {
        codegen = codegen.with_nota_offset_log();
    }
    let CodegenReturn { code, map, nota_offset_log, .. } = codegen.build(&program);

    let mappings = if recover {
        build_code_mappings(source_text, &code, &lowered.mappings, &nota_offset_log)
    } else {
        Vec::new()
    };
    let errors = errors.iter().map(NotaDiagnostic::from).collect();
    Ok(NotaOutput {
        code,
        map,
        free_names,
        fence_langs: lowered.fence_langs,
        mappings,
        errors,
        ast,
        highlights,
    })
}

/// Strip embedded TypeScript from the (already Nota-lowered) plain-JS/TS `program` in place, leaving
/// plain JS. Runs `oxc_transformer`'s TypeScript transform only — `EnvOptions::default()` leaves all
/// non-TS JS byte-identical (no arrow/class/etc. lowering), so an all-JS document is unchanged. The
/// transform needs scoping, so a `SemanticBuilder` pass runs first over the lowered program.
///
/// Returns the module's **free names** ([`NotaOutput::free_names`]), harvested from that same
/// semantic pass: the root-unresolved references that are used in *value* position (a type-only
/// reference — `const n: Foo = …` — is about to be stripped and must not count). Sorted + deduped
/// (the underlying map's iteration order is arbitrary).
fn strip_typescript<'a>(
    allocator: &'a Allocator,
    program: &mut oxc_ast::ast::Program<'a>,
) -> Result<Vec<String>, Vec<OxcDiagnostic>> {
    // The Nota lowering rebuilds the document `Program` with a JSX-flagged `SourceType`; mark it
    // TypeScript too (keeping module-ness) so the embedded TS nodes — already in the AST from the
    // tsx parse — get stripped.
    program.source_type = program.source_type.with_typescript(true);
    let scoping = SemanticBuilder::new().build(program).semantic.into_scoping();
    let free_names = free_names(&scoping);
    let options = TransformOptions {
        typescript: TypeScriptOptions::default(),
        jsx: JsxOptions::disable(),
        ..Default::default()
    };
    let ret = Transformer::new(allocator, Path::new("doc.nota"), &options)
        .build_with_scoping(scoping, program);
    if ret.errors.is_empty() { Ok(free_names) } else { Err(ret.errors) }
}

fn free_names(scoping: &Scoping) -> Vec<String> {
    let mut names: Vec<String> = scoping
        .root_unresolved_references()
        .iter()
        .filter(|(_, reference_ids)| {
            reference_ids.iter().any(|&id| scoping.get_reference(id).is_value())
        })
        .map(|(name, _)| (*name).to_string())
        .collect();
    names.sort_unstable();
    names
}

/// Compile a `.nota` source string to a JS module (+ optional source map).
///
/// The build path: parses the whole file in Nota *document mode* (markup at the top level → `Doc`),
/// lowers, **strips embedded TypeScript** to plain JS, and runs `oxc_codegen`. On a
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
) -> Result<NotaOutput, Vec<OxcDiagnostic>> {
    compile_internal(source_text, CompileMode::Build { source_map_path })
}

/// Parse once and derive every editor-facing view from the recovered AST.
///
/// # Panics
/// Only if the recovered path reaches a build-only error branch.
#[must_use]
pub fn analyze(source_text: &str) -> NotaOutput {
    compile_internal(source_text, CompileMode::Analyze)
        .expect("recoverable analysis has no fallible stage")
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
        // A props-completion anchor (EOF-recovered `@tag[|`) is resolved specially: it is *not* a
        // byte-exact leaf. The lowering gave the JSX opening element the mark's `[` span, so the
        // raw offset log has an entry `(bracket_start, bracket_end, gen_of_open_angle)`. Emit a
        // single **zero-width** segment mapping the source position just after `[` to the
        // generated position just inside the opening tag (after the tag name and its following
        // space, when present) — a completion request there gets the TSX service's JSX
        // *attribute* completions, the JSX-native form of prop completion.
        if mark.kind == NotaMappingKind::PropsAnchor {
            if let Some(&(_, _, gen_angle)) =
                offset_log.iter().find(|&&(gs, ge, _)| gs == mark.span.start && ge == mark.span.end)
            {
                let bytes = code.as_bytes();
                let mut at = gen_angle as usize + 1; // past `<`
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric()
                        || matches!(bytes[at], b'-' | b'_' | b'$'))
                {
                    at += 1;
                }
                if bytes.get(at) == Some(&b' ') {
                    at += 1;
                }
                #[expect(clippy::cast_possible_truncation)]
                mappings.push(CodeMapping {
                    source_offsets: vec![mark.span.end],
                    generated_offsets: vec![at as u32],
                    lengths: vec![0],
                    generated_lengths: Some(vec![0]),
                    data: MappingCapabilities::props_anchor(),
                });
            }
            continue;
        }

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
        assert!(out.code.contains(r#"<h1>{"Hello"}</h1>"#), "{}", out.code);
        assert!(out.code.contains("<NotaDoc>"), "{}", out.code);
        assert!(out.map.is_none());
    }

    #[test]
    fn adjacent_text_coalesces_with_blank_line_marker() {
        // Two paragraphs: the blank line must surface as `\n\n` INSIDE one string child
        // (Reforest's paragraph-break contract), not as separate `"\n"` children.
        let out = compile("one two\n\nthree four\n", None).expect("compiles");
        assert!(
            out.code.contains(r#"{"one two\n\nthree four"}"#),
            "coalesced text with interior blank line:\n{}",
            out.code
        );
    }

    #[test]
    fn list_markers_emit_ulli_and_for_lowers_to_solid_for() {
        let out = compile("@for (x of xs) {\n  - @em{a @(x)}\n}\n", None).expect("compiles");
        assert!(out.code.contains("<For each={xs}>"), "{}", out.code);
        assert!(out.code.contains("<UlLi>"), "{}", out.code);
        assert!(out.code.contains("<em>"), "{}", out.code);
        assert!(!out.code.contains(".map("), "no keyed-map emit remains:\n{}", out.code);
    }

    #[test]
    fn flow_container_interiors_get_reforest() {
        let out = compile("@blockquote{quoted @em{prose}}\n@p{tight}\n", None).expect("compiles");
        assert!(
            out.code.contains("<blockquote><Reforest>"),
            "flow tag wraps its interior:\n{}",
            out.code
        );
        assert!(out.code.contains(r#"<p>{"tight"}</p>"#), "tight tag does not:\n{}", out.code);
    }

    #[test]
    fn compile_emits_source_map_when_requested() {
        let out = compile("@p{hi}\n", Some("doc.nota".into())).expect("compiles");
        assert!(out.map.is_some(), "source map present");
    }

    /// `(line, col)` of byte offset `off` in `text` (0-based; ASCII sources, so byte cols are
    /// fine — the sourcemap's cols are code units).
    fn line_col_of(text: &str, off: usize) -> (u32, u32) {
        let before = &text[..off];
        let line = u32::try_from(before.matches('\n').count()).unwrap();
        let col = u32::try_from(off - before.rfind('\n').map_or(0, |i| i + 1)).unwrap();
        (line, col)
    }

    /// Byte offset of 0-based `(line, col)` in `text`.
    fn offset_at(text: &str, line: u32, col: u32) -> usize {
        let mut off = 0usize;
        for _ in 0..line {
            off += text[off..].find('\n').expect("line in range") + 1;
        }
        off + col as usize
    }

    /// The sourcemap has *content*, not just presence: a known embedded-JS token (`count`)
    /// round-trips — some token's source position is exactly the `.nota` offset of `count`, and
    /// the generated code at that token's generated position is the same text (the byte-exactness
    /// invariant of the mapping tests, applied to the sourcemap channel).
    #[test]
    fn source_map_round_trips_a_known_token() {
        let src = "% const n = count();\n@p{hi}\n";
        let out = compile(src, Some("doc.nota".into())).expect("compiles");
        let map = out.map.expect("source map present");

        assert!(
            map.get_sources().any(|s| s.as_ref() == "doc.nota"),
            "map names the source: {:?}",
            map.get_sources().collect::<Vec<_>>()
        );

        let (src_line, src_col) = line_col_of(src, src.find("count").unwrap());
        let token = map
            .get_tokens()
            .find(|t| t.get_src_line() == src_line && t.get_src_col() == src_col)
            .expect("a token maps the source position of `count`");
        let gen_off = offset_at(&out.code, token.get_dst_line(), token.get_dst_col());
        assert_eq!(
            &out.code[gen_off..gen_off + "count".len()],
            "count",
            "the token's generated position holds the same text:\n{}",
            out.code
        );
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
        // The build path parses TS-aware and strips the types → plain, runnable JS.
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
        // EVERY reserved emit name — `Doc`, the structural components, and the ambient-prelude
        // names the markup lowers to (`Tex`, `Heading`, …) — is a lowering diagnostic when a `%`
        // binding shadows it. Looped over the real list, so extending the emit surface extends
        // this test. (`h` is no longer reserved — the h-call surface is gone.)
        for name in oxc_transformer::reserved_emit_names() {
            match compile(&format!("%let {name} = 1\n@p{{x}}\n"), None) {
                Ok(out) => panic!("`{name}`: expected a collision diagnostic, got:\n{}", out.code),
                Err(errors) => assert!(!errors.is_empty(), "`{name}`: empty diagnostics"),
            }
        }
        assert!(compile("%let h = 1\n@p{x}\n", None).is_ok(), "`h` is an ordinary name now");
        assert!(
            compile("%let mathset = () => 1\n@p{x}\n", None).is_ok(),
            "config fns are ambient but not emit-referenced — not reserved"
        );
    }

    #[test]
    fn fence_langs_are_collected_sorted_and_deduplicated() {
        // Grammars are opt-in downstream, so the tags are reported rather than resolved here:
        // aliases (`js`) and unknown tags (`wibble`) come through verbatim, and the integrator
        // decides which of them names a grammar it can import.
        let out = compile(
            "```rust\nfn main() {}\n```\n\n```js\nlet x = 1\n```\n\n```rust\nfn f() {}\n```\n\n```wibble\n?\n```\n",
            None,
        )
        .expect("compiles");
        assert_eq!(out.fence_langs, vec!["js", "rust", "wibble"]);
    }

    #[test]
    fn fence_langs_omit_untagged_fences_and_inline_code() {
        // An untagged fence takes its language from `lstset` at runtime, which is a value this
        // pass cannot see; inline code has no tag at all. Neither may invent an import.
        let out = compile("```\nplain\n```\n\nSome `inline` code.\n", None).expect("compiles");
        assert!(out.fence_langs.is_empty(), "{:?}", out.fence_langs);
    }

    #[test]
    fn fence_langs_exclude_the_explicit_code_block_form() {
        // Only the fence sugar reports a tag. `@CodeBlock[lang: …]` is the escape hatch, lowered
        // as an ordinary tagged element whose `lang` prop is an arbitrary expression — literal
        // here, but `lang: chosen` tomorrow — so there is no tag this pass can honestly report.
        // Documents using that form register their grammar through `lstset({ langs })`.
        let out = compile("@CodeBlock[lang: \"python\"]|{f(x)}|\n", None).expect("compiles");
        assert!(out.fence_langs.is_empty(), "{:?}", out.fence_langs);
    }

    #[test]
    fn free_names_cover_lowering_synthesized_and_user_refs() {
        // `# t` synthesizes a free `Heading` ref; `$x$` a free `Tex`; `% secset(…)` is a free
        // user call; the structural surface (`NotaDoc`) is free because the `@nota-lang/core`
        // import is the wrapper's job. Sorted output.
        let out = compile("% secset({ n: 1 })\n# Title\n\n$y$\n", None).expect("compiles");
        for name in ["Heading", "Tex", "secset", "NotaDoc"] {
            assert!(out.free_names.iter().any(|n| n == name), "{name} free: {:?}", out.free_names);
        }
        for gone in ["h", "decode", "Fragment"] {
            assert!(
                !out.free_names.iter().any(|n| n == gone),
                "{gone} is no longer part of the emit: {:?}",
                out.free_names
            );
        }
        let mut sorted = out.free_names.clone();
        sorted.sort_unstable();
        assert_eq!(out.free_names, sorted, "free names are sorted");
    }

    #[test]
    fn free_names_cover_structural_jsx_references() {
        // List markers → `UlLi`; `@for` → `For`; `@if` → `Show`; a dynamic tag → `Dynamic`. All
        // JSX identifier references, all free (the wrapper binds them).
        let out =
            compile("@for (x of xs) {\n  - @if (x) {@(tags[0]){y}}\n}\n", None).expect("compiles");
        for name in ["NotaDoc", "UlLi", "For", "Show", "Dynamic"] {
            assert!(out.free_names.iter().any(|n| n == name), "{name} free: {:?}", out.free_names);
        }
    }

    #[test]
    fn free_names_exclude_bound_and_textual_mentions() {
        // A `%`-imported name is bound (not free), and prose/string mentions of a name's *text*
        // are not references at all — the regex failure modes the metadata exists to kill.
        // (`Chart`, not `Tex`: emit-surface names are reserved now — importing one is a
        // collision diagnostic, per-doc override happens at the integrator's prelude seam.)
        let out = compile(
            "%import { Chart } from \"./my-chart.js\"\n@p{secset( is not a call}\n$y$\n",
            None,
        )
        .expect("compiles");
        assert!(
            !out.free_names.iter().any(|n| n == "Chart"),
            "imported Chart bound: {:?}",
            out.free_names
        );
        assert!(
            !out.free_names.iter().any(|n| n == "secset"),
            "prose mention: {:?}",
            out.free_names
        );
    }

    #[test]
    fn free_names_exclude_type_only_references() {
        // `Foo` occurs only in a (stripped) type annotation — a type reference, not a value one.
        let out = compile("% const n: Foo = 1\n@p{@(n)}\n", None).expect("compiles");
        assert!(!out.free_names.iter().any(|n| n == "Foo"), "type-only: {:?}", out.free_names);
    }

    #[test]
    fn free_names_respect_nested_shadowing() {
        // `mathset` is bound only inside the component arrow — a name bound at an enclosing scope
        // of its every use is not free; `createSignal` beside it stays free.
        let out = compile(
            "%let C = (props) => { let mathset = () => 1; return createSignal(mathset()); }\n@C{x}\n",
            None,
        )
        .expect("compiles");
        assert!(
            !out.free_names.iter().any(|n| n == "mathset"),
            "locally-bound mathset: {:?}",
            out.free_names
        );
        assert!(
            out.free_names.iter().any(|n| n == "createSignal"),
            "createSignal free: {:?}",
            out.free_names
        );
    }
}

// ===============================================================================================
// Code-mapping + type-preserving analysis tests.
// ===============================================================================================
#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "test fixtures: substring offsets/lengths fit in u32 (oxc's Span model)"
)]
mod code_mappings {
    use super::{CodeMapping, MappingCapabilities, analyze};

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
        let out = analyze(src);

        // Analysis preserves the TS type annotation.
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
        let out = analyze(src);

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
        // `@Aside` → `<Aside>`: navigation + hover, NOT a completion/format/structure region.
        let src = "@Aside{hi}\n";
        let out = analyze(src);

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
        let out = analyze(src);

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
    fn analysis_preserves_ts_annotation_and_frames_tsx() {
        // Analysis keeps the TS type annotation `: number` (no strip step) and the
        // `@for` head `as` cast, ready for the language server's `.tsx` TS service.
        let src = "% const n: number = count();\n@for (x of xs as string[]) {@x}\n";
        let out = analyze(src);

        assert!(out.code.contains(": number"), "`: number` preserved:\n{}", out.code);
        assert!(out.code.contains("as string[]"), "`as string[]` preserved:\n{}", out.code);

        // The mappings still resolve embedded identifiers byte-exactly in the `.tsx`.
        let (g, _, caps) = segment_at(&out.mappings, offset_of(src, "count"));
        assert_eq!(&out.code[g as usize..g as usize + 5], "count");
        assert_eq!(caps, MappingCapabilities::full());
    }

    #[test]
    fn round_trip_source_offset_inside_embedded_js() {
        // The headline invariant: a source offset *inside* an embedded-JS span maps to the correct
        // generated offset, and back.
        let src = "@p[onClick: () => go()]{hi}\n";
        let out = analyze(src);

        // `go` is inside the embedded prop arrow body; it maps to the `go` in the generated code.
        let go_src = offset_of(src, "go()");
        let (g, _, caps) = segment_at(&out.mappings, go_src);
        assert_eq!(&out.code[g as usize..g as usize + 2], "go", "round-trips to `go`");
        assert_eq!(caps, MappingCapabilities::full());
        assert_eq!(out.code.as_bytes()[g as usize], b'g');
    }

    /// The canonical golden, exercising the code mappings: the component binding, the `@Colorized`
    /// tag reference, the `@for` iterable + binding, the `@x`/`@(props.children)` interps — all
    /// map byte-exactly, with the right capabilities, and no boilerplate leaks in.
    const CANONICAL_NOTA: &str = "%let Colorized = (props: { children?: unknown }) => {\n  let [color, setColor] = createSignal(\"red\");\n  return @span[onClick: () => setColor(\"green\")][style: {color: color()}]{@(props.children)};\n}\n\n@for (x of items) {\n  - @Colorized{@x}\n}\n";

    #[test]
    fn canonical_golden_mappings_are_byte_exact() {
        let out = analyze(CANONICAL_NOTA);

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
        // the `%` statement (a document-local component binding, prepended into Doc) → embedded JS,
        // full caps.
        let colorized_decl = offset_of(CANONICAL_NOTA, "Colorized = ") as usize; // unique form
        let (gd, _, capsd) = segment_at(&out.mappings, colorized_decl as u32);
        assert_eq!(&out.code[gd as usize..gd as usize + "Colorized".len()], "Colorized");
        assert_eq!(capsd, MappingCapabilities::full());
    }
}

// ===============================================================================================
// EOF error-recovery: analysis keeps the partial AST + reports
// diagnostics on an unterminated construct, and materialises a prop-completion anchor at `@tag[|`.
// ===============================================================================================
#[cfg(test)]
mod recover {
    use super::analyze;

    /// The load-bearing P5 case: `@a[` at EOF still emits the recovered JSX opening tag, and a
    /// mapping anchors a completion cursor (the position just after `[`) into it, just inside the
    /// tag's attribute position.
    #[test]
    fn unclosed_props_group_yields_opening_tag_with_completion_anchor() {
        let out = analyze("@a[");

        // The analysis contains the recovered JSX element.
        assert!(out.code.contains("<a"), "recovered opening tag present:\n{}", out.code);

        // A syntax diagnostic is reported, not swallowed.
        assert_eq!(out.errors.len(), 1, "one recovered diagnostic: {:?}", out.errors);

        // The completion anchor: source offset 3 (just after `[`, where the cursor sits) maps to
        // a zero-width generated point *inside the opening tag* (after the tag name), where the
        // TSX service serves JSX attribute completions.
        let anchor = out
            .mappings
            .iter()
            .find(|m| m.source_offsets == vec![3] && m.lengths == vec![0])
            .expect("props-completion anchor mapping present");
        assert!(anchor.data.completion, "anchor carries completion capability");
        let g = anchor.generated_offsets[0] as usize;
        let before = &out.code[..g];
        assert!(
            before.ends_with("<a") || before.ends_with("<a "),
            "anchor is inside the opening tag: {before:?}"
        );
    }

    /// A well-formed file recovers to *exactly* the strict result: no phantom errors, no anchor.
    #[test]
    fn well_formed_input_recovers_identically() {
        let out = analyze("@a[id: x]{ok}\n");
        assert!(out.errors.is_empty(), "no diagnostics on well-formed input: {:?}", out.errors);
        // No zero-width completion anchor is synthesised (props closed normally).
        assert!(
            !out.mappings.iter().any(|m| m.lengths == vec![0]),
            "no recovery anchor on well-formed input",
        );
    }

    /// An unterminated `{ … }` body keeps its already-collected children and reports the missing
    /// `}` — the body text survives into the analysis for the TS service.
    #[test]
    fn unclosed_body_keeps_children_and_reports() {
        let out = analyze("@p{unterminated");
        assert!(out.code.contains("\"unterminated\""), "body text preserved:\n{}", out.code);
        assert_eq!(out.errors.len(), 1, "missing-`}}` diagnostic: {:?}", out.errors);
    }

    /// A bare `@` at EOF drops to an empty fragment (no phantom identifier binding) + diagnostic.
    #[test]
    fn bare_at_drops_to_empty_fragment() {
        let out = analyze("@");
        assert_eq!(out.errors.len(), 1, "bare-`@` diagnostic: {:?}", out.errors);
        // Recovered as `<></>` — no dangling identifier reference.
        assert!(out.code.contains("<></>"), "empty fragment recovery:\n{}", out.code);
    }

    /// Recovery surfaces a reserved-name collision (`%let NotaDoc = …`) as a diagnostic too, exactly
    /// like a parse error — `compile_internal`'s `recover` branch extends `errors` with lowering
    /// diagnostics instead of returning `Err`.
    #[test]
    fn reserved_name_collision_surfaces_as_diagnostic() {
        let out = analyze("%let NotaDoc = 1\n@p{x}\n");
        assert!(!out.errors.is_empty(), "collision surfaced as a diagnostic");
    }

    /// An unterminated verbatim body (`@pre|{` with no `}|`) still yields framed `.tsx`
    /// (the `Doc` wrapper + the recovered element), with the parse diagnostic surfaced.
    #[test]
    fn unterminated_verbatim_recovers_framed_tsx() {
        let out = analyze("before\n@pre|{\nraw run");
        assert_eq!(out.errors.len(), 1, "verbatim diagnostic: {:?}", out.errors);
        assert!(out.errors[0].message.contains("verbatim"), "mentions verbatim: {:?}", out.errors);
        assert!(
            out.code.contains("export default function Doc()"),
            "still framed TSX:\n{}",
            out.code
        );
        assert!(out.code.contains("<pre"), "the recovered verbatim element:\n{}", out.code);
    }

    /// The unterminated-`%%%`-fence contract: the fence body parses as JS to EOF
    /// with NO diagnostic (`find_fence_close` treats EOF as the close), and the statements land
    /// in the framed emit — the parser-level pin lives in `oxc_parser`'s recover_tests.
    #[test]
    fn unterminated_fence_recovers_silently_with_statements() {
        let out = analyze("%%%\nconst x = 1\n");
        assert!(
            out.errors.is_empty(),
            "no diagnostic for an EOF-terminated fence: {:?}",
            out.errors
        );
        assert!(
            out.code.contains("export default function Doc()"),
            "still framed TSX:\n{}",
            out.code
        );
        assert!(out.code.contains("const x = 1"), "the fence statement survives:\n{}", out.code);
    }
}

// ===============================================================================================
// The analysis JSON contract: the exact key set and shapes
// the `@nota-lang/compiler` shim and the language server parse.
// ===============================================================================================
#[cfg(test)]
mod analysis_json {
    use serde_json::Value;

    use super::analyze;

    fn parse(source: &str) -> Value {
        let out = analyze(source);
        serde_json::to_value(&out).expect("NotaOutput is serializable")
    }

    /// Assert `value` is an object with exactly `keys` (in any order).
    #[track_caller]
    fn assert_keys(value: &Value, keys: &[&str]) {
        let obj = value.as_object().expect("a JSON object");
        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want = keys.to_vec();
        want.sort_unstable();
        assert_eq!(got, want, "exact key set");
    }

    #[test]
    fn top_level_and_mapping_shapes() {
        let json = parse("@a[k: theId]{@(user)}\n");
        assert_keys(
            &json,
            &["ast", "code", "errors", "fenceLangs", "freeNames", "highlights", "mappings"],
        );

        let code = json["code"].as_str().expect("`code` is a string");
        assert!(code.contains("export default function Doc()"), "framed TSX: {code}");
        assert!(json["ast"].as_str().is_some_and(|ast| ast.contains("NotaDocument")));
        assert!(json["freeNames"].as_array().is_some_and(|names| !names.is_empty()));
        let highlights = json["highlights"].as_array().expect("`highlights` is an array");
        assert_eq!(highlights.len() % 3, 0, "highlight triples");
        assert!(!highlights.is_empty(), "markup produces highlights");

        let mappings = json["mappings"].as_array().expect("`mappings` is an array");
        assert!(!mappings.is_empty(), "the embedded JS produced mappings");
        for m in mappings {
            assert_keys(
                m,
                &["sourceOffsets", "generatedOffsets", "lengths", "generatedLengths", "data"],
            );
            let source_offsets = m["sourceOffsets"].as_array().expect("array");
            let generated_offsets = m["generatedOffsets"].as_array().expect("array");
            let lengths = m["lengths"].as_array().expect("array");
            assert_eq!(source_offsets.len(), generated_offsets.len(), "parallel arrays");
            assert_eq!(source_offsets.len(), lengths.len(), "parallel arrays");
            for x in source_offsets.iter().chain(generated_offsets).chain(lengths) {
                assert!(x.is_u64(), "offsets/lengths are unsigned numbers: {x:?}");
            }
            // `generatedLengths` is null or a parallel array.
            match &m["generatedLengths"] {
                Value::Null => {}
                Value::Array(v) => assert_eq!(v.len(), source_offsets.len(), "parallel array"),
                other => panic!("generatedLengths must be null or an array: {other:?}"),
            }
            let data = &m["data"];
            assert_keys(
                data,
                &["completion", "format", "navigation", "semantic", "structure", "verification"],
            );
            for flag in data.as_object().unwrap().values() {
                assert!(flag.is_boolean(), "capability flags are booleans: {flag:?}");
            }
        }

        assert_eq!(json["errors"].as_array().expect("array").len(), 0, "well-formed → no errors");
    }

    #[test]
    fn errors_carry_message_and_span() {
        let json = parse("@p{unterminated");
        let errors = json["errors"].as_array().expect("`errors` is an array");
        assert_eq!(errors.len(), 1, "the recovered diagnostic is serialized: {errors:?}");
        for e in errors {
            assert_keys(e, &["message", "start", "len"]);
            assert!(!e["message"].as_str().expect("string").is_empty());
            assert!(e["start"].is_u64() && e["len"].is_u64(), "byte-span numbers: {e:?}");
        }
        // The code is still present and framed on the recover path.
        assert!(json["code"].as_str().unwrap().contains("export default function Doc()"));
    }

    /// The escaping path: `code` contains newlines, quotes, and backslashes.
    #[test]
    fn code_string_escaping_round_trips() {
        let src = "@p{a \"quoted\" \\@ literal}\n";
        let out = analyze(src);
        let json: Value = serde_json::to_value(&out).expect("NotaOutput is serializable");
        assert_eq!(json["code"].as_str().unwrap(), out.code, "code round-trips exactly");
    }
}
