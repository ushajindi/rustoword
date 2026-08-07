//! Контрольные суммы.
//!
//! ZIP требует CRC-32/ISO-HDLC для каждой записи, и считать его приходится по
//! всему распакованному объёму пакета. Побитовая свёртка тратит восемь итераций
//! на байт; здесь используется slice-by-8 — восемь предвычисленных таблиц,
//! обрабатывающих сразу восемь байт за один проход.

/// Порождающий полином CRC-32/ISO-HDLC в отражённом виде.
const POLY: u32 = 0xEDB8_8320;

/// Таблицы slice-by-8. `T[0]` — классическая побайтовая, `T[n]` — та же
/// свёртка, продвинутая ещё на `n` байт вперёд.
const TABLES: [[u32; 256]; 8] = build_tables();

// Индексы ограничены условиями циклов, а вся функция считается компилятором:
// выход за границу здесь был бы ошибкой сборки, а не паникой на входных данных.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
const fn build_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];

    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            bit += 1;
        }
        t[0][i] = c;
        i += 1;
    }

    let mut n = 1;
    while n < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[n - 1][i];
            t[n][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        n += 1;
    }

    t
}

/// Выборка из таблицы без индексации срезов.
///
/// Ветка `None` недостижима — таблица ровно 256 элементов, а индекс получен из
/// `u8`, — но выражена явно, чтобы в коде не было ни одного `[]`.
#[inline]
fn at(table: &[u32; 256], i: u8) -> u32 {
    match table.get(usize::from(i)) {
        Some(v) => *v,
        None => 0,
    }
}

/// CRC-32/ISO-HDLC — тот вариант, что используется в ZIP.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// Потоковый вычислитель CRC-32.
#[derive(Debug, Clone)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    #[must_use]
    pub const fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, data: &[u8]) {
        let [t0, t1, t2, t3, t4, t5, t6, t7] = &TABLES;
        let mut c = self.state;

        let (blocks, tail) = data.as_chunks::<8>();
        for &[b0, b1, b2, b3, b4, b5, b6, b7] in blocks {
            // Первые четыре байта входят в состояние, остальные четыре
            // подмешиваются напрямую — в этом и состоит выигрыш slice-by-8.
            let head = c ^ u32::from_le_bytes([b0, b1, b2, b3]);
            c = at(t7, head as u8)
                ^ at(t6, (head >> 8) as u8)
                ^ at(t5, (head >> 16) as u8)
                ^ at(t4, (head >> 24) as u8)
                ^ at(t3, b4)
                ^ at(t2, b5)
                ^ at(t1, b6)
                ^ at(t0, b7);
        }

        for &b in tail {
            c = at(t0, (c as u8) ^ b) ^ (c >> 8);
        }

        self.state = c;
    }

    #[must_use]
    pub const fn finish(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
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
    fn known_answer_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let mut c = Crc32::new();
        for chunk in data.chunks(97) {
            c.update(chunk);
        }
        assert_eq!(c.finish(), crc32(&data));
    }

    /// Побитовая свёртка — эталон, относительно которого проверяется таблица.
    fn crc32_bitwise(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            }
        }
        c ^ 0xFFFF_FFFF
    }

    #[test]
    fn table_matches_bitwise_on_all_lengths_around_block_size() {
        // Длины 0..40 покрывают все остатки от деления на 8 и переход
        // «хвост без блоков» → «блоки плюс хвост».
        let data: Vec<u8> = (0..40u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(11))
            .collect();
        for n in 0..=data.len() {
            let part = &data[..n];
            assert_eq!(crc32(part), crc32_bitwise(part), "длина {n}");
        }
    }

    #[test]
    fn chunk_boundaries_do_not_shift_result() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let one = crc32(&data);
        for step in [1usize, 3, 7, 8, 9, 64] {
            let mut c = Crc32::new();
            for chunk in data.chunks(step) {
                c.update(chunk);
            }
            assert_eq!(c.finish(), one, "шаг {step}");
        }
    }
}
