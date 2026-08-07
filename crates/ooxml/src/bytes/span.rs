//! Диапазон байт в исходном буфере.

use core::fmt;

/// Полуоткрытый диапазон `[start, end)` в исходном буфере.
///
/// Спан — основа сохранности: узлы дерева не копируют байты, а ссылаются на
/// исходник. Чистое поддерево сериализуется одним `extend_from_slice`, и никакая
/// лексическая деталь — стиль экранирования, пробелы внутри тега, символ
/// кавычки — не теряется просто потому, что её никто не переписывал.
///
/// Поля `u32`, а не `usize`: максимальный размер части ограничен
/// [`crate::Limits::max_part_bytes`], а компактность важна — `Span` встраивается
/// в каждый узел дерева, которых бывают миллионы.
#[derive(Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct Span {
    start: u32,
    end: u32,
}

impl Span {
    /// Пустой спан в позиции `at`.
    #[must_use]
    pub const fn empty(at: u32) -> Self {
        Self { start: at, end: at }
    }

    /// Создаёт спан. Возвращает `None`, если `end < start`.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Option<Self> {
        if end < start {
            None
        } else {
            Some(Self { start, end })
        }
    }

    /// Создаёт спан, схлопывая перевёрнутый диапазон в пустой.
    ///
    /// Для случаев, где перевёрнутость невозможна по построению и городить
    /// `Result` — лишний шум.
    #[must_use]
    pub const fn clamped(start: u32, end: u32) -> Self {
        if end < start {
            Self::empty(start)
        } else {
            Self { start, end }
        }
    }

    #[must_use]
    pub const fn start(self) -> u32 {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> u32 {
        self.end
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    /// Вырезает спан из буфера. `None`, если спан выходит за границы.
    #[must_use]
    pub fn slice(self, src: &[u8]) -> Option<&[u8]> {
        src.get(self.start as usize..self.end as usize)
    }

    /// Примыкает ли `other` вплотную справа.
    ///
    /// Используется для проверки инварианта покрытия: спаны детей должны
    /// плотно, без пропусков и наложений, покрывать внутренность родителя.
    #[must_use]
    pub const fn touches(self, other: Self) -> bool {
        self.end == other.start
    }

    /// Объединяет два примыкающих или пересекающихся спана.
    #[must_use]
    pub const fn join(self, other: Self) -> Self {
        Self {
            start: if self.start < other.start {
                self.start
            } else {
                other.start
            },
            end: if self.end > other.end {
                self.end
            } else {
                other.end
            },
        }
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn reversed_range_rejected() {
        assert!(Span::new(5, 3).is_none());
        assert!(Span::new(3, 3).is_some());
    }

    #[test]
    fn clamped_collapses_instead_of_panicking() {
        let s = Span::clamped(9, 4);
        assert!(s.is_empty());
        assert_eq!(s.start(), 9);
    }

    #[test]
    fn slice_out_of_bounds_is_none() {
        let src = b"abcdef";
        assert_eq!(Span::clamped(2, 4).slice(src), Some(&b"cd"[..]));
        assert_eq!(Span::clamped(4, 99).slice(src), None);
        // Пустой спан за пределами буфера тоже отвергается — иначе он стал бы
        // способом молча получить пустые данные там, где их нет.
        assert_eq!(Span::clamped(99, 99).slice(src), None);
    }

    #[test]
    fn touching_spans_tile() {
        let a = Span::clamped(0, 4);
        let b = Span::clamped(4, 9);
        assert!(a.touches(b));
        assert!(!b.touches(a));
        assert_eq!(a.join(b), Span::clamped(0, 9));
    }
}
