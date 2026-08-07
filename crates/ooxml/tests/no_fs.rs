//! Сторож требования «ядро не трогает окружение».
//!
//! Ядро должно собираться под `wasm32-unknown-unknown` и работать с `&[u8]`.
//! Один случайный `std::time::SystemTime::now()` в ZIP-writer'е — и сборка под
//! wasm ломается, а вместе с ней ломается детерминизм round-trip: DOS-время
//! записи обязано браться из исходного файла, а не из часов.

// В тестах паника — это способ сообщить о провале.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};

/// Запрещённые в ядре пути. Ключ — что искать, значение — почему нельзя.
const FORBIDDEN: &[(&str, &str)] = &[
    (
        "std::fs",
        "ядро работает с &[u8]; файловый I/O живёт в ooxml-cli",
    ),
    ("std::net", "ядру незачем сеть"),
    (
        "std::time",
        "DOS-время записи берётся из исходного файла, иначе round-trip недетерминирован",
    ),
    ("std::process", "ядру незачем порождать процессы"),
    ("std::env", "поведение ядра не должно зависеть от окружения"),
    ("unsafe", "unsafe_code = forbid, но проверим и текстом"),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn core_does_not_touch_the_environment() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "не найдено ни одного файла в {src:?}");

    let mut violations = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Комментарии и doc-комментарии не считаются: в них эти слова
            // как раз и объясняют, почему их нельзя использовать.
            if trimmed.starts_with("//") {
                continue;
            }
            // `#![forbid(unsafe_code)]` — это запрет unsafe, а не его применение.
            if trimmed.contains("forbid(unsafe_code)") {
                continue;
            }
            for (needle, why) in FORBIDDEN {
                if line.contains(needle) {
                    violations.push(format!(
                        "{}:{}: {needle} — {why}\n    {}",
                        file.display(),
                        lineno + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "ядро обратилось к окружению:\n{}",
        violations.join("\n")
    );
}
