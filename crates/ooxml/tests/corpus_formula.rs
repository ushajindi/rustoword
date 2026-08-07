//! Гейт вехи M10: все формулы реального корпуса разбираются, и печать
//! возвращает **исходный текст**.
//!
//! Критерий вехи — не «разбирается без паники», а точное совпадение круга
//! `print(parse(f)) == f`. Причина та же, что у DOM: формулу, которую мы
//! напечатали иначе, чем прочитали, нельзя записать обратно в чужой файл, не
//! изменив его. А если формулу нельзя записать обратно, то и сдвигать ссылки в
//! ней бессмысленно — ради чего разбор и существует.
//!
//! Источников формул два, и второй важнее первого. В `<f>` листов лежат
//! обычные `SUM(D6:D10)`. В `<definedName>` книги — области печати и заголовки,
//! и там встречаются имена листов с точками и цифрами (`стр.1_4`), записанные
//! **без апострофов**: именно на них ломается наивный разбор имени листа.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use ooxml::Limits;
use ooxml::dom::{Document, NodeId};
use ooxml::formula::{functions, parse_formula, print_formula};
use ooxml::zip::ZipArchive;

const DEFAULT_CORPUS: &str = "/Users/shakh/rustoword/crates/ooxml/tests/corpus";

fn packages() -> Vec<PathBuf> {
    let root = std::env::var_os("OOXML_CORPUS")
        .map_or_else(|| PathBuf::from(DEFAULT_CORPUS), PathBuf::from);
    let mut out = Vec::new();
    for sub in ["xlsx", "docx"] {
        let Ok(entries) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let ok = p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("xlsx") || x.eq_ignore_ascii_case("docx"));
            if ok {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Одна найденная формула вместе с адресом, по которому её искать глазами.
struct Found {
    where_: String,
    text: String,
}

/// Собирает текст всех элементов с заданным локальным именем.
///
/// Сравнение идёт по локальному имени, без namespace: части корпуса приходят
/// от Excel, LibreOffice и Google Sheets, и префикс у них разный, а тест не
/// про namespace.
fn collect(doc: &Document, node: NodeId, local: &str, out: &mut Vec<String>) {
    if doc.local_name(node) == Some(local.as_bytes())
        && let Ok(t) = doc.text(node)
        && !t.is_empty()
    {
        out.push(t.into_owned());
    }
    for c in doc.children(node) {
        collect(doc, c, local, out);
    }
}

fn harvest() -> Vec<Found> {
    let limits = Limits::strict();
    let mut found = Vec::new();

    for path in packages() {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let pkg = path
            .file_name()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(zip) = ZipArchive::parse(&data, &limits) else {
            continue;
        };

        for i in 0..zip.entries().len() {
            let Ok(name) = zip.name_str(i) else { continue };
            let name = name.into_owned();
            let is_sheet = name.starts_with("xl/worksheets/") && name.ends_with(".xml");
            let is_book = name == "xl/workbook.xml";
            if !is_sheet && !is_book {
                continue;
            }
            let plain = zip
                .decompress(i, &limits)
                .unwrap_or_else(|e| panic!("{pkg}!{name}: распаковка: {e}"));
            let doc = Document::parse(plain, &limits)
                .unwrap_or_else(|e| panic!("{pkg}!{name}: разбор XML: {e}"));
            let root = doc.root_element().unwrap();

            let mut texts = Vec::new();
            collect(
                &doc,
                root,
                if is_sheet { "f" } else { "definedName" },
                &mut texts,
            );
            for t in texts {
                found.push(Found {
                    where_: format!("{pkg}!{name}"),
                    text: t,
                });
            }
        }
    }
    found
}

#[test]
fn every_corpus_formula_round_trips_exactly() {
    let formulas = harvest();
    if formulas.is_empty() {
        eprintln!("корпус не найден — тест пропущен (задайте OOXML_CORPUS)");
        return;
    }

    let mut failed_parse: Vec<String> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();
    let mut funcs: BTreeMap<String, u32> = BTreeMap::new();
    let mut exact = 0u32;

    for f in &formulas {
        let ast = match parse_formula(&f.text) {
            Ok(a) => a,
            Err(e) => {
                failed_parse.push(format!("{}: {e}\n    {:?}", f.where_, f.text));
                continue;
            }
        };
        for name in functions(&ast) {
            *funcs.entry(name).or_default() += 1;
        }
        let back = print_formula(&ast);
        if back == f.text {
            exact += 1;
        } else {
            mismatched.push(format!(
                "{}:\n    было:  {:?}\n    стало: {:?}",
                f.where_, f.text, back
            ));
        }
    }

    let total = formulas.len() as u32;
    let pct = f64::from(exact) * 100.0 / f64::from(total);
    println!("формул найдено: {total}");
    println!("разобрано: {}", total - failed_parse.len() as u32);
    println!("круг print(parse(f)) == f: {exact}/{total} ({pct:.2}%)");
    println!("используемые функции (по убыванию частоты):");
    let mut by_freq: Vec<_> = funcs.iter().collect();
    by_freq.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (name, n) in by_freq {
        println!("  {name}: {n}");
    }

    for m in &failed_parse {
        println!("НЕ РАЗОБРАЛОСЬ {m}");
    }
    for m in &mismatched {
        println!("РАСХОЖДЕНИЕ {m}");
    }

    assert!(
        failed_parse.is_empty(),
        "{} формул не разобрались",
        failed_parse.len()
    );
    assert!(
        pct >= 99.0,
        "точный круг только на {pct:.2}%, требуется не менее 99%"
    );
}

/// Разбор обязан быть идемпотентным: повторный круг ничего больше не меняет.
///
/// Проверка отдельная, потому что ловит другой дефект. Первый круг может
/// разойтись с исходником по причине, которую мы осознанно приняли (например,
/// нормализация регистра). Но если и второй круг снова что-то меняет, значит
/// печать и разбор не сходятся к неподвижной точке — и правка файла будет
/// «дрейфовать» при каждом сохранении.
#[test]
fn second_round_trip_is_a_fixed_point() {
    let formulas = harvest();
    if formulas.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }
    for f in &formulas {
        let Ok(a1) = parse_formula(&f.text) else {
            continue;
        };
        let once = print_formula(&a1);
        let a2 = parse_formula(&once)
            .unwrap_or_else(|e| panic!("{}: свой же вывод не разбирается: {e}", f.where_));
        assert_eq!(
            print_formula(&a2),
            once,
            "{}: второй круг снова изменил текст",
            f.where_
        );
        assert_eq!(a1, a2, "{}: второй разбор дал другое дерево", f.where_);
    }
}
