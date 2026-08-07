//! Разбор реального корпуса — критерий готовности вехи M2.
//!
//! Корпус — 43 файла от трёх producer'ов (MS Office, Google Sheets,
//! LibreOffice) плюс несколько «прочих». Он существует потому, что APPNOTE
//! описывает формат, а не то, как его пишут на практике; все нетривиальные
//! решения в модели [`RawEntry`] взяты не из спецификации, а отсюда.
//!
//! Тест проверяет не «разобралось без ошибки», а два более сильных свойства:
//!
//! 1. **Полнота покрытия.** Спаны модели обязаны замостить файл целиком, без
//!    дыр и наложений: от нулевого байта до последнего. Байт, не попавший ни в
//!    один спан, — это байт, который веха M3 потеряет.
//! 2. **Наличие измеренных особенностей.** Если модель перестанет различать
//!    локальный и каталожный extra, все файлы всё равно «разберутся» — просто
//!    молча потеряв 520 байт на записи. Поэтому особенности проверяются
//!    поимённо.
//!
//! Каталога нет — тест печатает предупреждение и проходит: корпус состоит из
//! реальных документов и в репозиторий может не попасть.
//!
//! [`RawEntry`]: ooxml::zip::RawEntry

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ooxml::Limits;
use ooxml::bytes::Span;
use ooxml::zip::ZipArchive;

/// Путь по умолчанию — абсолютный: корпус лежит вне worktree агентов.
const DEFAULT_CORPUS: &str = "/Users/shakh/rustoword/crates/ooxml/tests/corpus";

fn corpus_dir() -> PathBuf {
    std::env::var("OOXML_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CORPUS))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "xlsx" || x == "docx") {
            out.push(p);
        }
    }
}

/// Проверяет, что спаны модели замостили файл целиком.
///
/// Это и есть предусловие вехи M3: `repack_all_verbatim(f) == f` возможен
/// ровно тогда, когда каждый байт входа принадлежит ровно одному спану.
fn assert_covers_whole_file(z: &ZipArchive<'_>, len: usize, what: &str) {
    let mut regions: Vec<(u32, u32, &'static str)> = Vec::new();
    let push = |s: Span, tag: &'static str, regions: &mut Vec<(u32, u32, &'static str)>| {
        if !s.is_empty() {
            regions.push((s.start(), s.end(), tag));
        }
    };
    push(z.prefix(), "prefix", &mut regions);
    for e in z.entries() {
        push(e.gap_before, "gap", &mut regions);
        push(e.verbatim(), "entry", &mut regions);
        push(e.cd_record, "cd", &mut regions);
    }
    push(z.gap_before_cd(), "gap-before-cd", &mut regions);
    push(z.gap_after_cd(), "gap-after-cd", &mut regions);
    if let Some(zz) = z.eocd().zip64 {
        push(zz.span, "zip64-eocd", &mut regions);
    }
    if let Some(l) = z.eocd().locator {
        push(l.span, "zip64-locator", &mut regions);
    }
    push(z.eocd().span, "eocd", &mut regions);
    push(z.trailing(), "trailing", &mut regions);

    regions.sort_unstable();
    let mut cursor = 0u32;
    for &(start, end, tag) in &regions {
        assert!(
            start >= cursor,
            "{what}: наложение спанов на {start} ({tag}), курсор {cursor}"
        );
        assert!(
            start == cursor,
            "{what}: не покрыты байты {cursor}..{start} перед {tag}"
        );
        cursor = end;
    }
    assert_eq!(
        cursor as usize, len,
        "{what}: хвост {cursor}..{len} не покрыт ни одним спаном"
    );
}

/// Строка манифеста: имя файла и заявленное число записей.
fn manifest(dir: &Path) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join("MANIFEST.tsv")) else {
        return out;
    };
    for line in text.lines().skip(1) {
        let mut cols = line.split('\t');
        let (Some(name), Some(_sha), Some(_producer), Some(entries)) =
            (cols.next(), cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        if let Ok(n) = entries.parse::<usize>() {
            out.insert(name.to_owned(), n);
        }
    }
    out
}

fn bump<K: Ord>(m: &mut BTreeMap<K, usize>, k: K) {
    *m.entry(k).or_insert(0) += 1;
}

#[test]
fn corpus_parses_and_model_captures_measured_quirks() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!(
            "ВНИМАНИЕ: корпус не найден в {dir:?}; тест пропущен. Путь задаётся OOXML_CORPUS."
        );
        return;
    }
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    if files.is_empty() {
        println!("ВНИМАНИЕ: в {dir:?} нет .xlsx/.docx; тест пропущен.");
        return;
    }
    let manifest = manifest(&dir);

    // Лимиты ослаблены только по числу записей и размеру входа: корпус
    // реальный, и упереться в квоту здесь означало бы проверить квоту, а не
    // разбор.
    let limits = Limits::permissive();

    // --- накопители статистики ---
    let mut made_by: BTreeMap<u16, usize> = BTreeMap::new();
    let mut flags: BTreeMap<u16, usize> = BTreeMap::new();
    let mut methods: BTreeMap<u16, usize> = BTreeMap::new();
    let mut local_extra_len: BTreeMap<u32, usize> = BTreeMap::new();
    let mut cd_extra_len: BTreeMap<u32, usize> = BTreeMap::new();
    let mut extra_ids: BTreeMap<u16, usize> = BTreeMap::new();
    let mut vneed_differs = 0usize;
    let mut extra_differs = 0usize;
    let mut total_entries = 0usize;
    let mut gaps = 0usize;
    let mut nonzero_offset_delta = 0usize;
    let mut archive_comments = 0usize;
    let mut cd_order_is_offset_order = 0usize;

    // --- флаги «особенность найдена» ---
    let mut found_office_extra: Option<String> = None;
    let mut found_made_by_45 = None;
    let mut found_made_by_788 = None;
    let mut found_streaming_descriptor: Option<String> = None;
    let mut found_stored = None;
    let mut found_dir_entry = None;
    let mut found_internal_1 = None;
    let mut found_zip64 = None;

    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("не прочитан {name}: {e}"));
        let z = ZipArchive::parse(&bytes, &limits)
            .unwrap_or_else(|e| panic!("не разобран {name}: {e}"));

        assert_covers_whole_file(&z, bytes.len(), &name);

        if let Some(&want) = manifest.get(&name) {
            assert_eq!(
                z.len(),
                want,
                "{name}: число записей разошлось с манифестом"
            );
        }
        if z.offset_delta() != 0 {
            nonzero_offset_delta += 1;
        }
        if !z.eocd().comment.is_empty() {
            archive_comments += 1;
        }
        let ordered: Vec<u32> = (0..z.len() as u32).collect();
        if z.order_by_offset() == ordered.as_slice() {
            cd_order_is_offset_order += 1;
        }

        for i in 0..z.len() {
            let e = z.entry(i).unwrap();
            total_entries += 1;
            bump(&mut made_by, e.version_made_by);
            bump(&mut flags, e.flags);
            bump(&mut methods, e.method);
            bump(&mut local_extra_len, e.local_extra.len());
            bump(&mut cd_extra_len, e.cd_extra.len());
            for f in ooxml::zip::ExtraFields::new(z.src(), e.local_extra) {
                bump(&mut extra_ids, f.id);
            }
            for f in ooxml::zip::ExtraFields::new(z.src(), e.cd_extra) {
                bump(&mut extra_ids, f.id);
            }
            if e.version_needed_local != e.version_needed_cd {
                vneed_differs += 1;
            }
            if e.local_extra.len() != e.cd_extra.len() {
                extra_differs += 1;
            }
            if !e.gap_before.is_empty() {
                gaps += 1;
            }

            // Имя обязано совпадать в обоих заголовках — иначе parse бы упал,
            // но проверим и здесь: это инвариант, а не деталь разбора.
            assert_eq!(
                e.name_cd.slice(z.src()),
                e.name_local.slice(z.src()),
                "{name}: имена заголовков разошлись"
            );

            if e.local_extra.len() == 520 && e.cd_extra.is_empty() {
                found_office_extra.get_or_insert_with(|| name.clone());
            }
            if e.version_made_by == 45 {
                found_made_by_45.get_or_insert_with(|| name.clone());
            }
            if e.version_made_by == 788 {
                found_made_by_788.get_or_insert_with(|| name.clone());
            }
            if e.flags & 0x0808 == 0x0808 {
                let d = e
                    .descriptor
                    .unwrap_or_else(|| panic!("{name}: bit 3 без дескриптора"));
                assert!(
                    d.has_signature,
                    "{name}: дескриптор Google Sheets обязан нести PK\\x07\\x08"
                );
                assert!(!d.wide, "{name}: запись не zip64, дескриптор 32-битный");
                assert_eq!(d.span.len(), 16, "{name}: дескриптор PK\\x07\\x08 + 3×u32");
                assert_eq!(e.crc32_local, 0, "{name}: при bit 3 crc локального нулевой");
                assert_eq!(e.comp_size_local, 0);
                assert_eq!(e.uncomp_size_local, 0);
                found_streaming_descriptor.get_or_insert_with(|| name.clone());
            }
            if e.method == 0 && !e.is_dir(z.src()) {
                found_stored.get_or_insert_with(|| name.clone());
            }
            if e.is_dir(z.src()) {
                assert_eq!(e.uncomp_size, 0, "{name}: каталог с ненулевым размером");
                found_dir_entry.get_or_insert_with(|| name.clone());
            }
            if e.internal_attrs == 1 {
                found_internal_1.get_or_insert_with(|| name.clone());
            }
            if e.zip64_layout.is_some() {
                found_zip64.get_or_insert_with(|| name.clone());
            }
        }
    }

    // ------------------------------------------------------- статистика ---
    println!(
        "\n=== Корпус: {} файлов, {total_entries} записей ===",
        files.len()
    );
    println!("version made by : {made_by:?}");
    println!("flags           : {:?}", hex_keys(&flags));
    println!("method          : {methods:?}");
    println!("local extra len : {local_extra_len:?}");
    println!("cd extra len    : {cd_extra_len:?}");
    println!("extra header id : {:?}", hex_keys(&extra_ids));
    println!("записей, у которых local extra != cd extra по длине: {extra_differs}");
    println!("записей, у которых version needed local != cd      : {vneed_differs}");
    println!("записей с непустой дырой перед заголовком          : {gaps}");
    println!("файлов с ненулевой поправкой офсетов (префикс)     : {nonzero_offset_delta}");
    println!("файлов с непустым комментарием архива              : {archive_comments}");
    println!(
        "файлов, где порядок каталога совпал с физическим   : {cd_order_is_offset_order} из {}",
        files.len()
    );
    println!("zip64 встретился в                                 : {found_zip64:?}");

    // ------------------------------------------------- проверки модели ----
    assert_eq!(files.len(), 43, "ожидалось 43 файла корпуса");
    assert_eq!(
        files.len(),
        manifest.len().max(files.len()),
        "манифест и каталог разошлись"
    );
    let office = found_office_extra.expect(
        "не найдено записи с local extra = 520 и cd extra = 0 — \
         модель не различает области extra двух заголовков",
    );
    println!("\n520-байтовый local extra при пустом cd extra: {office}");
    println!(
        "version made by = 45  : {}",
        found_made_by_45.expect("нет записи с made by 45")
    );
    println!(
        "version made by = 788 : {}",
        found_made_by_788.expect("нет записи с made by 788")
    );
    println!(
        "дескриптор 16 байт с сигнатурой (flags 0x0808): {}",
        found_streaming_descriptor.expect("нет записи с flags & 0x0808")
    );
    println!(
        "stored (method 0)     : {}",
        found_stored.expect("нет stored-записи")
    );
    println!(
        "directory-запись      : {}",
        found_dir_entry.expect("нет записи-каталога")
    );
    println!(
        "internal attrs = 1    : {}",
        found_internal_1.expect("нет записи с internal attrs = 1")
    );

    assert!(
        methods.contains_key(&0) && methods.contains_key(&8),
        "в корпусе должны быть оба метода"
    );
    assert!(
        local_extra_len.contains_key(&520),
        "в корпусе должен быть local extra в 520 байт"
    );
    assert!(
        extra_ids.contains_key(&0xA220),
        "0xA220 (Open Packaging Growth Hint) обязан встретиться"
    );
    assert!(
        extra_differs > 0,
        "если local и cd extra нигде не разошлись — проверять раздельное \
         хранение бессмысленно, значит корпус подменили"
    );
}

fn hex_keys(m: &BTreeMap<u16, usize>) -> BTreeMap<String, usize> {
    m.iter().map(|(k, v)| (format!("0x{k:04X}"), *v)).collect()
}
