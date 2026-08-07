//! Курсор по байтовому буферу с проверкой границ.

use crate::bytes::Span;

/// Курсор по `&[u8]`, где каждая операция проверяет границы.
///
/// Все методы возвращают `Option`, а не `Result`: `Reader` не знает, ошибкой
/// какого слоя является выход за границу — ZIP-заголовок обернёт это в
/// [`ZipError::Truncated`](crate::error::ZipError::Truncated), XML-лексер в
/// [`XmlError::UnexpectedEof`](crate::error::XmlError::UnexpectedEof). Слой
/// решает сам через `ok_or_else`.
///
/// Числа читаются little-endian: и ZIP, и все бинарные части OOXML — LE.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub const fn new(src: &'a [u8]) -> Self {
        Self { src, pos: 0 }
    }

    /// Курсор, установленный на позицию `pos`. `None`, если позиция за концом.
    #[must_use]
    pub fn at(src: &'a [u8], pos: usize) -> Option<Self> {
        if pos > src.len() {
            return None;
        }
        Some(Self { src, pos })
    }

    #[must_use]
    pub const fn pos(&self) -> usize {
        self.pos
    }

    #[must_use]
    pub const fn src(&self) -> &'a [u8] {
        self.src
    }

    /// Сколько байт осталось до конца буфера.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.src.len().saturating_sub(self.pos)
    }

    #[must_use]
    pub const fn is_eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Переставляет курсор. `None`, если позиция за концом буфера.
    pub fn seek(&mut self, pos: usize) -> Option<()> {
        if pos > self.src.len() {
            return None;
        }
        self.pos = pos;
        Some(())
    }

    /// Сдвигает курсор вперёд на `n`. `None` при выходе за конец.
    pub fn skip(&mut self, n: usize) -> Option<()> {
        let next = self.pos.checked_add(n)?;
        self.seek(next)
    }

    /// Читает `n` байт, сдвигая курсор.
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.src.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    /// Читает `n` байт как [`Span`], сдвигая курсор.
    ///
    /// Возвращает `None`, если диапазон не помещается в `u32` — спаны
    /// намеренно 32-битные, и молчаливое усечение здесь было бы источником
    /// тихой порчи данных.
    pub fn take_span(&mut self, n: usize) -> Option<Span> {
        let start = u32::try_from(self.pos).ok()?;
        let end_usize = self.pos.checked_add(n)?;
        if end_usize > self.src.len() {
            return None;
        }
        let end = u32::try_from(end_usize).ok()?;
        self.pos = end_usize;
        Span::new(start, end)
    }

    /// Смотрит `n` байт, не сдвигая курсор.
    #[must_use]
    pub fn peek(&self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        self.src.get(self.pos..end)
    }

    /// Смотрит байт по смещению `off` от курсора, не сдвигая его.
    #[must_use]
    pub fn peek_at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos.checked_add(off)?).copied()
    }

    pub fn u8(&mut self) -> Option<u8> {
        let b = self.src.get(self.pos).copied()?;
        self.pos = self.pos.checked_add(1)?;
        Some(b)
    }

    pub fn le_u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([*b.first()?, *b.get(1)?]))
    }

    pub fn le_u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([
            *b.first()?,
            *b.get(1)?,
            *b.get(2)?,
            *b.get(3)?,
        ]))
    }

    pub fn le_u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes([
            *b.first()?,
            *b.get(1)?,
            *b.get(2)?,
            *b.get(3)?,
            *b.get(4)?,
            *b.get(5)?,
            *b.get(6)?,
            *b.get(7)?,
        ]))
    }

    /// Читает 4 байта как сигнатуру, не сдвигая курсор при несовпадении.
    ///
    /// Курсор сдвигается только при успехе — это позволяет пробовать несколько
    /// сигнатур подряд, не сохраняя и не восстанавливая позицию вручную.
    pub fn expect_sig(&mut self, sig: u32) -> Option<()> {
        let saved = self.pos;
        match self.le_u32() {
            Some(v) if v == sig => Some(()),
            _ => {
                self.pos = saved;
                None
            }
        }
    }

    /// Ищет последнее вхождение образца в окне последних `window` байт.
    ///
    /// Нужно для поиска EOCD: он лежит в конце файла, но за ним может быть
    /// комментарий архива, и сама последовательность `PK\x05\x06` может
    /// встретиться в данных — поэтому нужны все вхождения, а не первое.
    #[must_use]
    pub fn rfind_in_tail(src: &[u8], needle: &[u8], window: usize) -> Option<usize> {
        let from = src.len().saturating_sub(window);
        let tail = src.get(from..)?;
        let idx = tail.windows(needle.len()).rposition(|w| w == needle)?;
        from.checked_add(idx)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn reads_little_endian() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(r.le_u16(), Some(0x0201));
        assert_eq!(r.le_u32(), Some(0x0605_0403));
        assert_eq!(r.pos(), 6);
    }

    #[test]
    fn truncated_read_leaves_none() {
        let mut r = Reader::new(&[0x01, 0x02]);
        assert_eq!(r.le_u32(), None);
        // Неудачное чтение не должно двигать курсор частично.
        assert_eq!(r.remaining(), 2);
    }

    #[test]
    fn skip_past_end_rejected_without_moving() {
        let mut r = Reader::new(&[0u8; 4]);
        assert_eq!(r.skip(5), None);
        assert_eq!(r.pos(), 0);
        assert_eq!(r.skip(4), Some(()));
        assert!(r.is_eof());
    }

    #[test]
    fn skip_overflow_does_not_wrap() {
        let mut r = Reader::new(&[0u8; 4]);
        assert_eq!(r.skip(usize::MAX), None);
        assert_eq!(r.pos(), 0);
    }

    #[test]
    fn failed_signature_rewinds() {
        let mut r = Reader::new(&[0x50, 0x4b, 0x03, 0x04, 0xff]);
        assert_eq!(r.expect_sig(0x0201_0403), None);
        assert_eq!(r.pos(), 0, "неудачная сигнатура не должна двигать курсор");
        assert_eq!(r.expect_sig(0x0403_4b50), Some(()));
        assert_eq!(r.pos(), 4);
    }

    #[test]
    fn rfind_finds_last_occurrence() {
        // Сигнатура встречается дважды: первая — данные, вторая — настоящий EOCD.
        //             0123 4567890
        let src = b"PK\x05\x06___PK\x05\x06xy";
        assert_eq!(Reader::rfind_in_tail(src, b"PK\x05\x06", 64), Some(7));
    }

    #[test]
    fn rfind_respects_window() {
        let src = b"PK\x05\x06________________";
        assert_eq!(Reader::rfind_in_tail(src, b"PK\x05\x06", 4), None);
    }

    #[test]
    fn take_span_rejects_overflow_of_u32() {
        let src = [0u8; 8];
        let mut r = Reader::new(&src);
        assert_eq!(r.take_span(usize::MAX), None);
        assert_eq!(r.pos(), 0);
        let s = r.take_span(4).unwrap();
        assert_eq!(s.slice(&src), Some(&[0u8, 0, 0, 0][..]));
    }
}
