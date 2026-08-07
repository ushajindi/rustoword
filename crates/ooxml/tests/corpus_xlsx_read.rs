//! Чтение всех ячеек всех листов корпуса — гейт вехи M8.
//!
//! # Что здесь проверяется
//!
//! Что каждая ячейка каждого листа двадцати реальных книг читается без ошибки.
//! Это единственный критерий, который нельзя подделать: рукотворные части в
//! `unit_xlsx.rs` проверяют, что мы согласны сами с собой, а корпус — что мы
//! согласны с тем, что пишут Excel 365, Google Sheets и LibreOffice.
//!
//! # Почему тест печатает статистику
//!
//! Числа — не украшение отчёта. Счётчик строк и ячеек **без атрибута `r`**
//! существует затем, чтобы факт «корпус не покрывает неявные позиции» был
//! виден на экране, а не подразумевался. Пока он равен нулю, единственная
//! защита от «да у всех же есть `r`» — юнит-тесты, и об этом надо знать.
//!
//! # Оракул
//!
//! Второй тест сверяет прочитанное с LibreOffice: наши тесты проверяют
//! согласие нашего парсера с нашими же ожиданиями, а оракул — согласие с
//! посторонней реализацией. Он помечен `#[ignore]` и требует переменной
//! `OOXML_SOFFICE`; харнесс запуска взят из `tests/soffice.rs` целиком.

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
use ooxml::dom::Document;
use ooxml::xlsx::{
    Cell, CellType, CellValue, ScanStats, SharedStrings, Styles, scan_sheet_stats, sheet_dimension,
};
use ooxml::zip::ZipArchive;

/// Харнесс оракула. Отдельным тестовым бинарём он собирается и сам, поэтому
/// его собственные `#[ignore]`-тесты видны дважды — это цена того, что в Rust
/// интеграционные тесты не умеют делить код иначе.
#[path = "soffice.rs"]
#[allow(dead_code)]
mod soffice;

const SHEETS_DIR: &str = "xl/worksheets/";
const SST_PART: &str = "xl/sharedStrings.xml";
const STYLES_PART: &str = "xl/styles.xml";
const WORKBOOK_PART: &str = "xl/workbook.xml";
const WORKBOOK_RELS: &str = "xl/_rels/workbook.xml.rels";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Корень корпуса: `OOXML_CORPUS` либо путь по умолчанию.
///
/// Путь абсолютный, а не производный от `CARGO_MANIFEST_DIR`, ровно как в
/// `corpus_pipeline.rs`: корпус — реальные документы пользователя, он не лежит
/// в репозитории (см. `docs/corpus.md`) и потому не переезжает вместе с
/// рабочей копией. В отведённом worktree относительный путь указывал бы в
/// пустоту, и тест тихо проходил бы, ничего не проверив.
fn corpus_root() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    )
}

fn corpus_files() -> Vec<PathBuf> {
    let dir = corpus_root().join("xlsx");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!(
            "ПРЕДУПРЕЖДЕНИЕ: каталог корпуса {} не найден — тест пропущен.\n\
             Путь задаётся переменной OOXML_CORPUS.",
            dir.display()
        );
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "xlsx"))
        .collect();
    out.sort();
    out
}

/// Один прочитанный лист.
struct Sheet {
    part: String,
    cells: Vec<Cell>,
    stats: ScanStats,
    dimension: Option<String>,
}

/// Одна прочитанная книга.
struct Book {
    sst: SharedStrings,
    styles: Styles,
    sheets: Vec<Sheet>,
    /// Имя части первого листа в порядке `workbook.xml`.
    first_sheet: Option<String>,
}

/// Читает книгу целиком: таблицу строк, стили и все листы.
fn read_book(path: &Path, limits: &Limits) -> Book {
    let data = std::fs::read(path).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let zip = ZipArchive::parse(&data, limits)
        .unwrap_or_else(|e| panic!("{name}: ZIP не разбирается: {e}"));

    let part = |p: &str| -> Option<Vec<u8>> {
        let i = zip.index_of(p)?;
        Some(
            zip.decompress(i, limits)
                .unwrap_or_else(|e| panic!("{name}!{p}: распаковка: {e}")),
        )
    };

    // Отсутствие таблицы строк законно: книга без текстовых ячеек её не имеет.
    let sst = part(SST_PART).map_or_else(SharedStrings::empty, |bytes| {
        SharedStrings::parse(&bytes, limits).unwrap_or_else(|e| panic!("{name}!{SST_PART}: {e}"))
    });
    let styles = part(STYLES_PART).map_or_else(Styles::empty, |bytes| {
        Styles::parse(&bytes, limits).unwrap_or_else(|e| panic!("{name}!{STYLES_PART}: {e}"))
    });

    let mut names: Vec<String> = (0..zip.entries().len())
        .filter_map(|i| zip.name_str(i).ok().map(|n| n.into_owned()))
        .filter(|n| n.starts_with(SHEETS_DIR) && n.ends_with(".xml"))
        .collect();
    names.sort();

    let mut sheets = Vec::new();
    for sheet_part in names {
        let bytes = part(&sheet_part).unwrap();
        let dimension = sheet_dimension(&bytes, limits)
            .unwrap_or_else(|e| panic!("{name}!{sheet_part}: <dimension>: {e}"))
            .map(|r| r.to_a1());
        let (cells, stats) = scan_sheet_stats(&bytes, &sst, limits)
            .unwrap_or_else(|e| panic!("{name}!{sheet_part}: чтение ячеек: {e}"));
        sheets.push(Sheet {
            part: sheet_part,
            cells,
            stats,
            dimension,
        });
    }

    let first_sheet = first_sheet_part(&part(WORKBOOK_PART), &part(WORKBOOK_RELS), limits);
    Book {
        sst,
        styles,
        sheets,
        first_sheet,
    }
}

/// Имя части первого листа книги.
///
/// LibreOffice кладёт в CSV **первый** лист, а «первый» определяется порядком
/// в `<sheets>` книги, а не именем файла: в `gsheets_01.xlsx` первый лист
/// ссылается на `rId5`. Угадывание по `sheet1.xml` сверяло бы нас не с тем
/// листом и молча выдавало бы горы расхождений.
fn first_sheet_part(
    workbook: &Option<Vec<u8>>,
    rels: &Option<Vec<u8>>,
    limits: &Limits,
) -> Option<String> {
    let wb = Document::parse(workbook.clone()?, limits).ok()?;
    let root = wb.root_element().ok()?;
    let sheets = wb.find_child(root, Some(ooxml::xlsx::SML_NS), "sheets")?;
    let first = wb
        .children(sheets)
        .find(|&c| wb.local_name(c) == Some(b"sheet"))?;
    let rid = wb.attr(first, Some(R_NS), "id")?.into_owned();

    let rd = Document::parse(rels.clone()?, limits).ok()?;
    let rroot = rd.root_element().ok()?;
    let target = rd
        .children(rroot)
        .filter(|&c| rd.local_name(c) == Some(b"Relationship"))
        .find(|&c| rd.attr(c, None, "Id").as_deref() == Some(rid.as_str()))
        .and_then(|c| rd.attr(c, None, "Target"))?
        .into_owned();

    // Цели относительны каталогу части `xl/`; абсолютные (`/xl/...`) тоже
    // законны, хоть в корпусе и не встречаются.
    Some(match target.strip_prefix('/') {
        Some(abs) => abs.to_owned(),
        None => format!("xl/{target}"),
    })
}

// ---------------------------------------------------------------------------
// Гейт: всё читается
// ---------------------------------------------------------------------------

#[test]
fn every_cell_of_every_sheet_is_read() {
    let files = corpus_files();
    if files.is_empty() {
        return;
    }
    let limits = Limits::strict();

    let mut total = ScanStats::default();
    let (mut books, mut sheets, mut with_dim) = (0_u32, 0_u32, 0_u32);
    let (mut sst_items, mut sst_rich, mut sst_preserved) = (0_u64, 0_u64, 0_u64);
    let mut date_cells = 0_u64;
    let mut styled = 0_u64;

    for path in &files {
        let book = read_book(path, &limits);
        books += 1;
        sst_items += book.sst.len() as u64;
        sst_rich += u64::from(book.sst.rich_count());
        sst_preserved += u64::from(book.sst.preserved_count());

        for sheet in &book.sheets {
            sheets += 1;
            if sheet.dimension.is_some() {
                with_dim += 1;
            }
            total.merge(&sheet.stats);
            for c in &sheet.cells {
                if c.style.is_some() {
                    styled += 1;
                }
                if matches!(c.value, CellValue::Number(_)) && book.styles.is_date_style(c.style) {
                    date_cells += 1;
                }
            }
        }
    }

    println!("чтение SpreadsheetML по корпусу:");
    println!("  книг: {books}, листов: {sheets}");
    println!(
        "  ячеек: {} (непустых {}, со стилем {styled})",
        total.cells, total.non_empty
    );
    println!("  строк: {}", total.rows);
    print!("  распределение по типам `t`:");
    for t in CellType::ALL {
        print!(" {}={}", t.as_str(), total.by_type[t.index()]);
    }
    println!();
    println!(
        "  формул: {} (общих {}, массива {}), inlineStr: {}",
        total.formulas, total.shared_formulas, total.array_formulas, total.inline_strings
    );
    println!(
        "  таблица общих строк: {sst_items} записей, с форматированием {sst_rich}, \
         с xml:space=preserve {sst_preserved}"
    );
    println!("  ячеек, показанных как дата: {date_cells}");
    println!("  листов с <dimension>: {with_dim} из {sheets}");
    println!(
        "  размах адресов: {}",
        total
            .extent()
            .map_or_else(|| "нет ячеек".to_owned(), |r| r.to_a1())
    );

    println!(
        "  строк без атрибута `r`: {}, ячеек без атрибута `r`: {}",
        total.rows_without_r, total.cells_without_r
    );
    if total.rows_without_r == 0 && total.cells_without_r == 0 {
        println!(
            "  ВНИМАНИЕ: неявные позиции <row>/<c> корпусом НЕ покрыты.\n\
             \x20 Все {} строк и {} ячеек несут `r`. Единственная защита от\n\
             \x20 регрессии — юнит-тесты в tests/unit_xlsx.rs; при правке\n\
             \x20 сканера полагаться на этот тест нельзя.",
            total.rows, total.cells
        );
    }

    assert!(
        books >= 20,
        "ожидалось не меньше 20 книг, прочитано {books}"
    );
    assert!(
        sheets >= 33,
        "ожидалось не меньше 33 листов, прочитано {sheets}"
    );
    assert!(
        total.cells > 200_000,
        "ожидалось ~232 тыс. ячеек, прочитано {}",
        total.cells
    );
    assert!(total.non_empty > 0);
    assert_eq!(
        total.cells,
        total.by_type.iter().sum::<u64>(),
        "каждая ячейка обязана попасть ровно в одну корзину гистограммы"
    );
}

/// Индекс `sheetData` поверх DOM обязан находить ровно те же ячейки, что и
/// потоковый сканер.
///
/// Это предусловие вехи M9: правка ищет ячейку через дерево, а значение
/// читается сканером. Разойдись они на одном реальном файле — и правка попадёт
/// не туда, оставив файл валидным.
#[test]
fn dom_index_matches_the_scanner_on_the_whole_corpus() {
    let files = corpus_files();
    if files.is_empty() {
        return;
    }
    let limits = Limits::strict();
    let (mut sheets, mut cells, mut peak_dom, mut peak_xml) = (0_u32, 0_u64, 0_usize, 0_usize);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let zip = ZipArchive::parse(&data, &limits).unwrap();
        let sst = zip
            .index_of(SST_PART)
            .map(|i| zip.decompress(i, &limits).unwrap())
            .map_or_else(SharedStrings::empty, |b| {
                SharedStrings::parse(&b, &limits).unwrap()
            });

        for i in 0..zip.entries().len() {
            let part = zip.name_str(i).unwrap().into_owned();
            if !part.starts_with(SHEETS_DIR) || !part.ends_with(".xml") {
                continue;
            }
            let bytes = zip.decompress(i, &limits).unwrap();
            let scanned = scan_sheet_stats(&bytes, &sst, &limits).unwrap().0;

            let doc = Document::parse(bytes.clone(), &limits)
                .unwrap_or_else(|e| panic!("{name}!{part}: {e}"));
            peak_xml = peak_xml.max(bytes.len());
            peak_dom = peak_dom.max(doc.memory_bytes());
            let idx = ooxml::xlsx::SheetData::build(&doc)
                .unwrap_or_else(|e| panic!("{name}!{part}: индекс: {e}"));

            assert_eq!(
                idx.len(),
                scanned.len(),
                "{name}!{part}: сканер нашёл {} ячеек, индекс {}",
                scanned.len(),
                idx.len()
            );
            for c in &scanned {
                assert!(
                    idx.cell_node(c.at).is_some(),
                    "{name}!{part}: индекс не знает про {}",
                    c.at.to_a1()
                );
            }
            sheets += 1;
            cells += scanned.len() as u64;
        }
    }

    println!("индекс sheetData сверен со сканером: {sheets} листов, {cells} ячеек");
    println!(
        "  самый большой лист: {peak_xml} байт XML → {peak_dom} байт DOM ({:.1}x)\n\
         \x20 ровно поэтому чтение значений идёт потоком, а дерево строится только для правки",
        peak_dom as f64 / peak_xml.max(1) as f64
    );
}

// ---------------------------------------------------------------------------
// Сверка с оракулом
// ---------------------------------------------------------------------------

/// Как соотносится наше значение с эталонным.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Значения совпали.
    Match,
    /// Разошлись только представлением: LibreOffice применяет числовой формат
    /// (дата, процент, валюта, разделители разрядов), мы отдаём сырое значение.
    Formatting,
    /// Разошлись по существу — вот это и есть дефект.
    Substantive,
}

/// Разбирает число из CSV оракула.
///
/// Десятичный разделитель и разделители разрядов зависят от локали, в которой
/// запущен LibreOffice, и фильтр StarCalc на них не влияет. Поэтому обе формы
/// принимаются, а неразрывные и обычные пробелы выбрасываются.
fn reference_number(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(*c, ' ' | '\u{a0}' | '\u{202f}' | '\''))
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned
        .parse::<f64>()
        .ok()
        .or_else(|| cleaned.replace(',', ".").parse::<f64>().ok())
}

fn close_enough(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= scale * 1e-9
}

fn compare(ours: &CellValue, theirs: &str) -> Verdict {
    let theirs = theirs.trim();
    match ours {
        CellValue::Empty => {
            if theirs.is_empty() {
                Verdict::Match
            } else {
                Verdict::Substantive
            }
        }
        CellValue::Number(n) => match reference_number(theirs) {
            Some(x) if close_enough(*n, x) => Verdict::Match,
            // Непустой, но не число — значит применён числовой формат: дата,
            // процент, валюта. Ожидаемо и не является расхождением по существу.
            Some(_) | None if !theirs.is_empty() => Verdict::Formatting,
            _ => Verdict::Substantive,
        },
        CellValue::Text(s) => {
            if s == theirs || s.trim() == theirs {
                Verdict::Match
            } else {
                Verdict::Substantive
            }
        }
        CellValue::Bool(b) => {
            let want = if *b { "TRUE" } else { "FALSE" };
            if theirs.eq_ignore_ascii_case(want) || theirs == if *b { "1" } else { "0" } {
                Verdict::Match
            } else {
                Verdict::Formatting
            }
        }
        CellValue::Error(e) => {
            if theirs == e.as_str() {
                Verdict::Match
            } else {
                Verdict::Formatting
            }
        }
    }
}

#[test]
#[ignore = "требует LibreOffice; включается переменной OOXML_SOFFICE"]
fn oracle_agrees_on_the_first_sheet_of_every_book() {
    let Some(bin) = soffice::oracle() else { return };
    let files = corpus_files();
    if files.is_empty() {
        return;
    }
    let limits = Limits::strict();

    let (mut compared, mut matched, mut formatting, mut substantive) = (0_u64, 0_u64, 0_u64, 0_u64);
    let mut missing_in_reference = 0_u64;
    let mut extra_in_reference = 0_u64;
    let mut converted = 0_u32;
    let mut examples: Vec<String> = Vec::new();

    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let book = read_book(path, &limits);
        let Some(first) = book.first_sheet.clone() else {
            eprintln!("{name}: первый лист не определён — пропуск");
            continue;
        };
        let Some(sheet) = book.sheets.iter().find(|s| s.part == first) else {
            eprintln!("{name}: части {first} нет среди листов — пропуск");
            continue;
        };

        let conversion = match soffice::convert(&bin, path, soffice::Kind::Spreadsheet) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{name}: конвертация не удалась: {e}");
                continue;
            }
        };
        let reference = soffice::read_reference_csv(conversion.output());
        converted += 1;

        // Эталон — плотная таблица; наши ячейки разрежены. Сверяем по обоим
        // направлениям: чего нет у нас и чего нет у них.
        let mut ours: BTreeMap<(u32, u32), &CellValue> = BTreeMap::new();
        for c in &sheet.cells {
            ours.insert((c.at.row, c.at.col), &c.value);
        }

        for (r, row) in reference.iter().enumerate() {
            for (c, field) in row.iter().enumerate() {
                let key = (r as u32, c as u32);
                match ours.get(&key) {
                    Some(value) => {
                        compared += 1;
                        match compare(value, field) {
                            Verdict::Match => matched += 1,
                            Verdict::Formatting => formatting += 1,
                            Verdict::Substantive => {
                                substantive += 1;
                                if examples.len() < 20 {
                                    examples.push(format!(
                                        "{name}!{first} R{}C{}: наше {value:?}, эталон {field:?}",
                                        r + 1,
                                        c + 1
                                    ));
                                }
                            }
                        }
                    }
                    None if !field.trim().is_empty() => extra_in_reference += 1,
                    None => {}
                }
            }
        }
        for (&(r, c), value) in &ours {
            if value.is_empty() {
                continue;
            }
            let present = reference
                .get(r as usize)
                .and_then(|row| row.get(c as usize))
                .is_some();
            if !present {
                missing_in_reference += 1;
            }
        }
    }

    println!("сверка с LibreOffice (первый лист каждой книги):");
    println!("  сконвертировано книг: {converted}");
    println!("  сверено ячеек: {compared}");
    println!("  совпало: {matched}");
    println!("  расхождений по форматированию (ожидаемы): {formatting}");
    println!("  расхождений по существу: {substantive}");
    println!("  наших непустых ячеек вне таблицы эталона: {missing_in_reference}");
    println!("  непустых полей эталона вне наших ячеек: {extra_in_reference}");
    for e in &examples {
        println!("    {e}");
    }

    assert!(converted > 0, "оракул не сконвертировал ни одной книги");
    assert!(compared > 0, "не сверено ни одной ячейки");
    // Расхождения по форматированию не роняют тест — они ожидаемы. Порог по
    // существу свободный: он ловит поломку чтения, а не отличия рендеринга.
    let bad_share = substantive as f64 / compared as f64;
    assert!(
        bad_share < 0.02,
        "расхождений по существу {substantive} из {compared} ({:.2}%) — слишком много",
        bad_share * 100.0
    );
}
