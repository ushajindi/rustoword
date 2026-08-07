//! Гейт вехи M6: `serialize(parse(part)) == part` побайтово для **всех**
//! XML-частей реального корпуса.
//!
//! Пока этот тест не показывает `N/N`, вехи над DOM не начинаются. Причина в
//! замерах: если сериализатор нормализует хотя бы одну лексическую деталь, он
//! разойдётся с исходником в большинстве частей — `<a></a>` встречается
//! 104 911 раз в 34 файлах, CRLF — в 204 частях, одиночный CR — в 110.
//!
//! # Две ловушки этого теста
//!
//! 1. `Path::extension()` для `_rels/.rels` возвращает `None`: расширением
//!    считается только то, что после точки **в середине** имени. Сравнение по
//!    расширению тихо выбросило бы по одной части из каждого пакета —
//!    и главный файл отношений остался бы непроверенным. Здесь сравнивается
//!    имя целиком.
//! 2. Расхождение без диагностики бесполезно: «не совпало» на файле в мегабайт
//!    — это гадание. Поэтому печатается позиция первого разошедшегося байта,
//!    окрестность обеих версий и путь до узла, которому этот байт принадлежит.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::PathBuf;

use ooxml::Limits;
use ooxml::dom::Document;
use ooxml::zip::ZipArchive;

const DEFAULT_CORPUS: &str = "/Users/shakh/rustoword/crates/ooxml/tests/corpus";

fn corpus_root() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(|| PathBuf::from(DEFAULT_CORPUS), PathBuf::from)
}

fn packages() -> Vec<PathBuf> {
    let root = corpus_root();
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

/// XML-часть определяется по имени целиком, а не по `Path::extension`.
fn is_xml_part(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".xml", ".rels", ".vml"].iter().any(|e| lower.ends_with(e))
}

/// Позиция первого различия и окрестность обеих версий.
fn diff_report(doc: &Document, want: &[u8], got: &[u8]) -> String {
    let at = want
        .iter()
        .zip(got.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(want.len().min(got.len()));
    let lo = at.saturating_sub(48);
    let hi_w = (at + 48).min(want.len());
    let hi_g = (at + 48).min(got.len());
    let node = doc
        .node_at(at.min(u32::MAX as usize) as u32)
        .map_or_else(|| "?".to_owned(), |n| doc.path(n));
    format!(
        "первое расхождение на байте {at} из {} (получено {} байт)\n\
         путь до узла: {node}\n\
         ожидалось: {:?}\n\
         получено:  {:?}",
        want.len(),
        got.len(),
        String::from_utf8_lossy(&want[lo..hi_w]),
        String::from_utf8_lossy(&got[lo..hi_g]),
    )
}

#[derive(Default)]
struct Stats {
    parts: usize,
    ok: usize,
    with_bom: usize,
    with_crlf: usize,
    with_lone_cr: usize,
    nodes: u64,
    bytes: u64,
    biggest: usize,
    biggest_name: String,
    biggest_nodes: usize,
    biggest_mem: usize,
    biggest_parts: Vec<(&'static str, usize)>,
}

/// В части есть `\r`, за которым НЕ идёт `\n`.
fn has_lone_cr(src: &[u8]) -> bool {
    src.iter()
        .enumerate()
        .any(|(i, &b)| b == b'\r' && src.get(i + 1) != Some(&b'\n'))
}

#[test]
fn every_xml_part_round_trips_byte_for_byte() {
    let pkgs = packages();
    if pkgs.is_empty() {
        eprintln!(
            "ПРЕДУПРЕЖДЕНИЕ: корпус не найден в {:?} — тест пропущен (задайте OOXML_CORPUS)",
            corpus_root()
        );
        return;
    }

    let limits = Limits::strict();
    let mut st = Stats::default();
    let mut failures: Vec<String> = Vec::new();

    for pkg in &pkgs {
        let data = std::fs::read(pkg).unwrap();
        let name = pkg.file_name().unwrap().to_string_lossy().into_owned();
        let zip = ZipArchive::parse(&data, &limits)
            .unwrap_or_else(|e| panic!("{name}: ZIP не разобрался: {e}"));

        for i in 0..zip.entries().len() {
            let part = zip.name_str(i).unwrap().into_owned();
            if !is_xml_part(&part) {
                continue;
            }
            let src = match zip.decompress(i, &limits) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("{name}!{part}: распаковка: {e}"));
                    continue;
                }
            };

            st.parts += 1;
            st.bytes += src.len() as u64;
            if src.starts_with(&[0xEF, 0xBB, 0xBF]) {
                st.with_bom += 1;
            }
            if src.windows(2).any(|w| w == b"\r\n") {
                st.with_crlf += 1;
            }
            if has_lone_cr(&src) {
                st.with_lone_cr += 1;
            }

            let doc = match Document::parse(src.clone(), &limits) {
                Ok(d) => d,
                Err(e) => {
                    failures.push(format!("{name}!{part}: разбор: {e}"));
                    continue;
                }
            };
            if let Err(b) = doc.check_coverage() {
                failures.push(format!("{name}!{part}: инвариант покрытия: {b:?}"));
                continue;
            }
            st.nodes += doc.node_count() as u64;
            if src.len() > st.biggest {
                st.biggest = src.len();
                st.biggest_name = format!("{name}!{part}");
                st.biggest_nodes = doc.node_count();
                st.biggest_mem = doc.memory_bytes();
                st.biggest_parts = doc.memory_breakdown().to_vec();
            }

            match doc.serialize() {
                Ok(back) if back == src => st.ok += 1,
                Ok(back) => {
                    failures.push(format!("{name}!{part}: {}", diff_report(&doc, &src, &back)));
                }
                Err(e) => failures.push(format!("{name}!{part}: запись: {e}")),
            }
        }
    }

    eprintln!("--- гейт M6: preserving DOM на реальном корпусе ------------------");
    eprintln!("пакетов:                  {}", pkgs.len());
    eprintln!("побайтовый round-trip:    {}/{}", st.ok, st.parts);
    eprintln!("суммарно байт:            {}", st.bytes);
    eprintln!("узлов дерева всего:       {}", st.nodes);
    eprintln!("частей с BOM:             {}", st.with_bom);
    eprintln!("частей с CRLF:            {}", st.with_crlf);
    eprintln!("частей с одиночным CR:    {}", st.with_lone_cr);
    eprintln!(
        "самая большая часть:      {} — {} байт, {} узлов, дерево ~{} КиБ",
        st.biggest_name,
        st.biggest,
        st.biggest_nodes,
        st.biggest_mem / 1024
    );
    for (what, bytes) in &st.biggest_parts {
        eprintln!("    {what:<12} {} КиБ", bytes / 1024);
    }
    eprintln!(
        "size_of::<Node>():        {}",
        size_of::<ooxml::dom::Node>()
    );
    eprintln!("-----------------------------------------------------------------");

    assert!(
        failures.is_empty(),
        "round-trip нарушен на {} частях из {}:\n{}",
        failures.len(),
        st.parts,
        failures.join("\n---\n")
    );
    assert!(st.parts > 0, "не найдено ни одной XML-части");
    assert_eq!(st.ok, st.parts, "гейт M6 не пройден");
}

/// Второе свойство гейта: правка одного атрибута меняет **один** регион.
///
/// Проверяется не «файл похож», а строго: общий префикс и общий суффикс с
/// оригиналом обязаны покрыть всё, кроме одного участка, длина которого
/// сопоставима с длиной нового значения. Если сериализатор где-то нормализует
/// пробел или кавычку, этот тест падает даже там, где round-trip чистого
/// документа проходит.
#[test]
fn one_attribute_edit_touches_one_region() {
    let pkgs = packages();
    if pkgs.is_empty() {
        eprintln!("ПРЕДУПРЕЖДЕНИЕ: корпус не найден — тест пропущен");
        return;
    }

    let limits = Limits::strict();
    let mut checked = 0usize;
    let mut worst = 0usize;

    for pkg in pkgs.iter().take(12) {
        let data = std::fs::read(pkg).unwrap();
        let name = pkg.file_name().unwrap().to_string_lossy().into_owned();
        let zip = ZipArchive::parse(&data, &limits).unwrap();

        for i in 0..zip.entries().len() {
            let part = zip.name_str(i).unwrap().into_owned();
            if !is_xml_part(&part) {
                continue;
            }
            let Ok(src) = zip.decompress(i, &limits) else {
                continue;
            };
            let mut doc = Document::parse(src.clone(), &limits).unwrap();

            // Правится корневой элемент: он есть всегда, и он же — худший
            // случай, потому что после него лежит весь остальной файл.
            let root = doc.root_element().unwrap();
            // Имя без префикса: любой префикс пришлось бы сначала объявить, а
            // это была бы уже не «одна правка».
            doc.set_attr(root, "rwProbe", "значение с \" и & и \r")
                .unwrap();
            let out = doc.serialize().unwrap();

            let pre = common_prefix(&src, &out);
            let suf = common_suffix(&src[pre..], &out[pre..]);
            let changed_src = src.len() - pre - suf;
            let changed_out = out.len() - pre - suf;

            assert!(
                changed_src == 0,
                "{name}!{part}: правка съела {changed_src} исходных байт, \
                 а должна была только вставить"
            );
            assert!(
                changed_out < 128,
                "{name}!{part}: вставлено {changed_out} байт вместо одного атрибута"
            );
            worst = worst.max(changed_out);

            // И, разумеется, файл обязан остаться разбираемым, а повторный
            // проход — неподвижной точкой.
            let again = Document::parse(out.clone(), &limits).unwrap();
            assert_eq!(
                again.serialize().unwrap(),
                out,
                "{name}!{part}: не идемпотентно"
            );
            checked += 1;
        }
    }

    eprintln!(
        "точечность правки проверена на {checked} частях; максимальный изменённый регион — {worst} байт"
    );
    assert!(checked > 50, "проверено слишком мало частей: {checked}");
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}
