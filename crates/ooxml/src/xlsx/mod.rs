//! Чтение SpreadsheetML — содержимое частей `.xlsx`.
//!
//! # Граница слоя
//!
//! Всё, что здесь есть, работает с **байтами части**, а не с пакетом. Ни одна
//! функция этого модуля не знает, что такое ZIP, `[Content_Types].xml` или
//! отношение OPC: на вход подаётся `&[u8]` уже распакованной части.
//!
//! Это не случайность и не временное упрощение. Чтение ячейки не должно
//! зависеть от того, откуда взялись байты — из архива, из памяти или из строки
//! в тесте. Прямые следствия:
//!
//! * юнит-тест на неявные позиции `<row>`/`<c>` пишется как одна строка XML, а
//!   не как сконструированный ZIP;
//! * модель можно развивать, не дожидаясь слоя OPC, и наоборот;
//! * фасад книги (`workbook`/`worksheet`) остаётся тонким: он достаёт часть из
//!   пакета и зовёт то, что лежит здесь.
//!
//! # Что где
//!
//! | Модуль | Отвечает за |
//! |---|---|
//! | [`refs`] | адрес `A1` ↔ пара индексов, границы листа |
//! | [`cell`] | значение ячейки, её тип, формула как текст |
//! | [`sst`] | `xl/sharedStrings.xml` — таблица общих строк |
//! | [`scan`] | быстрый обход листа потоком, без дерева |
//! | [`sheetdata`] | индекс «адрес → узел DOM» для правки |
//! | [`styles`] | `xl/styles.xml` в объёме «это дата?» |
//!
//! # Два способа читать лист
//!
//! [`scan_sheet`] идёт pull-парсером и не строит дерево: лист на 1,6 МБ
//! разворачивается в DOM объёмом 13,8 МиБ, и платить это за чтение значений
//! незачем. [`SheetData`] строит индекс поверх уже разобранного [`Document`] и
//! нужен там, где лист собираются **править**: править можно только узел.
//!
//! # Главная ловушка формата
//!
//! Атрибут `r` у `<row>` и `<c>` необязателен; без него позиция вычисляется по
//! порядку следования. Подробности и то, почему на «да у всех же есть `r`»
//! полагаться нельзя, — в шапке [`scan`].
//!
//! # Пример
//!
//! ```
//! use ooxml::Limits;
//! use ooxml::xlsx::{CellValue, SharedStrings, scan_sheet};
//!
//! let sst = SharedStrings::parse(
//!     "<sst><si><t>привет</t></si></sst>".as_bytes(),
//!     &Limits::strict(),
//! )?;
//! let sheet = r#"<worksheet><sheetData>
//!     <row><c t="s"><v>0</v></c><c><v>42</v></c></row>
//! </sheetData></worksheet>"#;
//!
//! let cells = scan_sheet(sheet.as_bytes(), &sst, &Limits::strict())?;
//! // Ни у `<row>`, ни у `<c>` нет атрибута `r` — позиции вычислены неявно.
//! assert_eq!(cells[0].at.to_a1(), "A1");
//! assert_eq!(cells[0].value, CellValue::Text("привет".to_owned()));
//! assert_eq!(cells[1].at.to_a1(), "B1");
//! assert_eq!(cells[1].value, CellValue::Number(42.0));
//! # Ok::<(), ooxml::Error>(())
//! ```

pub mod cell;
pub mod refs;
pub mod scan;
pub mod sheetdata;
pub mod sst;
pub mod styles;

pub use cell::{Cell, CellError, CellType, CellValue, Formula, FormulaKind};
pub use refs::{COLS, CellRange, CellRef, MAX_COL, MAX_ROW, ROWS};
pub use scan::{SML_NS, ScanStats, scan_sheet, scan_sheet_stats, sheet_dimension};
pub use sheetdata::SheetData;
pub use sst::{SharedStrings, decode_x_escapes};
pub use styles::{Styles, format_code_is_date};

use crate::error::Result;
use crate::limits::Limits;

/// Имя части с таблицей общих строк.
pub const SHARED_STRINGS_PART: &str = "xl/sharedStrings.xml";

/// Имя части с таблицей стилей.
pub const STYLES_PART: &str = "xl/styles.xml";

/// Имя части с описанием книги.
pub const WORKBOOK_PART: &str = "xl/workbook.xml";

/// Разбирает `xl/sharedStrings.xml`.
///
/// Функция-обёртка существует ради симметрии фасада: вызывающий не должен
/// помнить, у какого из типов есть `parse`, а у какого нет.
///
/// # Errors
///
/// См. [`SharedStrings::parse`].
pub fn parse_shared_strings(part: &[u8], limits: &Limits) -> Result<SharedStrings> {
    SharedStrings::parse(part, limits)
}

/// Разбирает `xl/styles.xml`.
///
/// # Errors
///
/// См. [`Styles::parse`].
pub fn parse_styles(part: &[u8], limits: &Limits) -> Result<Styles> {
    Styles::parse(part, limits)
}
