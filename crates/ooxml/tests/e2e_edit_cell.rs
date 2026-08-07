//! End-to-end: правка ячейки переживает настоящий Excel-совместимый читатель.
//!
//! Это предусловие вехи M9, написанное ДО неё и намеренно **мимо** будущего
//! фасада `xlsx`: ячейка правится напрямую через `Package` + `Document`. Такой
//! тест отвечает на вопрос, который не может закрыть ни один слой по
//! отдельности — принимает ли внешний читатель файл, который мы собрали.
//!
//! Проверяются три вещи, и третья самая важная:
//!
//! 1. значение действительно изменилось при повторном открытии нами;
//! 2. **все остальные записи архива побайтово те же** — правка одной части не
//!    имеет права задеть соседей;
//! 3. LibreOffice открывает результат и видит новое значение.
//!
//! Пункт 3 помечен `#[ignore]` и включается переменной `OOXML_SOFFICE`:
//! он зависит от внешней программы и медленный.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::dom::{Document, NodeId};
use ooxml::opc::{Package, PartName};
use ooxml::zip::ZipArchive;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHEETML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// Значение-маркер: не встречается в корпусе и заметно в CSV.
const MARKER: &str = "424242.5";

fn corpus_root() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    )
}

fn xlsx_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(corpus_root().join("xlsx")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "xlsx") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Ищет первую ячейку, у которой есть значение, и возвращает её и узел `<v>`.
///
/// В реальных файлах корпуса числовых ячеек почти нет: типовые формы состоят
/// из ссылок в общую таблицу строк (в одном листе — 7037 ячеек, из них со
/// значением 117, и все `t="s"`). Поэтому берётся любая ячейка со значением,
/// а числовой она становится при правке — выставлением `t="n"`. Это трогает
/// только часть листа, `sharedStrings.xml` остаётся нетронутым, и проверка
/// «правка не задела соседей» сохраняет силу.
fn first_value_node(doc: &Document) -> Option<(NodeId, NodeId, String, String)> {
    let root = doc.root_element().ok()?;
    let sheet_data = doc.find_child(root, Some(SHEETML), "sheetData")?;

    for row in doc.children(sheet_data) {
        for cell in doc.children(row) {
            // Ячейка с формулой не годится: её кэш пересчитается, и сверка
            // с LibreOffice проверяла бы не нас, а его вычислитель.
            if doc.find_child(cell, Some(SHEETML), "f").is_some() {
                continue;
            }
            // Пустая ячейка (`<c r="A1"/>`) — обычное дело; она не повод
            // прекращать поиск, только повод перейти к следующей.
            let Some(v) = doc.find_child(cell, Some(SHEETML), "v") else {
                continue;
            };
            let Ok(old) = doc.text(v) else { continue };
            let old = old.into_owned();
            if old.is_empty() {
                continue;
            }
            // Адрес нужен только для сообщений об ошибках. Он необязателен:
            // позиция может быть неявной.
            let at = doc.attr(cell, None, "r").map_or_else(
                || "<неявный адрес>".to_owned(),
                std::borrow::Cow::into_owned,
            );
            return Some((cell, v, at, old));
        }
    }
    None
}

/// Открывает пакет, правит первую числовую ячейку первого листа и сохраняет.
///
/// Возвращает `(новые байты, имя части листа, адрес ячейки, старое значение)`.
fn edit_first_cell(data: &[u8]) -> Option<(Vec<u8>, PartName, String, String)> {
    let mut pkg = Package::open(data, Limits::strict()).ok()?;

    let sheets: Vec<PartName> = pkg
        .part_names()
        .filter(|p| {
            let s = p.as_str();
            s.starts_with("/xl/worksheets/") && s.ends_with(".xml")
        })
        .cloned()
        .collect();

    for sheet in sheets {
        let doc = pkg.dom(&sheet).ok()?;
        let Some((cell, v, at, old)) = first_value_node(doc) else {
            continue;
        };
        // `t="n"` — явное объявление числа. Без него ячейка, бывшая `t="s"`,
        // осталась бы ссылкой в таблицу строк, и наш маркер прочитался бы как
        // индекс 424242 — то есть как ошибка.
        doc.set_attr(cell, "t", "n").ok()?;
        doc.set_text(v, MARKER).ok()?;
        let out = pkg.save().ok()?;
        return Some((out, sheet, at, old));
    }
    None
}

#[test]
fn editing_one_cell_leaves_every_other_entry_byte_identical() {
    let files = xlsx_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен (задайте OOXML_CORPUS)");
        return;
    }

    let limits = Limits::strict();
    let (mut edited, mut untouched_entries) = (0u32, 0u32);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        let Some((out, sheet, at, old)) = edit_first_cell(&data) else {
            eprintln!("{name}: числовых ячеек не нашлось, пропуск");
            continue;
        };

        // Правка обязана что-то изменить, иначе тест ничего не доказывает.
        assert_ne!(out, data, "{name}: правка не изменила ни байта");
        assert_ne!(old, MARKER, "{name}: маркер совпал со старым значением");

        let before = ZipArchive::parse(&data, &limits).unwrap();
        let after = ZipArchive::parse(&out, &limits).unwrap();
        assert_eq!(
            before.len(),
            after.len(),
            "{name}: изменилось число записей"
        );

        for i in 0..before.len() {
            let n_before = before.name_str(i).unwrap().into_owned();
            let n_after = after.name_str(i).unwrap().into_owned();
            assert_eq!(n_before, n_after, "{name}: сбился порядок записей");

            let raw_before = before.raw_data(i).unwrap();
            let raw_after = after.raw_data(i).unwrap();

            if PartName::from_zip_name(&n_before).is_ok_and(|p| p == sheet) {
                assert_ne!(
                    raw_before, raw_after,
                    "{name}!{n_before}: правленая часть обязана измениться"
                );
            } else {
                assert_eq!(
                    raw_before, raw_after,
                    "{name}!{n_before}: правка {at} задела чужую запись"
                );
                untouched_entries += 1;
            }
        }

        // Читаем своё же обратно: значение действительно новое.
        let mut pkg = Package::open(&out, limits.clone()).unwrap();
        let doc = pkg.dom(&sheet).unwrap();
        let (_, _, _, now) = first_value_node(doc).unwrap();
        assert_eq!(now, MARKER, "{name}: перечитали и не увидели правку");

        edited += 1;
    }

    println!("правка одной ячейки на пакет:");
    println!("  пакетов отредактировано: {edited}");
    println!("  соседних записей, оставшихся побайтово теми же: {untouched_entries}");
    assert!(edited > 10, "ожидалось много пакетов, получено {edited}");
}

// --- проверка внешним читателем -------------------------------------------

fn soffice() -> Option<PathBuf> {
    let p = std::env::var_os("OOXML_SOFFICE").map_or_else(
        || PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice"),
        PathBuf::from,
    );
    p.exists().then_some(p)
}

/// Конвертирует xlsx в CSV сторонним читателем и отдаёт текст.
///
/// Отдельный `-env:UserInstallation` обязателен: без него headless-процессы
/// конфликтуют с запущенным GUI и друг с другом, и тест начинает падать плавающе.
fn to_csv(soffice: &Path, xlsx: &Path, work: &Path) -> Option<String> {
    let status = Command::new(soffice)
        .arg("--headless")
        .arg(format!("-env:UserInstallation=file://{}", work.display()))
        .arg("--convert-to")
        .arg("csv")
        .arg("--outdir")
        .arg(work)
        .arg(xlsx)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let stem = xlsx.file_stem()?;
    std::fs::read_to_string(work.join(stem).with_extension("csv")).ok()
}

#[test]
#[ignore = "требует LibreOffice; включается переменной OOXML_SOFFICE"]
fn libreoffice_opens_our_edit_and_sees_the_new_value() {
    let Some(bin) = soffice() else {
        eprintln!("LibreOffice не найден — тест пропущен");
        return;
    };
    let files = xlsx_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let root = std::env::temp_dir().join(format!("ooxml_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();

    let (mut checked, mut seen) = (0u32, 0u32);

    // Пяти пакетов достаточно: каждый прогон LibreOffice занимает секунды,
    // а проверяется здесь свойство файла, а не разнообразие корпуса.
    for path in files.iter().take(5) {
        let data = std::fs::read(path).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let Some((out, _, at, _)) = edit_first_cell(&data) else {
            continue;
        };

        let work = root.join(&name);
        std::fs::create_dir_all(&work).unwrap();
        let edited_path = work.join(format!("{name}.xlsx"));
        std::fs::write(&edited_path, &out).unwrap();

        match to_csv(&bin, &edited_path, &work) {
            Some(csv) => {
                checked += 1;
                if csv.contains(MARKER) || csv.contains("424242,5") || csv.contains("424243") {
                    seen += 1;
                } else {
                    println!("  {name}: файл открылся, но маркера в ячейке {at} не видно");
                }
            }
            None => panic!("{name}: LibreOffice отказался открывать наш файл"),
        }
    }

    std::fs::remove_dir_all(&root).ok();

    println!("внешний читатель:");
    println!("  файлов открыто: {checked}");
    println!("  файлов, где виден маркер: {seen}");

    assert!(checked > 0, "ни один файл не удалось проверить");
    assert_eq!(
        seen, checked,
        "LibreOffice открыл файл, но не увидел нашу правку"
    );
}

#[test]
fn changing_font_size_keeps_everything_else() {
    // Кегль живёт не в ячейке, а в `xl/styles.xml`, и меняется он заведением
    // новой записи формата. Проверяем главное: правка задевает ровно две
    // части — лист и стили, — а прежнее оформление ячейки уцелело.
    let files = xlsx_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let limits = Limits::strict();
    let (mut books, mut untouched) = (0u32, 0u32);

    for path in files.iter().take(8) {
        let data = std::fs::read(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        let mut wb = ooxml::xlsx::Workbook::open(&data).unwrap();
        let sheets = wb.sheets().len();
        if sheets == 0 {
            continue;
        }

        // Берём ячейку, у которой уже есть оформление: на ней видно, что
        // остальные свойства пережили правку.
        let (at, before) = {
            let mut sh = wb.sheet(0).unwrap();
            let Some(c) = sh
                .read_all()
                .unwrap()
                .into_iter()
                .find(|c| c.style.is_some_and(|s| s > 0))
            else {
                continue;
            };
            (c.at, c.style)
        };

        wb.sheet(0).unwrap().set_font_size(at, 22.0).unwrap();
        let out = wb.save().unwrap();
        assert_ne!(out, data, "{name}: правка кегля ничего не изменила");

        let a = ZipArchive::parse(&data, &limits).unwrap();
        let b = ZipArchive::parse(&out, &limits).unwrap();
        for i in 0..a.len() {
            let part = a.name_str(i).unwrap().into_owned();
            let touched = part.contains("worksheets/") || part.ends_with("styles.xml");
            if !touched {
                assert_eq!(
                    a.raw_data(i).unwrap(),
                    b.raw_data(i).unwrap(),
                    "{name}!{part}: правка кегля задела чужую запись"
                );
                untouched += 1;
            }
        }

        // Перечитываем: номер стиля обязан смениться, а сам кегль стать 22.
        let mut wb2 = ooxml::xlsx::Workbook::open(&out).unwrap();
        let after = wb2.sheet(0).unwrap().get(at).unwrap().and_then(|c| c.style);
        assert_ne!(after, before, "{name}: номер стиля не изменился");

        let ap = wb2.appearance().unwrap().unwrap();
        let size = after.and_then(|s| ap.font_of(s)).and_then(|f| f.size);
        assert_eq!(size, Some(22.0), "{name}: кегль не применился");

        books += 1;
    }

    println!("правка кегля:");
    println!("  книг: {books}");
    println!("  соседних записей, оставшихся побайтово теми же: {untouched}");
    assert!(books >= 5, "проверено слишком мало книг: {books}");
}
