//! Сжатие и распаковка DEFLATE (RFC 1951).
//!
//! ЗАГЛУШКА ВЕХИ M0. Здесь зафиксированы только сигнатуры — они контракт между
//! вехами M1 (inflate), M4 (deflate) и слоем `zip`, который пишется параллельно.
//! Реализация приходит в M1/M4; до тех пор функции возвращают
//! [`Error::Unsupported`].
//!
//! # Почему лимиты передаются целиком
//!
//! Распаковщик обязан проверять коэффициент сжатия **по фактически
//! произведённому выходу**, каждые [`Limits::RATIO_CHECK_STRIDE`] байт. Заголовок
//! ZIP сообщает распакованный размер, но его пишет создатель архива — в zip-бомбе
//! там стоит правдоподобная цифра. Поэтому лимиты нужны внутри цикла, а не
//! снаружи него.

use crate::error::{Error, Result};
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
/// [`Error::Deflate`] на битом потоке, [`Error::Limit`] при превышении квоты.
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
/// [`Error::Deflate`] на битом потоке, [`Error::Limit`] при превышении квоты.
pub fn inflate_into(input: &[u8], out: &mut Vec<u8>, limits: &Limits) -> Result<usize> {
    let _ = (input, out, limits);
    Err(Error::Unsupported("inflate появится в вехе M1"))
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
