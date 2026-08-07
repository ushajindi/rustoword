//! Сжатие и распаковка DEFLATE (RFC 1951).
//!
//! Распаковка реализована в вехе M1; [`deflate`] остаётся заглушкой до M4.
//! Сигнатуры — контракт со слоем `zip`, который пишется параллельно.
//!
//! # Устройство распаковщика
//!
//! - `bitreader` отдаёт биты в порядке потока (LSB-first);
//! - `huffman` строит по длинам кодов двухуровневую таблицу просмотра;
//! - `inflate` разбирает блоки и разворачивает совпадения LZ77.
//!
//! # Почему лимиты передаются целиком
//!
//! Распаковщик обязан проверять коэффициент сжатия **по фактически
//! произведённому выходу**, каждые [`Limits::RATIO_CHECK_STRIDE`] байт. Заголовок
//! ZIP сообщает распакованный размер, но его пишет создатель архива — в zip-бомбе
//! там стоит правдоподобная цифра. Поэтому лимиты нужны внутри цикла, а не
//! снаружи него.

mod bitreader;
mod huffman;
mod inflate;

use crate::error::Result;
use crate::limits::Limits;

/// Уровень сжатия.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Без сжатия — блоки типа `stored`.
    Store,
    Fast,
    #[default]
    Default,
    Best,
}

/// Распаковывает поток DEFLATE целиком.
///
/// # Errors
/// [`crate::Error::Deflate`] на битом потоке, [`crate::Error::Limit`] при
/// превышении квоты.
pub fn inflate(input: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    inflate_into(input, &mut out, limits)?;
    Ok(out)
}

/// Распаковывает поток в переданный буфер.
///
/// Возвращает число **потреблённых входных байт** — оно нужно слою `zip`, чтобы
/// проверить, что за потоком стоит ожидаемая структура (дескриптор или
/// следующий заголовок), а не мусор.
///
/// # Errors
/// [`crate::Error::Deflate`] на битом потоке, [`crate::Error::Limit`] при
/// превышении квоты.
pub fn inflate_into(input: &[u8], out: &mut Vec<u8>, limits: &Limits) -> Result<usize> {
    inflate::inflate_into(input, out, limits)
}

/// Сжимает данные в поток DEFLATE.
///
/// Не возвращает `Result`: сжатие корректного входа не может провалиться.
/// Детерминирован — одинаковый вход даёт одинаковый выход, иначе round-trip
/// перестал бы быть воспроизводимым.
#[must_use]
pub fn deflate(input: &[u8], level: Level) -> Vec<u8> {
    let _ = (input, level);
    Vec::new()
}
