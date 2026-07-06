//! Compile a `.nota` file to a JS module string and print it to stdout.
//!
#![expect(clippy::exit, reason = "CLI example: exit non-zero on compile error")]
//!
//! The minimal CLI form of `oxc::nota::compile`, for demos and integration testing. The proper CLI
//! is `@nota-lang/cli`; the proper library bridge is `@nota-lang/compiler` (wasm/napi). This just
//! exposes the entry.
//!
//! ```sh
//! cargo run -q -p oxc --example nota_compile --features codegen -- path/to/doc.nota
//! ```
//!
//! ## `--virtual` mode (binary ↔ wrapper ↔ language-server JSON protocol)
//!
//! With `--virtual <file>` it instead calls [`oxc::nota::compile_virtual`] (the type-preserving
//! `.tsx` emit + [`CodeMapping`](oxc::nota::CodeMapping)s) and prints a single JSON object to
//! stdout. The virtual path uses **EOF error-recovery**: an unterminated
//! construct still yields `code` + `mappings`, and the syntax/lowering problems come back in
//! `errors` — so `--virtual` **exits 0** even on a malformed document:
//!
//! ```json
//! { "code": "<virtual .tsx>",
//!   "mappings": [ { "sourceOffsets":[u32], "generatedOffsets":[u32], "lengths":[u32],
//!                   "generatedLengths": [u32]|null,
//!                   "data": {"completion":bool,"format":bool,"navigation":bool,
//!                            "semantic":bool,"structure":bool,"verification":bool} } ],
//!   "errors": [ { "message": string, "start": u32, "len": u32 } ] }
//! ```
//!
//! ```sh
//! cargo run -q -p oxc --example nota_compile --features codegen -- --virtual path/to/doc.nota
//! ```
//!
//! The `@nota-lang/compiler` wrapper's `compileVirtual(source) → { code, mappings, errors }` parses
//! this; the language server's Volar `LanguagePlugin` prepends its runtime+ambient typing preamble
//! to `code` and shifts every `generatedOffsets` by the preamble length (`sourceOffsets` index the
//! `.nota`, unchanged), and surfaces `errors` as LSP diagnostics at the given `.nota` spans. The
//! JSON is hand-rolled (no `serde` dependency added to the published `oxc` crate); the only values
//! needing escaping are `code` and each error `message`.
#![expect(clippy::print_stdout, clippy::print_stderr)]

use oxc::diagnostics::OxcDiagnostic;
use oxc::nota::{CodeMapping, MappingCapabilities};

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().expect("usage: nota_compile [--virtual] <file.nota>");

    if first == "--virtual" {
        let path = args.next().expect("usage: nota_compile --virtual <file.nota>");
        let source = std::fs::read_to_string(&path).expect("read .nota file");
        run_virtual(&source);
    } else {
        // `first` is the path. Keep the no-flag behavior byte-identical to the original example.
        let source = std::fs::read_to_string(&first).expect("read .nota file");
        match oxc::nota::compile(&source, None) {
            Ok(compiled) => print!("{}", compiled.code),
            Err(errors) => {
                for error in errors {
                    eprintln!("{error:?}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// `--virtual` path: compile to the virtual `.tsx` + code mappings + recovered diagnostics and
/// print the JSON. EOF error-recovery means this **exits 0** even on a malformed document — the
/// syntax problems are reported in the `errors` array, not via a non-zero exit.
fn run_virtual(source: &str) {
    match oxc::nota::compile_virtual(source) {
        Ok(compiled) => {
            let mut out = String::new();
            write_virtual_json(&mut out, &compiled.code, &compiled.mappings, &compiled.errors);
            print!("{out}");
        }
        // Practically unreachable on the virtual path (recovery + no TS strip); keep it total.
        Err(errors) => {
            for error in errors {
                eprintln!("{error:?}");
            }
            std::process::exit(1);
        }
    }
}

/// Serialize `{ code, mappings, errors }` as JSON into `out`.
fn write_virtual_json(
    out: &mut String,
    code: &str,
    mappings: &[CodeMapping],
    errors: &[OxcDiagnostic],
) {
    out.push_str("{\"code\":");
    push_json_string(out, code);
    out.push_str(",\"mappings\":[");
    for (i, m) in mappings.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_mapping_json(out, m);
    }
    out.push_str("],\"errors\":[");
    for (i, e) in errors.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_error_json(out, e);
    }
    out.push_str("]}");
}

/// Serialize one diagnostic as `{ "message": string, "start": u32, "len": u32 }`. The span is the
/// first label's offset/length (byte offsets into the `.nota`); a label-less diagnostic reports
/// `start: 0, len: 0`.
fn write_error_json(out: &mut String, error: &OxcDiagnostic) {
    let (start, len) = error
        .labels
        .as_ref()
        .and_then(|labels| labels.first())
        .map_or((0u32, 0u32), |label| (label.offset() as u32, label.len() as u32));
    out.push_str("{\"message\":");
    push_json_string(out, &error.message);
    out.push_str(",\"start\":");
    out.push_str(itoa_u32(start).as_str());
    out.push_str(",\"len\":");
    out.push_str(itoa_u32(len).as_str());
    out.push('}');
}

/// Serialize one [`CodeMapping`] as JSON (camelCase keys, parallel u32 arrays).
fn write_mapping_json(out: &mut String, m: &CodeMapping) {
    out.push_str("{\"sourceOffsets\":");
    push_u32_array(out, &m.source_offsets);
    out.push_str(",\"generatedOffsets\":");
    push_u32_array(out, &m.generated_offsets);
    out.push_str(",\"lengths\":");
    push_u32_array(out, &m.lengths);
    out.push_str(",\"generatedLengths\":");
    match &m.generated_lengths {
        Some(v) => push_u32_array(out, v),
        None => out.push_str("null"),
    }
    out.push_str(",\"data\":");
    write_caps_json(out, m.data);
    out.push('}');
}

/// Serialize a [`MappingCapabilities`] as JSON (the six Volar `CodeInformation` flags).
fn write_caps_json(out: &mut String, c: MappingCapabilities) {
    out.push_str("{\"completion\":");
    push_bool(out, c.completion);
    out.push_str(",\"format\":");
    push_bool(out, c.format);
    out.push_str(",\"navigation\":");
    push_bool(out, c.navigation);
    out.push_str(",\"semantic\":");
    push_bool(out, c.semantic);
    out.push_str(",\"structure\":");
    push_bool(out, c.structure);
    out.push_str(",\"verification\":");
    push_bool(out, c.verification);
    out.push('}');
}

fn push_bool(out: &mut String, b: bool) {
    out.push_str(if b { "true" } else { "false" });
}

fn push_u32_array(out: &mut String, xs: &[u32]) {
    out.push('[');
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // u32 decimal is always valid JSON number text.
        out.push_str(itoa_u32(*x).as_str());
    }
    out.push(']');
}

/// Stack-free `u32` → decimal `String` (avoids pulling in a formatting crate).
fn itoa_u32(mut x: u32) -> String {
    if x == 0 {
        return "0".to_string();
    }
    let mut buf = [0u8; 10]; // u32::MAX = 4294967295 → 10 digits
    let mut i = buf.len();
    while x > 0 {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    // SAFETY-free: bytes are all ASCII digits.
    String::from_utf8(buf[i..].to_vec()).unwrap()
}

/// Push a JSON string literal (with surrounding quotes) for `s`, escaping per RFC 8259:
/// `"` `\` `\n` `\r` `\t` `\b` `\f`, and any other control character `< 0x20` as `\u00XX`.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                // Remaining control chars: \u00XX (two lowercase hex digits).
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let code = c as u32;
                out.push_str("\\u00");
                out.push(HEX[((code >> 4) & 0xF) as usize] as char);
                out.push(HEX[(code & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
