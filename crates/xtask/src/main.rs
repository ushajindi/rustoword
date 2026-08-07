//! Прогон проверок: `cargo xtask ci`.
//!
//! Отдельный крейт, а не скрипт, потому что кроссплатформенный shell — миф,
//! а зависимостей вроде `cargo-make` в проекте быть не может.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::{Command, ExitCode};

const SOFFICE: &str = "/Applications/LibreOffice.app/Contents/MacOS/soffice";

struct Step {
    name: &'static str,
    args: &'static [&'static str],
}

const CI: &[Step] = &[
    Step {
        name: "форматирование",
        args: &["fmt", "--all", "--check"],
    },
    Step {
        name: "линты",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    },
    Step {
        name: "тесты",
        args: &["test", "--workspace"],
    },
    Step {
        name: "сборка под wasm",
        args: &["check", "-p", "ooxml", "--target", "wasm32-unknown-unknown"],
    },
];

fn run(step: &Step) -> bool {
    println!("\n==> {}", step.name);
    match Command::new(env!("CARGO")).args(step.args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("!!! {} — провал ({s})", step.name);
            false
        }
        Err(e) => {
            eprintln!("!!! {} — не запустилось: {e}", step.name);
            false
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ci") => {
            let mut failed = Vec::new();
            for step in CI {
                if !run(step) {
                    failed.push(step.name);
                }
            }

            if std::path::Path::new(SOFFICE).exists() {
                println!("\n==> LibreOffice найден: {SOFFICE}");
                println!("    прогон оракула: OOXML_SOFFICE={SOFFICE} cargo test -- --ignored");
            } else {
                println!("\n==> LibreOffice не найден — оракул пропущен");
            }

            if failed.is_empty() {
                println!("\nвсе проверки пройдены");
                ExitCode::SUCCESS
            } else {
                eprintln!("\nпровалено: {}", failed.join(", "));
                ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!("использование: cargo xtask ci");
            ExitCode::FAILURE
        }
    }
}
