//! Ссылки A1: модель, разбор, печать и сдвиг при структурной правке.
//!
//! # Почему абсолютность — два независимых флага
//!
//! `A1`, `$A1`, `A$1`, `$A$1` — это не «ссылка плюс модификатор», а четыре
//! разных объекта: доллар перед буквой фиксирует столбец, доллар перед цифрой —
//! строку, и они ставятся независимо. Один флаг `absolute: bool` на всю ссылку
//! неизбежно потерял бы `A$1`, а именно смешанные формы и ломаются тише всего:
//! файл открывается, формула выглядит правдоподобно, значения неверны.
//!
//! # Две разные операции, которые легко перепутать
//!
//! Над ссылками выполняются две операции, и они подчиняются **противоположным**
//! правилам относительно `$`:
//!
//! * [`Reference::shift`] — структурная правка листа (вставка и удаление строк
//!   и столбцов). Здесь `$` **не спасает**: после вставки строки над `$A$5`
//!   Excel пишет `$A$6`. Доллар фиксирует ссылку при копировании формулы, а не
//!   привязывает её к номеру строки навсегда. Если бы абсолютные ссылки не
//!   сдвигались, вставка строки тихо разрушила бы каждую из них — ровно та
//!   поломка, ради предотвращения которой этот модуль и написан.
//! * [`Reference::translate`] — перенос самой формулы в другую ячейку
//!   (копирование, заполнение вниз). Здесь `$` работает как ожидается:
//!   относительная часть едет, абсолютная стоит.
//!
//! Обе нужны; путаница между ними — самый дорогой возможный баг в этом файле,
//! поэтому они разведены по именам, а не по флагу одной функции.
//!
//! # Границы
//!
//! Столбцы — до `XFD` ([`MAX_COL`]), строки — до 1 048 576 ([`MAX_ROW`]).
//! Внутри всё 0-based, снаружи — A1. Координата, вышедшая за границу или
//! попавшая в удалённый диапазон, превращается в [`RefBody::Invalid`] — то
//! самое `#REF!`, которое пишет Excel.

use core::fmt;

/// Наибольший номер столбца, 0-based. `XFD` — 16 384-й столбец.
pub const MAX_COL: u32 = 16_383;

/// Наибольший номер строки, 0-based. Строк в Excel 1 048 576.
pub const MAX_ROW: u32 = 1_048_575;

/// Ссылка на одну ячейку. Индексы 0-based: `A1` — это `col: 0, row: 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRef {
    pub col: u32,
    pub row: u32,
    /// Перед буквой столбца стоял `$`.
    pub col_abs: bool,
    /// Перед номером строки стоял `$`.
    pub row_abs: bool,
}

impl CellRef {
    /// Относительная ссылка по 0-based координатам.
    #[must_use]
    pub const fn new(col: u32, row: u32) -> Self {
        Self {
            col,
            row,
            col_abs: false,
            row_abs: false,
        }
    }

    /// Та же ячейка, но обе координаты абсолютные.
    #[must_use]
    pub const fn absolute(self) -> Self {
        Self {
            col_abs: true,
            row_abs: true,
            ..self
        }
    }

    fn write(self, out: &mut String) {
        if self.col_abs {
            out.push('$');
        }
        write_col(self.col, out);
        if self.row_abs {
            out.push('$');
        }
        // row + 1 не переполняется: row <= MAX_ROW < u32::MAX.
        out.push_str(&self.row.saturating_add(1).to_string());
    }
}

impl fmt::Display for CellRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

/// Одна граница открытого диапазона: номер строки или столбца плюс её `$`.
///
/// Открытые диапазоны `A:A` и `1:1` не имеют второй координаты вовсе — это не
/// «ячейка с неизвестной строкой», а другой тип ссылки, и хранить его через
/// [`CellRef`] с фиктивной строкой значило бы позволить фиктивному нулю утечь
/// в сдвиг и в зависимости.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Line {
    pub idx: u32,
    pub abs: bool,
}

impl Line {
    #[must_use]
    pub const fn new(idx: u32) -> Self {
        Self { idx, abs: false }
    }
}

/// Тело ссылки — то, что стоит после `!`, если имя листа вообще было.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefBody {
    /// `#REF!` — ссылка, разрушенная удалением того, на что она указывала.
    ///
    /// Это состояние ссылки, а не значение-ошибка: у него бывает имя листа
    /// (`Лист1!#REF!`), и печатается оно вместе с ним. Прочие `#DIV/0!` и
    /// `#N/A` живут в [`crate::formula::ErrKind`].
    Invalid,
    /// Одна ячейка: `A1`, `$A$1`.
    Cell(CellRef),
    /// Прямоугольник: `A1:B2`.
    Area { from: CellRef, to: CellRef },
    /// Целые столбцы: `A:A`, `$B:$D`.
    Cols { from: Line, to: Line },
    /// Целые строки: `1:1`, `$3:$7`.
    Rows { from: Line, to: Line },
}

/// Префикс имени листа перед `!`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SheetPrefix {
    /// Указание на внешнюю книгу вместе со скобками и путём, если он был:
    /// `[1]`, `C:\отчёты\[книга.xlsx]`.
    ///
    /// Хранится сырым текстом до `]` включительно, потому что путь снаружи
    /// скобок — часть формы записи, а разбирать его этому слою незачем:
    /// он всё равно печатается обратно как есть.
    pub book: Option<String>,
    /// Имя листа. Уже без апострофов и с развёрнутым удвоением `''` → `'`.
    pub first: String,
    /// Второй лист объёмной ссылки `Лист1:Лист3!A1`.
    pub last: Option<String>,
    /// Префикс был записан в апострофах.
    ///
    /// Флаг хранится, а не вычисляется по «нужны ли кавычки»: Excel ставит их
    /// не строго по необходимости, и пересчёт разошёлся бы с исходником.
    pub quoted: bool,
}

impl SheetPrefix {
    /// Простой префикс `Лист1!` без книги и без объёмности.
    #[must_use]
    pub fn plain(name: &str) -> Self {
        Self {
            book: None,
            first: name.to_owned(),
            last: None,
            quoted: false,
        }
    }

    /// Номер внешней книги, если префикс имеет вид `[N]`.
    #[must_use]
    pub fn book_index(&self) -> Option<u32> {
        let b = self.book.as_deref()?;
        b.strip_prefix('[')?.strip_suffix(']')?.parse().ok()
    }

    /// Упоминает ли префикс лист с таким именем.
    ///
    /// Для объёмной ссылки `Лист1:Лист3` проверяются обе границы, но не листы
    /// между ними: их порядок знает только книга, а не формула.
    #[must_use]
    pub fn mentions(&self, sheet: &str) -> bool {
        self.first == sheet || self.last.as_deref() == Some(sheet)
    }

    fn write(&self, out: &mut String) {
        if self.quoted {
            out.push('\'');
        }
        if let Some(b) = &self.book {
            push_sheet_name(b, self.quoted, out);
        }
        push_sheet_name(&self.first, self.quoted, out);
        if let Some(l) = &self.last {
            out.push(':');
            push_sheet_name(l, self.quoted, out);
        }
        if self.quoted {
            out.push('\'');
        }
        out.push('!');
    }
}

/// Апостроф внутри имени листа удваивается — но только если имя вообще в
/// апострофах. Вне их удваивать нечего и незачем.
fn push_sheet_name(name: &str, quoted: bool, out: &mut String) {
    if quoted {
        for c in name.chars() {
            if c == '\'' {
                out.push('\'');
            }
            out.push(c);
        }
    } else {
        out.push_str(name);
    }
}

/// Ссылка целиком: необязательное имя листа плюс тело.
///
/// Префикс листа лежит за `Box` не ради экономии кучи, а ради **стека**: без
/// него `Reference` весит 104 байта, столько же весит `ExprKind`, и кадр
/// рекурсивного спуска раздувается до килобайтов. Ссылок с именем листа —
/// меньшинство, а глубина рекурсии упирается в размер кадра на каждой формуле.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    pub sheet: Option<Box<SheetPrefix>>,
    pub body: RefBody,
}

impl Reference {
    /// Ссылка на текущий лист.
    #[must_use]
    pub const fn local(body: RefBody) -> Self {
        Self { sheet: None, body }
    }

    /// Ссылка с именем листа.
    #[must_use]
    pub fn on_sheet(sheet: SheetPrefix, body: RefBody) -> Self {
        Self {
            sheet: Some(Box::new(sheet)),
            body,
        }
    }

    /// Ссылка на одну ячейку текущего листа.
    #[must_use]
    pub const fn cell(col: u32, row: u32) -> Self {
        Self::local(RefBody::Cell(CellRef::new(col, row)))
    }

    /// Ссылка разрушена (`#REF!`).
    #[must_use]
    pub const fn is_invalid(&self) -> bool {
        matches!(self.body, RefBody::Invalid)
    }

    /// Печатает ссылку в текст A1.
    pub fn write(&self, out: &mut String) {
        if let Some(s) = &self.sheet {
            s.write(out);
        }
        match self.body {
            RefBody::Invalid => out.push_str("#REF!"),
            RefBody::Cell(c) => c.write(out),
            RefBody::Area { from, to } => {
                from.write(out);
                out.push(':');
                to.write(out);
            }
            RefBody::Cols { from, to } => {
                write_line(from, out, write_col);
                out.push(':');
                write_line(to, out, write_col);
            }
            RefBody::Rows { from, to } => {
                write_line(from, out, write_row);
                out.push(':');
                write_line(to, out, write_row);
            }
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

fn write_line(l: Line, out: &mut String, body: fn(u32, &mut String)) {
    if l.abs {
        out.push('$');
    }
    body(l.idx, out);
}

fn write_row(idx: u32, out: &mut String) {
    out.push_str(&idx.saturating_add(1).to_string());
}

/// Номер столбца в буквы: 0 → `A`, 26 → `AA`, 16383 → `XFD`.
///
/// Система счисления здесь биективная по основанию 26 («без нуля»): после `Z`
/// идёт `AA`, а не `BA`. Обычное деление с остатком даёт неверный ответ на
/// каждом `Z`, поэтому перед делением из числа вычитается единица.
pub fn write_col(idx: u32, out: &mut String) {
    let mut buf = [0u8; 3];
    let mut n = 0usize;
    // idx <= MAX_COL, значит букв не больше трёх и буфер не переполнится.
    let mut v = u64::from(idx);
    loop {
        let rem = v % 26;
        // Смещение 0..=25 к ASCII-букве: результат всегда в 'A'..='Z'.
        #[expect(clippy::arithmetic_side_effects, reason = "rem < 26, сумма < 128")]
        let ch = b'A' + u8::try_from(rem).unwrap_or(0);
        if let Some(slot) = buf.get_mut(n) {
            *slot = ch;
        }
        n = n.saturating_add(1);
        if v < 26 || n >= 3 {
            break;
        }
        v = v.saturating_div(26).saturating_sub(1);
    }
    for i in (0..n).rev() {
        if let Some(&b) = buf.get(i) {
            out.push(char::from(b));
        }
    }
}

/// Буквы столбца в номер. `None` — не столбец или за границей `XFD`.
#[must_use]
pub fn col_from_name(name: &str) -> Option<u32> {
    let mut v: u32 = 0;
    let mut n = 0usize;
    for c in name.chars() {
        let d = match c {
            'A'..='Z' | 'a'..='z' => {
                // to_ascii_uppercase не меняет диапазон, разность заведомо 0..=25.
                #[expect(clippy::arithmetic_side_effects, reason = "c в 'A'..='Z'")]
                let d = u32::from(c.to_ascii_uppercase() as u8 - b'A') + 1;
                d
            }
            _ => return None,
        };
        v = v.checked_mul(26)?.checked_add(d)?;
        n = n.saturating_add(1);
    }
    if n == 0 || n > 3 {
        return None;
    }
    // Биективная система: 'A' == 1, а 0-based индекс на единицу меньше.
    v.checked_sub(1).filter(|&x| x <= MAX_COL)
}

// ---------------------------------------------------------------------------
// Сдвиг: структурная правка листа
// ---------------------------------------------------------------------------

/// По какой оси идёт правка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Rows,
    Cols,
}

/// Вставка или удаление строк (столбцов).
///
/// Поля [`Self::sheet`] и [`Self::local`] отвечают на вопрос «какие ссылки эта
/// правка вообще касается». Ссылка без имени листа указывает на тот лист, где
/// живёт формула, — а живёт она не обязательно там, где идёт правка. Без
/// [`Self::local`] формула с чужого листа сдвигала бы свои собственные ссылки
/// из-за правки, которой не видела.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift<'a> {
    pub axis: Axis,
    /// Первая затронутая строка (столбец), 0-based.
    pub at: u32,
    /// Сколько строк (столбцов) вставлено или удалено.
    pub count: u32,
    /// `true` — вставка, `false` — удаление.
    pub insert: bool,
    /// Имя редактируемого листа: ссылки `Лист!A1` на него тоже поедут.
    /// `None` — имя неизвестно, явные ссылки на листы не трогаются.
    pub sheet: Option<&'a str>,
    /// Формула лежит на редактируемом листе, поэтому ссылки без префикса
    /// указывают именно на него.
    pub local: bool,
}

impl<'a> Shift<'a> {
    #[must_use]
    pub const fn insert_rows(at: u32, count: u32) -> Self {
        Self::make(Axis::Rows, at, count, true)
    }

    #[must_use]
    pub const fn delete_rows(at: u32, count: u32) -> Self {
        Self::make(Axis::Rows, at, count, false)
    }

    #[must_use]
    pub const fn insert_cols(at: u32, count: u32) -> Self {
        Self::make(Axis::Cols, at, count, true)
    }

    #[must_use]
    pub const fn delete_cols(at: u32, count: u32) -> Self {
        Self::make(Axis::Cols, at, count, false)
    }

    const fn make(axis: Axis, at: u32, count: u32, insert: bool) -> Self {
        Self {
            axis,
            at,
            count,
            insert,
            sheet: None,
            local: true,
        }
    }

    /// Задаёт имя редактируемого листа.
    #[must_use]
    pub const fn on_sheet(mut self, name: &'a str) -> Self {
        self.sheet = Some(name);
        self
    }

    /// Помечает формулу как лежащую на другом листе: её ссылки без префикса
    /// эта правка не касается.
    #[must_use]
    pub const fn from_other_sheet(mut self) -> Self {
        self.local = false;
        self
    }

    /// Касается ли правка этой ссылки.
    fn applies_to(&self, r: &Reference) -> bool {
        match (&r.sheet, self.sheet) {
            (None, _) => self.local,
            (Some(p), Some(name)) => p.mentions(name),
            (Some(_), None) => false,
        }
    }

    /// Верхняя граница координаты на этой оси.
    const fn max(&self) -> u32 {
        match self.axis {
            Axis::Rows => MAX_ROW,
            Axis::Cols => MAX_COL,
        }
    }

    /// Последняя затронутая строка (столбец) при удалении, включительно.
    /// `None` — удалять нечего.
    fn last(&self) -> Option<u32> {
        self.at.checked_add(self.count)?.checked_sub(1)
    }
}

/// Новое положение одиночной координаты. `None` — координата исчезла.
fn move_index(i: u32, sh: &Shift<'_>) -> Option<u32> {
    let last = sh.last()?;
    if sh.insert {
        if i >= sh.at {
            i.checked_add(sh.count).filter(|&x| x <= sh.max())
        } else {
            Some(i)
        }
    } else if i > last {
        i.checked_sub(sh.count)
    } else if i >= sh.at {
        None
    } else {
        Some(i)
    }
}

/// Новые границы диапазона. `None` — диапазон удалён целиком.
///
/// Правила, которые здесь легко перепутать:
///
/// * вставка **на верхнюю границу** сдвигает диапазон целиком, а вставка
///   **внутри него** — расширяет. `A1:A5` + строка на позиции 1 → `A2:A6`;
///   та же вставка на позиции 3 → `A1:A6`;
/// * при удалении координата, попавшая в вырезанный кусок, схлопывается к его
///   началу: нижняя граница едет вперёд, верхняя — назад. Если они при этом
///   разошлись, от диапазона ничего не осталось.
fn move_span(from: u32, to: u32, sh: &Shift<'_>) -> Option<(u32, u32)> {
    let last = sh.last()?;
    if sh.insert {
        if sh.at <= from {
            let f = from.checked_add(sh.count).filter(|&x| x <= sh.max())?;
            let t = to.checked_add(sh.count).filter(|&x| x <= sh.max())?;
            Some((f, t))
        } else if sh.at <= to {
            // Вставка внутри диапазона: начало на месте, конец уезжает.
            Some((from, to.checked_add(sh.count).filter(|&x| x <= sh.max())?))
        } else {
            Some((from, to))
        }
    } else {
        let f = if from > last {
            from.checked_sub(sh.count)?
        } else if from >= sh.at {
            sh.at
        } else {
            from
        };
        let t = if to > last {
            to.checked_sub(sh.count)?
        } else if to >= sh.at {
            // Верхняя граница схлопывается к позиции перед вырезом. Если
            // вырез начинается с нуля, перед ним ничего нет — диапазона тоже.
            sh.at.checked_sub(1)?
        } else {
            to
        };
        if f > t { None } else { Some((f, t)) }
    }
}

impl Reference {
    /// Сдвигает ссылку после вставки или удаления строк (столбцов).
    ///
    /// Возвращает `true`, если ссылка изменилась. Абсолютность **не влияет**:
    /// см. объяснение в заголовке модуля.
    pub fn shift(&mut self, sh: &Shift<'_>) -> bool {
        if !sh.applies_to(self) || sh.count == 0 {
            return false;
        }
        let before = self.body;
        self.body = shift_body(before, sh);
        self.body != before
    }

    /// Переносит формулу в другую ячейку: относительные части едут на
    /// `d_col`/`d_row`, абсолютные остаются.
    ///
    /// Возвращает `true`, если ссылка изменилась. Выезд за границы листа даёт
    /// `#REF!` — так же, как в Excel при копировании формулы к краю листа.
    pub fn translate(&mut self, d_col: i64, d_row: i64) -> bool {
        let before = self.body;
        self.body = translate_body(before, d_col, d_row);
        self.body != before
    }
}

fn shift_body(body: RefBody, sh: &Shift<'_>) -> RefBody {
    let rows = sh.axis == Axis::Rows;
    match body {
        RefBody::Invalid => RefBody::Invalid,
        RefBody::Cell(c) => {
            let idx = if rows { c.row } else { c.col };
            match move_index(idx, sh) {
                None => RefBody::Invalid,
                Some(v) => RefBody::Cell(if rows {
                    CellRef { row: v, ..c }
                } else {
                    CellRef { col: v, ..c }
                }),
            }
        }
        RefBody::Area { from, to } => {
            let (a, b) = if rows {
                (from.row, to.row)
            } else {
                (from.col, to.col)
            };
            // Диапазон может быть записан «задом наперёд» (B2:A1) — Excel
            // такое допускает. Нормализуем на время расчёта, иначе `f > t`
            // ниже посчитало бы живой диапазон удалённым.
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            match move_span(lo, hi, sh) {
                None => RefBody::Invalid,
                Some((nlo, nhi)) => {
                    let (na, nb) = if a <= b { (nlo, nhi) } else { (nhi, nlo) };
                    RefBody::Area {
                        from: if rows {
                            CellRef { row: na, ..from }
                        } else {
                            CellRef { col: na, ..from }
                        },
                        to: if rows {
                            CellRef { row: nb, ..to }
                        } else {
                            CellRef { col: nb, ..to }
                        },
                    }
                }
            }
        }
        // Открытый диапазон живёт только на своей оси: вставка строк не
        // трогает `A:A`, потому что этот диапазон и так содержит все строки.
        RefBody::Cols { from, to } => {
            if rows {
                body
            } else {
                move_line_span(from, to, sh)
                    .map_or(RefBody::Invalid, |(f, t)| RefBody::Cols { from: f, to: t })
            }
        }
        RefBody::Rows { from, to } => {
            if rows {
                move_line_span(from, to, sh)
                    .map_or(RefBody::Invalid, |(f, t)| RefBody::Rows { from: f, to: t })
            } else {
                body
            }
        }
    }
}

fn move_line_span(from: Line, to: Line, sh: &Shift<'_>) -> Option<(Line, Line)> {
    let (lo, hi) = if from.idx <= to.idx {
        (from.idx, to.idx)
    } else {
        (to.idx, from.idx)
    };
    let (nlo, nhi) = move_span(lo, hi, sh)?;
    let (a, b) = if from.idx <= to.idx {
        (nlo, nhi)
    } else {
        (nhi, nlo)
    };
    Some((Line { idx: a, ..from }, Line { idx: b, ..to }))
}

fn translate_body(body: RefBody, d_col: i64, d_row: i64) -> RefBody {
    let cell = |c: CellRef| -> Option<CellRef> {
        let col = if c.col_abs {
            c.col
        } else {
            offset(c.col, d_col, MAX_COL)?
        };
        let row = if c.row_abs {
            c.row
        } else {
            offset(c.row, d_row, MAX_ROW)?
        };
        Some(CellRef { col, row, ..c })
    };
    let line = |l: Line, d: i64, max: u32| -> Option<Line> {
        if l.abs {
            Some(l)
        } else {
            Some(Line {
                idx: offset(l.idx, d, max)?,
                ..l
            })
        }
    };
    match body {
        RefBody::Invalid => RefBody::Invalid,
        RefBody::Cell(c) => cell(c).map_or(RefBody::Invalid, RefBody::Cell),
        RefBody::Area { from, to } => match (cell(from), cell(to)) {
            (Some(f), Some(t)) => RefBody::Area { from: f, to: t },
            _ => RefBody::Invalid,
        },
        RefBody::Cols { from, to } => {
            match (line(from, d_col, MAX_COL), line(to, d_col, MAX_COL)) {
                (Some(f), Some(t)) => RefBody::Cols { from: f, to: t },
                _ => RefBody::Invalid,
            }
        }
        RefBody::Rows { from, to } => {
            match (line(from, d_row, MAX_ROW), line(to, d_row, MAX_ROW)) {
                (Some(f), Some(t)) => RefBody::Rows { from: f, to: t },
                _ => RefBody::Invalid,
            }
        }
    }
}

/// Смещение координаты со сверкой границ листа.
fn offset(i: u32, d: i64, max: u32) -> Option<u32> {
    let v = i64::from(i).checked_add(d)?;
    u32::try_from(v).ok().filter(|&x| x <= max)
}

// ---------------------------------------------------------------------------
// Разбор
// ---------------------------------------------------------------------------

/// Символ, с которого может начинаться имя (листа, функции, определённое).
pub(crate) fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '\\'
}

/// Символ, допустимый внутри имени.
///
/// Точка входит не для красоты: реальные книги корпуса содержат лист `стр.1_4`,
/// на который Excel ссылается **без апострофов**. Имена вроде `_xlfn.XLOOKUP`
/// держатся на том же правиле.
pub(crate) fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '\\' || c == '?'
}

fn at(s: &str, i: usize) -> Option<char> {
    s.get(i..).and_then(|t| t.chars().next())
}

fn starts(s: &str, i: usize, pat: &str) -> bool {
    s.get(i..).is_some_and(|t| t.starts_with(pat))
}

fn eat(s: &str, p: &mut usize, c: char) -> bool {
    if at(s, *p) == Some(c) {
        *p = p.saturating_add(c.len_utf8());
        true
    } else {
        false
    }
}

fn scan_digits(s: &str, i: usize) -> Option<(u32, usize)> {
    let mut p = i;
    let mut v: u32 = 0;
    while let Some(d) = at(s, p).and_then(|c| c.to_digit(10)) {
        v = v.checked_mul(10)?.checked_add(d)?;
        p = p.saturating_add(1);
    }
    if p == i { None } else { Some((v, p)) }
}

fn scan_name_run(s: &str, i: usize) -> usize {
    let mut p = i;
    while let Some(c) = at(s, p) {
        if is_name_char(c) {
            p = p.saturating_add(c.len_utf8());
        } else {
            break;
        }
    }
    p
}

/// Половина ссылки: `$A$1`, `$A` или `$1`.
enum Half {
    Cell(CellRef),
    Col(Line),
    Row(Line),
}

fn scan_half(s: &str, i: usize) -> Option<(Half, usize)> {
    let mut p = i;
    let abs1 = eat(s, &mut p, '$');

    let letters_from = p;
    let mut n = 0usize;
    while n < 3 && at(s, p).is_some_and(|c| c.is_ascii_alphabetic()) {
        p = p.saturating_add(1);
        n = n.saturating_add(1);
    }
    let letters = s.get(letters_from..p)?;

    if letters.is_empty() {
        // Только номер строки: `1` или `$1`.
        let (row, p2) = scan_digits(s, p)?;
        let idx = row.checked_sub(1).filter(|&x| x <= MAX_ROW)?;
        return Some((Half::Row(Line { idx, abs: abs1 }), p2));
    }
    // Четвёртая буква подряд означает, что это не столбец, а имя: `Лист1`,
    // `ISNUMBER`. Проверка обязана быть здесь, иначе `SUMX` разобралось бы как
    // столбец `SUM` со странным хвостом.
    if at(s, p).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let col = col_from_name(letters)?;

    let mut q = p;
    let abs2 = eat(s, &mut q, '$');
    match scan_digits(s, q) {
        Some((row, p2)) => {
            let idx = row.checked_sub(1).filter(|&x| x <= MAX_ROW)?;
            Some((
                Half::Cell(CellRef {
                    col,
                    row: idx,
                    col_abs: abs1,
                    row_abs: abs2,
                }),
                p2,
            ))
        }
        // `$A$` без числа — мусор, а не столбец.
        None if abs2 => None,
        None => Some((
            Half::Col(Line {
                idx: col,
                abs: abs1,
            }),
            p,
        )),
    }
}

/// Тело ссылки после необязательного имени листа.
fn scan_body(s: &str, i: usize) -> Option<(RefBody, usize)> {
    if starts(s, i, "#REF!") {
        return Some((RefBody::Invalid, i.saturating_add(5)));
    }
    let (h1, p) = scan_half(s, i)?;
    if at(s, p) == Some(':')
        && let Some((h2, p2)) = scan_half(s, p.saturating_add(1))
    {
        // Половинки обязаны быть одного рода: `A1:B` — не диапазон, и
        // достраивать его догадками нельзя.
        match (&h1, h2) {
            (Half::Cell(a), Half::Cell(b)) => {
                return Some((RefBody::Area { from: *a, to: b }, p2));
            }
            (Half::Col(a), Half::Col(b)) => {
                return Some((RefBody::Cols { from: *a, to: b }, p2));
            }
            (Half::Row(a), Half::Row(b)) => {
                return Some((RefBody::Rows { from: *a, to: b }, p2));
            }
            _ => {}
        }
    }
    // Одинокие `A` и `1` ссылками не являются: первое — определённое имя,
    // второе — число. Диапазонами они становятся только в паре.
    match h1 {
        Half::Cell(c) => Some((RefBody::Cell(c), p)),
        Half::Col(_) | Half::Row(_) => None,
    }
}

/// Имя листа в апострофах: `'Лист 1'`, `'Ivan''s Sheet'`.
fn scan_quoted(s: &str, i: usize) -> Option<(String, usize)> {
    let mut p = i.checked_add(1)?;
    let mut out = String::new();
    loop {
        let c = at(s, p)?;
        p = p.saturating_add(c.len_utf8());
        if c == '\'' {
            // Удвоенный апостроф — экранированный апостроф, а не конец имени.
            if at(s, p) == Some('\'') {
                out.push('\'');
                p = p.saturating_add(1);
            } else {
                return Some((out, p));
            }
        } else {
            out.push(c);
        }
    }
}

/// Разбирает содержимое префикса (уже без апострофов) на книгу и листы.
fn split_prefix(text: &str) -> Option<(Option<String>, String, Option<String>)> {
    // Путь к внешней книге может сам содержать `:` (`C:\...`), поэтому книга
    // отрезается по последней `]` до всякого дробления по двоеточию.
    let (book, rest) = match text.rfind(']') {
        Some(k) => {
            let end = k.checked_add(1)?;
            (Some(text.get(..end)?.to_owned()), text.get(end..)?)
        }
        None => (None, text),
    };
    if rest.is_empty() {
        return None;
    }
    match rest.split_once(':') {
        Some((a, b)) if !a.is_empty() && !b.is_empty() => {
            Some((book, a.to_owned(), Some(b.to_owned())))
        }
        Some(_) => None,
        None => Some((book, rest.to_owned(), None)),
    }
}

/// Префикс имени листа вместе с `!`. `None` — префикса здесь нет.
fn scan_prefix(s: &str, i: usize) -> Option<(SheetPrefix, usize)> {
    if at(s, i) == Some('\'') {
        let (text, p) = scan_quoted(s, i)?;
        if at(s, p) != Some('!') {
            return None;
        }
        let (book, first, last) = split_prefix(&text)?;
        return Some((
            SheetPrefix {
                book,
                first,
                last,
                quoted: true,
            },
            p.saturating_add(1),
        ));
    }

    let mut p = i;
    let mut book = None;
    if at(s, i) == Some('[') {
        let close = s.get(i..)?.find(']')?;
        let end = i.saturating_add(close).saturating_add(1);
        book = Some(s.get(i..end)?.to_owned());
        p = end;
    }

    // Лист удалён вместе со ссылкой на него: `#REF!!A1`.
    let first_from = p;
    if starts(s, p, "#REF!") {
        p = p.saturating_add(5);
    } else {
        p = scan_name_run(s, p);
    }
    if p == first_from {
        return None;
    }
    let first = s.get(first_from..p)?.to_owned();

    let mut last = None;
    if at(s, p) == Some(':') {
        let from2 = p.saturating_add(1);
        let end2 = scan_name_run(s, from2);
        if end2 > from2 && at(s, end2) == Some('!') {
            last = Some(s.get(from2..end2)?.to_owned());
            p = end2;
        }
    }
    if at(s, p) != Some('!') {
        return None;
    }
    Some((
        SheetPrefix {
            book,
            first,
            last,
            quoted: false,
        },
        p.saturating_add(1),
    ))
}

/// Разбирает ссылку A1, начиная с позиции `i`.
///
/// Возвращает ссылку и позицию сразу за ней. `None` означает «здесь ссылки
/// нет» — не ошибку: вызывающий лексер попробует прочитать имя или число.
pub(crate) fn scan(s: &str, i: usize) -> Option<(Reference, usize)> {
    let (sheet, p) = match scan_prefix(s, i) {
        Some((sp, p)) => (Some(Box::new(sp)), p),
        None => (None, i),
    };
    let (body, end) = scan_body(s, p)?;
    // Ссылка не может продолжаться символом имени: `A1B`, `A1_x` и `A1.5` —
    // это имена (или мусор), но точно не ячейка `A1` с хвостом.
    if at(s, end).is_some_and(|c| is_name_char(c) || c == '$') {
        return None;
    }
    Some((Reference { sheet, body }, end))
}

/// Разбирает ссылку A1 целиком. Хвост после ссылки делает разбор неуспешным.
#[must_use]
pub fn parse_ref(s: &str) -> Option<Reference> {
    let (r, end) = scan(s, 0)?;
    if end == s.len() { Some(r) } else { None }
}
