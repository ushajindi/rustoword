//! Лист книги: чтение значений и правка ячеек.
//!
//! # Два пути чтения и один путь правки
//!
//! [`Sheet::read_all`] идёт потоковым сканером и дерева не строит. Лист на
//! 1,6 МБ разворачивается в DOM объёмом 13,5 МиБ; платить это за чтение
//! значений незачем. Дерево появляется ровно в момент первой правки этого листа
//! и только для него — соседние листы остаются нераспакованными.
//!
//! После первой правки чтение идёт уже через дерево: лист сериализуется во
//! временный буфер и сканируется тем же сканером. Так чтение и запись физически
//! не могут разойтись — прочитанное всегда получено из того же XML, который
//! уйдёт в файл.
//!
//! # Порядок вставки — не косметика
//!
//! Новая `<c>` вставляется в порядке возрастания столбца внутри `<row>`, новая
//! `<row>` — в порядке возрастания номера внутри `<sheetData>`. Схема этого не
//! требует, и наивная реализация дописывала бы в конец: так проще и так работает
//! на первом же примере.
//!
//! Так делать нельзя. Все три генератора корпуса (Excel, LibreOffice, Google
//! Sheets) пишут отсортированный порядок, читатели на него опираются, и Excel
//! на нарушении иногда молчит, а иногда сообщает о нечитаемом содержимом и
//! «восстанавливает» файл, выбрасывая при этом всё, чего не понял. Отсортированный
//! порядок — единственная форма, про которую известно, что её принимают все.
//!
//! # Атрибут `r` ставится всегда
//!
//! У `<row>` и `<c>` он необязателен: без него позиция вычисляется по порядку
//! следования. Именно поэтому новый элемент обязан его иметь — иначе адрес
//! нашей ячейки зависел бы от соседей, и чужая правка (или наша следующая
//! вставка) молча сдвинула бы её.
//!
//! # Ячейка с формулой
//!
//! Запись значения в ячейку, где есть `<f>`, **удаляет формулу**. Иначе Excel
//! при первом же пересчёте вычислит её заново и затрёт то, что мы записали, —
//! то есть правка выглядела бы применённой ровно до открытия файла. Формула
//! удаляется целиком, вместе с её разновидностью (`shared`, `array`); ячейки
//! той же общей группы при этом не трогаются.
//!
//! # Чего правка не делает
//!
//! Не трогает стиль (`s`), не трогает соседние ячейки, не трогает `<mergeCell>`,
//! условное форматирование и проверки ввода. Ячейка, ставшая пустой после
//! [`Sheet::clear`], остаётся в файле со своим стилем: `<c r="A1" s="3"/>` — это
//! существующая ячейка, и удалять её значило бы потерять оформление.

use crate::dom::{Document, NodeId};
use crate::error::{Result, XlsxError};
use crate::xlsx::cell::{Cell, CellError};
use crate::xlsx::refs::{CellRange, CellRef, parse_row_number};
use crate::xlsx::scan::{
    SML_NS, SheetLayout, scan_layout, scan_merges, scan_sheet, sheet_dimension,
};
use crate::xlsx::workbook::Workbook;

/// Куда `set_string` кладёт текст.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StringPolicy {
    /// В `xl/sharedStrings.xml`, в ячейке — `t="s"` и индекс записи.
    ///
    /// Так пишут все три генератора корпуса, и так дешевле: повторяющийся текст
    /// хранится один раз. Цена — правка второй части книги.
    #[default]
    SharedTable,
    /// Прямо в ячейку: `t="inlineStr"` и `<is><t>…</t></is>`.
    ///
    /// Нужен там, где трогать общую таблицу нежелательно: правка не выходит за
    /// пределы одного листа, и запись архива с таблицей строк остаётся
    /// побайтово прежней.
    Inline,
}

/// Лист открытой книги.
///
/// Живёт ровно столько, сколько нужно на серию операций: держит книгу
/// изменяемо, поэтому два листа одновременно не открыть. Это не ограничение
/// реализации, а свойство задачи — правка листа может дописать общую таблицу
/// строк, то есть затронуть книгу целиком.
#[derive(Debug)]
pub struct Sheet<'w, 'a> {
    pub(crate) wb: &'w mut Workbook<'a>,
    pub(crate) at: usize,
}

impl Sheet<'_, '_> {
    /// Имя листа, как его видит пользователь.
    #[must_use]
    pub fn name(&self) -> &str {
        self.wb.meta(self.at).map_or("", |m| m.name.as_str())
    }

    /// Объявленный листом диапазон из `<dimension ref="…"/>`.
    ///
    /// `Ok(None)` — элемента нет, и это норма: в корпусе `<dimension>` есть у 21
    /// листа из 33. Настоящий размах данных даёт [`Sheet::read_all`]; здесь
    /// возвращается именно объявление, потому что править надо его.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки и XML; [`XlsxError::BadCellRef`] на неразбираемом `ref`.
    pub fn dimension(&mut self) -> Result<Option<CellRange>> {
        let i = self.at;
        self.wb
            .with_sheet_bytes(i, |data, _, limits| sheet_dimension(data, limits))
    }

    /// Читает все ячейки листа потоком, без построения дерева.
    ///
    /// В результат попадают и пустые ячейки: `<c r="A1" s="3"/>` — существующая
    /// ячейка со стилем, и решать за вызывающего, нужна ли она ему, здесь не
    /// место.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки, XML и модели: битый адрес, неизвестный `t`, индекс
    /// общей строки за границей таблицы.
    pub fn read_all(&mut self) -> Result<Vec<Cell>> {
        let i = self.at;
        self.wb.with_sheet_bytes(i, scan_sheet)
    }

    /// Объединённые диапазоны листа.
    ///
    /// Без них экспорт превращает объединённую шапку в набор пустых клеток
    /// рядом с текстом — таблица формально верна, а читается неправильно.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки и XML; [`XlsxError::BadCellRef`] на битом `ref`.
    pub fn merges(&mut self) -> Result<Vec<CellRange>> {
        let i = self.at;
        self.wb
            .with_sheet_bytes(i, |data, _, limits| scan_merges(data, limits))
    }

    /// Объявленная геометрия листа: ширины столбцов и высоты строк.
    ///
    /// Возвращается то, что записано в файле, без попытки что-либо вычислить.
    /// Для бланков это принципиально: у формы МВД все 68 столбцов шириной
    /// 1,33 символа, и без этих чисел она разъезжается в нечитаемую сетку.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки и XML.
    pub fn layout(&mut self) -> Result<SheetLayout> {
        let i = self.at;
        self.wb
            .with_sheet_bytes(i, |data, _, limits| scan_layout(data, limits))
    }

    /// Читает одну ячейку. `Ok(None)` — элемента `<c>` с таким адресом нет.
    ///
    /// Стоит целого прохода по листу — ровно столько же, сколько
    /// [`Sheet::read_all`]. Это сделано намеренно: отдельный путь чтения одной
    /// ячейки означал бы вторую реализацию правил формата (неявные позиции,
    /// типы, встроенный текст), а две реализации рано или поздно разъезжаются.
    /// Для выборки многих ячеек берите [`Sheet::read_all`].
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`], если адрес вне листа; ошибки чтения.
    pub fn get(&mut self, at: CellRef) -> Result<Option<Cell>> {
        check_ref(at)?;
        let cells = self.read_all()?;
        Ok(cells.into_iter().find(|c| c.at == at))
    }

    /// Записывает число.
    ///
    /// Атрибут `t` снимается: отсутствие `t` и `t="n"` — это одно и то же, а
    /// пишут все именно отсутствие.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`] на адресе вне листа;
    /// [`XlsxError::BadNumber`], если число не конечно — ни бесконечность, ни
    /// NaN в `<v>` записать нечем; ошибки правки дерева.
    pub fn set_number(&mut self, at: CellRef, v: f64) -> Result<()> {
        let text = format_number(v)?;
        let cell = self.prepare(at)?;
        let doc = self.wb.sheet_dom(self.at)?;
        let _ = doc.remove_attr(cell, None, "t")?;
        set_value(doc, cell, &text)?;
        self.finish(at)
    }

    /// Записывает строку по действующей политике ([`Sheet::set_string_policy`]).
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`]; ошибки правки листа и общей таблицы строк.
    pub fn set_string(&mut self, at: CellRef, s: &str) -> Result<()> {
        match self.wb.policy(self.at) {
            StringPolicy::SharedTable => self.set_shared_string(at, s),
            StringPolicy::Inline => self.set_inline_string(at, s),
        }
    }

    /// Записывает логическое значение: `t="b"` и `<v>0|1</v>`.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`]; ошибки правки дерева.
    pub fn set_bool(&mut self, at: CellRef, b: bool) -> Result<()> {
        let cell = self.prepare(at)?;
        let doc = self.wb.sheet_dom(self.at)?;
        doc.set_attr(cell, "t", "b")?;
        set_value(doc, cell, if b { "1" } else { "0" })?;
        self.finish(at)
    }

    /// Записывает ошибку вычисления: `t="e"` и `<v>#REF!</v>`.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`]; ошибки правки дерева.
    pub fn set_error(&mut self, at: CellRef, e: CellError) -> Result<()> {
        let cell = self.prepare(at)?;
        let doc = self.wb.sheet_dom(self.at)?;
        doc.set_attr(cell, "t", "e")?;
        set_value(doc, cell, e.as_str())?;
        self.finish(at)
    }

    /// Опустошает ячейку, сохраняя её саму и её стиль.
    ///
    /// Удаляются `<v>`, `<f>`, `<is>` и атрибут `t`. Сама `<c>` остаётся:
    /// в ней может лежать единственная ссылка на формат, а `<dimension>` и
    /// адреса соседей от её исчезновения поехали бы. Ячейки, которой не было,
    /// правка не создаёт — очищать нечего.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`]; ошибки правки дерева.
    pub fn clear(&mut self, at: CellRef) -> Result<()> {
        check_ref(at)?;
        let i = self.at;
        let existing = {
            let doc = self.wb.sheet_dom(i)?;
            match sheet_data_node(doc) {
                Some(sd) => match find_row(doc, sd, at.row)? {
                    Some(row) => find_cell(doc, row, at)?,
                    None => None,
                },
                None => None,
            }
        };
        let Some(cell) = existing else {
            return Ok(());
        };
        let doc = self.wb.sheet_dom(i)?;
        let _ = strip_content(doc, cell)?;
        let _ = doc.remove_attr(cell, None, "t")?;
        self.wb.mark_edited();
        Ok(())
    }

    /// Меняет способ записи строк для этого листа.
    pub fn set_string_policy(&mut self, p: StringPolicy) {
        self.wb.set_policy(self.at, p);
    }

    // --- внутреннее --------------------------------------------------------

    /// Строка в общую таблицу, в ячейку — её индекс.
    fn set_shared_string(&mut self, at: CellRef, s: &str) -> Result<()> {
        check_ref(at)?;
        // Таблица правится ДО листа: если строку записать некуда, лист останется
        // нетронутым, и книга не окажется в полуправленом состоянии.
        let idx = self.wb.intern_string(s)?;
        let cell = self.prepare(at)?;
        let doc = self.wb.sheet_dom(self.at)?;
        doc.set_attr(cell, "t", "s")?;
        set_value(doc, cell, &idx.to_string())?;
        self.finish(at)
    }

    /// Строка прямо в ячейку.
    fn set_inline_string(&mut self, at: CellRef, s: &str) -> Result<()> {
        let enc = encode_x_escapes(s);
        let cell = self.prepare(at)?;
        let doc = self.wb.sheet_dom(self.at)?;
        doc.set_attr(cell, "t", "inlineStr")?;
        let is_name = prefixed(doc, cell, "is");
        let is = doc.new_element(&is_name)?;
        let t_name = prefixed(doc, cell, "t");
        let t = doc.new_element(&t_name)?;
        if enc.starts_with(' ') || enc.ends_with(' ') {
            doc.set_attr(t, "xml:space", "preserve")?;
        }
        doc.set_text(t, &enc)?;
        doc.append_child(is, t)?;
        doc.append_child(cell, is)?;
        self.finish(at)
    }

    /// Готовит ячейку к записи значения: находит или создаёт `<c>` и выносит
    /// из неё всё старое.
    ///
    /// Возвращает узел `<c>`.
    fn prepare(&mut self, at: CellRef) -> Result<NodeId> {
        check_ref(at)?;
        let i = self.at;
        let doc = self.wb.sheet_dom(i)?;
        let sd = ensure_sheet_data(doc)?;
        let row = ensure_row(doc, sd, at.row)?;
        let cell = ensure_cell(doc, row, at)?;
        let _ = strip_content(doc, cell)?;
        Ok(cell)
    }

    /// Общий хвост записи: раздвинуть объявленный диапазон и пометить книгу
    /// изменённой.
    fn finish(&mut self, at: CellRef) -> Result<()> {
        let doc = self.wb.sheet_dom(self.at)?;
        expand_dimension(doc, at)?;
        self.wb.mark_edited();
        Ok(())
    }
}

// --- поиск и вставка узлов -------------------------------------------------

/// Элемент SpreadsheetML с данным локальным именем.
///
/// Namespace проверяется, но его отсутствие принимается: рукотворные части в
/// тестах пишутся без `xmlns`, а чужой namespace с тем же локальным именем —
/// это уже не наш элемент. Правило дословно то же, что в
/// [`crate::xlsx::sheetdata`]: разъехаться им нельзя.
pub(crate) fn is_sml_named(doc: &Document, n: NodeId, local: &[u8]) -> bool {
    if doc.local_name(n) != Some(local) {
        return false;
    }
    matches!(doc.element_uri(n), None | Some(SML_NS))
}

/// Первый дочерний элемент SpreadsheetML с данным локальным именем.
pub(crate) fn find_child_named(doc: &Document, parent: NodeId, local: &[u8]) -> Option<NodeId> {
    doc.children(parent).find(|&c| is_sml_named(doc, c, local))
}

/// Имя нового элемента с тем же префиксом, что у соседа по дереву.
///
/// В корпусе SpreadsheetML всегда лежит в namespace по умолчанию, и префикса
/// нет ни у кого. Но `<x:worksheet xmlns:x="…">` — законная запись того же
/// документа, и новый `<c>` в ней обязан быть `<x:c>`: без префикса он попал бы
/// в namespace по умолчанию, которого в таком файле нет, и перестал бы быть
/// ячейкой.
pub(crate) fn prefixed(doc: &Document, like: NodeId, local: &str) -> String {
    let qname = doc.qname(like).unwrap_or(b"");
    let Some(colon) = qname.iter().position(|&b| b == b':') else {
        return local.to_owned();
    };
    let prefix = qname
        .get(..colon)
        .and_then(|p| core::str::from_utf8(p).ok());
    match prefix {
        Some(p) => format!("{p}:{local}"),
        None => local.to_owned(),
    }
}

/// Узел `<sheetData>`, если он в листе есть.
fn sheet_data_node(doc: &Document) -> Option<NodeId> {
    let root = doc.root_element().ok()?;
    find_child_named(doc, root, b"sheetData")
}

/// Элементы `<worksheet>`, которые по схеме идут ПОСЛЕ `<sheetData>`.
///
/// Список нужен для единственной цели — вставить `<sheetData>` на его законное
/// место в листе, где его почему-то нет. Порядок детей `<worksheet>` схемой
/// зафиксирован, и элемент, оказавшийся не там, делает часть невалидной.
const AFTER_SHEET_DATA: &[&[u8]] = &[
    b"sheetCalcPr",
    b"sheetProtection",
    b"protectedRanges",
    b"scenarios",
    b"autoFilter",
    b"sortState",
    b"dataConsolidate",
    b"customSheetViews",
    b"mergeCells",
    b"phoneticPr",
    b"conditionalFormatting",
    b"dataValidations",
    b"hyperlinks",
    b"printOptions",
    b"pageMargins",
    b"pageSetup",
    b"headerFooter",
    b"rowBreaks",
    b"colBreaks",
    b"customProperties",
    b"cellWatches",
    b"ignoredErrors",
    b"smartTags",
    b"drawing",
    b"drawingHF",
    b"picture",
    b"oleObjects",
    b"controls",
    b"webPublishItems",
    b"tableParts",
    b"extLst",
];

/// Находит `<sheetData>`, создавая его, если листу его не досталось.
fn ensure_sheet_data(doc: &mut Document) -> Result<NodeId> {
    let root = doc.root_element()?;
    if let Some(sd) = find_child_named(doc, root, b"sheetData") {
        return Ok(sd);
    }
    let name = prefixed(doc, root, "sheetData");
    let node = doc.new_element(&name)?;
    match first_child_in(doc, root, AFTER_SHEET_DATA) {
        Some(anchor) => doc.insert_before(anchor, node)?,
        None => doc.append_child(root, node)?,
    }
    Ok(node)
}

/// Первый дочерний элемент, чьё локальное имя есть в списке.
pub(crate) fn first_child_in(doc: &Document, parent: NodeId, names: &[&[u8]]) -> Option<NodeId> {
    doc.children(parent)
        .find(|&c| doc.local_name(c).is_some_and(|l| names.contains(&l)))
}

/// Ищет `<row>` с данным номером.
///
/// Правила вычисления неявной позиции те же, что в
/// [`crate::xlsx::sheetdata::SheetData::build`]: строка без атрибута `r`
/// получает номер, следующий за предыдущей.
fn find_row(doc: &Document, sd: NodeId, row: u32) -> Result<Option<NodeId>> {
    let mut next: u32 = 0;
    for n in doc.children(sd) {
        if !is_sml_named(doc, n, b"row") {
            continue;
        }
        let r = match doc.attr(n, None, "r") {
            Some(v) => parse_row_number(&v)?,
            None => next,
        };
        next = r.saturating_add(1);
        if r == row {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// Находит `<row>`, создавая её на месте, сохраняющем возрастающий порядок.
fn ensure_row(doc: &mut Document, sd: NodeId, row: u32) -> Result<NodeId> {
    let mut anchor: Option<NodeId> = None;
    let mut next: u32 = 0;
    for n in doc.children(sd) {
        if !is_sml_named(doc, n, b"row") {
            continue;
        }
        let r = match doc.attr(n, None, "r") {
            Some(v) => parse_row_number(&v)?,
            None => next,
        };
        next = r.saturating_add(1);
        if r == row {
            return Ok(n);
        }
        // Якорем становится ПЕРВАЯ строка с большим номером: вставка перед ней
        // сохраняет возрастающий порядок, если он был. Если строки в файле шли
        // вразнобой, порядок мы не чиним — чинить чужую раскладку правкой одной
        // ячейки значило бы переписать весь лист.
        if r > row && anchor.is_none() {
            anchor = Some(n);
        }
    }

    let name = prefixed(doc, sd, "row");
    let node = doc.new_element(&name)?;
    doc.set_attr(node, "r", &row.saturating_add(1).to_string())?;
    match anchor {
        Some(a) => doc.insert_before(a, node)?,
        None => doc.append_child(sd, node)?,
    }
    Ok(node)
}

/// Ищет `<c>` с данным адресом внутри строки.
fn find_cell(doc: &Document, row: NodeId, at: CellRef) -> Result<Option<NodeId>> {
    let mut next: u32 = 0;
    for n in doc.children(row) {
        if !is_sml_named(doc, n, b"c") {
            continue;
        }
        let a = match doc.attr(n, None, "r") {
            Some(v) => CellRef::parse(&v)?,
            None => CellRef::checked(at.row, next)?,
        };
        next = a.col.saturating_add(1);
        if a == at {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// Находит `<c>`, создавая её на месте, сохраняющем возрастающий порядок.
fn ensure_cell(doc: &mut Document, row: NodeId, at: CellRef) -> Result<NodeId> {
    let mut anchor: Option<NodeId> = None;
    let mut next: u32 = 0;
    for n in doc.children(row) {
        if !is_sml_named(doc, n, b"c") {
            continue;
        }
        let a = match doc.attr(n, None, "r") {
            Some(v) => CellRef::parse(&v)?,
            None => CellRef::checked(at.row, next)?,
        };
        next = a.col.saturating_add(1);
        if a == at {
            return Ok(n);
        }
        if a.col > at.col && anchor.is_none() {
            anchor = Some(n);
        }
    }

    let name = prefixed(doc, row, "c");
    let node = doc.new_element(&name)?;
    // Адрес ставится всегда, хотя схема его не требует: без него позиция
    // ячейки определялась бы соседями, и вставка следующей сдвинула бы эту.
    doc.set_attr(node, "r", &at.to_a1())?;
    match anchor {
        Some(a) => doc.insert_before(a, node)?,
        None => doc.append_child(row, node)?,
    }
    Ok(node)
}

/// Выносит из ячейки всё, что относится к её прежнему содержимому.
///
/// Возвращает `true`, если у ячейки была формула. Формула удаляется вместе со
/// значением: оставить её значило бы отдать наше значение на затирание первому
/// же пересчёту.
fn strip_content(doc: &mut Document, cell: NodeId) -> Result<bool> {
    let victims: Vec<NodeId> = doc
        .children(cell)
        .filter(|&n| {
            is_sml_named(doc, n, b"f") || is_sml_named(doc, n, b"v") || is_sml_named(doc, n, b"is")
        })
        .collect();
    let mut had_formula = false;
    for n in victims {
        if is_sml_named(doc, n, b"f") {
            had_formula = true;
        }
        doc.remove(n)?;
    }
    Ok(had_formula)
}

/// Кладёт в ячейку `<v>` с готовым текстом.
///
/// Предполагается, что [`strip_content`] уже отработал: старого `<v>` нет.
fn set_value(doc: &mut Document, cell: NodeId, text: &str) -> Result<()> {
    let name = prefixed(doc, cell, "v");
    let v = doc.new_element(&name)?;
    doc.set_text(v, text)?;
    doc.append_child(cell, v)
}

/// Раздвигает `<dimension ref="…"/>`, если новая ячейка вышла за его границы.
///
/// Отсутствующий `<dimension>` не создаётся. Он необязателен (в корпусе его нет
/// у 12 листов из 33), а создание означало бы взять на себя обязательство
/// держать его верным при всех последующих правках — включая чужие.
fn expand_dimension(doc: &mut Document, at: CellRef) -> Result<()> {
    let root = doc.root_element()?;
    let Some(node) = find_child_named(doc, root, b"dimension") else {
        return Ok(());
    };
    let Some(text) = doc.attr(node, None, "ref").map(|v| v.into_owned()) else {
        return Ok(());
    };
    let cur = CellRange::parse(&text)?;
    if cur.contains(at) {
        return Ok(());
    }
    let wider = CellRange {
        from: CellRef::new(cur.from.row.min(at.row), cur.from.col.min(at.col)),
        to: CellRef::new(cur.to.row.max(at.row), cur.to.col.max(at.col)),
    };
    doc.set_attr(node, "ref", &wider.to_a1())
}

// --- значения --------------------------------------------------------------

/// Отвергает адрес вне листа.
fn check_ref(at: CellRef) -> Result<()> {
    if at.is_valid() {
        return Ok(());
    }
    let r = at.row.saturating_add(1);
    let c = at.col.saturating_add(1);
    Err(XlsxError::BadCellRef(format!("R{r}C{c}")).into())
}

/// Насколько длинной может быть десятичная запись, прежде чем станет
/// осмысленной экспоненциальная.
///
/// Кратчайшая точная запись `f64` укладывается в 17 значащих цифр, то есть в
/// 24 байта вместе со знаком, точкой и нулями. Всё, что длиннее, — это число
/// вроде `1e300`, чья десятичная форма занимает три сотни цифр.
const MAX_PLAIN_DIGITS: usize = 24;

/// Печатает число так, как его пишет Excel.
///
/// `Display` для `f64` в Rust даёт кратчайшую запись, которая при обратном
/// разборе даёт **ровно тот же** `f64`, и никогда не использует экспоненту. Это
/// то, что нужно: `424242.5` обязано остаться `424242.5`, а не стать
/// `4.242425e5`, которое Excel хоть и прочтёт, но которого он никогда не пишет.
///
/// Экспонента включается только там, где десятичная запись вырождается в сотни
/// цифр. Форма приводится к виду Excel: заглавная `E` и явный знак порядка.
///
/// # Errors
///
/// [`XlsxError::BadNumber`], если число не конечно: ни бесконечность, ни NaN в
/// `<v>` не представимы (ошибки вычисления живут в `t="e"`).
pub(crate) fn format_number(v: f64) -> Result<String> {
    if !v.is_finite() {
        return Err(XlsxError::BadNumber(format!("{v}")).into());
    }
    let plain = format!("{v}");
    if plain.len() <= MAX_PLAIN_DIGITS && plain.parse::<f64>().is_ok_and(|back| back == v) {
        return Ok(plain);
    }
    let sci = format!("{v:E}");
    let out = match sci.split_once('E') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}E+{e}"),
        _ => sci,
    };
    // Последний рубеж: запись обязана читаться обратно в то же число. Если бы
    // она этим свойством не обладала, правка молча меняла бы данные.
    if out.parse::<f64>().is_ok_and(|back| back == v) {
        Ok(out)
    } else {
        Err(XlsxError::BadNumber(out).into())
    }
}

/// Кодирует строку конвенцией Excel `_xHHHH_` — операция, обратная
/// [`crate::xlsx::sst::decode_x_escapes`].
///
/// XML 1.0 не умеет представлять управляющие символы: ни `&#0;`, ни литеральный
/// `\0` в документ не положить. Excel записывает такой символ как `_x0000_`.
///
/// # Почему экранируется и само подчёркивание
///
/// Пользовательский текст `_x0041_` при обратном чтении неотличим от escape'а.
/// Excel решает это, записывая ведущее подчёркивание как `_x005F_`, и здесь
/// делается ровно то же: если с этой позиции начинается что-то, похожее на
/// `_xHHHH_`, подчёркивание экранируется. Правило намеренно грубее, чем у
/// декодера (тот раскрывает только escape'ы символов, которые Excel обязан был
/// экранировать): лишнее экранирование безвредно, недостающее — потеря данных.
#[must_use]
pub(crate) fn encode_x_escapes(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i: usize = 0;
    while let Some(ch) = s.get(i..).and_then(|rest| rest.chars().next()) {
        if looks_like_escape(b, i) {
            out.push_str("_x005F_");
            i = i.saturating_add(1);
            continue;
        }
        let c = ch as u32;
        if c < 0x20 || !is_xml_char(c) {
            // Четыре шестнадцатеричные цифры — ровно та форма, которую пишет
            // Excel; символов вне BMP среди непредставимых не бывает.
            out.push_str(&format!("_x{c:04X}_"));
        } else {
            out.push(ch);
        }
        i = i.saturating_add(ch.len_utf8());
    }
    out
}

/// С позиции `i` начинается последовательность вида `_xHHHH_`.
fn looks_like_escape(b: &[u8], i: usize) -> bool {
    if b.get(i) != Some(&b'_') || b.get(i.saturating_add(1)) != Some(&b'x') {
        return false;
    }
    let from = i.saturating_add(2);
    let to = from.saturating_add(4);
    if b.get(to) != Some(&b'_') {
        return false;
    }
    b.get(from..to)
        .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
}

/// Символ допустим в XML 1.0 — ровно продукция `Char` спецификации.
const fn is_xml_char(c: u32) -> bool {
    matches!(c, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

/// Раскладка `<sheetData>`: номера строк и адреса ячеек в порядке документа.
///
/// Существует ради проверки, ради которой затевалась вся возня с якорями:
/// вставка обязана сохранять возрастающий порядок, а увидеть это можно только
/// обойдя дерево. Живёт здесь, а не в тестовом файле, потому что повторяет
/// правила неявных позиций — вторая их копия начала бы жить своей жизнью.
/// Возвращает пары «номер строки (0-based), адреса её ячеек».
///
/// # Errors
///
/// [`XlsxError::BadCellRef`] на неразбираемом адресе или номере строки.
pub fn sheet_layout(doc: &Document) -> Result<Vec<(u32, Vec<CellRef>)>> {
    let Some(sd) = sheet_data_node(doc) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut next_row: u32 = 0;
    for row in doc.children(sd) {
        if !is_sml_named(doc, row, b"row") {
            continue;
        }
        let r = match doc.attr(row, None, "r") {
            Some(v) => parse_row_number(&v)?,
            None => next_row,
        };
        next_row = r.saturating_add(1);
        let mut cells = Vec::new();
        let mut next_col: u32 = 0;
        for c in doc.children(row) {
            if !is_sml_named(doc, c, b"c") {
                continue;
            }
            let a = match doc.attr(c, None, "r") {
                Some(v) => CellRef::parse(&v)?,
                None => CellRef::checked(r, next_col)?,
            };
            next_col = a.col.saturating_add(1);
            cells.push(a);
        }
        out.push((r, cells));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use crate::xlsx::sst::decode_x_escapes;

    #[test]
    fn escapes_round_trip_through_the_decoder() {
        // Свойство, без которого правка меняла бы данные: то, что мы записали,
        // обязано прочитаться нами же дословно.
        for s in [
            "обычный текст",
            "&<>\"'",
            "\u{0}",
            "строка\rещё",
            "таб\tи\nперевод",
            "_x0041_",
            "_x005F_x0000_",
            "___",
            "_x00",
            "_xZZZZ_",
            "",
        ] {
            let enc = encode_x_escapes(s);
            assert_eq!(decode_x_escapes(&enc), s, "не пережило кодирование: {s:?}");
        }
    }

    #[test]
    fn control_characters_become_excel_escapes() {
        assert_eq!(encode_x_escapes("a\u{0}b"), "a_x0000_b");
        assert_eq!(encode_x_escapes("a\rb"), "a_x000D_b");
        // Обычный текст не должен обрастать ничем.
        assert_eq!(encode_x_escapes("привет"), "привет");
    }

    #[test]
    fn numbers_print_without_an_exponent_where_excel_does_not_use_one() {
        assert_eq!(format_number(424_242.5).unwrap(), "424242.5");
        assert_eq!(format_number(1.0).unwrap(), "1");
        assert_eq!(format_number(0.0).unwrap(), "0");
        assert_eq!(format_number(-0.1).unwrap(), "-0.1");
        assert_eq!(format_number(1e-7).unwrap(), "0.0000001");
        assert!(format_number(f64::NAN).is_err());
        assert!(format_number(f64::INFINITY).is_err());
    }

    #[test]
    fn extreme_magnitudes_switch_to_the_excel_exponent_form() {
        assert_eq!(format_number(1e300).unwrap(), "1E+300");
        assert_eq!(format_number(5e-324).unwrap(), "5E-324");
    }

    #[test]
    fn every_printed_number_reads_back_as_the_same_bits() {
        let samples = [
            0.1,
            0.1 + 0.2,
            1.0 / 3.0,
            f64::MIN_POSITIVE,
            f64::MAX,
            -1.5e-200,
            123_456_789.987_654_32,
        ];
        for v in samples {
            let s = format_number(v).unwrap();
            assert_eq!(s.parse::<f64>().unwrap(), v, "разошлось на {v}: {s}");
        }
    }
}
