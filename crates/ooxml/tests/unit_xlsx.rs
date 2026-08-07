//! Рукотворные части SpreadsheetML.
//!
//! # Зачем не хватает корпуса
//!
//! Корпус — двадцать реальных книг трёх генераторов, и это ровно те файлы,
//! которые уже работают. Он ничего не говорит про случаи, которых в нём нет, а
//! их достаточно: неявные позиции `<row>`/`<c>`, встроенный текст `<is>`,
//! `t="d"`, конвенция `_xHHHH_`, фонетические подсказки, битые индексы строк.
//!
//! Каждый такой случай встречается в реальных файлах, просто не в наших. Здесь
//! он записан руками — одной строкой XML, потому что весь слой чтения работает
//! с байтами части и конструировать ради теста ZIP не нужно.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::dom::Document;
use ooxml::error::{Error, XlsxError};
use ooxml::xlsx::{
    Cell, CellError, CellRange, CellRef, CellType, CellValue, FormulaKind, SharedStrings,
    SheetData, Styles, scan_sheet, scan_sheet_stats, sheet_dimension,
};

fn limits() -> Limits {
    Limits::strict()
}

fn sst(xml: &str) -> SharedStrings {
    SharedStrings::parse(xml.as_bytes(), &limits()).unwrap()
}

fn scan(xml: &str) -> Vec<Cell> {
    scan_sheet(xml.as_bytes(), &SharedStrings::empty(), &limits()).unwrap()
}

fn scan_with(xml: &str, table: &SharedStrings) -> Vec<Cell> {
    scan_sheet(xml.as_bytes(), table, &limits()).unwrap()
}

/// Оборачивает содержимое `sheetData` в минимальный лист.
fn sheet(inner: &str) -> String {
    format!("<worksheet><sheetData>{inner}</sheetData></worksheet>")
}

fn addrs(cells: &[Cell]) -> Vec<String> {
    cells.iter().map(|c| c.at.to_a1()).collect()
}

// ---------------------------------------------------------------------------
// Адреса
// ---------------------------------------------------------------------------

#[test]
fn a1_notation_maps_to_zero_based_indices() {
    assert_eq!(CellRef::parse("A1").unwrap(), CellRef::new(0, 0));
    assert_eq!(
        CellRef::parse("XFD1048576").unwrap(),
        CellRef::new(1_048_575, 16_383)
    );
    assert_eq!(CellRef::new(0, 0).to_a1(), "A1");
    assert_eq!(CellRef::new(1_048_575, 16_383).to_a1(), "XFD1048576");
}

#[test]
fn addresses_outside_the_sheet_are_refused() {
    // Первая ячейка за правым краем и первая за нижним. Молчаливое переполнение
    // здесь было бы порчей данных: адрес уехал бы в другую строку.
    for s in ["XFE1", "A1048577"] {
        match CellRef::parse(s) {
            Err(Error::Xlsx(XlsxError::BadCellRef(got))) => assert_eq!(got, s),
            other => panic!("{s}: ожидался BadCellRef, получено {other:?}"),
        }
    }
}

#[test]
fn formula_syntax_is_not_an_address() {
    // `$A$1` — синтаксис ссылок в формулах; адрес ячейки абсолютным не бывает.
    // Разбирать его здесь — значит дублировать `formula/refs.rs` и разойтись
    // с ним при первой же правке.
    for s in ["1A", "A", "$A$1", "A$1", "Лист1!A1", "A1:B2", ""] {
        assert!(
            CellRef::parse(s).is_err(),
            "{s:?} не должно разбираться как адрес ячейки"
        );
    }
}

// ---------------------------------------------------------------------------
// Неявные позиции — то, чего нет в корпусе
// ---------------------------------------------------------------------------

#[test]
fn rows_and_cells_without_r_get_implicit_positions() {
    let cells = scan(&sheet(concat!(
        "<row><c><v>1</v></c><c><v>2</v></c></row>",
        "<row><c><v>3</v></c></row>",
    )));
    assert_eq!(addrs(&cells), ["A1", "B1", "A2"]);
    assert_eq!(cells[2].value, CellValue::Number(3.0));
}

#[test]
fn implicit_column_counter_resets_on_every_row() {
    // Счётчик столбцов свой у каждой строки: три ячейки в первой строке не
    // должны сдвинуть первую ячейку второй строки в столбец D.
    let cells = scan(&sheet(concat!(
        "<row><c/><c/><c/></row>",
        "<row><c/></row>"
    )));
    assert_eq!(addrs(&cells), ["A1", "B1", "C1", "A2"]);
}

#[test]
fn explicit_r_on_a_cell_wins_over_the_counter() {
    // После явного `C1` следующая безымянная ячейка обязана стать `D1`, а не
    // `B1`: неявный счётчик продолжается от того, что реально прочитано.
    let cells = scan(&sheet(r#"<row r="1"><c/><c r="C1"/><c/></row>"#));
    assert_eq!(addrs(&cells), ["A1", "C1", "D1"]);
}

#[test]
fn rows_may_come_out_of_order_and_with_holes() {
    let cells = scan(&sheet(concat!(
        r#"<row r="10"><c r="B10"><v>1</v></c></row>"#,
        r#"<row r="3"><c r="A3"><v>2</v></c></row>"#,
        // Строки без `r` продолжают счёт от предыдущей — от 3-й, а не от 10-й.
        "<row><c><v>3</v></c></row>",
    )));
    assert_eq!(addrs(&cells), ["B10", "A3", "A4"]);
}

#[test]
fn mixed_explicit_and_implicit_rows() {
    let cells = scan(&sheet(concat!(
        "<row><c/></row>",
        r#"<row r="5"><c/></row>"#,
        "<row><c/></row>",
    )));
    assert_eq!(addrs(&cells), ["A1", "A5", "A6"]);
}

#[test]
fn implicit_position_past_the_last_column_is_an_error() {
    // Столбцов ровно 16384. Продолжать счёт молча означало бы записать
    // ячейку в строку ниже.
    let mut inner = String::from(r#"<row r="1"><c r="XFD1"/>"#);
    inner.push_str("<c/></row>");
    let err = scan_sheet(sheet(&inner).as_bytes(), &SharedStrings::empty(), &limits()).unwrap_err();
    assert!(
        matches!(err, Error::Xlsx(XlsxError::BadCellRef(_))),
        "получено {err:?}"
    );
}

#[test]
fn dom_index_agrees_with_the_scanner_on_implicit_positions() {
    // Два механизма, одни правила. Разойдись они — правка попадала бы не в ту
    // ячейку, а файл при этом оставался бы валидным.
    let xml = sheet(concat!(
        "<row><c/><c/></row>",
        r#"<row r="7"><c/><c r="E7"/><c/></row>"#,
        "<row><c/></row>",
    ));
    let cells = scan(&xml);
    let doc = Document::parse(xml.into_bytes(), &limits()).unwrap();
    let idx = SheetData::build(&doc).unwrap();

    assert_eq!(idx.len(), cells.len());
    for c in &cells {
        assert!(
            idx.cell_node(c.at).is_some(),
            "индекс не знает про {}",
            c.at.to_a1()
        );
    }
    assert_eq!(addrs(&cells), ["A1", "B1", "A7", "E7", "F7", "A8"]);
}

// ---------------------------------------------------------------------------
// Типы ячеек
// ---------------------------------------------------------------------------

#[test]
fn every_cell_type_is_read() {
    let table = sst(r#"<sst><si><t>из таблицы</t></si></sst>"#);
    let cells = scan_with(
        &sheet(concat!(
            r#"<row r="1">"#,
            r#"<c r="A1"><v>1.5</v></c>"#,
            r#"<c r="B1" t="n"><v>-2</v></c>"#,
            r#"<c r="C1" t="s"><v>0</v></c>"#,
            r#"<c r="D1" t="str"><v>результат</v></c>"#,
            r#"<c r="E1" t="b"><v>1</v></c>"#,
            r#"<c r="F1" t="e"><v>#DIV/0!</v></c>"#,
            r#"<c r="G1" t="inlineStr"><is><t>внутри</t></is></c>"#,
            r#"<c r="H1" t="d"><v>2023-07-01T10:20:30Z</v></c>"#,
            "</row>",
        )),
        &table,
    );

    assert_eq!(cells[0].value, CellValue::Number(1.5));
    assert_eq!(cells[0].ty, CellType::N);
    assert_eq!(cells[1].value, CellValue::Number(-2.0));
    assert_eq!(cells[2].value, CellValue::Text("из таблицы".to_owned()));
    assert_eq!(cells[3].value, CellValue::Text("результат".to_owned()));
    assert_eq!(cells[4].value, CellValue::Bool(true));
    assert_eq!(cells[5].value, CellValue::Error(CellError::Div0));
    assert_eq!(cells[6].value, CellValue::Text("внутри".to_owned()));
    assert_eq!(cells[6].ty, CellType::InlineStr);
    // Дата отдаётся ISO-строкой как есть: календаря у ядра нет и быть не может
    // (`std::time` запрещён), а терять запись нельзя.
    assert_eq!(
        cells[7].value,
        CellValue::Text("2023-07-01T10:20:30Z".to_owned())
    );
    assert_eq!(cells[7].ty, CellType::D);
}

#[test]
fn missing_t_means_number() {
    let cells = scan(&sheet(r#"<row r="1"><c r="A1"><v>42</v></c></row>"#));
    assert_eq!(cells[0].ty, CellType::N);
    assert_eq!(cells[0].value, CellValue::Number(42.0));
}

#[test]
fn inline_string_collects_all_runs() {
    let cells = scan(&sheet(concat!(
        r#"<row r="1"><c r="A1" t="inlineStr"><is>"#,
        "<r><rPr><b/></rPr><t>жир</t></r><r><t>ный</t></r>",
        "</is></c></row>",
    )));
    assert_eq!(cells[0].value, CellValue::Text("жирный".to_owned()));
}

#[test]
fn unknown_cell_type_is_refused() {
    let err = scan_sheet(
        sheet(r#"<row r="1"><c r="A1" t="wat"><v>1</v></c></row>"#).as_bytes(),
        &SharedStrings::empty(),
        &limits(),
    )
    .unwrap_err();
    match err {
        Error::Xlsx(XlsxError::UnknownCellType(t)) => assert_eq!(t, "wat"),
        other => panic!("ожидался UnknownCellType, получено {other:?}"),
    }
}

#[test]
fn shared_string_index_out_of_range_is_an_error_not_a_panic() {
    let table = sst("<sst><si><t>единственная</t></si></sst>");
    let err = scan_sheet(
        sheet(r#"<row r="1"><c r="A1" t="s"><v>7</v></c></row>"#).as_bytes(),
        &table,
        &limits(),
    )
    .unwrap_err();
    match err {
        Error::Xlsx(XlsxError::SharedStringOutOfRange(7)) => {}
        other => panic!("ожидался SharedStringOutOfRange(7), получено {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Формулы: читаются как текст, не разбираются
// ---------------------------------------------------------------------------

#[test]
fn formulas_are_kept_verbatim() {
    let cells = scan(&sheet(concat!(
        r#"<row r="1">"#,
        r#"<c r="A1"><f>SUM(B1:B9)</f><v>45</v></c>"#,
        r#"<c r="B1"><f t="shared" ref="B1:B3" si="0">A1*2</f><v>90</v></c>"#,
        r#"<c r="C1"><f t="shared" si="0"/><v>90</v></c>"#,
        r#"<c r="D1"><f t="array" ref="D1:D9">IF(A1&gt;0,&quot;да&quot;,&quot;нет&quot;)</f></c>"#,
        r#"<c r="E1"><f t="dataTable" ref="E1:E2"/></c>"#,
        "</row>",
    )));

    let f = cells[0].formula.as_ref().unwrap();
    assert_eq!(f.text, "SUM(B1:B9)");
    assert_eq!(f.kind, FormulaKind::Normal);

    let f = cells[1].formula.as_ref().unwrap();
    assert_eq!(
        f.kind,
        FormulaKind::Shared {
            si: 0,
            master: Some(CellRange::parse("B1:B3").unwrap())
        }
    );

    let f = cells[2].formula.as_ref().unwrap();
    assert!(f.is_shared_follower(), "последователь без своего текста");

    let f = cells[3].formula.as_ref().unwrap();
    // Сущности раскрыты, но формула не разобрана — это работа вехи M10.
    assert_eq!(f.text, r#"IF(A1>0,"да","нет")"#);
    assert_eq!(
        f.kind,
        FormulaKind::Array {
            range: CellRange::parse("D1:D9").unwrap()
        }
    );

    assert_eq!(
        cells[4].formula.as_ref().unwrap().kind,
        FormulaKind::DataTable
    );
}

// ---------------------------------------------------------------------------
// Таблица общих строк
// ---------------------------------------------------------------------------

#[test]
fn shared_strings_cover_plain_rich_preserve_and_escapes() {
    let table = sst(concat!(
        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
        "<si><t>простая</t></si>",
        "<si><r><rPr><b/></rPr><t>несколько</t></r><r><t> кусков</t></r></si>",
        r#"<si><t xml:space="preserve">  с пробелами  </t></si>"#,
        "<si><t>нуль_x0000_внутри</t></si>",
        "<si><t>_x005F_x0041_</t></si>",
        "<si><t/></si>",
        "</sst>",
    ));

    assert_eq!(table.len(), 6);
    assert_eq!(table.get(0).unwrap(), "простая");
    assert_eq!(table.get(1).unwrap(), "несколько кусков");
    // `xml:space="preserve"` — пробелы значимы и не подрезаются.
    assert_eq!(table.get(2).unwrap(), "  с пробелами  ");
    assert_eq!(table.get(3).unwrap(), "нуль\u{0}внутри");
    // Экранированное подчёркивание: пользователь набрал `_x0041_` руками.
    assert_eq!(table.get(4).unwrap(), "_x0041_");
    assert_eq!(table.get(5).unwrap(), "");

    assert_eq!(table.rich_count(), 1);
    assert_eq!(table.preserved_count(), 1);
}

#[test]
fn shared_strings_ignore_phonetic_hints() {
    let table = sst(concat!(
        "<sst><si><t>東京</t>",
        r#"<rPh sb="0" eb="2"><t>とうきょう</t></rPh>"#,
        r#"<phoneticPr fontId="1"/></si></sst>"#,
    ));
    // `<rPh>` — подсказка чтения, а не часть значения; Excel показывает её
    // отдельной строкой над содержимым.
    assert_eq!(table.get(0).unwrap(), "東京");
}

#[test]
fn missing_shared_strings_part_is_not_a_crash() {
    let table = SharedStrings::empty();
    assert!(table.is_empty());
    match table.get(0) {
        Err(Error::Xlsx(XlsxError::SharedStringOutOfRange(0))) => {}
        other => panic!("ожидался SharedStringOutOfRange, получено {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Вырожденные листы
// ---------------------------------------------------------------------------

#[test]
fn degenerate_sheets_read_as_empty() {
    for xml in [
        "<worksheet><sheetData/></worksheet>",
        "<worksheet><sheetData></sheetData></worksheet>",
        // Лист вообще без `sheetData` — законен и просто пуст.
        "<worksheet/>",
        r#"<worksheet><dimension ref="A1:C3"/></worksheet>"#,
        // Строка без ячеек: так пишут строку, у которой задана только высота.
        r#"<worksheet><sheetData><row r="1" ht="30" customHeight="1"/></sheetData></worksheet>"#,
    ] {
        assert!(scan(xml).is_empty(), "{xml} должен читаться как пустой");
    }
}

#[test]
fn dimension_is_optional_and_never_required() {
    let with = r#"<worksheet><dimension ref="B2:D9"/><sheetData/></worksheet>"#;
    assert_eq!(
        sheet_dimension(with.as_bytes(), &limits())
            .unwrap()
            .map(|r| r.to_a1()),
        Some("B2:D9".to_owned())
    );
    // Отсутствие `<dimension>` — не ошибка: в корпусе его нет у трети листов.
    for xml in [
        "<worksheet><sheetData/></worksheet>",
        "<worksheet/>",
        // `<dimension>` по схеме стоит до `<sheetData>`; после — не ищем.
        r#"<worksheet><sheetData/><dimension ref="A1"/></worksheet>"#,
    ] {
        assert_eq!(sheet_dimension(xml.as_bytes(), &limits()).unwrap(), None);
    }
}

#[test]
fn a_sheet_with_only_styled_cells_still_reports_them() {
    let cells = scan(&sheet(
        r#"<row r="1"><c r="A1" s="4"/><c r="B1" s="4"><v/></c></row>"#,
    ));
    assert_eq!(cells.len(), 2);
    assert!(cells.iter().all(|c| c.value == CellValue::Empty));
    assert!(cells.iter().all(|c| c.style == Some(4)));
}

// ---------------------------------------------------------------------------
// Стили: только «это дата?»
// ---------------------------------------------------------------------------

#[test]
fn styles_answer_only_the_date_question() {
    let s = Styles::parse(
        concat!(
            "<styleSheet>",
            r#"<numFmts count="2">"#,
            r#"<numFmt numFmtId="164" formatCode="dd.mm.yyyy"/>"#,
            r#"<numFmt numFmtId="165" formatCode="[Red]#,##0.00"/>"#,
            "</numFmts>",
            r#"<cellStyleXfs count="1"><xf numFmtId="14"/></cellStyleXfs>"#,
            r#"<cellXfs count="4">"#,
            r#"<xf numFmtId="0"/>"#,
            r#"<xf numFmtId="14"/>"#,
            r#"<xf numFmtId="164"><alignment horizontal="center"/></xf>"#,
            r#"<xf numFmtId="165"/>"#,
            "</cellXfs></styleSheet>",
        )
        .as_bytes(),
        &limits(),
    )
    .unwrap();

    assert_eq!(s.xf_count(), 4, "`<xf>` из `cellStyleXfs` не считается");
    assert!(!s.is_date_style(Some(0)));
    assert!(s.is_date_style(Some(1)), "встроенный формат 14");
    assert!(s.is_date_style(Some(2)), "пользовательский dd.mm.yyyy");
    // `d` в слове `Red` внутри скобок — не признак даты.
    assert!(!s.is_date_style(Some(3)));
}

// ---------------------------------------------------------------------------
// Недоверенный вход
// ---------------------------------------------------------------------------

#[test]
fn a_million_cells_hit_a_quota_instead_of_memory() {
    // Атака: часть валидна по XML, но раздувает модель. Отказ обязан прийти от
    // квоты, а не от аллокатора.
    let mut xml = String::with_capacity(16 << 20);
    xml.push_str("<worksheet><sheetData><row r=\"1\">");
    for _ in 0..1_000_000 {
        xml.push_str(r#"<c r="A1"/>"#);
    }
    xml.push_str("</row></sheetData></worksheet>");

    let mut tight = Limits::strict();
    tight.max_nodes_per_part = 100_000;

    let err = scan_sheet(xml.as_bytes(), &SharedStrings::empty(), &tight).unwrap_err();
    assert!(err.is_limit(), "ожидалась квота, получено {err:?}");

    // И то же самое для пути через дерево.
    let err = Document::parse(xml.into_bytes(), &tight).unwrap_err();
    assert!(err.is_limit(), "ожидалась квота, получено {err:?}");
}

#[test]
fn malformed_row_numbers_are_refused() {
    for inner in [
        r#"<row r="0"><c/></row>"#,
        r#"<row r="1048577"><c/></row>"#,
        r#"<row r="abc"><c/></row>"#,
        r#"<row r="1"><c r="A0"/></row>"#,
        r#"<row r="1"><c r="1"/></row>"#,
    ] {
        let err = scan_sheet(sheet(inner).as_bytes(), &SharedStrings::empty(), &limits());
        assert!(err.is_err(), "{inner} обязано быть отвергнуто");
    }
}

#[test]
fn non_numeric_value_in_a_numeric_cell_is_refused() {
    let err = scan_sheet(
        sheet(r#"<row r="1"><c r="A1"><v>не число</v></c></row>"#).as_bytes(),
        &SharedStrings::empty(),
        &limits(),
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Xlsx(XlsxError::BadNumber(_))),
        "получено {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Статистика обхода
// ---------------------------------------------------------------------------

#[test]
fn scan_stats_count_what_the_corpus_test_reports() {
    let xml = sheet(concat!(
        r#"<row r="1"><c r="A1"><v>1</v></c><c/></row>"#,
        r#"<row><c r="A2" t="inlineStr"><is><t>текст</t></is></c></row>"#,
        r#"<row r="3"><c r="C3"><f>1+1</f><v>2</v></c></row>"#,
    ));
    let (cells, st) = scan_sheet_stats(xml.as_bytes(), &SharedStrings::empty(), &limits()).unwrap();

    assert_eq!(cells.len(), 4);
    assert_eq!(st.rows, 3);
    assert_eq!(st.rows_without_r, 1);
    assert_eq!(st.cells, 4);
    assert_eq!(st.cells_without_r, 1);
    assert_eq!(st.formulas, 1);
    assert_eq!(st.inline_strings, 1);
    assert_eq!(st.by_type[CellType::N.index()], 3);
    assert_eq!(st.by_type[CellType::InlineStr.index()], 1);
    assert_eq!(st.extent().map(|r| r.to_a1()), Some("A1:C3".to_owned()));
}
