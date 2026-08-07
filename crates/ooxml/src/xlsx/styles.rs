//! Числовые форматы — ровно столько, чтобы отличить дату от числа.
//!
//! # Почему только это
//!
//! Полная модель стилей `xl/styles.xml` — это шрифты, заливки, границы,
//! выравнивания, защита, дифференциальные форматы и таблица связей между ними.
//! Строить её ради чтения значений незачем: сохранность оформления обеспечивает
//! preserving DOM, который эти байты и не трогает.
//!
//! Но одну вещь чтение значений без стилей сделать не может. В SpreadsheetML
//! **даты — это числа**: `45000` может быть и количеством рублей, и 1 июля
//! 2023 года. Отличает их только числовой формат ячейки. Поэтому здесь ровно
//! одно отображение: индекс `cellXfs` → `numFmtId` → «это дата?».
//!
//! # Две ловушки
//!
//! 1. `<xf>` встречается в **двух** контейнерах — `<cellXfs>` и
//!    `<cellStyleXfs>`. Атрибут `s` у ячейки индексирует первый; второй — это
//!    именованные стили. Перепутать их — значит выдать даты не там, где надо.
//! 2. Пользовательский формат распознаётся по коду, и в коде есть участки, где
//!    буквы `y`/`m`/`d`/`h`/`s` **не** означают дату: литералы в кавычках
//!    (`"Red"`), экранированные символы (`\d`), секции в скобках (`[Red]`,
//!    `[$USD]`, `[$-419]`). Наивный `code.contains('d')` объявляет датой формат
//!    `[Red]#,##0`.

use crate::error::Result;
use crate::limits::Limits;
use crate::xlsx::cell::parse_index;
use crate::xlsx::scan::{attr_str, local_name};
use crate::xml::{Event, Reader};

/// Встроенные форматы дат и времени.
///
/// ECMA-376 закрепляет id 0–49 за встроенными форматами. Датами и временем из
/// них являются 14–22 и 45–47. Восточноазиатские наборы (27–36, 50–58) тоже
/// даты, но их коды зависят от локали книги; ядро их не разбирает, и ячейка с
/// таким форматом останется числом — это известное упрощение, а не оплошность.
const BUILTIN_DATE: [core::ops::RangeInclusive<u32>; 2] = [14..=22, 45..=47];

/// Числовые форматы книги.
#[derive(Debug, Clone, Default)]
pub struct Styles {
    /// `numFmtId` для каждого индекса `cellXfs`.
    xf: Vec<u32>,
    /// Пользовательские форматы: (id, это дата), отсортировано по id.
    custom: Vec<(u32, bool)>,
}

impl Styles {
    /// Пустая таблица — для книг без `xl/styles.xml`.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            xf: Vec::new(),
            custom: Vec::new(),
        }
    }

    /// Число записей `cellXfs`.
    #[must_use]
    pub fn xf_count(&self) -> usize {
        self.xf.len()
    }

    /// Число пользовательских форматов.
    #[must_use]
    pub fn custom_count(&self) -> usize {
        self.custom.len()
    }

    /// `numFmtId` по индексу из атрибута `s` ячейки.
    #[must_use]
    pub fn num_fmt_id(&self, style: u32) -> Option<u32> {
        usize::try_from(style)
            .ok()
            .and_then(|i| self.xf.get(i))
            .copied()
    }

    /// Формат с таким id форматирует значение как дату или время.
    #[must_use]
    pub fn is_date_num_fmt(&self, id: u32) -> bool {
        if BUILTIN_DATE.iter().any(|r| r.contains(&id)) {
            return true;
        }
        self.custom
            .binary_search_by_key(&id, |&(k, _)| k)
            .ok()
            .and_then(|i| self.custom.get(i))
            .is_some_and(|&(_, is_date)| is_date)
    }

    /// Ячейка с таким `s` показывается как дата или время.
    ///
    /// Неизвестный индекс — не дата: числовой формат по умолчанию (`General`)
    /// датой не является, и придумывать за отсутствующую таблицу стилей нельзя.
    #[must_use]
    pub fn is_date_style(&self, style: Option<u32>) -> bool {
        style
            .and_then(|s| self.num_fmt_id(s))
            .is_some_and(|id| self.is_date_num_fmt(id))
    }

    /// Разбирает `xl/styles.xml`.
    ///
    /// # Errors
    ///
    /// Ошибки XML нижнего слоя, превышение квот и [`crate::error::XlsxError::BadNumber`]
    /// на нечисловом `numFmtId`.
    pub fn parse(part: &[u8], limits: &Limits) -> Result<Self> {
        let mut rd = Reader::with_limits(part, limits.clone());
        let mut out = Self::empty();
        let mut depth: usize = 0;
        // Какой контейнер второго уровня сейчас открыт. Без этого `<xf>` из
        // `<cellStyleXfs>` попал бы в таблицу `<cellXfs>` — см. шапку модуля.
        let mut container: Container = Container::Other;

        loop {
            match rd.next_event()? {
                Event::Start { empty, .. } => {
                    let d = depth.saturating_add(1);
                    let local = local_name(&rd);
                    match d {
                        2 => {
                            container = match local {
                                b"numFmts" => Container::NumFmts,
                                b"cellXfs" => Container::CellXfs,
                                _ => Container::Other,
                            };
                        }
                        3 => out.take_entry(container, local, &rd)?,
                        _ => {}
                    }
                    if !empty {
                        depth = d;
                    } else if d == 2 {
                        container = Container::Other;
                    }
                }
                Event::End { .. } => {
                    if depth == 2 {
                        container = Container::Other;
                    }
                    depth = depth.saturating_sub(1);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        out.custom.sort_by_key(|&(id, _)| id);
        Ok(out)
    }

    fn take_entry(&mut self, container: Container, local: &[u8], rd: &Reader<'_>) -> Result<()> {
        match (container, local) {
            (Container::NumFmts, b"numFmt") => {
                let Some(id) = attr_str(rd, b"numFmtId")? else {
                    return Ok(());
                };
                let id = parse_index(id)?;
                let code = attr_str(rd, b"formatCode")?.unwrap_or("");
                self.custom.push((id, format_code_is_date(code)));
            }
            (Container::CellXfs, b"xf") => {
                let id = match attr_str(rd, b"numFmtId")? {
                    Some(v) => parse_index(v)?,
                    None => 0,
                };
                self.xf.push(id);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Контейнер второго уровня `styleSheet`, который нас интересует.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    NumFmts,
    CellXfs,
    Other,
}

/// Код числового формата описывает дату или время.
///
/// Разбор идёт по правилам секции «Number Formats» ECMA-376, но только до той
/// глубины, которая нужна для ответа «да/нет»:
///
/// * `"…"` — литерал, буквы внутри значения не имеют;
/// * `\x` — экранированный символ, тоже литерал;
/// * `_x` — «отступ шириной символа `x`», следующий символ не код;
/// * `[…]` — цвет, условие или локаль; **исключение** — `[h]`, `[mm]`, `[ss]`:
///   это «прошедшее время», и оно как раз дата;
/// * всё прочее: `y`, `m`, `d`, `h`, `s` в любом регистре означают дату.
///
/// Секции формата, разделённые `;`, проверяются одинаково: если хоть одна
/// показывает дату, формат считается датой.
#[must_use]
pub fn format_code_is_date(code: &str) -> bool {
    let b = code.as_bytes();
    let mut i: usize = 0;
    while let Some(&c) = b.get(i) {
        match c {
            // Экранированный символ и «отступ шириной символа» съедают
            // следующий байт целиком.
            b'\\' | b'_' => i = i.saturating_add(2),
            b'"' => {
                i = i.saturating_add(1);
                while let Some(&x) = b.get(i) {
                    i = i.saturating_add(1);
                    if x == b'"' {
                        break;
                    }
                }
            }
            b'[' => {
                let from = i.saturating_add(1);
                let mut j = from;
                while b.get(j).is_some_and(|&x| x != b']') {
                    j = j.saturating_add(1);
                }
                let inner = b.get(from..j).unwrap_or(&[]);
                if !inner.is_empty()
                    && inner
                        .iter()
                        .all(|&x| matches!(x.to_ascii_lowercase(), b'h' | b'm' | b's'))
                {
                    return true;
                }
                i = j.saturating_add(1);
            }
            _ => {
                if matches!(c.to_ascii_lowercase(), b'y' | b'm' | b'd' | b'h' | b's') {
                    return true;
                }
                i = i.saturating_add(1);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn builtin_date_formats_are_recognised() {
        let s = Styles::empty();
        for id in [14_u32, 18, 22, 45, 47] {
            assert!(s.is_date_num_fmt(id), "встроенный {id} — дата");
        }
        for id in [0_u32, 1, 9, 13, 23, 44, 48, 49] {
            assert!(!s.is_date_num_fmt(id), "встроенный {id} — не дата");
        }
    }

    #[test]
    fn format_codes_are_read_outside_quotes_and_brackets() {
        for code in ["dd.mm.yyyy", "d/m/yy h:mm", "[h]:mm:ss", "yyyy", "mmm-yy"] {
            assert!(format_code_is_date(code), "{code} — дата");
        }
        for code in [
            "General",
            "0.00",
            "#,##0.00",
            "0.00E+00",
            "@",
            "[Red]#,##0",
            r#"[$USD]#,##0.00"#,
            r#"[$-409]#,##0"#,
            r#"#,##0.00\ "руб.""#,
            r#"0" days""#,
            "_(* #,##0.00_);_(* (#,##0.00);_(* \"-\"??_)",
        ] {
            assert!(!format_code_is_date(code), "{code} — не дата");
        }
    }

    #[test]
    fn cell_xfs_and_cell_style_xfs_do_not_mix() {
        let xml = concat!(
            "<styleSheet>",
            r#"<numFmts count="1"><numFmt numFmtId="164" formatCode="dd.mm.yyyy"/></numFmts>"#,
            r#"<cellStyleXfs count="1"><xf numFmtId="14"/></cellStyleXfs>"#,
            r#"<cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs>"#,
            "</styleSheet>"
        );
        let s = Styles::parse(xml.as_bytes(), &Limits::strict()).unwrap();
        // Ровно два `<xf>`: из `cellStyleXfs` не взято ничего.
        assert_eq!(s.xf_count(), 2);
        assert!(!s.is_date_style(Some(0)));
        assert!(s.is_date_style(Some(1)));
        assert!(!s.is_date_style(None));
        assert!(!s.is_date_style(Some(99)));
    }
}
