//! Быстрое чтение всех ячеек листа — pull-парсером, без построения дерева.
//!
//! # Почему не через DOM
//!
//! Это не преждевременная оптимизация, а замеренное требование. Лист корпуса
//! размером 1,6 МБ разворачивается в preserving DOM объёмом 13,8 МиБ: дерево
//! хранит спаны, атрибуты и порядок каждого пробела, потому что обязано вернуть
//! файл побайтово. Для чтения значений всё это — мёртвый груз, и на книге из
//! десятка листов он превращается в сотни мегабайт.
//!
//! Поэтому чтение и правка разведены по разным механизмам:
//!
//! * читать значения — этот модуль, потоком, память O(число ячеек);
//! * править ячейку — [`crate::xlsx::sheetdata`] поверх DOM, где нужна
//!   сохранность байт.
//!
//! # Ловушка, ради которой написан этот модуль
//!
//! **Атрибут `r` у `<row>` и `<c>` необязателен.** Спецификация ECMA-376
//! разрешает его опускать, и тогда позиция вычисляется неявно: строка идёт
//! следующей за предыдущей, ячейка — следующей за предыдущей в своей строке.
//!
//! В тестовом корпусе (20 книг, 33 листа, 232 140 ячеек) `r` есть **у всех**
//! строк и **у всех** ячеек. Отсюда легко сделать вывод «да у всех же есть `r`»
//! и упростить код. Не делайте этого: корпус состоит из файлов трёх генераторов
//! (Excel 365, Google Sheets, LibreOffice), а `r` опускают ради размера
//! потоковые писатели — их в корпусе просто нет. Файл без `r` полностью
//! легален, Excel его открывает, и прочитанный без учёта неявных позиций лист
//! будет не «слегка неточным», а полностью сдвинутым.
//!
//! Неявные позиции покрыты юнит-тестами в `tests/unit_xlsx.rs`; корпусный тест
//! печатает счётчик строк и ячеек без `r` именно затем, чтобы отсутствие
//! покрытия было видно, а не подразумевалось.

use crate::error::{Result, XlsxError};
use crate::limits::Limits;
use crate::xlsx::cell::{
    Cell, CellError, CellType, CellValue, Formula, FormulaKind, parse_bool, parse_index,
    parse_number,
};
use crate::xlsx::refs::{CellRange, CellRef, parse_row_number};
use crate::xlsx::sst::{SharedStrings, TextRuns, decode_x_escapes};
use crate::xml::{Event, Reader};

/// Namespace SpreadsheetML.
pub const SML_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// Локальное имя текущего элемента читателя.
pub(crate) fn local_name<'a>(rd: &Reader<'a>) -> &'a [u8] {
    rd.element_name().local_bytes(rd.src())
}

/// Сырое значение атрибута по **полному** имени, как оно записано в файле.
///
/// Сравнение по полному имени, а не по паре (URI, локальная часть), потому что
/// нужные нам атрибуты (`r`, `t`, `s`, `si`, `ref`, `xml:space`) в OOXML всегда
/// пишутся одинаково, а разрешение namespace для каждого атрибута каждой из
/// сотен тысяч ячеек стоило бы дороже самого разбора.
///
/// Значение возвращается **недекодированным**. Все перечисленные атрибуты —
/// ASCII-токены без ссылок на сущности; `String` на каждый из них означал бы
/// три лишние аллокации на ячейку.
///
/// Внимание: результат действителен только до следующего события `Start` —
/// буфер атрибутов читателя переиспользуется.
pub(crate) fn raw_attr<'a>(rd: &Reader<'a>, name: &[u8]) -> Option<&'a [u8]> {
    let src = rd.src();
    rd.attrs()
        .iter()
        .find(|a| a.name.slice(src) == Some(name))
        .and_then(|a| a.value.slice(src))
}

/// Атрибут как строка. `None` — атрибута нет.
///
/// # Errors
///
/// [`XlsxError::BadSheetData`], если байты значения не UTF-8. Лексер валидирует
/// UTF-8 текста, но здесь мы смотрим на сырой спан в обход декодера.
pub(crate) fn attr_str<'a>(rd: &Reader<'a>, name: &[u8]) -> Result<Option<&'a str>> {
    match raw_attr(rd, name) {
        None => Ok(None),
        Some(raw) => core::str::from_utf8(raw)
            .map(Some)
            .map_err(|_| XlsxError::BadSheetData.into()),
    }
}

/// Элемент принадлежит SpreadsheetML.
///
/// Отсутствие namespace тоже принимается: рукотворные части в тестах пишутся
/// без `xmlns`, а требовать его там — значит проверять не то, что нужно.
pub(crate) fn in_sml(rd: &Reader<'_>) -> bool {
    rd.element_ns().is_none_or(|id| rd.uri(id) == Some(SML_NS))
}

/// Что удалось увидеть при обходе листа.
///
/// Счётчики существуют не для красоты отчёта: `rows_without_r` и
/// `cells_without_r`, равные нулю на всём корпусе, — это прямое доказательство,
/// что неявные позиции корпусом **не** покрыты и держатся только на
/// юнит-тестах. Без счётчика этот факт пришлось бы принимать на веру.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Элементов `<row>`.
    pub rows: u64,
    /// Из них без атрибута `r` — позиция вычислена неявно.
    pub rows_without_r: u64,
    /// Элементов `<c>`.
    pub cells: u64,
    /// Из них без атрибута `r`.
    pub cells_without_r: u64,
    /// Ячеек с непустым значением.
    pub non_empty: u64,
    /// Ячеек с формулой.
    pub formulas: u64,
    /// Из них общих (`t="shared"`).
    pub shared_formulas: u64,
    /// Из них формул массива (`t="array"`).
    pub array_formulas: u64,
    /// Ячеек со встроенным текстом `<is>`.
    pub inline_strings: u64,
    /// Гистограмма по [`CellType::index`].
    pub by_type: [u64; 7],
    /// Максимальный встреченный индекс строки.
    pub max_row: Option<u32>,
    /// Максимальный встреченный индекс столбца.
    pub max_col: Option<u32>,
}

impl ScanStats {
    /// Складывает статистику другого листа в эту.
    pub fn merge(&mut self, other: &Self) {
        self.rows = self.rows.saturating_add(other.rows);
        self.rows_without_r = self.rows_without_r.saturating_add(other.rows_without_r);
        self.cells = self.cells.saturating_add(other.cells);
        self.cells_without_r = self.cells_without_r.saturating_add(other.cells_without_r);
        self.non_empty = self.non_empty.saturating_add(other.non_empty);
        self.formulas = self.formulas.saturating_add(other.formulas);
        self.shared_formulas = self.shared_formulas.saturating_add(other.shared_formulas);
        self.array_formulas = self.array_formulas.saturating_add(other.array_formulas);
        self.inline_strings = self.inline_strings.saturating_add(other.inline_strings);
        for (a, b) in self.by_type.iter_mut().zip(other.by_type.iter()) {
            *a = a.saturating_add(*b);
        }
        self.max_row = max_opt(self.max_row, other.max_row);
        self.max_col = max_opt(self.max_col, other.max_col);
    }

    /// Размах адресов как диапазон, если хоть одна ячейка была.
    #[must_use]
    pub fn extent(&self) -> Option<CellRange> {
        Some(CellRange {
            from: CellRef::new(0, 0),
            to: CellRef::new(self.max_row?, self.max_col?),
        })
    }
}

fn max_opt(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Читает все ячейки листа.
///
/// В результат попадают **все** элементы `<c>`, включая пустые: `<c r="A1"
/// s="3"/>` — это существующая ячейка со стилем, и в корпусе таких
/// подавляющее большинство. Отбрасывать их здесь значило бы решать за
/// вызывающего.
///
/// # Errors
///
/// Ошибки XML нижнего слоя, превышение квот, а также ошибки модели: битый
/// адрес, неизвестный `t`, индекс общей строки за границей таблицы.
pub fn scan_sheet(part: &[u8], sst: &SharedStrings, limits: &Limits) -> Result<Vec<Cell>> {
    Ok(scan_sheet_stats(part, sst, limits)?.0)
}

/// То же, что [`scan_sheet`], но со статистикой обхода.
///
/// # Errors
///
/// См. [`scan_sheet`].
pub fn scan_sheet_stats(
    part: &[u8],
    sst: &SharedStrings,
    limits: &Limits,
) -> Result<(Vec<Cell>, ScanStats)> {
    let mut sc = Scanner::new(part, sst, limits);
    sc.run()?;
    Ok((sc.out, sc.stats))
}

/// Читает объявленный листом диапазон из `<dimension ref="…"/>`.
///
/// `Ok(None)` — элемента нет. Это норма: в корпусе `<dimension>` есть у 21 из
/// 33 листов, и опираться на него нельзя. Настоящий размах даёт [`ScanStats`].
///
/// Поиск прекращается на `<sheetData>`: `<dimension>` по схеме стоит раньше, и
/// читать ради него весь лист (а это мегабайты) незачем.
///
/// # Errors
///
/// Ошибки XML и [`XlsxError::BadCellRef`], если `ref` не разбирается.
pub fn sheet_dimension(part: &[u8], limits: &Limits) -> Result<Option<CellRange>> {
    let mut rd = Reader::with_limits(part, limits.clone());
    let mut depth: usize = 0;
    loop {
        match rd.next_event()? {
            Event::Start { empty, .. } => {
                let d = depth.saturating_add(1);
                let local = local_name(&rd);
                if d == 2 && in_sml(&rd) {
                    if local == b"dimension" {
                        return match attr_str(&rd, b"ref")? {
                            Some(v) => CellRange::parse(v).map(Some),
                            None => Ok(None),
                        };
                    }
                    if local == b"sheetData" {
                        return Ok(None);
                    }
                }
                if !empty {
                    depth = d;
                }
            }
            Event::End { .. } => depth = depth.saturating_sub(1),
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

/// Глубина `<row>` относительно `<sheetData>`.
const REL_ROW: usize = 1;
/// Глубина `<c>` относительно `<sheetData>`.
const REL_CELL: usize = 2;
/// Глубина `<v>`, `<f>`, `<is>` относительно `<sheetData>`.
const REL_VALUE: usize = 3;

/// Ячейка, разобранная не до конца: значение станет известно на `</c>`.
#[derive(Debug, Default)]
struct Pending {
    at: CellRef,
    ty: CellType,
    style: Option<u32>,
    kind: Option<FormulaKind>,
    f_text: String,
    v: String,
    is_text: Option<String>,
    saw_v: bool,
}

struct Scanner<'a> {
    part: &'a [u8],
    sst: &'a SharedStrings,
    limits: &'a Limits,
    out: Vec<Cell>,
    stats: ScanStats,
    /// Глубина элемента `<sheetData>`, пока он открыт.
    sd: Option<usize>,
    /// Индекс текущей строки.
    row: u32,
    /// Индекс, который получит следующая строка без атрибута `r`.
    next_row: u32,
    /// Индекс, который получит следующая ячейка без атрибута `r`.
    next_col: u32,
    cur: Option<Pending>,
    in_v: bool,
    in_f: bool,
    in_is: bool,
    runs: TextRuns,
}

impl<'a> Scanner<'a> {
    fn new(part: &'a [u8], sst: &'a SharedStrings, limits: &'a Limits) -> Self {
        Self {
            part,
            sst,
            limits,
            out: Vec::new(),
            stats: ScanStats::default(),
            sd: None,
            row: 0,
            next_row: 0,
            next_col: 0,
            cur: None,
            in_v: false,
            in_f: false,
            in_is: false,
            runs: TextRuns::new(),
        }
    }

    fn run(&mut self) -> Result<()> {
        let mut rd = Reader::with_limits(self.part, self.limits.clone());
        let mut depth: usize = 0;
        loop {
            match rd.next_event()? {
                Event::Start { empty, .. } => {
                    let d = depth.saturating_add(1);
                    let local = local_name(&rd);
                    self.open(d, local, &rd)?;
                    if empty {
                        self.close(d, local)?;
                    } else {
                        depth = d;
                    }
                }
                Event::End { .. } => {
                    let local = local_name(&rd);
                    self.close(depth, local)?;
                    depth = depth.saturating_sub(1);
                }
                Event::Text { span, .. } | Event::CData { span } => {
                    if self.in_v || self.in_f || (self.in_is && self.runs.wants_text()) {
                        let text = rd.text(span)?;
                        self.on_text(&text);
                    }
                }
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    fn on_text(&mut self, s: &str) {
        if self.in_v {
            if let Some(cur) = self.cur.as_mut() {
                cur.v.push_str(s);
            }
        } else if self.in_f {
            if let Some(cur) = self.cur.as_mut() {
                cur.f_text.push_str(s);
            }
        } else {
            self.runs.push(s);
        }
    }

    fn open(&mut self, d: usize, local: &[u8], rd: &Reader<'_>) -> Result<()> {
        let Some(sd) = self.sd else {
            if d == 2 && local == b"sheetData" && in_sml(rd) {
                self.sd = Some(d);
            }
            return Ok(());
        };
        match d.saturating_sub(sd) {
            REL_ROW if local == b"row" => self.open_row(rd)?,
            REL_CELL if local == b"c" => self.open_cell(rd)?,
            REL_VALUE if self.cur.is_some() => self.open_cell_child(local, rd)?,
            _ if self.in_is => self.runs.open(local, d, rd),
            _ => {}
        }
        Ok(())
    }

    fn open_row(&mut self, rd: &Reader<'_>) -> Result<()> {
        self.stats.rows = self.stats.rows.saturating_add(1);
        self.row = match attr_str(rd, b"r")? {
            Some(v) => parse_row_number(v)?,
            None => {
                // Позиция неявная: строка идёт следующей за предыдущей. См.
                // предупреждение в шапке модуля — это не редкий случай, а
                // просто не встретившийся в нашем корпусе.
                self.stats.rows_without_r = self.stats.rows_without_r.saturating_add(1);
                self.next_row
            }
        };
        self.next_row = self.row.saturating_add(1);
        // Счётчик столбцов свой у каждой строки — дыры в одной строке не
        // сдвигают следующую.
        self.next_col = 0;
        Ok(())
    }

    fn open_cell(&mut self, rd: &Reader<'_>) -> Result<()> {
        self.stats.cells = self.stats.cells.saturating_add(1);
        // Число ячеек упирается в ту же квоту, что и число узлов XML: одна
        // ячейка — минимум один узел, поэтому проверка не отвергает ничего,
        // что прошло бы разбор, но делает предел явным и в этом слое.
        self.limits.check_nodes(self.stats.cells)?;

        let at = match attr_str(rd, b"r")? {
            // Атрибут `r` у `<c>` — полный адрес, а не только столбец. Если он
            // есть, ему и верим целиком: он точнее, чем позиция в файле.
            Some(v) => CellRef::parse(v)?,
            None => {
                self.stats.cells_without_r = self.stats.cells_without_r.saturating_add(1);
                CellRef::checked(self.row, self.next_col)?
            }
        };
        self.next_col = at.col.saturating_add(1);
        self.stats.max_row = Some(self.stats.max_row.map_or(at.row, |m| m.max(at.row)));
        self.stats.max_col = Some(self.stats.max_col.map_or(at.col, |m| m.max(at.col)));

        let ty = match attr_str(rd, b"t")? {
            Some(v) => CellType::parse(v)?,
            None => CellType::N,
        };
        let style = match attr_str(rd, b"s")? {
            Some(v) => Some(parse_index(v)?),
            None => None,
        };
        self.cur = Some(Pending {
            at,
            ty,
            style,
            ..Pending::default()
        });
        Ok(())
    }

    fn open_cell_child(&mut self, local: &[u8], rd: &Reader<'_>) -> Result<()> {
        match local {
            b"v" => {
                self.in_v = true;
                if let Some(cur) = self.cur.as_mut() {
                    cur.saw_v = true;
                }
            }
            b"is" => {
                self.in_is = true;
                self.runs.reset();
            }
            b"f" => {
                self.in_f = true;
                let kind = self.formula_kind(rd)?;
                if let Some(cur) = self.cur.as_mut() {
                    cur.kind = Some(kind);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Разновидность формулы по атрибутам `<f>`.
    ///
    /// Неизвестное значение `t` считается обычной формулой: семантика формул —
    /// веха M10, и отказ читать лист из-за незнакомого вида формулы был бы
    /// отказом там, где значение ячейки уже лежит рядом, в `<v>`.
    fn formula_kind(&self, rd: &Reader<'_>) -> Result<FormulaKind> {
        let at = self.cur.as_ref().map_or(CellRef::new(0, 0), |c| c.at);
        let range = match attr_str(rd, b"ref")? {
            Some(v) => Some(CellRange::parse(v)?),
            None => None,
        };
        let si = match attr_str(rd, b"si")? {
            Some(v) => Some(parse_index(v)?),
            None => None,
        };
        Ok(match attr_str(rd, b"t")? {
            Some("shared") => match si {
                Some(si) => FormulaKind::Shared { si, master: range },
                // Без `si` группу не собрать; текст при этом на месте, и
                // считать такую формулу обычной — меньшее из зол.
                None => FormulaKind::Normal,
            },
            Some("array") => FormulaKind::Array {
                range: range.unwrap_or_else(|| CellRange::single(at)),
            },
            Some("dataTable") => FormulaKind::DataTable,
            _ => FormulaKind::Normal,
        })
    }

    fn close(&mut self, d: usize, local: &[u8]) -> Result<()> {
        let Some(sd) = self.sd else {
            return Ok(());
        };
        match d.saturating_sub(sd) {
            0 if local == b"sheetData" => {
                self.sd = None;
                self.cur = None;
            }
            REL_CELL if local == b"c" => self.finish_cell()?,
            REL_VALUE => match local {
                b"v" => self.in_v = false,
                b"f" => self.in_f = false,
                b"is" => {
                    self.in_is = false;
                    let (text, _, _) = self.runs.finish();
                    if let Some(cur) = self.cur.as_mut() {
                        cur.is_text = Some(text);
                    }
                }
                _ => {}
            },
            _ if self.in_is => self.runs.close(local, d),
            _ => {}
        }
        Ok(())
    }

    fn finish_cell(&mut self) -> Result<()> {
        let Some(cur) = self.cur.take() else {
            return Ok(());
        };
        if let Some(slot) = self.stats.by_type.get_mut(cur.ty.index()) {
            *slot = slot.saturating_add(1);
        }
        if cur.is_text.is_some() {
            self.stats.inline_strings = self.stats.inline_strings.saturating_add(1);
        }

        let formula = match cur.kind {
            None => None,
            Some(kind) => {
                self.stats.formulas = self.stats.formulas.saturating_add(1);
                match kind {
                    FormulaKind::Shared { .. } => {
                        self.stats.shared_formulas = self.stats.shared_formulas.saturating_add(1);
                    }
                    FormulaKind::Array { .. } => {
                        self.stats.array_formulas = self.stats.array_formulas.saturating_add(1);
                    }
                    FormulaKind::Normal | FormulaKind::DataTable => {}
                }
                Some(Formula {
                    text: cur.f_text,
                    kind,
                })
            }
        };

        let value = value_of(cur.ty, &cur.v, cur.saw_v, cur.is_text, self.sst)?;
        if !value.is_empty() {
            self.stats.non_empty = self.stats.non_empty.saturating_add(1);
        }
        self.out.push(Cell {
            at: cur.at,
            value,
            ty: cur.ty,
            style: cur.style,
            formula,
        });
        Ok(())
    }
}

/// Собирает значение ячейки из её типа и прочитанного содержимого.
fn value_of(
    ty: CellType,
    v: &str,
    saw_v: bool,
    is_text: Option<String>,
    sst: &SharedStrings,
) -> Result<CellValue> {
    // `<is>` побеждает `<v>` независимо от `t`: некоторые генераторы пишут
    // встроенный текст, забыв поставить `t="inlineStr"`, и текст, лежащий
    // прямо перед нами, — более надёжный источник, чем отсутствующий атрибут.
    if let Some(text) = is_text {
        return Ok(CellValue::Text(text));
    }
    if !saw_v {
        // Ячейка без `<v>` — существующая, но пустая. Так выглядит клетка,
        // созданная только ради стиля или ширины столбца.
        return Ok(CellValue::Empty);
    }
    match ty {
        CellType::N => {
            if v.trim().is_empty() {
                Ok(CellValue::Empty)
            } else {
                Ok(CellValue::Number(parse_number(v)?))
            }
        }
        CellType::S => {
            let i = parse_index(v)?;
            Ok(CellValue::Text(sst.get(i)?.to_owned()))
        }
        // `t="str"` и `t="d"` кладут содержимое прямо в `<v>`, а значит оно
        // прошло обычное XML-экранирование — но не конвенцию `_xHHHH_`, её
        // Excel применяет и здесь.
        CellType::Str | CellType::D => Ok(CellValue::Text(decode_x_escapes(v))),
        CellType::B => Ok(CellValue::Bool(parse_bool(v)?)),
        CellType::E => Ok(CellValue::Error(CellError::parse(v.trim())?)),
        CellType::InlineStr => {
            // `t="inlineStr"` без `<is>` — сломанная ячейка. Значение при этом
            // всё же есть смысл отдать: `<v>` рядом.
            Ok(CellValue::Text(decode_x_escapes(v)))
        }
    }
}

/// Собирает объединённые диапазоны листа из `<mergeCells>`.
///
/// Вынесено отдельным проходом, а не встроено в [`scan_sheet`]: `<mergeCells>`
/// стоит **после** `<sheetData>`, и сканер значений завершает работу раньше,
/// чем до него дойдёт. Второй проход по той же части дешевле, чем тащить
/// состояние через весь разбор ячеек ради элемента, который нужен не всем.
///
/// # Errors
///
/// Ошибки XML; [`XlsxError::BadCellRef`] на неразбираемом `ref`.
pub fn scan_merges(part: &[u8], limits: &Limits) -> Result<Vec<CellRange>> {
    let mut rd = Reader::with_limits(part, limits.clone());
    let mut out = Vec::new();
    loop {
        match rd.next_event()? {
            Event::Start { .. } => {
                if local_name(&rd) == b"mergeCell"
                    && in_sml(&rd)
                    && let Some(v) = attr_str(&rd, b"ref")?
                {
                    out.push(CellRange::parse(v)?);
                }
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
    }
}

/// Диапазон столбцов с общей шириной.
#[derive(Debug, Clone, PartialEq)]
pub struct ColSpan {
    /// Первый столбец диапазона, 0-based.
    pub from: u32,
    /// Последний столбец диапазона включительно, 0-based.
    pub to: u32,
    /// Ширина в «символах» — единица Excel, привязанная к ширине цифры
    /// шрифта по умолчанию.
    pub width: f64,
    pub hidden: bool,
}

/// Объявленная геометрия листа.
///
/// Это не результат вёрстки, а то, что записано в файле. Экспорт переносит эти
/// числа как есть; вычислять ширину по содержимому — уже работа движка
/// типографики, которого у ядра нет.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SheetLayout {
    /// `<sheetFormatPr defaultColWidth=>`, в символах.
    pub default_col_width: Option<f64>,
    /// `<sheetFormatPr defaultRowHeight=>`, в пунктах.
    pub default_row_height: Option<f64>,
    pub cols: Vec<ColSpan>,
    /// Высоты строк в пунктах: `(строка 0-based, высота)`.
    pub row_heights: Vec<(u32, f64)>,
    /// Скрытые строки, 0-based.
    pub hidden_rows: Vec<u32>,
}

impl SheetLayout {
    /// Ширина столбца в символах с учётом диапазонов и значения по умолчанию.
    #[must_use]
    pub fn col_width(&self, col: u32) -> Option<f64> {
        for c in &self.cols {
            if col >= c.from && col <= c.to {
                return if c.hidden { Some(0.0) } else { Some(c.width) };
            }
        }
        self.default_col_width
    }

    /// Высота строки в пунктах.
    #[must_use]
    pub fn row_height(&self, row: u32) -> Option<f64> {
        if self.hidden_rows.binary_search(&row).is_ok() {
            return Some(0.0);
        }
        self.row_heights
            .binary_search_by_key(&row, |&(r, _)| r)
            .ok()
            .and_then(|i| self.row_heights.get(i))
            .map(|&(_, h)| h)
            .or(self.default_row_height)
    }
}

/// Читает объявленную геометрию листа: ширины столбцов и высоты строк.
///
/// Отдельный проход, а не часть [`scan_sheet`]: геометрия нужна только
/// экспорту, а платить за неё при каждом чтении значений незачем.
///
/// # Errors
///
/// Ошибки XML; [`XlsxError::BadCellRef`] на неразбираемом номере строки.
pub fn scan_layout(part: &[u8], limits: &Limits) -> Result<SheetLayout> {
    let mut rd = Reader::with_limits(part, limits.clone());
    let mut out = SheetLayout::default();
    loop {
        match rd.next_event()? {
            Event::Start { .. } => {
                if !in_sml(&rd) {
                    continue;
                }
                match local_name(&rd) {
                    b"sheetFormatPr" => {
                        out.default_col_width = attr_f64(&rd, b"defaultColWidth")?;
                        out.default_row_height = attr_f64(&rd, b"defaultRowHeight")?;
                    }
                    b"col" => {
                        // `min`/`max` в файле 1-based и включительные.
                        let min = attr_f64(&rd, b"min")?.unwrap_or(1.0);
                        let max = attr_f64(&rd, b"max")?.unwrap_or(min);
                        if let Some(width) = attr_f64(&rd, b"width")? {
                            out.cols.push(ColSpan {
                                from: (min.max(1.0) as u32).saturating_sub(1),
                                to: (max.max(1.0) as u32).saturating_sub(1),
                                width,
                                hidden: attr_bool(&rd, b"hidden"),
                            });
                        }
                    }
                    b"row" => {
                        let Some(r) = attr_str(&rd, b"r")? else {
                            continue;
                        };
                        // `parse_row_number` уже возвращает нулевой индекс.
                        // Вычесть здесь ещё единицу — значит сдвинуть все
                        // высоты на строку вверх; текст поедет в чужие строки,
                        // и выглядеть это будет как дефект отрисовки, хотя
                        // сломан разбор.
                        let row = parse_row_number(r)?;
                        if let Some(h) = attr_f64(&rd, b"ht")? {
                            out.row_heights.push((row, h));
                        }
                        if attr_bool(&rd, b"hidden") {
                            out.hidden_rows.push(row);
                        }
                    }
                    b"mergeCells" => break,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out.row_heights.sort_unstable_by_key(|&(r, _)| r);
    out.hidden_rows.sort_unstable();
    Ok(out)
}

/// Числовой атрибут. `None` — атрибута нет или он не число.
fn attr_f64(rd: &Reader<'_>, name: &[u8]) -> Result<Option<f64>> {
    Ok(attr_str(rd, name)?.and_then(|v| v.trim().parse::<f64>().ok()))
}

/// Логический атрибут OOXML: истина — это `1` или `true`.
fn attr_bool(rd: &Reader<'_>, name: &[u8]) -> bool {
    matches!(raw_attr(rd, name), Some(b"1" | b"true"))
}

/// Показывает ли Excel сетку на этом листе (`<sheetView showGridLines=>`).
///
/// Значение по умолчанию — истина, но у бланков оно снято, и это ключевой
/// факт для показа документа: без сетки видны только рамки из стилей, а
/// экспорт, рисующий границу у каждой ячейки, топит форму в сплошной сетке.
///
/// # Errors
///
/// Ошибки XML.
pub fn sheet_shows_grid_lines(part: &[u8], limits: &Limits) -> Result<bool> {
    let mut rd = Reader::with_limits(part, limits.clone());
    loop {
        match rd.next_event()? {
            Event::Start { .. } => {
                if local_name(&rd) == b"sheetView" && in_sml(&rd) {
                    return Ok(!matches!(
                        raw_attr(&rd, b"showGridLines"),
                        Some(b"0" | b"false")
                    ));
                }
                if local_name(&rd) == b"sheetData" && in_sml(&rd) {
                    return Ok(true);
                }
            }
            Event::Eof => return Ok(true),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn scan(xml: &str) -> Vec<Cell> {
        scan_sheet(xml.as_bytes(), &SharedStrings::empty(), &Limits::strict()).unwrap()
    }

    #[test]
    fn dimension_is_optional() {
        let with = r#"<worksheet><dimension ref="A1:C3"/><sheetData/></worksheet>"#;
        assert_eq!(
            sheet_dimension(with.as_bytes(), &Limits::strict())
                .unwrap()
                .map(|r| r.to_a1()),
            Some("A1:C3".to_owned())
        );
        let without = "<worksheet><sheetData/></worksheet>";
        assert_eq!(
            sheet_dimension(without.as_bytes(), &Limits::strict()).unwrap(),
            None
        );
    }

    #[test]
    fn empty_sheet_yields_no_cells() {
        assert!(scan("<worksheet><sheetData/></worksheet>").is_empty());
        assert!(scan("<worksheet/>").is_empty());
    }

    #[test]
    fn styled_but_valueless_cells_are_still_cells() {
        let cells = scan(
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="3"/></row></sheetData></worksheet>"#,
        );
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, CellValue::Empty);
        assert_eq!(cells[0].style, Some(3));
    }
}
