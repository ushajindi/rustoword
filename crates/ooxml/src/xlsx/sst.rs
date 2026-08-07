//! Таблица общих строк — `xl/sharedStrings.xml`.
//!
//! # Зачем она вообще есть
//!
//! Текст ячейки в SpreadsheetML почти никогда не лежит в самой ячейке. Ячейка
//! пишет `<c t="s"><v>17</v></c>`, а сама строка — семнадцатая запись `<si>` в
//! отдельной части. В корпусе 12 418 записей на 232 140 ячеек: экономия
//! очевидна, но цена — вторая часть, без которой лист нечитаем.
//!
//! # Две формы записи одной строки
//!
//! ```xml
//! <si><t>простая</si>
//! <si><r><rPr>…</rPr><t>кусок</t></r><r><t>ещё</t></r></si>
//! ```
//!
//! Вторая — строка с посимвольным форматированием: она разбита на «прогоны»
//! `<r>`, у каждого своё оформление. **Значением** строки является склейка
//! текста всех `<t>` подряд; оформление к значению отношения не имеет и здесь
//! не хранится (правка форматирования — не эта веха, а исходные байты всё
//! равно лежат в DOM нетронутыми).
//!
//! # Ловушка: `<rPh>`
//!
//! Внутри `<si>` может лежать `<rPh>` — фонетическая подсказка (чтение
//! иероглифов каной). У неё **тоже** есть `<t>`, и наивная склейка «всех `<t>`
//! подряд» пришьёт к значению текст, которого в ячейке нет. Excel показывает
//! `rPh` отдельной строкой над содержимым, а не как часть значения.

use crate::error::{Result, XlsxError};
use crate::limits::Limits;
use crate::xlsx::scan::{local_name, raw_attr};
use crate::xml::{Event, Reader};

/// Таблица общих строк.
#[derive(Debug, Clone, Default)]
pub struct SharedStrings {
    items: Vec<String>,
    /// Сколько записей собрано более чем из одного прогона `<r>`.
    rich: u32,
    /// Сколько `<t>` несли `xml:space="preserve"`.
    preserved: u32,
}

impl SharedStrings {
    /// Пустая таблица — для книг без `sharedStrings.xml`.
    ///
    /// Такая книга законна: если в ней нет ни одной текстовой ячейки, части
    /// просто нет. Отсутствие части не ошибка, а обращение к ней по индексу —
    /// ошибка, и её выдаст [`Self::get`].
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            rich: 0,
            preserved: 0,
        }
    }

    /// Число записей.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Таблица пуста.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Сколько записей собрано более чем из одного прогона `<r>`.
    #[must_use]
    pub const fn rich_count(&self) -> u32 {
        self.rich
    }

    /// Сколько фрагментов несли `xml:space="preserve"`.
    #[must_use]
    pub const fn preserved_count(&self) -> u32 {
        self.preserved
    }

    /// Строка по индексу из `<v>` ячейки с `t="s"`.
    ///
    /// # Errors
    ///
    /// [`XlsxError::SharedStringOutOfRange`] — индекс за границей таблицы.
    /// Именно ошибка, а не пустая строка: битый индекс означает, что лист и
    /// таблица строк из разных файлов, и подставить `""` — значит потерять
    /// данные молча.
    pub fn get(&self, i: u32) -> Result<&str> {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.items.get(i))
            .map(String::as_str)
            .ok_or_else(|| XlsxError::SharedStringOutOfRange(i).into())
    }

    /// Все строки в порядке индексов.
    #[must_use]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// Разбирает часть `xl/sharedStrings.xml`.
    ///
    /// Идёт pull-парсером без построения дерева: таблица строк — вторая по
    /// размеру часть книги после листа, а нужен из неё только вектор строк.
    ///
    /// # Errors
    ///
    /// Ошибки XML нижнего слоя и превышение квот.
    pub fn parse(part: &[u8], limits: &Limits) -> Result<Self> {
        let mut rd = Reader::with_limits(part, limits.clone());
        let mut out = Self::empty();
        let mut depth: usize = 0;
        // Глубина открытого `<si>`; вне записи текст `<t>` собирать не с чего.
        let mut si: Option<usize> = None;
        let mut builder = TextRuns::new();

        loop {
            match rd.next_event()? {
                Event::Start { empty, .. } => {
                    let d = depth.saturating_add(1);
                    let local = local_name(&rd);
                    // `<si>` — прямой ребёнок `<sst>`, то есть глубина 2.
                    if d == 2 && local == b"si" {
                        si = Some(d);
                        builder.reset();
                    } else if si.is_some() {
                        builder.open(local, d, &rd);
                    }
                    if empty {
                        Self::close(&mut out, &mut si, &mut builder, d, local);
                    } else {
                        depth = d;
                    }
                }
                Event::End { .. } => {
                    let local = local_name(&rd);
                    Self::close(&mut out, &mut si, &mut builder, depth, local);
                    depth = depth.saturating_sub(1);
                }
                Event::Text { span, .. } => {
                    if si.is_some() && builder.wants_text() {
                        builder.push(&rd.text(span)?);
                    }
                }
                Event::CData { span } => {
                    if si.is_some() && builder.wants_text() {
                        builder.push(&rd.text(span)?);
                    }
                }
                Event::Eof => break,
                Event::StartDoc { .. } | Event::Decl { .. } | Event::Pi { .. } => {}
                Event::Comment { .. } => {}
            }
        }
        Ok(out)
    }

    /// Общий хвост закрытия элемента: и для `</x>`, и для пустого `<x/>`.
    fn close(
        out: &mut Self,
        si: &mut Option<usize>,
        builder: &mut TextRuns,
        d: usize,
        local: &[u8],
    ) {
        if *si == Some(d) && local == b"si" {
            *si = None;
            let (text, runs, preserved) = builder.finish();
            if runs > 1 {
                out.rich = out.rich.saturating_add(1);
            }
            out.preserved = out.preserved.saturating_add(preserved);
            out.items.push(text);
            return;
        }
        if si.is_some() {
            builder.close(local, d);
        }
    }
}

/// Сборщик значения строки из прогонов `<r>` и фрагментов `<t>`.
///
/// Общий для `<si>` таблицы строк и для `<is>` встроенного текста: правила
/// склейки там дословно одинаковые, и разъехаться они не должны.
#[derive(Debug, Default)]
pub(crate) struct TextRuns {
    text: String,
    /// Мы внутри `<t>`, чей текст идёт в значение.
    in_t: bool,
    /// Глубина открытого `<rPh>`: его `<t>` в значение не идёт.
    rph: Option<usize>,
    runs: u32,
    preserved: u32,
}

impl TextRuns {
    pub(crate) const fn new() -> Self {
        Self {
            text: String::new(),
            in_t: false,
            rph: None,
            runs: 0,
            preserved: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.text.clear();
        self.in_t = false;
        self.rph = None;
        self.runs = 0;
        self.preserved = 0;
    }

    /// Идёт ли сейчас текст, который надо собирать.
    pub(crate) const fn wants_text(&self) -> bool {
        self.in_t
    }

    pub(crate) fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// Начало элемента внутри `<si>`/`<is>`.
    pub(crate) fn open(&mut self, local: &[u8], d: usize, rd: &Reader<'_>) {
        match local {
            b"rPh" if self.rph.is_none() => self.rph = Some(d),
            b"r" if self.rph.is_none() => self.runs = self.runs.saturating_add(1),
            b"t" if self.rph.is_none() => {
                self.in_t = true;
                // `xml:space="preserve"` мы не интерпретируем, а лишь считаем:
                // текст не подрезается НИКОГДА. Подрезка по отсутствию атрибута
                // была бы потерей данных, а Excel и так ставит preserve всюду,
                // где пробел значим (584 раза в корпусе).
                if raw_attr(rd, b"xml:space") == Some(b"preserve") {
                    self.preserved = self.preserved.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    /// Конец элемента внутри `<si>`/`<is>`.
    pub(crate) fn close(&mut self, local: &[u8], d: usize) {
        if self.rph == Some(d) {
            self.rph = None;
            return;
        }
        if local == b"t" {
            self.in_t = false;
        }
    }

    /// Готовое значение: текст, число прогонов, число `preserve`.
    pub(crate) fn finish(&mut self) -> (String, u32, u32) {
        let text = decode_x_escapes(&self.text);
        let out = (text, self.runs, self.preserved);
        self.reset();
        out
    }
}

/// Раскрывает конвенцию Excel `_xHHHH_` для символов, недопустимых в XML.
///
/// XML 1.0 не умеет представлять управляющие символы: ни `&#0;`, ни литерал
/// `\0` в документ не положить. Excel обходит это, записывая такой символ как
/// `_x0000_`, а сам буквальный `_x`, который иначе выглядел бы как escape, — как
/// `_x005F_x`.
///
/// # Почему раскрытие консервативное
///
/// Прямолинейное «любое `_xHHHH_` — это escape» ломает чужие данные: строка
/// `_x0041_`, набранная руками в Google Sheets, превратилась бы в `A`.
/// Поэтому раскрывается только то, что Excel действительно обязан был бы
/// экранировать:
///
/// * `_xHHHH_`, если `HHHH` — управляющий символ (`< 0x20`) или символ,
///   недопустимый в XML 1.0. Заметьте, что `0x0D` сюда входит, хотя формально
///   XML его допускает: любой XML-парсер нормализует CR в LF, поэтому Excel
///   обязан экранировать возврат каретки, и `_x000D_` в реальных файлах —
///   самый частый случай этой конвенции;
/// * `_x005F_`, если следом идёт такой escape — это экранированное
///   подчёркивание перед настоящим escape'ом, ровно тот случай, ради которого
///   правило и существует.
///
/// Всё прочее остаётся текстом. Обратной операции здесь нет: писать умеет
/// веха M9, и делает это по тем же правилам.
#[must_use]
pub fn decode_x_escapes(s: &str) -> String {
    if !s.contains("_x") {
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i: usize = 0;
    while i < b.len() {
        match escape_at(b, i) {
            Some((c, next)) => {
                if let Some(ch) = char::from_u32(c) {
                    if is_escapable(c) {
                        out.push(ch);
                        i = next;
                        continue;
                    }
                    // `_x005F_`, чьё закрывающее подчёркивание одновременно
                    // открывает следующий escape, — это экранированное `_`
                    // перед текстом, который иначе прочитался бы как escape.
                    // Хвост после него отдаётся дословно: он и есть то, что
                    // пользователь набрал руками.
                    if c == 0x5F && escape_at(b, next.saturating_sub(1)).is_some() {
                        out.push('_');
                        i = next;
                        continue;
                    }
                }
                // Не escape по смыслу — отдаём байты как есть.
                out.push('_');
                i = i.saturating_add(1);
            }
            None => {
                // Идём по символам, а не по байтам: UTF-8 рвать нельзя.
                let rest = s.get(i..).unwrap_or("");
                match rest.chars().next() {
                    Some(ch) => {
                        out.push(ch);
                        i = i.saturating_add(ch.len_utf8());
                    }
                    None => break,
                }
            }
        }
    }
    out
}

/// Если по позиции `i` стоит `_xHHHH_`, возвращает код символа и позицию за
/// закрывающим подчёркиванием.
fn escape_at(b: &[u8], i: usize) -> Option<(u32, usize)> {
    if b.get(i) != Some(&b'_') || b.get(i.saturating_add(1)) != Some(&b'x') {
        return None;
    }
    let from = i.saturating_add(2);
    let to = from.saturating_add(4);
    if b.get(to) != Some(&b'_') {
        return None;
    }
    let hex = b.get(from..to)?;
    let mut c: u32 = 0;
    for &d in hex {
        let v = match d {
            b'0'..=b'9' => u32::from(d.wrapping_sub(b'0')),
            b'a'..=b'f' => u32::from(d.wrapping_sub(b'a')).saturating_add(10),
            b'A'..=b'F' => u32::from(d.wrapping_sub(b'A')).saturating_add(10),
            _ => return None,
        };
        c = c.saturating_mul(16).saturating_add(v);
    }
    Some((c, to.saturating_add(1)))
}

/// Символ допустим в XML 1.0. Ровно продукция `Char` спецификации.
const fn is_xml_char(c: u32) -> bool {
    matches!(c, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

/// Символ, который Excel обязан записать конвенцией `_xHHHH_`.
///
/// Управляющие символы плюс всё, что XML не представляет вовсе. Табуляция и
/// перевод строки формально допустимы, но Excel экранирует и их, а спутать
/// `_x0009_` с осмысленным пользовательским текстом невозможно.
const fn is_escapable(c: u32) -> bool {
    c < 0x20 || !is_xml_char(c)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn sst(xml: &str) -> SharedStrings {
        SharedStrings::parse(xml.as_bytes(), &Limits::strict()).unwrap()
    }

    #[test]
    fn plain_and_rich_strings_produce_the_same_kind_of_value() {
        let t = sst(concat!(
            r#"<sst xmlns="urn:x" count="2" uniqueCount="2">"#,
            "<si><t>простая</t></si>",
            "<si><r><rPr><b/></rPr><t>ку</t></r><r><t>сок</t></r></si>",
            "</sst>"
        ));
        assert_eq!(t.get(0).unwrap(), "простая");
        assert_eq!(t.get(1).unwrap(), "кусок");
        assert_eq!(t.rich_count(), 1);
    }

    #[test]
    fn phonetic_hints_are_not_part_of_the_value() {
        let t = sst(concat!(
            "<sst>",
            "<si><t>東京</t><rPh sb=\"0\" eb=\"2\"><t>とうきょう</t></rPh>",
            "<phoneticPr fontId=\"1\"/></si>",
            "</sst>"
        ));
        assert_eq!(t.get(0).unwrap(), "東京");
    }

    #[test]
    fn out_of_range_index_is_an_error_not_a_panic() {
        let t = sst("<sst><si><t>a</t></si></sst>");
        assert!(t.get(0).is_ok());
        match t.get(1) {
            Err(crate::error::Error::Xlsx(XlsxError::SharedStringOutOfRange(1))) => {}
            other => panic!("ожидался SharedStringOutOfRange, получено {other:?}"),
        }
    }

    #[test]
    fn x_escapes_expand_only_where_excel_had_no_choice() {
        assert_eq!(decode_x_escapes("a_x0000_b"), "a\u{0}b");
        assert_eq!(decode_x_escapes("строка_x000D_ещё"), "строка\rещё");
        // `A` (0x41) экранировать не от чего, значит это не escape, а текст.
        assert_eq!(decode_x_escapes("_x0041_"), "_x0041_");
        assert_eq!(decode_x_escapes("_x005F_x0041_"), "_x0041_");
        assert_eq!(decode_x_escapes("подчёрк_ивание"), "подчёрк_ивание");
        assert_eq!(decode_x_escapes("_xZZZZ_"), "_xZZZZ_");
        assert_eq!(decode_x_escapes("_x00"), "_x00");
    }
}
