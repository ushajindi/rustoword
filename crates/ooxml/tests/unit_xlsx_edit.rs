//! Правка ячеек на сконструированных книгах.
//!
//! Корпус доказывает, что мы не ломаем реальные файлы. Здесь проверяется то,
//! чего в корпусе нет вовсе: пустой лист, `xl/calcChain.xml`, книга без
//! `<calcPr>` и без таблицы общих строк, строка с недопустимыми в XML
//! символами, элементы с префиксом namespace. Каждый из этих случаев
//! встречается в живых книгах за пределами корпуса, и обнаружить ошибку
//! хочется здесь, а не в чужом файле.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::deflate::Level;
use ooxml::error::{Error, XlsxError};
use ooxml::opc::{Package, PartName};
use ooxml::xlsx::worksheet::sheet_layout;
use ooxml::xlsx::{CellError, CellRef, CellValue, SheetState, StringPolicy, Workbook};
use ooxml::zip::{EntrySource, WriteOptions, ZipArchive, ZipWriter};

// --- сборка книги для тестов ----------------------------------------------

const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const CT_SHEET: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
const CT_SST: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml";
const CT_CHAIN: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml";

/// Из чего собирается книга. Каждое поле — то, что отдельный тест хочет менять.
struct BookSpec<'a> {
    /// Содержимое `<sheetData>` первого листа.
    sheet_body: &'a str,
    /// Готовый XML первого листа целиком; перекрывает `sheet_body`.
    sheet_xml: Option<String>,
    /// `<dimension ref="…"/>`, если он нужен.
    dimension: Option<&'a str>,
    /// Записи `<si>` таблицы общих строк; `None` — части нет вовсе.
    shared: Option<&'a [&'a str]>,
    /// В книге есть `<calcPr>`.
    calc_pr: bool,
    /// В книге есть `xl/calcChain.xml` — вместе с типом и отношением.
    calc_chain: bool,
}

impl Default for BookSpec<'_> {
    fn default() -> Self {
        Self {
            sheet_body: "",
            sheet_xml: None,
            dimension: None,
            shared: Some(&[]),
            calc_pr: true,
            calc_chain: false,
        }
    }
}

fn build(spec: &BookSpec<'_>) -> Vec<u8> {
    let dim = spec
        .dimension
        .map_or_else(String::new, |d| format!("<dimension ref=\"{d}\"/>"));
    let sheet = spec.sheet_xml.clone().unwrap_or_else(|| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"{SML}\">{dim}<sheetData>{}</sheetData></worksheet>",
            spec.sheet_body
        )
    });

    let calc_pr = if spec.calc_pr {
        "<calcPr calcId=\"191029\" calcMode=\"manual\"/>"
    } else {
        ""
    };
    let workbook = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<workbook xmlns=\"{SML}\" xmlns:r=\"{R_NS}\">\
<sheets>\
<sheet name=\"Лист1\" sheetId=\"1\" r:id=\"rId1\"/>\
<sheet name=\"Второй\" sheetId=\"7\" state=\"hidden\" r:id=\"rId2\"/>\
</sheets>{calc_pr}<extLst><ext uri=\"{{X}}\"/></extLst></workbook>"
    );

    let rel = |id: &str, ty: &str, target: &str| {
        format!("<Relationship Id=\"{id}\" Type=\"{R_NS}/{ty}\" Target=\"{target}\"/>")
    };
    let over =
        |part: &str, ct: &str| format!("<Override PartName=\"{part}\" ContentType=\"{ct}\"/>");

    let mut rels = format!("<?xml version=\"1.0\"?><Relationships xmlns=\"{PKG_NS}\">");
    rels.push_str(&rel("rId1", "worksheet", "worksheets/sheet1.xml"));
    rels.push_str(&rel("rId2", "worksheet", "worksheets/sheet2.xml"));

    let mut types = format!("<?xml version=\"1.0\"?><Types xmlns=\"{CT_NS}\">");
    types.push_str(
        "<Default Extension=\"rels\" \
ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
    );
    types.push_str("<Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    types.push_str(&over("/xl/worksheets/sheet1.xml", CT_SHEET));
    types.push_str(&over("/xl/worksheets/sheet2.xml", CT_SHEET));

    let mut extra: Vec<(String, Vec<u8>)> = Vec::new();

    if let Some(items) = spec.shared {
        let body: String = items
            .iter()
            .map(|s| format!("<si><t>{s}</t></si>"))
            .collect();
        let n = items.len();
        let sst = format!(
            "<?xml version=\"1.0\"?><sst xmlns=\"{SML}\" count=\"{n}\" uniqueCount=\"{n}\">\
{body}</sst>"
        );
        extra.push(("xl/sharedStrings.xml".to_owned(), sst.into_bytes()));
        rels.push_str(&rel("rId3", "sharedStrings", "sharedStrings.xml"));
        types.push_str(&over("/xl/sharedStrings.xml", CT_SST));
    }

    if spec.calc_chain {
        let chain = format!("<calcChain xmlns=\"{SML}\"><c r=\"A1\" i=\"1\"/></calcChain>");
        extra.push(("xl/calcChain.xml".to_owned(), chain.into_bytes()));
        rels.push_str(&rel("rId4", "calcChain", "calcChain.xml"));
        types.push_str(&over("/xl/calcChain.xml", CT_CHAIN));
    }

    rels.push_str("</Relationships>");
    types.push_str("</Types>");

    let root_rels = format!(
        "<?xml version=\"1.0\"?><Relationships xmlns=\"{PKG_NS}\">{}</Relationships>",
        rel("rId1", "officeDocument", "xl/workbook.xml")
    );

    let mut all: Vec<(String, Vec<u8>)> = vec![
        ("[Content_Types].xml".to_owned(), types.into_bytes()),
        ("_rels/.rels".to_owned(), root_rels.into_bytes()),
        ("xl/workbook.xml".to_owned(), workbook.into_bytes()),
        ("xl/_rels/workbook.xml.rels".to_owned(), rels.into_bytes()),
        ("xl/worksheets/sheet1.xml".to_owned(), sheet.into_bytes()),
        (
            "xl/worksheets/sheet2.xml".to_owned(),
            format!("<worksheet xmlns=\"{SML}\"><sheetData/></worksheet>").into_bytes(),
        ),
    ];
    all.append(&mut extra);

    let mut w = ZipWriter::new(WriteOptions::default());
    for (name, data) in all {
        // Store, а не deflate: тесту нужен предсказуемый контейнер, а не
        // проверка упаковщика — её делают гейты M3 и M4.
        w.push(EntrySource::New {
            name,
            data,
            level: Level::Store,
            dos: (0, 0x0021),
            external_attrs: 0,
        })
        .unwrap();
    }
    w.finish().unwrap()
}

fn book(body: &str) -> Vec<u8> {
    build(&BookSpec {
        sheet_body: body,
        ..BookSpec::default()
    })
}

fn at(a1: &str) -> CellRef {
    CellRef::parse(a1).unwrap()
}

const SHEET1: &str = "xl/worksheets/sheet1.xml";

/// Текст записи архива по имени. Годится и для `[Content_Types].xml`, который
/// частью не является и через пакет не адресуется.
fn entry_text(data: &[u8], name: &str) -> String {
    let limits = Limits::strict();
    let zip = ZipArchive::parse(data, &limits).unwrap();
    for i in 0..zip.len() {
        if zip.name_str(i).unwrap() == name {
            return String::from_utf8(zip.decompress(i, &limits).unwrap()).unwrap();
        }
    }
    panic!("записи {name} в архиве нет");
}

fn has_entry(data: &[u8], name: &str) -> bool {
    let limits = Limits::strict();
    let zip = ZipArchive::parse(data, &limits).unwrap();
    (0..zip.len()).any(|i| zip.name_str(i).unwrap() == name)
}

/// Раскладка `<sheetData>` первого листа сохранённой книги.
fn layout(data: &[u8]) -> Vec<(u32, Vec<CellRef>)> {
    let mut pkg = Package::open(data, Limits::strict()).unwrap();
    let part = PartName::new("/xl/worksheets/sheet1.xml").unwrap();
    let doc = pkg.dom(&part).unwrap();
    sheet_layout(doc).unwrap()
}

/// Адреса ячеек первого листа в порядке документа, плоским списком.
fn addresses(data: &[u8]) -> Vec<String> {
    layout(data)
        .into_iter()
        .flat_map(|(_, cells)| cells)
        .map(|c| c.to_a1())
        .collect()
}

/// Значение ячейки после перечитывания сохранённой книги.
fn reread(data: &[u8], a1: &str) -> Option<CellValue> {
    let mut wb = Workbook::open(data).unwrap();
    let mut sh = wb.sheet(0).unwrap();
    sh.get(at(a1)).unwrap().map(|c| c.value)
}

fn text(s: &str) -> Option<CellValue> {
    Some(CellValue::Text(s.to_owned()))
}

// --- вставка --------------------------------------------------------------

#[test]
fn writing_into_an_empty_sheet_creates_the_row_and_the_cell() {
    let src = book("");
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("C7"), 42.0).unwrap();
    let out = wb.save().unwrap();

    assert_eq!(layout(&out), vec![(6, vec![at("C7")])]);
    assert_eq!(reread(&out, "C7"), Some(CellValue::Number(42.0)));
    // Атрибут `r` обязателен у обоих новых элементов: без него позиция ячейки
    // зависела бы от соседей.
    let xml = entry_text(&out, SHEET1);
    assert!(xml.contains("<row r=\"7\""), "у новой строки нет r: {xml}");
    assert!(xml.contains("<c r=\"C7\""), "у новой ячейки нет r: {xml}");
}

#[test]
fn a_new_cell_lands_before_the_first_and_after_the_last() {
    let src = book("<row r=\"1\"><c r=\"C1\"/><c r=\"E1\"/></row>");

    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 1.0).unwrap();
    let before = wb.save().unwrap();
    assert_eq!(addresses(&before), ["A1", "C1", "E1"]);

    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("Z1"), 1.0).unwrap();
    let after = wb.save().unwrap();
    assert_eq!(addresses(&after), ["C1", "E1", "Z1"]);
}

#[test]
fn a_new_cell_lands_between_its_neighbours() {
    let src = book("<row r=\"1\"><c r=\"A1\"/><c r=\"E1\"/></row>");
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("C1"), 1.0).unwrap();
    let out = wb.save().unwrap();
    assert_eq!(addresses(&out), ["A1", "C1", "E1"]);
}

#[test]
fn a_new_row_lands_in_ascending_order() {
    let src = book("<row r=\"2\"><c r=\"A2\"/></row><row r=\"9\"><c r=\"A9\"/></row>");

    for (target, expected) in [
        ("A1", vec![0_u32, 1, 8]),
        ("A5", vec![1, 4, 8]),
        ("A20", vec![1, 8, 19]),
    ] {
        let mut wb = Workbook::open(&src).unwrap();
        wb.sheet(0).unwrap().set_number(at(target), 1.0).unwrap();
        let out = wb.save().unwrap();
        let rows: Vec<u32> = layout(&out).into_iter().map(|(r, _)| r).collect();
        assert_eq!(rows, expected, "строка {target} встала не туда");
    }
}

#[test]
fn inserting_many_cells_keeps_both_orders_ascending() {
    let src = book("<row r=\"5\"><c r=\"C5\"/></row>");
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        // Порядок записи намеренно обратный: результат обязан быть возрастающим
        // независимо от того, в каком порядке звали API.
        for a1 in ["Z9", "B2", "A5", "D1", "B5", "A1"] {
            sh.set_number(at(a1), 1.0).unwrap();
        }
    }
    let out = wb.save().unwrap();

    let l = layout(&out);
    let rows: Vec<u32> = l.iter().map(|&(r, _)| r).collect();
    assert!(
        rows.windows(2).all(|w| w[0] < w[1]),
        "строки не возрастают: {rows:?}"
    );
    for (r, cells) in &l {
        let cols: Vec<u32> = cells.iter().map(|c| c.col).collect();
        assert!(
            cols.windows(2).all(|w| w[0] < w[1]),
            "столбцы строки {r} не возрастают: {cols:?}"
        );
    }
    assert_eq!(addresses(&out), ["A1", "D1", "B2", "A5", "B5", "C5", "Z9"]);
}

// --- типы значений --------------------------------------------------------

#[test]
fn every_value_type_survives_a_round_trip() {
    let src = book("<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>");

    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        sh.set_number(at("A1"), -0.5).unwrap();
        sh.set_string(at("B1"), "привет").unwrap();
        sh.set_bool(at("C1"), true).unwrap();
        sh.set_error(at("D1"), CellError::Ref).unwrap();
    }
    let out = wb.save().unwrap();

    assert_eq!(reread(&out, "A1"), Some(CellValue::Number(-0.5)));
    assert_eq!(reread(&out, "B1"), text("привет"));
    assert_eq!(reread(&out, "C1"), Some(CellValue::Bool(true)));
    assert_eq!(reread(&out, "D1"), Some(CellValue::Error(CellError::Ref)));

    // Число пишется без атрибута `t`: так делают все три генератора корпуса.
    let xml = entry_text(&out, SHEET1);
    assert!(
        xml.contains("<c r=\"A1\"><v>-0.5</v></c>"),
        "числовая ячейка записана не так: {xml}"
    );
    assert!(xml.contains("<c r=\"C1\" t=\"b\"><v>1</v></c>"), "{xml}");
    assert!(
        xml.contains("<c r=\"D1\" t=\"e\"><v>#REF!</v></c>"),
        "{xml}"
    );
}

#[test]
fn editing_a_formula_cell_drops_the_formula() {
    let src = book("<row r=\"1\"><c r=\"A1\" s=\"4\"><f>SUM(B1:C1)</f><v>7</v></c></row>");
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 42.0).unwrap();
    let out = wb.save().unwrap();

    let mut re = Workbook::open(&out).unwrap();
    let cell = re.sheet(0).unwrap().get(at("A1")).unwrap().unwrap();
    // Формула, оставленная в ячейке, была бы пересчитана Excel'ем и затёрла бы
    // наше значение при первом же открытии.
    assert!(cell.formula.is_none(), "формула пережила правку: {cell:?}");
    assert_eq!(cell.value, CellValue::Number(42.0));
    // Стиль — не наше дело, он обязан остаться.
    assert_eq!(cell.style, Some(4));
    let xml = entry_text(&out, SHEET1);
    assert!(!xml.contains("<f>"), "текст формулы остался: {xml}");
}

#[test]
fn clear_empties_the_cell_but_keeps_it_and_its_style() {
    let src = book(
        "<row r=\"1\"><c r=\"A1\" s=\"3\" t=\"s\"><v>0</v></c>\
<c r=\"B1\"><f>A1</f><v>5</v></c></row>",
    );
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        sh.clear(at("A1")).unwrap();
        sh.clear(at("B1")).unwrap();
        // Очистка того, чего нет, — не ошибка и ячейку не создаёт.
        sh.clear(at("Z9")).unwrap();
    }
    let out = wb.save().unwrap();

    assert_eq!(addresses(&out), ["A1", "B1"]);
    let mut re = Workbook::open(&out).unwrap();
    let cell = re.sheet(0).unwrap().get(at("A1")).unwrap().unwrap();
    assert_eq!(cell.value, CellValue::Empty);
    assert_eq!(cell.style, Some(3), "стиль потерян при очистке");
    let xml = entry_text(&out, SHEET1);
    assert!(!xml.contains("t=\"s\""), "атрибут t остался: {xml}");
    assert!(!xml.contains("<f>"), "формула осталась: {xml}");
}

// --- строки ---------------------------------------------------------------

#[test]
fn shared_strings_are_deduplicated() {
    let src = build(&BookSpec {
        shared: Some(&["первая", "вторая"]),
        ..BookSpec::default()
    });
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        sh.set_string(at("A1"), "вторая").unwrap();
        sh.set_string(at("A2"), "новая").unwrap();
        sh.set_string(at("A3"), "новая").unwrap();
        sh.set_string(at("A4"), "первая").unwrap();
    }
    let out = wb.save().unwrap();

    let sst = entry_text(&out, "xl/sharedStrings.xml");
    assert_eq!(
        sst.matches("<si>").count(),
        3,
        "дедупликация не сработала: {sst}"
    );
    // Врущий счётчик хуже отсутствующего: точное число использований строк
    // известно только после обхода всех листов книги.
    assert!(!sst.contains("count="), "счётчики остались: {sst}");

    assert_eq!(reread(&out, "A1"), text("вторая"));
    assert_eq!(reread(&out, "A2"), text("новая"));
    assert_eq!(reread(&out, "A3"), text("новая"));
    assert_eq!(reread(&out, "A4"), text("первая"));

    let sheet = entry_text(&out, SHEET1);
    assert!(
        sheet.contains("<c r=\"A2\" t=\"s\"><v>2</v></c>"),
        "{sheet}"
    );
    assert!(
        sheet.contains("<c r=\"A3\" t=\"s\"><v>2</v></c>"),
        "{sheet}"
    );
}

#[test]
fn rich_text_entries_are_not_reused_for_a_plain_string() {
    // Значение у `<si><r>` то же, а оформление — чужое. Переиспользовать такую
    // запись значит записать в ячейку текст, набранный чужим шрифтом.
    let raw = build(&BookSpec::default());
    let mut pkg = Package::open(&raw, Limits::strict()).unwrap();
    let sst_part = PartName::new("/xl/sharedStrings.xml").unwrap();
    {
        let doc = pkg.dom(&sst_part).unwrap();
        let root = doc.root_element().unwrap();
        let si = doc.new_element("si").unwrap();
        let r = doc.new_element("r").unwrap();
        let t = doc.new_element("t").unwrap();
        doc.set_text(t, "жирная").unwrap();
        doc.append_child(r, t).unwrap();
        doc.append_child(si, r).unwrap();
        doc.append_child(root, si).unwrap();
    }
    let src = pkg.save().unwrap();

    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_string(at("A1"), "жирная").unwrap();
    let out = wb.save().unwrap();

    let sst = entry_text(&out, "xl/sharedStrings.xml");
    assert_eq!(
        sst.matches("<si>").count(),
        2,
        "переиспользована запись с оформлением: {sst}"
    );
    assert_eq!(reread(&out, "A1"), text("жирная"));
}

#[test]
fn the_inline_policy_never_touches_the_shared_table() {
    let src = build(&BookSpec {
        shared: Some(&["уже есть"]),
        ..BookSpec::default()
    });
    let before = entry_text(&src, "xl/sharedStrings.xml");

    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        sh.set_string_policy(StringPolicy::Inline);
        sh.set_string(at("A1"), "уже есть").unwrap();
    }
    let out = wb.save().unwrap();

    assert_eq!(
        entry_text(&out, "xl/sharedStrings.xml"),
        before,
        "политика Inline тронула общую таблицу"
    );
    let sheet = entry_text(&out, SHEET1);
    assert!(
        sheet.contains("t=\"inlineStr\"><is><t>уже есть</t></is>"),
        "встроенный текст записан не так: {sheet}"
    );
    assert_eq!(reread(&out, "A1"), text("уже есть"));
}

#[test]
fn a_missing_shared_string_part_is_created_on_demand() {
    let src = build(&BookSpec {
        shared: None,
        ..BookSpec::default()
    });
    assert!(!has_entry(&src, "xl/sharedStrings.xml"));

    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_string(at("A1"), "текст").unwrap();
    let out = wb.save().unwrap();

    // Часть без `Override` и без отношения сделала бы книгу невалидной.
    let types = entry_text(&out, "[Content_Types].xml");
    assert!(
        types.contains("sharedStrings+xml"),
        "нет типа новой части: {types}"
    );
    let rels = entry_text(&out, "xl/_rels/workbook.xml.rels");
    assert!(
        rels.contains("sharedStrings.xml"),
        "нет отношения к новой части: {rels}"
    );
    assert_eq!(reread(&out, "A1"), text("текст"));
}

#[test]
fn markup_and_unrepresentable_characters_survive() {
    let samples = [
        "a & b < c > d \" e ' f",
        "нулевой\u{0}байт",
        "возврат\rкаретки",
        "_x0041_",
        "_x005F_x0000_",
        " по краям пробелы ",
    ];
    let src = book("");

    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        for (i, s) in samples.iter().enumerate() {
            sh.set_string(CellRef::new(i as u32, 0), s).unwrap();
        }
    }
    let out = wb.save().unwrap();

    for (i, s) in samples.iter().enumerate() {
        let a1 = CellRef::new(i as u32, 0).to_a1();
        assert_eq!(
            reread(&out, &a1),
            text(s),
            "строка {s:?} не пережила запись"
        );
    }

    let sst = entry_text(&out, "xl/sharedStrings.xml");
    assert!(sst.contains("&amp;") && sst.contains("&lt;"), "{sst}");
    assert!(
        sst.contains("_x0000_"),
        "нулевой байт не закодирован: {sst}"
    );
    // Пробел по краям без `xml:space` съедается любым читателем.
    assert!(sst.contains("xml:space=\"preserve\""), "{sst}");

    // То же самое встроенным текстом.
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        sh.set_string_policy(StringPolicy::Inline);
        for (i, s) in samples.iter().enumerate() {
            sh.set_string(CellRef::new(i as u32, 0), s).unwrap();
        }
    }
    let out = wb.save().unwrap();
    for (i, s) in samples.iter().enumerate() {
        let a1 = CellRef::new(i as u32, 0).to_a1();
        assert_eq!(
            reread(&out, &a1),
            text(s),
            "строка {s:?} не пережила запись встроенным текстом"
        );
    }
}

// --- dimension ------------------------------------------------------------

#[test]
fn dimension_grows_to_cover_the_new_cell() {
    let src = build(&BookSpec {
        sheet_body: "<row r=\"2\"><c r=\"B2\"/></row>",
        dimension: Some("B2:C3"),
        ..BookSpec::default()
    });
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        assert_eq!(sh.dimension().unwrap().unwrap().to_a1(), "B2:C3");
        sh.set_number(at("E9"), 1.0).unwrap();
        sh.set_number(at("A1"), 1.0).unwrap();
        // Ячейка внутри диапазона его не меняет.
        sh.set_number(at("C3"), 1.0).unwrap();
    }
    let out = wb.save().unwrap();

    let mut re = Workbook::open(&out).unwrap();
    assert_eq!(
        re.sheet(0).unwrap().dimension().unwrap().unwrap().to_a1(),
        "A1:E9"
    );
}

#[test]
fn a_missing_dimension_is_not_invented() {
    let src = book("");
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        assert_eq!(sh.dimension().unwrap(), None);
        sh.set_number(at("E9"), 1.0).unwrap();
    }
    let out = wb.save().unwrap();
    // Держать `<dimension>` верным при всех будущих правках — обязательство,
    // которого мы на себя не брали. Отсутствующий элемент честнее выдуманного.
    assert!(!entry_text(&out, SHEET1).contains("<dimension"));
}

// --- пересчёт -------------------------------------------------------------

#[test]
fn any_edit_asks_for_a_full_recalculation() {
    let src = book("");
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 1.0).unwrap();
    let out = wb.save().unwrap();

    let xml = entry_text(&out, "xl/workbook.xml");
    assert!(xml.contains("fullCalcOnLoad=\"1\""), "{xml}");
    assert!(xml.contains("calcId=\"0\""), "{xml}");
    // Режим пересчёта — выбор пользователя, отменять его правкой ячейки не наше
    // дело.
    assert!(xml.contains("calcMode=\"manual\""), "{xml}");
}

#[test]
fn calc_pr_is_created_where_the_workbook_had_none() {
    let src = build(&BookSpec {
        calc_pr: false,
        ..BookSpec::default()
    });
    assert!(!entry_text(&src, "xl/workbook.xml").contains("<calcPr"));

    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 1.0).unwrap();
    let out = wb.save().unwrap();

    let xml = entry_text(&out, "xl/workbook.xml");
    assert!(
        xml.contains("<calcPr calcId=\"0\" fullCalcOnLoad=\"1\"/>"),
        "{xml}"
    );
    // Порядок детей `<workbook>` схемой зафиксирован: `<calcPr>` идёт до
    // `<extLst>`, а не в конец.
    let calc = xml.find("<calcPr").unwrap();
    let ext = xml.find("<extLst").unwrap();
    assert!(calc < ext, "calcPr встал после extLst: {xml}");
}

#[test]
fn the_stale_calc_chain_is_dropped_with_its_type_and_relationship() {
    let src = build(&BookSpec {
        calc_chain: true,
        ..BookSpec::default()
    });
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 1.0).unwrap();
    let out = wb.save().unwrap();

    assert!(!has_entry(&out, "xl/calcChain.xml"), "часть осталась");
    // Ссылка на несуществующую часть — самая частая причина «Excel обнаружил
    // нечитаемое содержимое». Все три места обязаны быть чисты.
    assert!(!entry_text(&out, "[Content_Types].xml").contains("calcChain"));
    assert!(!entry_text(&out, "xl/_rels/workbook.xml.rels").contains("calcChain"));
}

// --- границы и ошибки -----------------------------------------------------

#[test]
fn opening_a_book_changes_nothing() {
    let src = book("<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>");
    let mut wb = Workbook::open(&src).unwrap();
    // Чтение — тоже не правка, даже если ради него разобрана половина книги.
    let _ = wb.sheet(0).unwrap().read_all().unwrap();
    let _ = wb.sheet(0).unwrap().dimension().unwrap();
    let _ = wb.sheet(0).unwrap().get(at("A1")).unwrap();
    assert_eq!(wb.save().unwrap(), src, "открытие изменило байты");
}

#[test]
fn sheets_come_in_workbook_order_with_their_metadata() {
    let src = book("");
    let wb = Workbook::open(&src).unwrap();
    let sheets = wb.sheets();
    assert_eq!(sheets.len(), 2);
    assert_eq!(sheets[0].name, "Лист1");
    assert_eq!(sheets[0].sheet_id, 1);
    assert_eq!(sheets[0].rel_id, "rId1");
    assert_eq!(sheets[0].part.as_str(), "/xl/worksheets/sheet1.xml");
    assert_eq!(sheets[0].state, SheetState::Visible);
    assert_eq!(sheets[1].sheet_id, 7);
    assert_eq!(sheets[1].state, SheetState::Hidden);
}

#[test]
fn a_sheet_that_does_not_exist_is_an_error_not_a_panic() {
    let src = book("");
    let mut wb = Workbook::open(&src).unwrap();
    match wb.sheet(2) {
        Err(Error::Xlsx(XlsxError::SheetNotFound(s))) => assert_eq!(s, "2"),
        other => panic!("ожидался SheetNotFound, получено {other:?}"),
    }
    match wb.sheet_by_name("Третий") {
        Err(Error::Xlsx(XlsxError::SheetNotFound(s))) => assert_eq!(s, "Третий"),
        other => panic!("ожидался SheetNotFound, получено {other:?}"),
    }
    assert_eq!(wb.sheet_by_name("Второй").unwrap().name(), "Второй");
}

#[test]
fn an_address_outside_the_sheet_is_refused() {
    let src = book("");
    let mut wb = Workbook::open(&src).unwrap();
    let mut sh = wb.sheet(0).unwrap();

    // XFD1048576 — последняя ячейка листа; всё, что за ней, адресом не является.
    let last = CellRef::new(1_048_575, 16_383);
    assert_eq!(last.to_a1(), "XFD1048576");
    sh.set_number(last, 1.0).unwrap();

    for bad in [
        CellRef::new(1_048_576, 0),
        CellRef::new(0, 16_384),
        CellRef::new(u32::MAX, u32::MAX),
    ] {
        let mut refused = Vec::new();
        refused.push(sh.set_number(bad, 1.0));
        refused.push(sh.set_string(bad, "x"));
        refused.push(sh.set_bool(bad, true));
        refused.push(sh.set_error(bad, CellError::Na));
        refused.push(sh.clear(bad));
        refused.push(sh.get(bad).map(|_| ()));
        for res in refused {
            assert!(
                matches!(res, Err(Error::Xlsx(XlsxError::BadCellRef(_)))),
                "адрес {bad:?} обязан быть отвергнут, получено {res:?}"
            );
        }
    }
}

#[test]
fn a_non_finite_number_is_refused() {
    let src = book("");
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(
                    sh.set_number(at("A1"), bad),
                    Err(Error::Xlsx(XlsxError::BadNumber(_)))
                ),
                "{bad} обязано быть отвергнуто"
            );
        }
    }
    // Отвергнутая правка не имеет права оставить след.
    assert_eq!(wb.save().unwrap(), src);
}

#[test]
fn implicit_positions_are_understood_the_same_way_by_index_and_by_edit() {
    // Ни один генератор корпуса не пишет `<row>`/`<c>` без `r`, поэтому это
    // единственное место, где такая раскладка вообще проверяется. Разойдись
    // правка со сканером — правилась бы не та ячейка, а файл остался бы валиден.
    let src = book("<row><c/><c/></row><row><c/></row>");
    let mut wb = Workbook::open(&src).unwrap();
    {
        let mut sh = wb.sheet(0).unwrap();
        assert_eq!(sh.read_all().unwrap().len(), 3);
        sh.set_number(at("B1"), 7.0).unwrap();
    }
    let out = wb.save().unwrap();
    assert_eq!(reread(&out, "B1"), Some(CellValue::Number(7.0)));
    // Ячейка была найдена, а не создана: их по-прежнему три.
    let mut re = Workbook::open(&out).unwrap();
    assert_eq!(re.sheet(0).unwrap().read_all().unwrap().len(), 3);
}

#[test]
fn a_prefixed_namespace_gets_prefixed_new_elements() {
    // `<x:worksheet xmlns:x="…">` — законная запись того же документа. Новый
    // `<c>` без префикса попал бы в namespace по умолчанию, которого здесь нет.
    let src = build(&BookSpec {
        sheet_xml: Some(format!(
            "<?xml version=\"1.0\"?><x:worksheet xmlns:x=\"{SML}\">\
<x:sheetData><x:row r=\"1\"><x:c r=\"C1\"/></x:row></x:sheetData></x:worksheet>"
        )),
        ..BookSpec::default()
    });

    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 5.0).unwrap();
    let out = wb.save().unwrap();

    let xml = entry_text(&out, SHEET1);
    assert!(
        xml.contains("<x:c r=\"A1\""),
        "новая ячейка без префикса: {xml}"
    );
    assert!(
        xml.contains("<x:v>5</x:v>"),
        "новое значение без префикса: {xml}"
    );
    assert_eq!(addresses(&out), ["A1", "C1"]);
    assert_eq!(reread(&out, "A1"), Some(CellValue::Number(5.0)));
}

#[test]
fn a_sheet_without_sheet_data_gets_one_in_its_schema_place() {
    let src = build(&BookSpec {
        sheet_xml: Some(format!(
            "<?xml version=\"1.0\"?><worksheet xmlns=\"{SML}\">\
<sheetFormatPr defaultRowHeight=\"15\"/><pageMargins left=\"0.7\"/></worksheet>"
        )),
        ..BookSpec::default()
    });
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 1.0).unwrap();
    let out = wb.save().unwrap();

    let xml = entry_text(&out, SHEET1);
    let sd = xml.find("<sheetData").unwrap();
    let pm = xml.find("<pageMargins").unwrap();
    assert!(sd < pm, "sheetData встал после pageMargins: {xml}");
    assert_eq!(reread(&out, "A1"), Some(CellValue::Number(1.0)));
}

#[test]
fn two_sheets_can_be_edited_in_one_session() {
    // Правка второго листа не имеет права ни отменить правку первого, ни
    // заставить нас перечитать его из архива: дерево у каждого своё.
    let src = book("<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>");
    let mut wb = Workbook::open(&src).unwrap();
    wb.sheet(0).unwrap().set_number(at("A1"), 11.0).unwrap();
    wb.sheet_by_name("Второй")
        .unwrap()
        .set_string(at("B2"), "второй")
        .unwrap();
    // Возврат к первому листу обязан видеть уже записанное.
    {
        let mut sh = wb.sheet(0).unwrap();
        assert_eq!(
            sh.get(at("A1")).unwrap().map(|c| c.value),
            Some(CellValue::Number(11.0))
        );
        sh.set_bool(at("C3"), false).unwrap();
    }
    let out = wb.save().unwrap();

    let mut re = Workbook::open(&out).unwrap();
    assert_eq!(
        re.sheet(0).unwrap().get(at("A1")).unwrap().map(|c| c.value),
        Some(CellValue::Number(11.0))
    );
    assert_eq!(
        re.sheet(0).unwrap().get(at("C3")).unwrap().map(|c| c.value),
        Some(CellValue::Bool(false))
    );
    assert_eq!(
        re.sheet(1).unwrap().get(at("B2")).unwrap().map(|c| c.value),
        text("второй")
    );
}
