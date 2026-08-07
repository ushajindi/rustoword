//! Индекс `sheetData`: адрес ячейки → узел дерева.
//!
//! # Зачем нужен отдельно от [`crate::xlsx::scan`]
//!
//! Сканирование отвечает на вопрос «что лежит в ячейках» и работает потоком.
//! Этот модуль отвечает на другой вопрос — «где в дереве лежит ячейка A5» — и
//! без дерева существовать не может: правка обязана менять узел, а не текст.
//!
//! Такой индекс нужен вехе M9 (правка ячейки) и типизированному API листа.
//! Строить его имеет смысл только для тех частей, которые действительно правят:
//! DOM листа в 8–10 раз больше самого XML.
//!
//! # Та же ловушка, что и в сканере
//!
//! Атрибут `r` у `<row>` и `<c>` **необязателен**, строки могут идти не по
//! порядку и с дырами. Правила вычисления неявной позиции здесь ровно те же,
//! что в [`crate::xlsx::scan`], и разъезжаться они не имеют права: индекс,
//! построенный по одним правилам, и значение, прочитанное по другим, дали бы
//! правку не той ячейки — самый неприятный вид ошибки, потому что файл после
//! неё остаётся валидным.

use crate::dom::{Document, NodeId};
use crate::error::Result;
use crate::xlsx::refs::{CellRef, parse_row_number};
use crate::xlsx::scan::SML_NS;

/// Отображение адресов на узлы `<c>` внутри `<sheetData>`.
#[derive(Debug, Clone, Default)]
pub struct SheetData {
    sheet_data: Option<NodeId>,
    /// Пары (индекс строки, узел `<row>`) в порядке появления.
    rows: Vec<(u32, NodeId)>,
    /// Пары (адрес, узел `<c>`), отсортированные по адресу для поиска.
    cells: Vec<(CellRef, NodeId)>,
}

impl SheetData {
    /// Строит индекс по разобранному листу.
    ///
    /// Отсутствие `<sheetData>` не ошибка: лист без него законен и просто пуст.
    ///
    /// # Errors
    ///
    /// [`crate::error::XlsxError::BadCellRef`] — атрибут `r` не разбирается или
    /// адрес вне листа.
    pub fn build(doc: &Document) -> Result<Self> {
        let mut out = Self::default();
        let root = doc.root_element()?;
        let Some(sd) = find_child(doc, root, b"sheetData") else {
            return Ok(out);
        };
        out.sheet_data = Some(sd);

        let mut next_row: u32 = 0;
        for row_node in doc.children(sd) {
            if !is_named(doc, row_node, b"row") {
                continue;
            }
            let row = match doc.attr(row_node, None, "r") {
                Some(v) => parse_row_number(&v)?,
                None => next_row,
            };
            next_row = row.saturating_add(1);
            out.rows.push((row, row_node));

            let mut next_col: u32 = 0;
            for cell_node in doc.children(row_node) {
                if !is_named(doc, cell_node, b"c") {
                    continue;
                }
                let at = match doc.attr(cell_node, None, "r") {
                    Some(v) => CellRef::parse(&v)?,
                    None => CellRef::checked(row, next_col)?,
                };
                next_col = at.col.saturating_add(1);
                out.cells.push((at, cell_node));
            }
        }

        // Сортировка стабильная: при повторном адресе (файл сломан, но открыт)
        // первым остаётся тот, что встретился в документе раньше, и поиск даёт
        // его же — то есть поведение детерминировано, а не «как повезёт».
        out.cells.sort_by_key(|&(at, _)| at);
        Ok(out)
    }

    /// Узел `<sheetData>`, если он в листе есть.
    #[must_use]
    pub const fn sheet_data_node(&self) -> Option<NodeId> {
        self.sheet_data
    }

    /// Узел `<c>` по адресу.
    #[must_use]
    pub fn cell_node(&self, at: CellRef) -> Option<NodeId> {
        let i = self.cells.binary_search_by_key(&at, |&(a, _)| a).ok()?;
        self.cells.get(i).map(|&(_, n)| n)
    }

    /// Узел `<row>` по индексу строки.
    #[must_use]
    pub fn row_node(&self, row: u32) -> Option<NodeId> {
        self.rows.iter().find(|&&(r, _)| r == row).map(|&(_, n)| n)
    }

    /// Все ячейки в порядке адресов.
    #[must_use]
    pub fn cells(&self) -> &[(CellRef, NodeId)] {
        &self.cells
    }

    /// Все строки в порядке появления в файле.
    #[must_use]
    pub fn rows(&self) -> &[(u32, NodeId)] {
        &self.rows
    }

    /// Число проиндексированных ячеек.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Ячеек нет.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// Элемент SpreadsheetML с данным локальным именем.
///
/// Namespace проверяется, но его отсутствие принимается: рукотворные части в
/// тестах пишутся без `xmlns`, а чужой namespace с тем же локальным именем —
/// это уже не наш элемент.
fn is_named(doc: &Document, n: NodeId, local: &[u8]) -> bool {
    if doc.local_name(n) != Some(local) {
        return false;
    }
    matches!(doc.element_uri(n), None | Some(SML_NS))
}

fn find_child(doc: &Document, parent: NodeId, local: &[u8]) -> Option<NodeId> {
    doc.children(parent).find(|&c| is_named(doc, c, local))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use crate::limits::Limits;

    fn doc(xml: &str) -> Document {
        Document::parse(xml.as_bytes().to_vec(), &Limits::strict()).unwrap()
    }

    #[test]
    fn index_finds_cells_by_address() {
        let d = doc(concat!(
            "<worksheet><sheetData>",
            r#"<row r="3"><c r="B3"/><c r="D3"/></row>"#,
            r#"<row r="1"><c r="A1"/></row>"#,
            "</sheetData></worksheet>"
        ));
        let idx = SheetData::build(&d).unwrap();
        assert_eq!(idx.len(), 3);
        assert!(idx.cell_node(CellRef::parse("D3").unwrap()).is_some());
        assert!(idx.cell_node(CellRef::parse("C3").unwrap()).is_none());
        // Строки шли не по порядку — индекс обязан находить обе.
        assert!(idx.row_node(0).is_some());
        assert!(idx.row_node(2).is_some());
    }

    #[test]
    fn missing_sheet_data_is_not_an_error() {
        let idx = SheetData::build(&doc("<worksheet/>")).unwrap();
        assert!(idx.is_empty());
        assert!(idx.sheet_data_node().is_none());
    }
}
