//! Буфер вывода с отложенной записью полей.

use crate::error::{Error, Result, ZipError};

/// Зарезервированное место, которое будет заполнено позже.
///
/// ZIP-заголовок содержит поля (размеры, CRC, офсет центрального каталога),
/// значение которых известно только после того, как данные записаны. Вместо
/// двух проходов резервируется место и запоминается [`Patch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch {
    at: usize,
    len: usize,
}

/// Растущий буфер вывода.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Текущая длина вывода — она же офсет следующего записанного байта.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Текущая позиция как `u64` — для полей офсетов в ZIP-заголовках.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.buf.len() as u64
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn le_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn le_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn le_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Резервирует 4 байта под значение, которое станет известно позже.
    pub fn reserve_u32(&mut self) -> Patch {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0u8; 4]);
        Patch { at, len: 4 }
    }

    /// Резервирует 8 байт под значение, которое станет известно позже.
    pub fn reserve_u64(&mut self) -> Patch {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0u8; 8]);
        Patch { at, len: 8 }
    }

    /// Заполняет ранее зарезервированное место.
    ///
    /// Ошибка, а не паника: [`Patch`] мог прийти из другого `Writer`, и молча
    /// проигнорировать это — значит выдать наружу битый архив.
    pub fn patch_u32(&mut self, p: Patch, v: u32) -> Result<()> {
        if p.len != 4 {
            return Err(Error::Zip {
                kind: ZipError::OffsetOverflow,
                offset: p.at as u64,
            });
        }
        let dst = self
            .buf
            .get_mut(p.at..p.at.saturating_add(4))
            .ok_or(Error::Zip {
                kind: ZipError::OffsetOverflow,
                offset: p.at as u64,
            })?;
        dst.copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    /// Заполняет ранее зарезервированное 8-байтовое место.
    pub fn patch_u64(&mut self, p: Patch, v: u64) -> Result<()> {
        if p.len != 8 {
            return Err(Error::Zip {
                kind: ZipError::OffsetOverflow,
                offset: p.at as u64,
            });
        }
        let dst = self
            .buf
            .get_mut(p.at..p.at.saturating_add(8))
            .ok_or(Error::Zip {
                kind: ZipError::OffsetOverflow,
                offset: p.at as u64,
            })?;
        dst.copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn patch_fills_reserved_slot() {
        let mut w = Writer::new();
        w.le_u16(0xBEEF);
        let p = w.reserve_u32();
        w.bytes(b"tail");
        assert_eq!(w.offset(), 10);
        w.patch_u32(p, 0x1122_3344).unwrap();
        assert_eq!(
            w.finish(),
            vec![0xEF, 0xBE, 0x44, 0x33, 0x22, 0x11, b't', b'a', b'i', b'l']
        );
    }

    #[test]
    fn patch_of_wrong_width_is_error_not_silent() {
        let mut w = Writer::new();
        let p = w.reserve_u64();
        assert!(w.patch_u32(p, 1).is_err());
    }

    #[test]
    fn foreign_patch_rejected() {
        let mut a = Writer::new();
        let mut b = Writer::new();
        a.bytes(&[0u8; 32]);
        let p = a.reserve_u32();
        // `b` короче — патч из `a` в него не помещается.
        assert!(b.patch_u32(p, 7).is_err());
    }
}
