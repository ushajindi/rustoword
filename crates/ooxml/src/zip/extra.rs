//! Обход extra-полей ZIP-заголовка.
//!
//! # Почему extra-поля непрозрачны
//!
//! Область extra — это последовательность записей `id: u16, size: u16, data`.
//! Идентификаторов существуют десятки: NTFS-времена (`0x000A`), Unix uid/gid
//! (`0x7875`), Info-ZIP timestamps (`0x5455`), «Open Packaging Growth Hint»
//! (`0xA220`, 520 байт у MS Office) и вендорские, которых нет ни в одном
//! реестре.
//!
//! Ядро интерпретирует **только** [`EXTRA_ZIP64`](super::consts::EXTRA_ZIP64):
//! без него нельзя узнать настоящие размеры записи. Всё остальное
//! копируется байт в байт. Именно поэтому итератор возвращает [`Span`], а не
//! разобранные структуры: разобрать — значит потом собрать обратно, а собрать
//! обратно байт в байт неизвестное поле невозможно.
//!
//! # Почему обрыв области не ошибка
//!
//! Часть writer'ов дописывает в конец extra выравнивающие нули, из-за чего
//! последняя «запись» оказывается огрызком. Отвергать такие файлы нельзя —
//! они открываются везде. Итератор просто останавливается и поднимает флаг
//! [`ExtraFields::is_truncated`]; байты области при этом всё равно сохранены
//! спаном целиком, так что round-trip не страдает.

use crate::bytes::{Reader, Span};

/// Одно extra-поле.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtraField {
    /// Идентификатор заголовка (`Header ID` в терминах APPNOTE).
    pub id: u16,
    /// Спан полезной нагрузки — без 4 байт `id`/`size`.
    pub data: Span,
    /// Спан всей записи, включая `id` и `size`.
    ///
    /// Нужен вехе M3: чтобы вырезать или заменить одно поле, не трогая
    /// соседние, надо знать границы записи вместе с её заголовком.
    pub span: Span,
}

/// Итератор по extra-полям области.
///
/// Спаны абсолютны относительно исходного буфера файла, а не относительно
/// начала области: иначе каждый потребитель складывал бы офсеты сам и рано
/// или поздно ошибся.
#[derive(Debug, Clone)]
pub struct ExtraFields<'a> {
    src: &'a [u8],
    pos: usize,
    end: usize,
    truncated: bool,
}

impl<'a> ExtraFields<'a> {
    /// Итератор по области `area` буфера `src`.
    #[must_use]
    pub fn new(src: &'a [u8], area: Span) -> Self {
        // Область, вылезающая за буфер, схлопывается в пустую: разбор
        // заголовков уже проверил границы, а на несогласованном входе
        // тихое «полей нет» безопаснее выхода за границу.
        let start = area.start() as usize;
        let end = area.end() as usize;
        let ok = start <= end && end <= src.len();
        Self {
            src,
            pos: if ok { start } else { end.min(src.len()) },
            end: if ok { end } else { end.min(src.len()) },
            truncated: false,
        }
    }

    /// Остались ли в конце области байты, не образующие целой записи.
    ///
    /// Валиден только после того, как итератор дошёл до конца.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Первое поле с заданным идентификатором.
    ///
    /// Дубликаты по APPNOTE запрещены, но встречаются; берётся первое —
    /// так же поступают Info-ZIP и Windows Explorer.
    #[must_use]
    pub fn find(src: &'a [u8], area: Span, id: u16) -> Option<ExtraField> {
        Self::new(src, area).find(|f| f.id == id)
    }
}

impl Iterator for ExtraFields<'_> {
    type Item = ExtraField;

    fn next(&mut self) -> Option<ExtraField> {
        if self.pos >= self.end {
            return None;
        }
        let mut r = Reader::at(self.src, self.pos)?;
        // На заголовок записи нужно 4 байта; если их нет — область оборвана.
        let header_end = self.pos.checked_add(4)?;
        if header_end > self.end {
            self.truncated = true;
            self.pos = self.end;
            return None;
        }
        let id = r.le_u16()?;
        let size = usize::from(r.le_u16()?);
        let data_end = header_end.checked_add(size)?;
        if data_end > self.end {
            // Поле объявило больше данных, чем есть в области. Дальше идти
            // некуда: следующая запись начинается неизвестно где.
            self.truncated = true;
            self.pos = self.end;
            return None;
        }
        let data = r.take_span(size)?;
        let span = Span::new(u32::try_from(self.pos).ok()?, u32::try_from(data_end).ok()?)?;
        self.pos = data_end;
        Some(ExtraField { id, data, span })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    /// Первые два байта — мусор перед областью: проверяем, что спаны
    /// абсолютны. Дальше поле `0x000A` длиной 3 и поле `0xA220` длиной 2.
    #[rustfmt::skip]
    const AREA: &[u8] = &[
        0xFF, 0xFF,
        0x0A, 0x00, 0x03, 0x00, 1, 2, 3,
        0x20, 0xA2, 0x02, 0x00, 9, 9,
    ];

    #[test]
    fn walks_all_fields_with_absolute_spans() {
        let area = Span::new(2, AREA.len() as u32).unwrap();
        let got: Vec<_> = ExtraFields::new(AREA, area).collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, 0x000A);
        assert_eq!(got[0].data.slice(AREA), Some(&[1u8, 2, 3][..]));
        assert_eq!(got[0].span, Span::new(2, 9).unwrap());
        assert_eq!(got[1].id, 0xA220);
        assert_eq!(got[1].data.slice(AREA), Some(&[9u8, 9][..]));
    }

    #[test]
    fn find_returns_requested_id() {
        let area = Span::new(2, AREA.len() as u32).unwrap();
        assert!(ExtraFields::find(AREA, area, 0xA220).is_some());
        assert!(ExtraFields::find(AREA, area, 0x0001).is_none());
    }

    #[test]
    fn truncated_area_stops_without_error() {
        // Хвост в 3 байта не образует даже заголовка записи.
        let src = &[0x0A, 0x00, 0x00, 0x00, 1, 2, 3];
        let area = Span::new(0, 7).unwrap();
        let mut it = ExtraFields::new(src, area);
        assert_eq!(it.next().map(|f| f.id), Some(0x000A));
        assert!(it.next().is_none());
        assert!(it.is_truncated());
    }

    #[test]
    fn oversized_field_does_not_escape_area() {
        // size = 0xFFFF при области в 5 байт.
        let src = &[0x0A, 0x00, 0xFF, 0xFF, 0];
        let area = Span::new(0, 5).unwrap();
        let mut it = ExtraFields::new(src, area);
        assert!(it.next().is_none());
        assert!(it.is_truncated());
    }

    #[test]
    fn area_outside_buffer_yields_nothing() {
        let src = &[1u8, 2, 3];
        let area = Span::new(2, 99).unwrap();
        assert_eq!(ExtraFields::new(src, area).count(), 0);
    }
}
