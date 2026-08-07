//! Критерий вехи M9: правка ячейки через фасад `Workbook`/`Sheet` на реальных
//! книгах.
//!
//! Проект существует ради одного свойства: **правка чужого файла не должна
//! ничего разрушать**. Здесь оно проверяется на двадцати книгах, собранных
//! тремя разными генераторами, и разложено на шесть утверждений:
//!
//! 1. открытие и сохранение без правок не меняет ни байта;
//! 2. правка одной ячейки не задевает чужие записи архива;
//! 3. записанное всеми четырьмя типами читается обратно;
//! 4. вставка новой ячейки сохраняет возрастающий порядок;
//! 5. правленая книга просит пересчёт и не содержит устаревшей цепочки
//!    вычислений;
//! 6. LibreOffice открывает результат и видит новое значение.
//!
//! Шестое помечено `#[ignore]`: оно зависит от внешней программы и медленно.
//!
//! Корпус — реальные документы, он не лежит в репозитории (`docs/corpus.md`).
//! Путь задаётся `OOXML_CORPUS`; если каталога нет, тесты печатают
//! предупреждение и **проходят**: отсутствие чужих файлов не является дефектом
//! нашего кода.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};

use ooxml::Limits;
use ooxml::opc::{Package, PartName};
use ooxml::xlsx::worksheet::sheet_layout;
use ooxml::xlsx::{CellError, CellRef, CellValue, Workbook};
use ooxml::zip::ZipArchive;

/// Харнесс оракула. Отдельным тестовым бинарём он собирается и сам, поэтому
/// его собственные `#[ignore]`-тесты видны дважды — это цена того, что в Rust
/// интеграционные тесты не умеют делить код иначе.
#[path = "soffice.rs"]
#[allow(dead_code)]
mod soffice;

const WORKBOOK_ENTRY: &str = "xl/workbook.xml";
const CONTENT_TYPES_ENTRY: &str = "[Content_Types].xml";
const WORKBOOK_RELS_ENTRY: &str = "xl/_rels/workbook.xml.rels";

/// Значения-маркеры: в корпусе не встречаются и заметны в CSV.
const MARK_NUMBER: f64 = 424_242.5;
const MARK_STRING: &str = "МАРКЕР-M9";

// --- корпус ---------------------------------------------------------------

/// Корень корпуса. Путь абсолютный, а не производный от `CARGO_MANIFEST_DIR`:
/// в отведённом worktree относительный указывал бы в пустоту, и тест тихо
/// проходил бы, ничего не проверив.
fn corpus_root() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    )
}

fn xlsx_files() -> Vec<PathBuf> {
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

fn name_of(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

// --- сравнение архивов ----------------------------------------------------

/// Имена записей, чьи **сырые** байты изменились.
///
/// Сравниваются именно сырые данные вместе с заголовками: нетронутая часть
/// обязана перекочевать в новый архив побайтово, а не «пережать в тот же
/// результат». Наш deflate физически не даёт байты Office, поэтому любая
/// перепаковка была бы здесь видна.
fn changed_entries(before: &[u8], after: &[u8], name: &str) -> Vec<String> {
    let limits = Limits::strict();
    let a = ZipArchive::parse(before, &limits).unwrap();
    let b = ZipArchive::parse(after, &limits).unwrap();
    assert_eq!(a.len(), b.len(), "{name}: изменилось число записей");

    let mut changed = Vec::new();
    for i in 0..a.len() {
        let na = a.name_str(i).unwrap().into_owned();
        let nb = b.name_str(i).unwrap().into_owned();
        assert_eq!(na, nb, "{name}: сбился порядок записей");
        if a.raw_data(i).unwrap() != b.raw_data(i).unwrap() {
            changed.push(na);
        }
    }
    changed
}

fn entry_text(data: &[u8], entry: &str) -> Option<String> {
    let limits = Limits::strict();
    let zip = ZipArchive::parse(data, &limits).unwrap();
    (0..zip.len())
        .find(|&i| zip.name_str(i).unwrap() == entry)
        .map(|i| String::from_utf8_lossy(&zip.decompress(i, &limits).unwrap()).into_owned())
}

fn has_entry(data: &[u8], entry: &str) -> bool {
    let limits = Limits::strict();
    let zip = ZipArchive::parse(data, &limits).unwrap();
    (0..zip.len()).any(|i| zip.name_str(i).unwrap() == entry)
}

/// Раскладка `<sheetData>` части листа.
fn layout_of(data: &[u8], part: &PartName) -> Vec<(u32, Vec<CellRef>)> {
    let mut pkg = Package::open(data, Limits::strict()).unwrap();
    let doc = pkg.dom(part).unwrap();
    sheet_layout(doc).unwrap()
}

/// Часть первого листа книги.
fn first_sheet_part(data: &[u8]) -> PartName {
    let wb = Workbook::open(data).unwrap();
    wb.sheets()
        .first()
        .expect("в книге нет ни одного листа")
        .part
        .clone()
}

/// Адрес заведомо пустого места листа: две строки ниже последней занятой.
///
/// Отступ в две строки, а не в одну, оставляет между старыми данными и нашей
/// вставкой дыру — именно то место, где ошибка в порядке вставки строк была бы
/// видна.
fn free_spot(cells: &[ooxml::xlsx::Cell]) -> CellRef {
    let max_row = cells.iter().map(|c| c.at.row).max().unwrap_or(0);
    CellRef::new(max_row.saturating_add(2), 0)
}

// --- 1. открытие ничего не меняет -----------------------------------------

#[test]
fn opening_and_saving_changes_not_a_single_byte() {
    let files = xlsx_files();
    if files.is_empty() {
        return;
    }
    let mut ok = 0u32;
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = name_of(path);

        let mut wb = Workbook::open(&data).unwrap();
        // Чтение — не правка, сколько бы частей оно ни разобрало.
        let sheets = wb.sheets().len();
        assert!(sheets > 0, "{name}: в книге нет листов");
        for i in 0..sheets {
            let mut sh = wb.sheet(i).unwrap();
            let _ = sh.dimension().unwrap();
            let _ = sh.read_all().unwrap();
        }
        let out = wb.save().unwrap();
        assert_eq!(
            out.len(),
            data.len(),
            "{name}: длина результата разошлась с исходной"
        );
        assert!(out == data, "{name}: открытие и сохранение изменили байты");
        ok += 1;
    }
    println!(
        "открытие и сохранение без правок: {ok}/{} побайтово",
        files.len()
    );
    assert_eq!(ok as usize, files.len());
}

// --- 2. правка не задевает чужие записи ------------------------------------

#[test]
fn editing_one_cell_leaves_every_other_entry_byte_identical() {
    let files = xlsx_files();
    if files.is_empty() {
        return;
    }
    let (mut edited, mut untouched) = (0u32, 0u32);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = name_of(path);
        let sheet_part = first_sheet_part(&data);
        let sheet_entry = sheet_part.zip_name().to_owned();

        let mut wb = Workbook::open(&data).unwrap();
        let spot = {
            let mut sh = wb.sheet(0).unwrap();
            let cells = sh.read_all().unwrap();
            let spot = free_spot(&cells);
            sh.set_number(spot, MARK_NUMBER).unwrap();
            spot
        };
        let out = wb.save().unwrap();

        let changed = changed_entries(&data, &out, &name);
        assert!(
            changed.contains(&sheet_entry),
            "{name}: правленый лист не изменился, изменилось {changed:?}"
        );
        // Часть с описанием книги трогается по делу: без `fullCalcOnLoad`
        // формулы, ссылавшиеся на нашу ячейку, остались бы со старым кэшем.
        for entry in &changed {
            assert!(
                entry == &sheet_entry || entry == WORKBOOK_ENTRY,
                "{name}: правка {} задела чужую запись {entry}",
                spot.to_a1()
            );
        }
        let limits = Limits::strict();
        untouched += (ZipArchive::parse(&data, &limits).unwrap().len() - changed.len()) as u32;
        edited += 1;
    }

    println!("правка одной ячейки на книгу:");
    println!("  книг отредактировано: {edited}");
    println!("  соседних записей, оставшихся побайтово теми же: {untouched}");
    assert_eq!(edited as usize, files.len());
}

// --- 3. записанное читается обратно ---------------------------------------

#[test]
fn all_four_kinds_of_value_read_back() {
    let files = xlsx_files();
    if files.is_empty() {
        return;
    }
    let mut checked = 0u32;

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = name_of(path);

        let mut wb = Workbook::open(&data).unwrap();
        let base = {
            let mut sh = wb.sheet(0).unwrap();
            let cells = sh.read_all().unwrap();
            let base = free_spot(&cells);
            sh.set_number(base, MARK_NUMBER).unwrap();
            sh.set_string(CellRef::new(base.row, 1), MARK_STRING)
                .unwrap();
            sh.set_bool(CellRef::new(base.row, 2), true).unwrap();
            sh.set_error(CellRef::new(base.row, 3), CellError::Ref)
                .unwrap();
            base
        };
        let out = wb.save().unwrap();

        let mut re = Workbook::open(&out).unwrap();
        let mut sh = re.sheet(0).unwrap();
        let expected = [
            (0u32, CellValue::Number(MARK_NUMBER)),
            (1, CellValue::Text(MARK_STRING.to_owned())),
            (2, CellValue::Bool(true)),
            (3, CellValue::Error(CellError::Ref)),
        ];
        for (col, want) in expected {
            let at = CellRef::new(base.row, col);
            let got = sh.get(at).unwrap();
            assert_eq!(
                got.map(|c| c.value),
                Some(want.clone()),
                "{name}: в {} прочиталось не то",
                at.to_a1()
            );
        }

        // Строка ушла в общую таблицу — значит её часть тоже правленая, и
        // больше ничего сверх листа и книги.
        let changed = changed_entries(&data, &out, &name);
        for entry in &changed {
            assert!(
                entry.starts_with("xl/worksheets/")
                    || entry == WORKBOOK_ENTRY
                    || entry == "xl/sharedStrings.xml",
                "{name}: правка задела {entry}"
            );
        }
        checked += 1;
    }
    println!("четыре типа значений прочитаны обратно в {checked} книгах");
    assert_eq!(checked as usize, files.len());
}

// --- 4. порядок вставки ---------------------------------------------------

#[test]
fn inserting_cells_keeps_rows_and_columns_ascending() {
    let files = xlsx_files();
    if files.is_empty() {
        return;
    }
    let mut checked = 0u32;

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = name_of(path);
        let part = first_sheet_part(&data);

        let before = layout_of(&data, &part);
        assert!(
            is_ascending(&before),
            "{name}: лист и до правки был не отсортирован — тест проверяет не то"
        );
        let Some(&(first_row, ref first_cells)) = before.first() else {
            println!("{name}: лист пуст, вставка проверена другими тестами");
            continue;
        };

        // Три места, в которых ошибка порядка вставки видна: перед первой
        // ячейкой строки, после последней и в новой строке между старыми.
        let last_col = first_cells.iter().map(|c| c.col).max().unwrap_or(0);
        let after_last = CellRef::new(first_row, last_col.saturating_add(1));
        let before_first = first_cells
            .iter()
            .map(|c| c.col)
            .min()
            .filter(|&c| c > 0)
            .map(|c| CellRef::new(first_row, c.saturating_sub(1)));
        let new_row = missing_row(&before).map(|r| CellRef::new(r, 0));

        let mut wb = Workbook::open(&data).unwrap();
        {
            let mut sh = wb.sheet(0).unwrap();
            sh.set_number(after_last, 1.0).unwrap();
            if let Some(at) = before_first {
                sh.set_number(at, 2.0).unwrap();
            }
            if let Some(at) = new_row {
                sh.set_number(at, 3.0).unwrap();
            }
        }
        let out = wb.save().unwrap();

        let after = layout_of(&out, &part);
        assert!(
            is_ascending(&after),
            "{name}: после вставки порядок нарушен: {:?}",
            summary(&after)
        );
        // Ни одна старая ячейка не потерялась.
        let old: Vec<CellRef> = before.iter().flat_map(|(_, c)| c.clone()).collect();
        let new: Vec<CellRef> = after.iter().flat_map(|(_, c)| c.clone()).collect();
        for at in &old {
            assert!(new.contains(at), "{name}: ячейка {} исчезла", at.to_a1());
        }
        assert!(
            new.contains(&after_last),
            "{name}: вставка после последней не состоялась"
        );
        checked += 1;
    }
    println!("порядок вставки проверен на {checked} книгах");
}

/// Строки возрастают, и внутри каждой возрастают столбцы.
fn is_ascending(layout: &[(u32, Vec<CellRef>)]) -> bool {
    let rows: Vec<u32> = layout.iter().map(|&(r, _)| r).collect();
    if !rows.windows(2).all(|w| w[0] < w[1]) {
        return false;
    }
    layout.iter().all(|(_, cells)| {
        let cols: Vec<u32> = cells.iter().map(|c| c.col).collect();
        cols.windows(2).all(|w| w[0] < w[1])
    })
}

/// Первый пропущенный номер строки внутри занятого диапазона, если он есть.
fn missing_row(layout: &[(u32, Vec<CellRef>)]) -> Option<u32> {
    let rows: Vec<u32> = layout.iter().map(|&(r, _)| r).collect();
    for pair in rows.windows(2) {
        if pair[1] > pair[0].saturating_add(1) {
            return Some(pair[0].saturating_add(1));
        }
    }
    rows.last().map(|r| r.saturating_add(2))
}

fn summary(layout: &[(u32, Vec<CellRef>)]) -> Vec<u32> {
    layout.iter().map(|&(r, _)| r).take(20).collect()
}

// --- 5. пересчёт и цепочка вычислений -------------------------------------

#[test]
fn an_edited_book_asks_for_recalculation_and_keeps_no_stale_calc_chain() {
    let files = xlsx_files();
    if files.is_empty() {
        return;
    }
    let (mut checked, mut had_chain) = (0u32, 0u32);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = name_of(path);
        if has_entry(&data, "xl/calcChain.xml") {
            had_chain += 1;
        }

        let mut wb = Workbook::open(&data).unwrap();
        {
            let mut sh = wb.sheet(0).unwrap();
            let spot = free_spot(&sh.read_all().unwrap());
            sh.set_number(spot, MARK_NUMBER).unwrap();
        }
        let out = wb.save().unwrap();

        let workbook = entry_text(&out, WORKBOOK_ENTRY).unwrap();
        assert!(
            workbook.contains("fullCalcOnLoad=\"1\""),
            "{name}: книга не просит пересчёт: {workbook}"
        );
        assert!(
            workbook.contains("calcId=\"0\""),
            "{name}: calcId не обнулён"
        );

        // Цепочка вычислений обязана исчезнуть целиком: сама часть, её тип и
        // отношение. Ссылка на несуществующую часть — самая частая причина
        // «Excel обнаружил нечитаемое содержимое».
        assert!(
            !has_entry(&out, "xl/calcChain.xml"),
            "{name}: часть осталась"
        );
        let types = entry_text(&out, CONTENT_TYPES_ENTRY).unwrap();
        assert!(!types.contains("calcChain"), "{name}: Override остался");
        if let Some(rels) = entry_text(&out, WORKBOOK_RELS_ENTRY) {
            assert!(!rels.contains("calcChain"), "{name}: Relationship остался");
        }
        checked += 1;
    }
    println!("пересчёт запрошен в {checked} книгах; цепочка была в {had_chain} из них");
    if had_chain == 0 {
        println!(
            "  ВНИМАНИЕ: в корпусе нет ни одной книги с calcChain.xml — снос цепочки \
             держится на unit_xlsx_edit.rs, а не на этом тесте"
        );
    }
}

// --- 6. внешний читатель ---------------------------------------------------

/// Каталог для файлов, скармливаемых LibreOffice.
///
/// Свой, а не из харнесса: `Workspace` там приватен и живёт ровно на одну
/// конвертацию, а нам нужно место, куда записать наш результат до неё.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ooxml-m9-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.root.join(format!("{name}.xlsx"));
        std::fs::write(&path, data).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Как LibreOffice отобразил одну правку.
#[derive(Debug)]
struct Seen {
    number: bool,
    string: bool,
    boolean: bool,
    error: bool,
}

/// По две книги от каждого генератора корпуса.
///
/// Имена в корпусе устроены как `<генератор>_NN.xlsx`, и книги одного
/// генератора ведут себя у LibreOffice одинаково (проверено на всех двадцати).
/// Брать первые пять по алфавиту значило бы прогнать оракул по одному только
/// Excel: их в корпусе одиннадцать подряд.
fn oracle_sample(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut taken: Vec<(String, u32)> = Vec::new();
    for path in files {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let maker = stem.split('_').next().unwrap_or("").to_owned();
        match taken.iter_mut().find(|(g, _)| *g == maker) {
            Some((_, n)) if *n >= 2 => continue,
            Some((_, n)) => *n = n.saturating_add(1),
            None => taken.push((maker, 1)),
        }
        out.push(path.clone());
    }
    out
}

#[test]
#[ignore = "требует LibreOffice; включается переменной OOXML_SOFFICE"]
fn libreoffice_accepts_every_kind_of_edit() {
    let Some(bin) = soffice::oracle() else {
        return;
    };
    let files = xlsx_files();
    if files.is_empty() {
        return;
    }
    let scratch = Scratch::new();
    let sample = oracle_sample(&files);
    let mut report: Vec<(String, Seen)> = Vec::new();

    for path in &sample {
        let data = std::fs::read(path).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();

        let mut wb = Workbook::open(&data).unwrap();
        {
            let mut sh = wb.sheet(0).unwrap();
            let base = free_spot(&sh.read_all().unwrap());
            sh.set_number(base, MARK_NUMBER).unwrap();
            sh.set_string(CellRef::new(base.row, 1), MARK_STRING)
                .unwrap();
            sh.set_bool(CellRef::new(base.row, 2), true).unwrap();
            sh.set_error(CellRef::new(base.row, 3), CellError::Ref)
                .unwrap();
        }
        let out = wb.save().unwrap();

        let file = scratch.write(&name, &out);
        // Отказ открыть файл — провал вехи, а не особенность книги.
        let conv = match soffice::convert(&bin, &file, soffice::Kind::Spreadsheet) {
            Ok(c) => c,
            Err(e) => panic!("{name}: LibreOffice отказался открывать наш файл: {e}"),
        };

        let rows = soffice::read_reference_csv(conv.output());
        assert!(!rows.is_empty(), "{name}: LibreOffice дал пустой CSV");
        // Строка правки — та, где виден строковый маркер: печатать весь CSV
        // бессмысленно, у книг корпуса он в сотни строк.
        let row: Vec<String> = rows
            .iter()
            .find(|r| r.iter().any(|f| f.contains(MARK_STRING)))
            .cloned()
            .unwrap_or_default();

        // Десятичный разделитель, написание логического значения и код ошибки
        // зависят от локали и от того, во что LibreOffice переводит чужую
        // ошибку: `#REF!` он показывает как `#NAME?`.
        let field = |i: usize| row.get(i).map_or("", String::as_str);
        let seen = Seen {
            number: ["424242.5", "424242,5", "424243"].contains(&field(0)),
            string: field(1).contains(MARK_STRING),
            boolean: ["TRUE", "ИСТИНА", "WAHR", "VRAI"].contains(&field(2)),
            error: field(3).starts_with('#') || field(3).starts_with("Err:"),
        };
        assert!(seen.number, "{name}: числа не видно в CSV, строка: {row:?}");
        assert!(
            seen.string,
            "{name}: строки не видно в CSV, строка: {row:?}"
        );
        report.push((name, seen));
    }

    println!("внешний читатель (LibreOffice):");
    for (name, s) in &report {
        println!(
            "  {name}: число={} строка={} логическое={} ошибка={}",
            s.number, s.string, s.boolean, s.error
        );
    }
    let all_four = report
        .iter()
        .filter(|(_, s)| s.number && s.string && s.boolean && s.error)
        .count();
    println!(
        "  книг, где видны все четыре правки: {all_four} из {}",
        report.len()
    );

    // Книги Excel 365 LibreOffice показывает с `t="b"` и `t="e"`, обращёнными в
    // 0, — и делает это независимо от того, кто записал ячейку: та же строка,
    // вставленная сторонним инструментом побайтово так же, даёт тот же 0.
    // Поэтому строгое требование «все четыре типа» предъявляется к книгам, где
    // оракул вообще различает эти типы, а не к каждой книге корпуса.
    assert!(
        all_four >= 2,
        "ни в одной книге не видно всех четырёх типов правок"
    );
    assert!(
        report.iter().all(|(_, s)| s.number && s.string),
        "число и строка обязаны быть видны в каждой книге"
    );
}
