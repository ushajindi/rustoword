//! Критерий приёмки вехи M5: главный инвариант плитования на реальном корпусе.
//!
//! Проверяется каждая XML-часть каждого файла корпуса: лексер обязан разобрать
//! её без ошибок, а конкатенация спанов выданных событий — совпасть с
//! содержимым части побайтово. Именно здесь ловится любая «нормализация»,
//! которая на рукотворных примерах незаметна: BOM, `\r\n` после декларации,
//! пробел перед `/>`, смесь `&quot;` с `&#34;`.
//!
//! # Почему `unzip`, а не свой ZIP
//!
//! Слои `zip` и `deflate` — вехи M1–M2, к моменту M5 их ещё нет. Ждать их
//! незачем: `std::process` запрещён только в `src/` (это стережёт `no_fs.rs`),
//! а интеграционному тесту он доступен. Распаковка внешним `unzip` — временный
//! костыль ровно на одну веху.
//!
//! Отсутствие корпуса или `unzip` — предупреждение и проход, а не падение:
//! корпус состоит из настоящих файлов пользователя и в репозиторий не входит.

// В тестах паника — это способ сообщить о провале; арифметика здесь — счётчики
// статистики, переполнение которых в debug-сборке паникует само.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use ooxml::xml::lexer::{Event, Lexer, retile};
use ooxml::xml::reader::Reader;

const DEFAULT_CORPUS: &str = "/Users/shakh/rustoword/crates/ooxml/tests/corpus";

fn corpus_root() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(|| PathBuf::from(DEFAULT_CORPUS), PathBuf::from)
}

/// Части OPC, содержимое которых — XML.
///
/// Сравнение по имени файла, а не по `Path::extension`: главный файл отношений
/// пакета называется `_rels/.rels`, и для него `extension()` возвращает `None`
/// — расширением он считает только то, что после точки в середине имени. Так по
/// одной части на каждый файл корпуса тихо выпало бы из проверки.
fn is_xml_part(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    // `.vml` — тоже XML: это векторная разметка Office из docx.
    [".xml", ".rels", ".vml"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, pred: &dyn Fn(&Path) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out, pred);
        } else if pred(&p) {
            out.push(p);
        }
    }
}

fn have_unzip() -> bool {
    Command::new("unzip")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Статистика лексических особенностей: она и есть доказательство того, что
/// корпус действительно разнообразен, а не состоит из клонов одного файла.
#[derive(Default)]
struct Stats {
    parts: usize,
    with_bom: usize,
    with_crlf: usize,
    with_decl: usize,
    decl_standalone: usize,
    comments: usize,
    cdata: usize,
    pis: usize,
    space_before_self_close: usize,
    single_quoted_attrs: usize,
    ends_with_gt: usize,
    entities: BTreeMap<String, usize>,
    max_depth: usize,
    bytes: u64,
}

impl Stats {
    /// Считает вхождения `&...;` дословно: нас интересует, какие ФОРМЫ записи
    /// встречаются, а не какие символы они обозначают.
    fn count_entities(&mut self, src: &[u8]) {
        let mut i = 0usize;
        while i < src.len() {
            if src[i] != b'&' {
                i += 1;
                continue;
            }
            let end = src[i..]
                .iter()
                .take(32)
                .position(|&b| b == b';')
                .map(|p| i + p + 1);
            match end {
                Some(e) => {
                    let form = String::from_utf8_lossy(&src[i..e]).into_owned();
                    *self.entities.entry(form).or_insert(0) += 1;
                    i = e;
                }
                None => i += 1,
            }
        }
    }
}

/// Разбирает одну часть и проверяет инвариант. Возвращает описание провала.
fn check_part(src: &[u8], stats: &mut Stats) -> Result<(), String> {
    match retile(src) {
        Ok(back) if back == src => {}
        Ok(back) => {
            return Err(format!(
                "плитование нарушено: {} байт на входе, {} на выходе",
                src.len(),
                back.len()
            ));
        }
        Err(e) => return Err(format!("лексер отверг часть: {e}")),
    }

    // Полный разбор — сверх инварианта, но реальный корпус обязан проходить и
    // структурные проверки, иначе на нём нельзя будет строить DOM.
    let mut r = Reader::new(src);
    let mut depth = 0usize;
    loop {
        match r.next_event() {
            Ok(Event::Eof) => break,
            Ok(ev) => {
                if let Event::Start { empty: false, .. } = ev {
                    depth = depth.max(r.depth());
                }
            }
            Err(e) => return Err(format!("парсер отверг часть: {e}")),
        }
    }

    stats.parts += 1;
    stats.bytes += src.len() as u64;
    stats.max_depth = stats.max_depth.max(depth);
    if src.starts_with(&[0xEF, 0xBB, 0xBF]) {
        stats.with_bom += 1;
    }
    if src.windows(2).any(|w| w == b"\r\n") {
        stats.with_crlf += 1;
    }
    if src.last() == Some(&b'>') {
        stats.ends_with_gt += 1;
    }
    stats.count_entities(src);

    // Пересчёт лексических особенностей по событиям, а не по сырому поиску:
    // так `<!--` внутри CDATA не считается комментарием.
    let mut lex = Lexer::new(src);
    while let Ok(ev) = lex.next_event() {
        match ev {
            Event::Eof => break,
            Event::Decl { span } => {
                stats.with_decl += 1;
                if span
                    .slice(src)
                    .is_some_and(|b| b.windows(10).any(|w| w == b"standalone"))
                {
                    stats.decl_standalone += 1;
                }
            }
            Event::Comment { .. } => stats.comments += 1,
            Event::CData { .. } => stats.cdata += 1,
            Event::Pi { .. } => stats.pis += 1,
            Event::Start {
                pre_close_ws,
                attrs_from,
                attrs_len,
                ..
            } => {
                if !pre_close_ws.is_empty() {
                    stats.space_before_self_close += 1;
                }
                for a in lex.attr_range(attrs_from, attrs_len) {
                    if a.quote == b'\'' {
                        stats.single_quoted_attrs += 1;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[test]
fn every_xml_part_of_the_corpus_tiles_byte_for_byte() {
    let root = corpus_root();
    if !root.is_dir() {
        eprintln!("ПРЕДУПРЕЖДЕНИЕ: корпус не найден в {root:?} — тест пропущен");
        return;
    }
    if !have_unzip() {
        eprintln!("ПРЕДУПРЕЖДЕНИЕ: `unzip` недоступен — тест пропущен");
        return;
    }

    let mut packages = Vec::new();
    for sub in ["xlsx", "docx"] {
        let dir = root.join(sub);
        if dir.is_dir() {
            collect(&dir, &mut packages, &|p| {
                p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("xlsx") || e.eq_ignore_ascii_case("docx")
                })
            });
        }
    }
    packages.sort();
    if packages.is_empty() {
        eprintln!("ПРЕДУПРЕЖДЕНИЕ: в {root:?} нет .xlsx/.docx — тест пропущен");
        return;
    }

    // Один каталог на весь прогон: 43 вызова `unzip` вместо 800.
    let work = std::env::temp_dir().join(format!("ooxml_corpus_xml_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("не создаётся временный каталог");

    let mut stats = Stats::default();
    let mut failures: Vec<String> = Vec::new();
    let mut packages_done = 0usize;

    for (i, pkg) in packages.iter().enumerate() {
        let dest = work.join(format!("p{i}"));
        let ok = Command::new("unzip")
            .args(["-qq", "-o", "-d"])
            .arg(&dest)
            .arg(pkg)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            // Битый архив — не повод валить веху про XML; отметим и пойдём дальше.
            eprintln!("ПРЕДУПРЕЖДЕНИЕ: не распаковался {pkg:?}");
            continue;
        }
        packages_done += 1;

        let mut parts = Vec::new();
        collect(&dest, &mut parts, &is_xml_part);
        parts.sort();
        for part in &parts {
            let Ok(bytes) = std::fs::read(part) else {
                continue;
            };
            let rel = part.strip_prefix(&dest).unwrap_or(part);
            if let Err(why) = check_part(&bytes, &mut stats) {
                failures.push(format!("{}!{}: {why}", pkg.display(), rel.display()));
            }
        }
        let _ = std::fs::remove_dir_all(&dest);
    }
    let _ = std::fs::remove_dir_all(&work);

    eprintln!("--- корпус XML -------------------------------------------------");
    eprintln!(
        "пакетов разобрано:        {packages_done} из {}",
        packages.len()
    );
    eprintln!("XML-частей:               {}", stats.parts);
    eprintln!("суммарно байт:            {}", stats.bytes);
    eprintln!("с BOM:                    {}", stats.with_bom);
    eprintln!("с CRLF:                   {}", stats.with_crlf);
    eprintln!("оканчиваются на `>`:      {}", stats.ends_with_gt);
    eprintln!(
        "с декларацией:            {} (из них со standalone: {})",
        stats.with_decl, stats.decl_standalone
    );
    eprintln!("комментариев:             {}", stats.comments);
    eprintln!("секций CDATA:             {}", stats.cdata);
    eprintln!("инструкций обработки:     {}", stats.pis);
    eprintln!(
        "тегов с пробелом до `>`:  {}",
        stats.space_before_self_close
    );
    eprintln!("атрибутов в апострофах:   {}", stats.single_quoted_attrs);
    eprintln!("максимальная глубина:     {}", stats.max_depth);
    eprintln!("формы ссылок:");
    let mut forms: Vec<(&String, &usize)> = stats.entities.iter().collect();
    forms.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (form, n) in forms.iter().take(24) {
        eprintln!("    {form:<12} {n}");
    }
    eprintln!("----------------------------------------------------------------");

    assert!(
        failures.is_empty(),
        "инвариант плитования нарушен на {} частях:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(stats.parts > 0, "не найдено ни одной XML-части");
    assert_eq!(
        stats.parts, stats.ends_with_gt,
        "часть корпуса оканчивается не на `>` — это меняет ожидания сериализатора"
    );
}
