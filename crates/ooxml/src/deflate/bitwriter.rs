//! Битовый writer LSB-first для DEFLATE.
//!
//! Зеркало [`BitReader`](crate::deflate::bitreader::BitReader): биты
//! укладываются от младшего к старшему внутри байта (RFC 1951 §3.1.1).
//!
//! Коды Хаффмана лежат в потоке старшим битом кода вперёд, но разворачивать их
//! здесь не нужно: энкодер разворачивает код один раз при построении таблицы,
//! и writer видит уже готовое к укладке значение. Разворот на каждом символе
//! стоил бы одну инструкцию на токен — при сотнях тысяч токенов это заметно.
//!
//! Накопитель на 64 бита. За один вызов приходит не больше 32 бит, а выгрузка
//! идёт целыми байтами: толкать `Vec` побитово было бы на порядок дороже.

/// Курсор записи в битовый поток.
#[derive(Debug)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    /// Ещё не выгруженные биты; ближайший к записи — младший.
    acc: u64,
    /// Сколько бит занято в `acc`. Между вызовами всегда меньше восьми.
    n: u32,
}

impl BitWriter {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            out: Vec::with_capacity(cap),
            acc: 0,
            n: 0,
        }
    }

    /// Пишет младшие `bits` (не больше 32) бит значения.
    #[inline]
    pub(crate) fn write_bits(&mut self, value: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        // Маска обрезает мусор в старших разрядах: вызывающий вправе передать
        // код вместе с сопутствующими ему битами в одном `u32`.
        let mask = 1u64.wrapping_shl(bits).wrapping_sub(1);
        self.acc |= (u64::from(value) & mask).wrapping_shl(self.n);
        self.n = self.n.wrapping_add(bits);
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n = self.n.wrapping_sub(8);
        }
    }

    /// Сколько бит уже записано.
    ///
    /// Нужно оценке стоимости stored-блока: тот выравнивается на байт, и цена
    /// выравнивания зависит от того, на каком бите его застали.
    pub(crate) fn bit_len(&self) -> u64 {
        (self.out.len() as u64)
            .saturating_mul(8)
            .saturating_add(u64::from(self.n))
    }

    /// Добивает текущий байт нулями.
    ///
    /// Нули, а не мусор: RFC не определяет содержимое добивки, но воспроизводимый
    /// выход требует, чтобы оно было фиксированным.
    pub(crate) fn align_to_byte(&mut self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    /// Пишет байты как есть. Поток обязан быть выровнен — иначе байты сдвинутся
    /// относительно границы и stored-блок станет нечитаемым.
    pub(crate) fn write_bytes(&mut self, data: &[u8]) {
        self.align_to_byte();
        self.out.extend_from_slice(data);
    }

    /// Закрывает поток, добивая последний байт нулями.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.out
    }
}

#[cfg(test)]
mod tests {
    // В тестах паника — это способ сообщить о провале, а не дефект.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use crate::deflate::bitreader::BitReader;

    #[test]
    fn writes_lsb_first() {
        let mut w = BitWriter::with_capacity(0);
        w.write_bits(0, 1);
        w.write_bits(0b01, 2);
        w.write_bits(0b1_0110, 5);
        assert_eq!(w.finish(), vec![0xB2], "тот же байт, что читает bitreader");
    }

    #[test]
    fn round_trips_through_bitreader() {
        let pattern: [(u32, u32); 6] = [
            (0x1, 1),
            (0x2A, 6),
            (0x1FF, 9),
            (0x0, 3),
            (0xFFFF_FFFF, 32),
            (0x5, 4),
        ];
        let mut w = BitWriter::with_capacity(0);
        for &(v, n) in &pattern {
            w.write_bits(v, n);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for &(v, n) in &pattern {
            let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
            assert_eq!(r.bits(n), Some(v & mask), "{v:#x} в {n} бит");
        }
    }

    #[test]
    fn align_pads_with_zeros() {
        let mut w = BitWriter::with_capacity(0);
        w.write_bits(0b111, 3);
        assert_eq!(w.bit_len(), 3);
        w.align_to_byte();
        assert_eq!(w.bit_len(), 8);
        w.write_bytes(&[0xAA, 0xBB]);
        assert_eq!(w.finish(), vec![0x07, 0xAA, 0xBB]);
    }

    #[test]
    fn write_bytes_aligns_by_itself() {
        let mut w = BitWriter::with_capacity(0);
        w.write_bits(1, 1);
        w.write_bytes(&[0xFF]);
        assert_eq!(w.finish(), vec![0x01, 0xFF]);
    }

    #[test]
    fn empty_writer_yields_nothing() {
        assert!(BitWriter::with_capacity(0).finish().is_empty());
    }

    #[test]
    fn zero_width_write_is_a_noop() {
        let mut w = BitWriter::with_capacity(0);
        w.write_bits(0xFFFF, 0);
        assert_eq!(w.bit_len(), 0);
        assert!(w.finish().is_empty());
    }
}
