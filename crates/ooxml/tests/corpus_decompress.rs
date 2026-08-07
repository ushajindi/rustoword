//! Стык слоёв `zip` и `deflate` на реальных файлах.
//!
//! Вехи M1 и M2 писались параллельно, поэтому `ZipArchive::decompress` — код,
//! который у M2 не было возможности проверить: на момент его написания `inflate`
//! был заглушкой. Здесь этот пробел закрывается.
//!
//! Проверка не дублирует `corpus_inflate.rs`: там центральный каталог
//! разбирался отдельным минимальным кодом внутри теста, здесь — настоящим
//! `ZipArchive`. Расхождение между этими двумя разборами и есть то, что тест
//! ловит: неверный офсет данных, неверную длину, перепутанный источник crc
//! у записей с data descriptor.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ooxml::Limits;
use ooxml::zip::ZipArchive;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    )
}

fn corpus_files() -> Vec<PathBuf> {
    let root = corpus_dir();
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

#[test]
fn zip_layer_and_deflate_layer_agree_on_every_entry() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!(
            "корпус не найден в {} — тест пропущен (задайте OOXML_CORPUS)",
            corpus_dir().display()
        );
        return;
    }

    let limits = Limits::strict();
    let (mut entries, mut deflated, mut stored, mut bytes) = (0u32, 0u32, 0u32, 0u64);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        let zip = ZipArchive::parse(&data, &limits)
            .unwrap_or_else(|e| panic!("{name}: разбор провалился: {e}"));

        for i in 0..zip.entries().len() {
            let entry = &zip.entries()[i];
            let part = zip.name_str(i).unwrap().into_owned();

            // Каталоги не несут данных — у них нулевой размер и нечего сверять.
            if entry.uncomp_size == 0 && part.ends_with('/') {
                continue;
            }

            // `decompress` сверяет CRC и размер сам, поэтому успешный возврат
            // уже означает совпадение с тем, что записал Office.
            let out = zip
                .decompress(i, &limits)
                .unwrap_or_else(|e| panic!("{name}!{part}: {e}"));

            assert_eq!(
                out.len() as u64,
                entry.uncomp_size,
                "{name}!{part}: длина разошлась с каталогом"
            );

            entries += 1;
            bytes += out.len() as u64;
            match entry.method {
                0 => stored += 1,
                _ => deflated += 1,
            }
        }
    }

    println!("ZipArchive::decompress на реальном корпусе:");
    println!("  файлов: {}", files.len());
    println!("  записей: {entries} (deflate {deflated}, stored {stored})");
    println!("  распаковано байт: {bytes}");

    assert!(entries > 500, "ожидалось ~600 записей, получено {entries}");
}

#[test]
fn raw_data_stays_compressed() {
    // Основа сохранности: `raw_data` отдаёт ещё сжатые байты ровно той длины,
    // что записана в каталоге. Если бы он возвращал распакованное, замысел
    // «копируем не перепаковывая» рухнул бы незаметно.
    //
    // Проверять «сжатое короче исходного» можно только на XML-частях: в .docx
    // лежат уже сжатые изображения, и deflate поверх них даёт РОСТ на величину
    // накладных расходов блока. Это нормальное поведение формата, а не дефект.
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let limits = Limits::strict();
    let (mut checked, mut xml_parts, mut grew) = (0u32, 0u32, 0u32);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let zip = ZipArchive::parse(&data, &limits).unwrap();
        for i in 0..zip.entries().len() {
            let entry = &zip.entries()[i];
            if entry.method != 8 || entry.uncomp_size == 0 {
                continue;
            }
            let raw = zip.raw_data(i).unwrap();
            assert_eq!(
                raw.len() as u64,
                entry.comp_size,
                "сырой срез должен быть ровно сжатым размером"
            );
            checked += 1;

            let part = zip.name_str(i).unwrap().into_owned();
            let plain = zip.decompress(i, &limits).unwrap();
            if part.ends_with(".xml") || part.ends_with(".rels") {
                assert!(
                    raw.len() < plain.len(),
                    "{part}: XML обязан сжиматься, а тут {} -> {}",
                    plain.len(),
                    raw.len()
                );
                xml_parts += 1;
            } else if raw.len() >= plain.len() {
                grew += 1;
            }
        }
    }

    println!("deflate-записей: {checked}, из них XML: {xml_parts}");
    println!("не-XML записей, выросших после сжатия: {grew}");
    assert!(
        xml_parts > 100,
        "ожидалось много XML-частей, а их {xml_parts}"
    );
}
