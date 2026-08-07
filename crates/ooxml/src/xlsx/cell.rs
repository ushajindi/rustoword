//! Значение ячейки и её тип.
//!
//! # Почему тип ячейки и тип значения — разные вещи
//!
//! Атрибут `t` у `<c>` — это не тип значения, а **способ хранения**. Три разных
//! `t` (`s`, `str`, `inlineStr`) дают одно и то же значение — строку, но лежит
//! она в трёх разных местах: в таблице общих строк, прямо в `<v>` как результат
//! формулы и прямо в `<is>` как встроенный текст. Обратно: `t="n"` и
//! отсутствие `t` — одно и то же число.
//!
//! Поэтому [`Cell`] хранит и то и другое: [`CellValue`] — что там лежит,
//! [`CellType`] — как оно было записано. Без второго нельзя записать ячейку
//! обратно так, как её ждёт Excel; без первого нельзя ничего посчитать.
//!
//! # Формула не разбирается
//!
//! [`Formula::text`] — текст как есть. Разбор формул — веха M10 и отдельный
//! слой; чтение значений от него не зависит и не должно зависеть: файл с
//! формулой, которую мы не умеем разобрать, обязан читаться целиком.

use crate::error::{Result, XlsxError};
use crate::xlsx::refs::{CellRange, CellRef};

/// Как значение ячейки записано в файле — атрибут `t` элемента `<c>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CellType {
    /// Число. Значение `t` отсутствует либо равно `n` — это одно и то же, и
    /// подавляющее большинство ячеек корпуса именно такие.
    #[default]
    N,
    /// Индекс в таблице общих строк (`sharedStrings.xml`).
    S,
    /// Строковый результат формулы, лежащий прямо в `<v>`.
    Str,
    /// Логическое: `<v>0</v>` или `<v>1</v>`.
    B,
    /// Ошибка вычисления: `<v>#REF!</v>`.
    E,
    /// Текст прямо в ячейке, в `<is>`.
    InlineStr,
    /// Дата в формате ISO 8601 прямо в `<v>` (расширение ECMA-376 2-й редакции;
    /// Excel его не пишет, но читать обязаны).
    D,
}

impl CellType {
    /// Все варианты в порядке [`CellType::index`] — для гистограмм и отчётов.
    pub const ALL: [Self; 7] = [
        Self::N,
        Self::S,
        Self::Str,
        Self::B,
        Self::E,
        Self::InlineStr,
        Self::D,
    ];

    /// Значение атрибута `t`, как оно пишется в файл.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::N => "n",
            Self::S => "s",
            Self::Str => "str",
            Self::B => "b",
            Self::E => "e",
            Self::InlineStr => "inlineStr",
            Self::D => "d",
        }
    }

    /// Плотный индекс варианта — ключ массива-счётчика.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::N => 0,
            Self::S => 1,
            Self::Str => 2,
            Self::B => 3,
            Self::E => 4,
            Self::InlineStr => 5,
            Self::D => 6,
        }
    }

    /// Разбирает атрибут `t`.
    ///
    /// # Errors
    ///
    /// [`XlsxError::UnknownCellType`] — значение не из списка ECMA-376.
    /// Молча считать неизвестный тип числом нельзя: `t="foo"` означает, что мы
    /// читаем не тот файл или не тот формат, и тихая подстановка спрячет это.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "n" => Ok(Self::N),
            "s" => Ok(Self::S),
            "str" => Ok(Self::Str),
            "b" => Ok(Self::B),
            "e" => Ok(Self::E),
            "inlineStr" => Ok(Self::InlineStr),
            "d" => Ok(Self::D),
            other => Err(XlsxError::UnknownCellType(other.to_owned()).into()),
        }
    }
}

/// Ошибка вычисления, хранящаяся в ячейке.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellError {
    /// `#NULL!`
    Null,
    /// `#DIV/0!`
    Div0,
    /// `#VALUE!`
    Value,
    /// `#REF!`
    Ref,
    /// `#NAME?`
    Name,
    /// `#NUM!`
    Num,
    /// `#N/A`
    Na,
    /// `#GETTING_DATA` — не ошибка в обычном смысле, а «значение ещё едет с
    /// сервера»; в файле встречается наравне с остальными.
    GettingData,
}

impl CellError {
    /// Все варианты.
    pub const ALL: [Self; 8] = [
        Self::Null,
        Self::Div0,
        Self::Value,
        Self::Ref,
        Self::Name,
        Self::Num,
        Self::Na,
        Self::GettingData,
    ];

    /// Запись, как она лежит в `<v>`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "#NULL!",
            Self::Div0 => "#DIV/0!",
            Self::Value => "#VALUE!",
            Self::Ref => "#REF!",
            Self::Name => "#NAME?",
            Self::Num => "#NUM!",
            Self::Na => "#N/A",
            Self::GettingData => "#GETTING_DATA",
        }
    }

    /// Разбирает содержимое `<v>` ячейки с `t="e"`.
    ///
    /// # Errors
    ///
    /// [`XlsxError::UnknownCellType`] — такой строки нет в списке ошибок.
    pub fn parse(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| XlsxError::UnknownCellType(s.to_owned()).into())
    }
}

/// Значение ячейки.
///
/// Даты сюда отдельным вариантом не попадают намеренно. В SpreadsheetML дата —
/// это число (серийный день от эпохи книги) плюс числовой формат; отличить дату
/// от числа можно только по стилю, и делает это [`crate::xlsx::styles`]. Редкий
/// `t="d"` хранит ISO-строку и отдаётся как [`CellValue::Text`] без разбора:
/// разбирать календарь ядру, у которого запрещён `std::time`, нечем и незачем.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CellValue {
    /// Ячейка есть, значения нет. Так выглядит `<c r="A1" s="3"/>` — ячейка
    /// существует ради стиля. В корпусе таких большинство.
    #[default]
    Empty,
    /// Число.
    Number(f64),
    /// Текст: из таблицы общих строк, из `<is>`, из `<v>` при `t="str"` или
    /// ISO-строка при `t="d"`.
    Text(String),
    /// Логическое.
    Bool(bool),
    /// Ошибка вычисления.
    Error(CellError),
}

impl CellValue {
    /// Значения нет.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(*self, Self::Empty)
    }

    /// Число, если значение числовое. Логическое числом **не** считается:
    /// приведение типов — дело формул, а не чтения.
    #[must_use]
    pub const fn as_number(&self) -> Option<f64> {
        match *self {
            Self::Number(n) => Some(n),
            _ => None,
        }
    }

    /// Текст, если значение текстовое.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match *self {
            Self::Text(ref s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Разновидность формулы — атрибут `t` элемента `<f>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FormulaKind {
    /// Обычная формула, своя у каждой ячейки.
    #[default]
    Normal,
    /// Общая формула. Текст записан только в ячейке-хозяине; остальные ячейки
    /// группы несут пустой `<f t="shared" si="…"/>` и берут текст оттуда, сдвигая
    /// относительные ссылки. Сдвиг — работа вехи M10, здесь только факт.
    Shared {
        /// Идентификатор группы.
        si: u32,
        /// Диапазон группы. Заполнен только у ячейки-хозяина (атрибут `ref`).
        master: Option<CellRange>,
    },
    /// Формула массива; `ref` — диапазон, который она заполняет.
    Array {
        /// Диапазон результата.
        range: CellRange,
    },
    /// Таблица подстановки (`<f t="dataTable">`).
    DataTable,
}

/// Формула ячейки.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Formula {
    /// Текст формулы без ведущего `=`, как он лежит в `<f>`. Не разобран.
    pub text: String,
    /// Разновидность.
    pub kind: FormulaKind,
}

impl Formula {
    /// Обычная формула с данным текстом.
    #[must_use]
    pub fn normal(text: String) -> Self {
        Self {
            text,
            kind: FormulaKind::Normal,
        }
    }

    /// Формула-последователь общей группы: текста своего не имеет.
    #[must_use]
    pub const fn is_shared_follower(&self) -> bool {
        matches!(self.kind, FormulaKind::Shared { .. }) && self.text.is_empty()
    }
}

/// Прочитанная ячейка.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cell {
    /// Адрес — явный из атрибута `r` либо вычисленный по позиции.
    pub at: CellRef,
    /// Значение.
    pub value: CellValue,
    /// Как оно было записано.
    pub ty: CellType,
    /// Индекс в `cellXfs` (атрибут `s`). `None` — стиль по умолчанию.
    pub style: Option<u32>,
    /// Формула, если ячейка вычисляемая.
    pub formula: Option<Formula>,
}

impl Cell {
    /// Пустая ячейка по адресу.
    #[must_use]
    pub fn empty_at(at: CellRef) -> Self {
        Self {
            at,
            ..Self::default()
        }
    }
}

/// Разбирает содержимое `<v>` числовой ячейки.
///
/// # Errors
///
/// [`XlsxError::BadNumber`], если строка не число или число не конечно.
/// Бесконечность и NaN отвергаются: записать их в `<v>` нечем (ошибки
/// вычисления живут в `t="e"`), а значит их появление означает подделанный
/// файл, а не легальные данные.
pub fn parse_number(s: &str) -> Result<f64> {
    let n: f64 = s
        .trim()
        .parse()
        .map_err(|_| XlsxError::BadNumber(s.to_owned()))?;
    if !n.is_finite() {
        return Err(XlsxError::BadNumber(s.to_owned()).into());
    }
    Ok(n)
}

/// Разбирает содержимое `<v>` логической ячейки.
///
/// Excel пишет `0`/`1`; сторонние генераторы встречаются с `false`/`true`.
///
/// # Errors
///
/// [`XlsxError::BadNumber`] — ни то ни другое.
pub fn parse_bool(s: &str) -> Result<bool> {
    match s.trim() {
        "1" | "true" | "TRUE" => Ok(true),
        "0" | "false" | "FALSE" => Ok(false),
        other => Err(XlsxError::BadNumber(other.to_owned()).into()),
    }
}

/// Разбирает беззнаковый индекс (`s`, `si`, индекс общей строки).
///
/// # Errors
///
/// [`XlsxError::BadNumber`], если это не десятичное число в диапазоне `u32`.
pub fn parse_index(s: &str) -> Result<u32> {
    s.trim()
        .parse()
        .map_err(|_| XlsxError::BadNumber(s.to_owned()).into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn cell_type_round_trips_through_its_attribute_form() {
        for t in CellType::ALL {
            assert_eq!(CellType::parse(t.as_str()).unwrap(), t);
        }
        assert!(CellType::parse("foo").is_err());
        // Индексы плотные и различные — иначе гистограмма по типам врёт.
        let mut seen = [false; 7];
        for t in CellType::ALL {
            assert!(!seen[t.index()], "индекс {} повторился", t.index());
            seen[t.index()] = true;
        }
    }

    #[test]
    fn cell_errors_round_trip() {
        for e in CellError::ALL {
            assert_eq!(CellError::parse(e.as_str()).unwrap(), e);
        }
        assert!(CellError::parse("#OOPS!").is_err());
    }

    #[test]
    fn numbers_reject_non_finite_values() {
        assert_eq!(parse_number("1.5").unwrap(), 1.5);
        assert_eq!(parse_number("1E+3").unwrap(), 1000.0);
        assert_eq!(parse_number("-0").unwrap(), 0.0);
        for s in ["inf", "NaN", "", "1,5", "abc"] {
            assert!(parse_number(s).is_err(), "{s} обязано быть отвергнуто");
        }
    }

    #[test]
    fn booleans_accept_both_spellings() {
        assert!(parse_bool("1").unwrap());
        assert!(!parse_bool("0").unwrap());
        assert!(parse_bool("true").unwrap());
        assert!(parse_bool("2").is_err());
    }
}
