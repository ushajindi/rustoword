//! Битовый ридер LSB-first для DEFLATE.
//!
//! RFC 1951 §3.1.1: биты упаковываются от младшего к старшему внутри байта, но
//! коды Хаффмана записываются начиная со старшего бита кода. Ридер отдаёт биты
//! ровно в порядке потока; разворот кода — забота построителя таблиц, иначе
//! пришлось бы разворачивать на каждом символе вместо одного раза на блок.
//!
//! Накопитель на 64 бита позволяет декодировать символ одним подглядыванием:
//! после подкачки в нём заведомо не меньше 57 бит, а самый длинный код DEFLATE
//! вместе с дополнительными битами короче.

/// Курсор по битовому потоку с проверкой границ.
#[derive(Debug, Clone)]
pub(crate) struct BitReader<'a> {
    src: &'a [u8],
    /// Индекс байта, с которого пойдёт следующая подкачка.
    next: usize,
    /// Непрочитанные биты; ближайший к чтению — младший.
    buf: u64,
    /// Сколько бит в `buf` реально прочитано из `src`.
    count: u32,
}

impl<'a> BitReader<'a> {
    pub(crate) const fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            next: 0,
            buf: 0,
            count: 0,
        }
    }

    /// Добирает накопитель до 57+ бит, пока во входе есть байты.
    #[inline]
    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(&b) = self.src.get(self.next) else {
                break;
            };
            self.buf |= u64::from(b) << self.count;
            self.count = self.count.wrapping_add(8);
            self.next = self.next.wrapping_add(1);
        }
    }

    /// Смотрит `n` (не больше 32) бит, добивая нулями за концом потока.
    ///
    /// Нули за концом безопасны: перерасход ловит [`Self::consume`], а декодеру
    /// Хаффмана нужно увидеть биты раньше, чем станет известна длина кода.
    #[inline]
    pub(crate) fn peek(&mut self, n: u32) -> u32 {
        self.refill();
        let mask = 1u64.wrapping_shl(n).wrapping_sub(1);
        (self.buf & mask) as u32
    }

    /// Отбрасывает `n` бит. `None`, если столько бит во входе уже нет.
    #[inline]
    pub(crate) fn consume(&mut self, n: u32) -> Option<()> {
        self.refill();
        if n > self.count {
            return None;
        }
        self.buf = self.buf.wrapping_shr(n);
        self.count = self.count.wrapping_sub(n);
        Some(())
    }

    /// Читает `n` (не больше 32) бит. `None` при обрыве потока.
    #[inline]
    pub(crate) fn bits(&mut self, n: u32) -> Option<u32> {
        let v = self.peek(n);
        self.consume(n)?;
        Some(v)
    }

    /// Позиция чтения в битах от начала входа.
    pub(crate) fn bit_pos(&self) -> u64 {
        (self.next as u64)
            .saturating_mul(8)
            .saturating_sub(u64::from(self.count))
    }

    /// Число потреблённых входных байт с округлением вверх.
    ///
    /// Округление вверх — то, что нужно слою `zip`: за последним блоком стоит
    /// либо дескриптор, либо следующий заголовок, и оба начинаются с границы
    /// байта, поэтому недочитанные биты последнего байта считаются съеденными.
    pub(crate) fn bytes_consumed(&self) -> usize {
        let bits = self
            .next
            .saturating_mul(8)
            .saturating_sub(self.count as usize);
        bits.saturating_add(7) / 8
    }

    /// Выравнивает чтение на границу байта, отбрасывая остаток текущего байта.
    pub(crate) fn align_to_byte(&mut self) {
        // Потреблено `next * 8 - count` бит, значит до границы байта надо
        // отбросить ровно `count % 8` бит — они заведомо прочитаны из `src`.
        let extra = self.count % 8;
        self.buf = self.buf.wrapping_shr(extra);
        self.count = self.count.wrapping_sub(extra);
    }

    /// Выравнивает поток и читает `n` байт подряд. `None` при обрыве.
    pub(crate) fn take_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.align_to_byte();
        // После выравнивания в накопителе лежат только целые байты — вернём их
        // обратно во вход, чтобы срез считался прямо из `src`.
        let start = self.next.checked_sub((self.count / 8) as usize)?;
        let end = start.checked_add(n)?;
        let out = self.src.get(start..end)?;
        self.buf = 0;
        self.count = 0;
        self.next = end;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    // В тестах паника — это способ сообщить о провале, а не дефект.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;

    #[test]
    fn reads_lsb_first() {
        // 0b1011_0010 = 0xB2: младший бит первым.
        let mut r = BitReader::new(&[0xB2]);
        assert_eq!(r.bits(1), Some(0));
        assert_eq!(r.bits(2), Some(0b01));
        assert_eq!(r.bits(5), Some(0b1_0110));
    }

    #[test]
    fn crosses_byte_boundary() {
        let mut r = BitReader::new(&[0xFF, 0x00]);
        assert_eq!(r.bits(12), Some(0x0FF));
    }

    #[test]
    fn peek_pads_with_zeros_but_consume_refuses() {
        let mut r = BitReader::new(&[0x01]);
        assert_eq!(r.peek(16), 0x0001, "за концом потока подставляются нули");
        assert_eq!(r.consume(16), None, "но потребить их нельзя");
        assert_eq!(r.consume(8), Some(()));
    }

    #[test]
    fn empty_input_yields_nothing() {
        let mut r = BitReader::new(&[]);
        assert_eq!(r.peek(9), 0);
        assert_eq!(r.bits(1), None);
        assert_eq!(r.bytes_consumed(), 0);
    }

    #[test]
    fn align_rounds_up_consumed_bytes() {
        let mut r = BitReader::new(&[1, 2, 3, 4]);
        assert_eq!(r.bits(3), Some(1));
        assert_eq!(r.bytes_consumed(), 1, "три бита — это уже начатый байт");
        r.align_to_byte();
        assert_eq!(r.bit_pos(), 8);
        assert_eq!(r.take_bytes(2), Some(&[2u8, 3][..]));
        assert_eq!(r.bytes_consumed(), 3);
    }

    #[test]
    fn take_bytes_past_end_is_none_and_harmless() {
        let mut r = BitReader::new(&[1, 2]);
        assert_eq!(r.take_bytes(3), None);
        assert_eq!(r.take_bytes(usize::MAX), None);
        assert_eq!(r.take_bytes(2), Some(&[1u8, 2][..]));
    }
}
