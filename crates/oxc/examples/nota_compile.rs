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
//! stdout — [`NotaVirtualCompiled::to_json`](oxc::nota::NotaVirtualCompiled::to_json), where the
//! shape is documented and tested. The virtual path uses **EOF error-recovery**: an unterminated
//! construct still yields `code` + `mappings`, and the syntax/lowering problems come back in
//! `errors` — so `--virtual` **exits 0** even on a malformed document.
//!
//! ```sh
//! cargo run -q -p oxc --example nota_compile --features codegen -- --virtual path/to/doc.nota
//! ```
//!
//! The `@nota-lang/compiler` wrapper's `compileVirtual(source) → { code, mappings, errors }` parses
//! this; the language server's Volar `LanguagePlugin` prepends its runtime+ambient typing preamble
//! to `code` and shifts every `generatedOffsets` by the preamble length (`sourceOffsets` index the
//! `.nota`, unchanged), and surfaces `errors` as LSP diagnostics at the given `.nota` spans.
#![expect(clippy::print_stdout, clippy::print_stderr)]

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
        Ok(compiled) => print!("{}", compiled.to_json()),
        // Practically unreachable on the virtual path (recovery + no TS strip); keep it total.
        Err(errors) => {
            for error in errors {
                eprintln!("{error:?}");
            }
            std::process::exit(1);
        }
    }
}
