//! Полный конвейер всех слоёв на реальных файлах.
//!
//! Каждая веха проверялась в одиночку. Здесь они соединяются:
//! `zip → inflate → dom → serialize → deflate → zip`. Ошибка, которую ловит
//! только такой тест, — рассогласование на стыке: слой A отдаёт то, что слой B
//! принимает иначе, хотя по отдельности оба корректны.
//!
//! Это же предусловие для вехи M9 (правка ячейки): там ровно эта цепочка, но с
//! изменением в середине.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::deflate::{Level, deflate, inflate};
use ooxml::dom::Document;
use ooxml::zip::ZipArchive;
use std::path::PathBuf;

fn corpus_files() -> Vec<PathBuf> {
    let root = std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    );
    let mut out = Vec::new();
    for sub in ["xlsx", "docx"] {
        let Ok(entries) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "xlsx" || x == "docx") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Является ли часть XML.
///
/// Сравнение по имени целиком, а не по `Path::extension`: для `_rels/.rels`
/// расширения нет вовсе, и по одной части на пакет тихо выпадала бы из проверки.
fn is_xml_part(name: &str) -> bool {
    name.ends_with(".xml") || name.ends_with(".rels") || name.ends_with(".vml")
}

#[test]
fn every_xml_part_survives_the_whole_stack() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен (задайте OOXML_CORPUS)");
        return;
    }

    let limits = Limits::strict();
    let (mut parts, mut bytes_in, mut ours, mut theirs) = (0u32, 0u64, 0u64, 0u64);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg = path.file_name().unwrap().to_string_lossy().into_owned();
        let zip = ZipArchive::parse(&data, &limits).unwrap();

        for i in 0..zip.entries().len() {
            let name = zip.name_str(i).unwrap().into_owned();
            if !is_xml_part(&name) {
                continue;
            }

            // zip -> inflate
            let plain = zip
                .decompress(i, &limits)
                .unwrap_or_else(|e| panic!("{pkg}!{name}: распаковка: {e}"));

            // inflate -> dom -> serialize
            let doc = Document::parse(plain.clone(), &limits)
                .unwrap_or_else(|e| panic!("{pkg}!{name}: разбор XML: {e}"));
            let back = doc
                .serialize()
                .unwrap_or_else(|e| panic!("{pkg}!{name}: сериализация: {e}"));
            assert_eq!(back, plain, "{pkg}!{name}: DOM не восстановил байты");

            // serialize -> deflate -> inflate
            let packed = deflate(&back, Level::Default);
            let unpacked = inflate(&packed, &limits)
                .unwrap_or_else(|e| panic!("{pkg}!{name}: наш deflate не читается назад: {e}"));
            assert_eq!(
                unpacked, plain,
                "{pkg}!{name}: круг через наш упаковщик потерял данные"
            );

            parts += 1;
            bytes_in += plain.len() as u64;
            ours += packed.len() as u64;
            theirs += zip.entries()[i].comp_size;
        }
    }

    println!("полный конвейер zip -> inflate -> dom -> deflate -> inflate:");
    println!("  пакетов: {}", files.len());
    println!("  XML-частей: {parts}");
    println!("  исходных байт: {bytes_in}");
    println!(
        "  их сжатие: {theirs}, наше: {ours} ({:.4}x)",
        ours as f64 / theirs as f64
    );

    assert!(parts > 500, "ожидалось ~570 частей, получено {parts}");
}

#[test]
fn dom_round_trip_does_not_disturb_the_archive() {
    // Ключевое свойство для вехи M9: если правки не было, часть обязана
    // деградировать обратно в verbatim-копию. Здесь это проверяется на уровне
    // байт — прогон через DOM не должен менять НИЧЕГО в архиве.
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let limits = Limits::strict();
    let mut checked = 0u32;

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let zip = ZipArchive::parse(&data, &limits).unwrap();

        for i in 0..zip.entries().len() {
            let name = zip.name_str(i).unwrap().into_owned();
            if !is_xml_part(&name) {
                continue;
            }
            let plain = zip.decompress(i, &limits).unwrap();
            let doc = Document::parse(plain.clone(), &limits).unwrap();

            assert!(
                !doc.is_dirty(),
                "{name}: свежеразобранный документ не может быть грязным"
            );
            assert_eq!(
                doc.serialize().unwrap(),
                plain,
                "{name}: чистый документ обязан отдавать исходные байты"
            );
            checked += 1;
        }
    }
    println!("частей, признавших себя чистыми после разбора: {checked}");
    assert!(checked > 500);
}
