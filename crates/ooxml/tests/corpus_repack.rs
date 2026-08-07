//! Блокирующий гейт вехи M3: `repack_all_verbatim(f) == f` побайтово.
//!
//! Пока этот тест не даёт 43/43, следующие вехи не начинаются. Достижимость
//! доказана независимо: корпус был разобран в ту же модель и собран обратно с
//! пересчётом единственного поля `relative offset of local header` — 43/43
//! (`docs/zip-fidelity.md`, §0). Значит, любое расхождение здесь — дефект
//! writer'а или неполнота модели M2, а не «формат такой».
//!
//! Поэтому при расхождении тест не ограничивается «не совпало»: он печатает
//! позицию первого разошедшегося байта, старое и новое значения и **структуру,
//! которой этот байт принадлежит** — какая запись, какой заголовок, какое поле.
//! Без этого отладка байтовой идентичности превращается в гадание.
//!
//! Каталога корпуса нет — предупреждение и проход: корпус состоит из реальных
//! документов и в репозиторий может не попасть.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};

use ooxml::Limits;
use ooxml::zip::{RawEntry, ZipArchive, repack_all_verbatim};

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

// ---------------------------------------------------------------- диагностика

/// Поле фиксированной части локального заголовка по смещению внутри него.
fn local_field(rel: u32) -> &'static str {
    match rel {
        0..=3 => "signature PK\\x03\\x04",
        4..=5 => "version needed",
        6..=7 => "gp flags",
        8..=9 => "method",
        10..=11 => "dos time",
        12..=13 => "dos date",
        14..=17 => "crc32",
        18..=21 => "compressed size",
        22..=25 => "uncompressed size",
        26..=27 => "name len",
        28..=29 => "extra len",
        _ => "name / local extra",
    }
}

/// Поле фиксированной части записи каталога по смещению внутри неё.
fn cd_field(rel: u32) -> &'static str {
    match rel {
        0..=3 => "signature PK\\x01\\x02",
        4..=5 => "version made by",
        6..=7 => "version needed",
        8..=9 => "gp flags",
        10..=11 => "method",
        12..=13 => "dos time",
        14..=15 => "dos date",
        16..=19 => "crc32",
        20..=23 => "compressed size",
        24..=27 => "uncompressed size",
        28..=29 => "name len",
        30..=31 => "extra len",
        32..=33 => "comment len",
        34..=35 => "disk number start",
        36..=37 => "internal attrs",
        38..=41 => "external attrs",
        42..=45 => "relative offset of local header (ЕДИНСТВЕННОЕ пересчитываемое поле)",
        _ => "name / cd extra / comment",
    }
}

/// Поле EOCD по смещению внутри него.
fn eocd_field(rel: u32) -> &'static str {
    match rel {
        0..=3 => "signature PK\\x05\\x06",
        4..=5 => "disk number",
        6..=7 => "cd start disk",
        8..=9 => "entries on this disk",
        10..=11 => "total entries",
        12..=15 => "cd size",
        16..=19 => "cd offset",
        20..=21 => "comment len",
        _ => "archive comment",
    }
}

/// Где в структуре архива находится байт с офсетом `at`.
///
/// Ответ строится по спанам модели, а не по повторному разбору байт: если
/// разошёлся именно спан, увидеть это надо здесь.
fn describe(z: &ZipArchive<'_>, at: u32) -> String {
    let inside = |s: ooxml::bytes::Span| !s.is_empty() && at >= s.start() && at < s.end();

    if inside(z.prefix()) {
        return format!("префикс архива, +{}", at - z.prefix().start());
    }
    for (i, e) in z.entries().iter().enumerate() {
        let name = String::from_utf8_lossy(e.name_cd.slice(z.src()).unwrap_or(b"?")).into_owned();
        if inside(e.gap_before) {
            return format!("зазор перед записью #{i} ({name})");
        }
        if inside(e.local_header) {
            let rel = at - e.local_header.start();
            return format!(
                "локальный заголовок записи #{i} ({name}), +{rel}: {}",
                local_field(rel)
            );
        }
        if inside(e.data) {
            return format!(
                "сжатые данные записи #{i} ({name}), +{} из {}",
                at - e.data.start(),
                e.data.len()
            );
        }
        if let Some(d) = e.descriptor
            && inside(d.span)
        {
            return format!(
                "data descriptor записи #{i} ({name}), +{}",
                at - d.span.start()
            );
        }
        if inside(e.cd_record) {
            let rel = at - e.cd_record.start();
            return format!("запись каталога #{i} ({name}), +{rel}: {}", cd_field(rel));
        }
    }
    if inside(z.gap_before_cd()) {
        return "зазор между последней записью и каталогом".to_owned();
    }
    if inside(z.gap_after_cd()) {
        return "зазор между каталогом и хвостом".to_owned();
    }
    if let Some(zz) = z.eocd().zip64
        && inside(zz.span)
    {
        return format!("zip64 EOCD record, +{}", at - zz.span.start());
    }
    if let Some(l) = z.eocd().locator
        && inside(l.span)
    {
        return format!("zip64 EOCD locator, +{}", at - l.span.start());
    }
    if inside(z.eocd().span) {
        let rel = at - z.eocd().span.start();
        return format!("EOCD, +{rel}: {}", eocd_field(rel));
    }
    if inside(z.trailing()) {
        return format!("байты после EOCD, +{}", at - z.trailing().start());
    }
    "вне известных спанов модели — байт потерян или добавлен".to_owned()
}

/// Короткий hex-контекст вокруг позиции.
fn context(buf: &[u8], at: usize) -> String {
    let from = at.saturating_sub(8);
    let to = (at + 9).min(buf.len());
    buf[from..to]
        .iter()
        .enumerate()
        .map(|(k, b)| {
            if from + k == at {
                format!("[{b:02X}]")
            } else {
                format!(" {b:02X} ")
            }
        })
        .collect()
}

/// Первый разошедшийся байт с полным описанием места.
fn first_diff(z: &ZipArchive<'_>, want: &[u8], got: &[u8], file: &str) -> Option<String> {
    let n = want.len().min(got.len());
    let at = (0..n).find(|&i| want[i] != got[i]);
    match at {
        Some(i) => Some(format!(
            "{file}: первое расхождение на байте {i} (0x{i:X}) из {}\n  \
             было 0x{:02X}, стало 0x{:02X}\n  \
             место: {}\n  \
             ожидалось: {}\n  \
             получено : {}",
            want.len(),
            want[i],
            got[i],
            describe(z, i as u32),
            context(want, i),
            context(got, i),
        )),
        None if want.len() != got.len() => Some(format!(
            "{file}: байты совпали на всей общей длине, но длина разошлась: \
             было {}, стало {} (разница {}). Место обрыва: {}",
            want.len(),
            got.len(),
            got.len() as i64 - want.len() as i64,
            describe(z, n as u32),
        )),
        None => None,
    }
}

// ---------------------------------------------------------------------- тесты

#[test]
fn repack_all_verbatim_is_byte_identical_on_whole_corpus() {
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

    let limits = Limits::permissive();
    let mut ok = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("не прочитан {name}: {e}"));
        let z = ZipArchive::parse(&bytes, &limits)
            .unwrap_or_else(|e| panic!("не разобран {name}: {e}"));
        let out = repack_all_verbatim(&z).unwrap_or_else(|e| panic!("не пересобран {name}: {e}"));

        match first_diff(&z, &bytes, &out, &name) {
            None => ok += 1,
            Some(report) => failures.push(report),
        }
    }

    println!("\n=== M3: repack_all_verbatim ===");
    println!("{ok}/{} файлов побайтово идентичны", files.len());
    for f in &failures {
        println!("\n{f}");
    }
    assert!(
        failures.is_empty(),
        "{}/{} файлов разошлись — см. вывод выше",
        failures.len(),
        files.len()
    );
    assert_eq!(files.len(), 43, "ожидалось 43 файла корпуса");
    assert_eq!(ok, 43, "гейт M3 требует 43/43");
}

/// Совпадение байт может быть случайным, если сломаны и разбор, и сборка
/// одинаковым образом. Поэтому результат ещё раз разбирается и сравнивается
/// с исходной моделью поле за полем.
#[test]
fn reparsing_the_repack_gives_the_same_model() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("ВНИМАНИЕ: корпус не найден в {dir:?}; тест пропущен.");
        return;
    }
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    if files.is_empty() {
        return;
    }
    let limits = Limits::permissive();
    let mut checked = 0usize;

    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        let bytes = std::fs::read(path).unwrap();
        let a = ZipArchive::parse(&bytes, &limits).unwrap();
        let out = repack_all_verbatim(&a).unwrap();
        let b = ZipArchive::parse(&out, &limits)
            .unwrap_or_else(|e| panic!("{name}: пересобранный архив не разбирается: {e}"));

        assert_eq!(a.len(), b.len(), "{name}: число записей");
        assert_eq!(
            a.order_by_offset(),
            b.order_by_offset(),
            "{name}: физический порядок записей"
        );
        assert_eq!(
            a.offset_delta(),
            b.offset_delta(),
            "{name}: поправка офсетов"
        );
        assert_eq!(a.prefix().len(), b.prefix().len(), "{name}: длина префикса");

        let ea = a.eocd();
        let eb = b.eocd();
        assert_eq!(ea.disk_number, eb.disk_number, "{name}: EOCD disk number");
        assert_eq!(ea.cd_start_disk, eb.cd_start_disk, "{name}: EOCD cd disk");
        assert_eq!(
            ea.entries_this_disk, eb.entries_this_disk,
            "{name}: EOCD entries this disk"
        );
        assert_eq!(
            ea.entries_total, eb.entries_total,
            "{name}: EOCD total entries"
        );
        assert_eq!(ea.cd_size, eb.cd_size, "{name}: EOCD cd size");
        assert_eq!(ea.cd_offset, eb.cd_offset, "{name}: EOCD cd offset");
        assert_eq!(
            ea.comment.slice(a.src()),
            eb.comment.slice(b.src()),
            "{name}: комментарий архива"
        );

        for i in 0..a.len() {
            let x = a.entry(i).unwrap();
            let y = b.entry(i).unwrap();
            assert_same_entry(x, a.src(), y, b.src(), &name, i);
            assert_eq!(
                a.raw_data(i).unwrap(),
                b.raw_data(i).unwrap(),
                "{name}: запись #{i} — сжатые данные"
            );
        }
        checked += 1;
    }
    println!("модель совпала после пересборки у {checked} файлов");
}

#[allow(clippy::too_many_arguments)]
fn assert_same_entry(x: &RawEntry, xs: &[u8], y: &RawEntry, ys: &[u8], file: &str, i: usize) {
    macro_rules! same {
        ($f:ident) => {
            assert_eq!(x.$f, y.$f, "{file}: запись #{i} — поле {}", stringify!($f));
        };
    }
    same!(version_made_by);
    same!(version_needed_cd);
    same!(version_needed_local);
    same!(flags);
    same!(method);
    same!(dos_time);
    same!(dos_date);
    same!(crc32);
    same!(comp_size);
    same!(uncomp_size);
    same!(flags_local);
    same!(method_local);
    same!(dos_time_local);
    same!(dos_date_local);
    same!(crc32_local);
    same!(comp_size_local);
    same!(uncomp_size_local);
    same!(internal_attrs);
    same!(external_attrs);
    same!(disk_start);
    same!(local_header_off);
    same!(zip64_layout);
    // Дескриптор сравнивается по форме и длине: спаны абсолютны и совпасть
    // не обязаны, а вот «сигнатура была — сигнатура осталась» обязано.
    assert_eq!(
        x.descriptor
            .map(|d| (d.has_signature, d.wide, d.span.len())),
        y.descriptor
            .map(|d| (d.has_signature, d.wide, d.span.len())),
        "{file}: запись #{i} — форма data descriptor'а"
    );

    assert_eq!(
        x.name_cd.slice(xs),
        y.name_cd.slice(ys),
        "{file}: запись #{i} — имя в каталоге"
    );
    assert_eq!(
        x.name_local.slice(xs),
        y.name_local.slice(ys),
        "{file}: запись #{i} — имя в локальном заголовке"
    );
    assert_eq!(
        x.local_extra.slice(xs),
        y.local_extra.slice(ys),
        "{file}: запись #{i} — local extra"
    );
    assert_eq!(
        x.cd_extra.slice(xs),
        y.cd_extra.slice(ys),
        "{file}: запись #{i} — cd extra"
    );
    assert_eq!(
        x.comment.slice(xs),
        y.comment.slice(ys),
        "{file}: запись #{i} — комментарий записи"
    );
    assert_eq!(
        x.gap_before.len(),
        y.gap_before.len(),
        "{file}: запись #{i} — длина зазора перед заголовком"
    );
}
