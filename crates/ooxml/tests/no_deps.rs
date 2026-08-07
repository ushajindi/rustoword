//! Сторож требования «ноль внешних крейтов».
//!
//! Требование жёсткое и распространяется на `[dev-dependencies]`. Единственный
//! способ его нарушить — добавить строку в `Cargo.toml`, и человек это заметит
//! не раньше, чем `Cargo.lock` разрастётся. Тест ловит это сразу.

// В тестах паника — это способ сообщить о провале.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/ooxml, корень на два уровня выше.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

#[test]
fn lockfile_contains_only_workspace_members() {
    let lock = workspace_root().join("Cargo.lock");
    let text = match std::fs::read_to_string(&lock) {
        Ok(t) => t,
        Err(e) => panic!("не читается {}: {e}", lock.display()),
    };

    let names: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();

    let mut sorted = names.clone();
    sorted.sort_unstable();

    assert_eq!(
        sorted,
        vec!["ooxml", "ooxml-cli", "ooxml-wasm", "xtask"],
        "в Cargo.lock появилась внешняя зависимость: {names:?}.\n\
         Ноль зависимостей — архитектурное требование, а не рекомендация."
    );
}

#[test]
fn manifest_declares_no_dependencies() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(t) => t,
        Err(e) => panic!("не читается {}: {e}", manifest.display()),
    };

    // Собираем содержимое секций [dependencies] и [dev-dependencies].
    let mut in_deps = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]" || t == "[dev-dependencies]";
            continue;
        }
        if in_deps && !t.is_empty() && !t.starts_with('#') {
            panic!("в секции зависимостей ooxml появилась строка: {t}");
        }
    }
}
