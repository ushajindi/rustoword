//! Адресация ячеек в стиле A1.
//!
//! # Почему здесь нет абсолютности
//!
//! `$A$1` — синтаксис **формул**, а не адресов. Внутри `sheetData` доллара не
//! бывает никогда: атрибут `r` у `<c>` всегда относительный, потому что он не
//! ссылка, а имя ячейки. Полная модель ссылки (абсолютность строки и столбца,
//! имя листа, диапазоны с открытым концом) живёт в `formula/refs.rs` и нужна
//! только разбору формул.
//!
//! Разделение намеренное: если бы адрес ячейки умел быть абсолютным, каждая
//! запись в индекс `sheetData` таскала бы два лишних бита, а каждое сравнение
//! адресов обязано было бы решать, считать ли `$A$1` и `A1` одной ячейкой.
//! Здесь адрес — просто пара чисел.
//!
//! # Нумерация
//!
//! Внутри — 0-based (`A1` → строка 0, столбец 0), снаружи — A1. Так принято
//! везде в ядре: индексы массивов и индексы ячеек не должны различаться на
//! единицу, иначе ошибка на единицу становится вопросом времени.

use crate::error::{Result, XlsxError};

/// Максимальный индекс столбца: `XFD`, 0-based.
pub const MAX_COL: u32 = 16_383;

/// Максимальный индекс строки: 1048576-я строка, 0-based.
pub const MAX_ROW: u32 = 1_048_575;

/// Столбцов в листе: `MAX_COL + 1`.
pub const COLS: u32 = 16_384;

/// Строк в листе: `MAX_ROW + 1`.
pub const ROWS: u32 = 1_048_576;

/// Больше трёх букв столбца не бывает: `XFD` — предел, `AAAA` заведомо вне
/// листа. Ограничение стоит до накопления значения, чтобы длинная строка букв
/// не могла переполнить `u32` раньше, чем сработает проверка диапазона.
const MAX_COL_LETTERS: usize = 3;

/// Больше семи цифр номера строки не бывает: 1048576 — предел.
const MAX_ROW_DIGITS: usize = 7;

/// Адрес ячейки: пара 0-based индексов.
///
/// Поле `row` объявлено первым не случайно: производный `Ord` сравнивает поля
/// в порядке объявления, а естественный порядок обхода листа — построчный.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CellRef {
    /// Индекс строки, 0-based. `A1` → 0.
    pub row: u32,
    /// Индекс столбца, 0-based. `A1` → 0.
    pub col: u32,
}

impl CellRef {
    /// Адрес без проверки границ.
    ///
    /// Нужен там, где границы уже проверены (например, разбором `r`), и в
    /// константах. Для данных из файла берите [`CellRef::checked`].
    #[must_use]
    pub const fn new(row: u32, col: u32) -> Self {
        Self { row, col }
    }

    /// Адрес с проверкой границ листа.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`], если строка или столбец вне листа. Именно
    /// ошибка, а не «подрезать до предела»: неявная позиция, уехавшая за `XFD`,
    /// означает, что мы неверно поняли структуру `sheetData`, и молчаливое
    /// подрезание превратило бы это в порчу данных.
    pub fn checked(row: u32, col: u32) -> Result<Self> {
        if row > MAX_ROW || col > MAX_COL {
            // Печатается в R1C1: у адреса за пределами листа нет записи A1.
            let r = row.saturating_add(1);
            let c = col.saturating_add(1);
            return Err(XlsxError::BadCellRef(format!("R{r}C{c}")).into());
        }
        Ok(Self { row, col })
    }

    /// Адрес внутри листа.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.row <= MAX_ROW && self.col <= MAX_COL
    }

    /// Разбирает адрес `A1`.
    ///
    /// Принимаются только буквы столбца, за которыми идут цифры строки, и
    /// ничего кроме. Отвергаются `1A` (нет буквенной части), `A` (нет номера
    /// строки), `$A$1` (это синтаксис формул), `XFE1` и `A1048577` (вне листа).
    ///
    /// Регистр букв не важен: `a1` и `A1` — один адрес. Excel всегда пишет
    /// верхний, но `<dimension ref=…>` у сторонних генераторов встречается в
    /// нижнем, и отказ здесь стоил бы дороже, чем терпимость.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`] с исходной строкой — она попадёт в сообщение,
    /// и по нему видно, что именно лежало в файле.
    pub fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        let (col, i) = parse_col(b).ok_or_else(|| bad(s))?;
        let row = parse_row(b.get(i..).unwrap_or(&[])).ok_or_else(|| bad(s))?;
        Ok(Self { row, col })
    }

    /// Записывает адрес в форме `A1`.
    #[must_use]
    pub fn to_a1(&self) -> String {
        let mut out = String::with_capacity(10);
        write_col(self.col, &mut out);
        // Номер строки 1-based; вне листа адрес всё равно печатается — эта
        // функция диагностическая и не имеет права ничего скрывать.
        out.push_str(&self.row.saturating_add(1).to_string());
        out
    }
}

/// Прямоугольный диапазон, включая обе границы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRange {
    /// Левый верхний угол.
    pub from: CellRef,
    /// Правый нижний угол.
    pub to: CellRef,
}

impl CellRange {
    /// Диапазон из одной ячейки.
    #[must_use]
    pub const fn single(at: CellRef) -> Self {
        Self { from: at, to: at }
    }

    /// Разбирает `A1:B2` либо одиночный адрес `A1`.
    ///
    /// # Errors
    ///
    /// [`XlsxError::BadCellRef`], если одна из половин не разбирается или
    /// двоеточий больше одного.
    pub fn parse(s: &str) -> Result<Self> {
        match s.split_once(':') {
            None => Ok(Self::single(CellRef::parse(s)?)),
            Some((a, b)) => {
                if b.contains(':') {
                    return Err(bad(s));
                }
                Ok(Self {
                    from: CellRef::parse(a)?,
                    to: CellRef::parse(b)?,
                }
                .normalized())
            }
        }
    }

    /// Приводит углы к «левый верхний / правый нижний».
    ///
    /// `B2:A1` — законная запись того же диапазона; без нормализации
    /// [`Self::contains`] отвечал бы `false` для всех ячеек внутри.
    #[must_use]
    pub const fn normalized(self) -> Self {
        let (r0, r1) = if self.from.row <= self.to.row {
            (self.from.row, self.to.row)
        } else {
            (self.to.row, self.from.row)
        };
        let (c0, c1) = if self.from.col <= self.to.col {
            (self.from.col, self.to.col)
        } else {
            (self.to.col, self.from.col)
        };
        Self {
            from: CellRef::new(r0, c0),
            to: CellRef::new(r1, c1),
        }
    }

    /// Лежит ли адрес внутри диапазона. Предполагает нормализованный диапазон.
    #[must_use]
    pub const fn contains(&self, at: CellRef) -> bool {
        at.row >= self.from.row
            && at.row <= self.to.row
            && at.col >= self.from.col
            && at.col <= self.to.col
    }

    /// Записывает диапазон в форме `A1:B2`.
    #[must_use]
    pub fn to_a1(&self) -> String {
        let mut out = self.from.to_a1();
        out.push(':');
        out.push_str(&self.to.to_a1());
        out
    }
}

/// Ошибка «некорректный адрес», несущая исходную строку.
fn bad(s: &str) -> crate::error::Error {
    XlsxError::BadCellRef(s.to_owned()).into()
}

/// Разбирает буквенную часть. Возвращает 0-based индекс столбца и позицию
/// первого байта за буквами. `None` — букв нет, их больше трёх или столбец
/// вне листа.
fn parse_col(b: &[u8]) -> Option<(u32, usize)> {
    let mut acc: u32 = 0;
    let mut i: usize = 0;
    while let Some(&c) = b.get(i) {
        // Диапазон проверен матчем, поэтому wrapping_sub здесь не может
        // «завернуться»; она стоит только чтобы не тащить лишний lint.
        let d = match c {
            b'A'..=b'Z' => u32::from(c.wrapping_sub(b'A')),
            b'a'..=b'z' => u32::from(c.wrapping_sub(b'a')),
            _ => break,
        };
        i = i.saturating_add(1);
        if i > MAX_COL_LETTERS {
            return None;
        }
        // Биективная система счисления по основанию 26: `A`=1, `Z`=26, `AA`=27.
        acc = acc.saturating_mul(26).saturating_add(d).saturating_add(1);
    }
    if i == 0 {
        return None;
    }
    let col = acc.saturating_sub(1);
    if col > MAX_COL {
        return None;
    }
    Some((col, i))
}

/// Разбирает цифровую часть в 0-based индекс строки. `None` — цифр нет, их
/// больше семи, есть посторонние байты или строка вне листа.
fn parse_row(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > MAX_ROW_DIGITS {
        return None;
    }
    let mut acc: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc
            .checked_mul(10)?
            .checked_add(u32::from(c.wrapping_sub(b'0')))?;
    }
    if acc == 0 || acc > ROWS {
        return None;
    }
    Some(acc.saturating_sub(1))
}

/// Дописывает буквы столбца в конец строки.
fn write_col(col: u32, out: &mut String) {
    let mut letters = [0_u8; 8];
    let mut n = col;
    let mut k: usize = 0;
    loop {
        // Деление на ненулевой литерал; остаток и частное определены всегда.
        let rem = n % 26;
        if let Some(slot) = letters.get_mut(k) {
            *slot = b'A'.saturating_add(rem as u8);
        }
        k = k.saturating_add(1);
        n /= 26;
        if n == 0 || k >= letters.len() {
            break;
        }
        // Биективность: следующий разряд считается от `n - 1`, потому что
        // «нулевой» буквы не существует и `Z` (25) переносится не в `AA`, а в
        // «единицу старшего разряда».
        n = n.saturating_sub(1);
    }
    for i in (0..k).rev() {
        if let Some(&c) = letters.get(i) {
            out.push(char::from(c));
        }
    }
}

/// Разбирает номер строки из атрибута `r` элемента `<row>` (1-based в файле).
///
/// # Errors
///
/// [`XlsxError::BadCellRef`], если номер не число или вне листа.
pub fn parse_row_number(s: &str) -> Result<u32> {
    parse_row(s.as_bytes()).ok_or_else(|| bad(s))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn a1_is_the_origin() {
        assert_eq!(CellRef::parse("A1").unwrap(), CellRef::new(0, 0));
        assert_eq!(CellRef::new(0, 0).to_a1(), "A1");
    }

    #[test]
    fn round_trip_over_the_whole_sheet_edge() {
        let cases = [
            ("A1", 0_u32, 0_u32),
            ("Z1", 0, 25),
            ("AA1", 0, 26),
            ("AZ1", 0, 51),
            ("BA1", 0, 52),
            ("ZZ1", 0, 701),
            ("AAA1", 0, 702),
            ("XFD1048576", MAX_ROW, MAX_COL),
        ];
        for (a1, row, col) in cases {
            let at = CellRef::parse(a1).unwrap();
            assert_eq!(at, CellRef::new(row, col), "{a1}");
            assert_eq!(at.to_a1(), a1, "обратная запись {a1}");
        }
    }

    #[test]
    fn beyond_the_sheet_is_rejected() {
        for s in ["XFE1", "A1048577", "ZZZ1", "AAAA1", "A0", "A99999999"] {
            assert!(CellRef::parse(s).is_err(), "{s} обязан быть отвергнут");
        }
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        for s in [
            "", "1A", "A", "1", "$A$1", "A1:B2", "A 1", "A1 ", " A1", "A1x",
        ] {
            assert!(CellRef::parse(s).is_err(), "{s} обязан быть отвергнут");
        }
    }

    #[test]
    fn ranges_normalise_their_corners() {
        let r = CellRange::parse("B2:A1").unwrap();
        assert_eq!(r.from, CellRef::new(0, 0));
        assert_eq!(r.to, CellRef::new(1, 1));
        assert!(r.contains(CellRef::new(1, 0)));
        assert!(!r.contains(CellRef::new(2, 0)));
        assert_eq!(r.to_a1(), "A1:B2");
        assert_eq!(CellRange::parse("C3").unwrap().to_a1(), "C3:C3");
    }
}
