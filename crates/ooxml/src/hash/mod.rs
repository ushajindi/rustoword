//! Контрольные суммы.
//!
//! ЗАГЛУШКА ВЕХИ M0. Сигнатуры зафиксированы, чтобы слой `zip` компилировался
//! до готовности M1. Реализацию приносит веха M1.

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
        let mut c = self.state;
        for &b in data {
            let idx = usize::from((c as u8) ^ b);
            let mut e = idx as u32;
            for _ in 0..8 {
                e = if e & 1 != 0 {
                    (e >> 1) ^ 0xEDB8_8320
                } else {
                    e >> 1
                };
            }
            c = e ^ (c >> 8);
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
}
