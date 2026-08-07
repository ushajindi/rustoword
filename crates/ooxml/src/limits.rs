//! Квоты для обработки недоверенного входа.
//!
//! Все ограничения собраны в одну структуру, чтобы политику безопасности можно
//! было прочитать целиком в одном месте, а не выискивать константы по коду.
//!
//! Значения по умолчанию рассчитаны на файлы до ~50 МБ и оставляют заметный
//! запас над реальными документами: самый большой файл тестового корпуса —
//! 1,7 МБ при 313 тыс. ячеек.
//!
//! # Почему заголовкам нельзя верить
//!
//! Поле `uncompressed size` в ZIP-заголовке пишет тот, кто создал архив. Проверка
//! только по нему бесполезна: атакующий поставит там `100`, а в потоке будет
//! гигабайт. Поэтому [`Limits::max_total_uncompressed`] и
//! [`Limits::max_compression_ratio`] проверяются **инкрементально внутри цикла
//! распаковки**, а заголовок используется лишь для раннего отказа.

use crate::error::{LimitError, Result};

/// Квоты на разбор пакета.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Максимальный размер входного буфера.
    pub max_input_bytes: u64,
    /// Максимальное число записей в ZIP-контейнере.
    pub max_entries: u64,
    /// Максимальный распакованный размер одной части.
    pub max_part_bytes: u64,
    /// Максимальный суммарный распакованный размер всех частей.
    pub max_total_uncompressed: u64,
    /// Максимальное отношение распакованного размера к сжатому для одной записи.
    ///
    /// Проверяется каждые [`Self::RATIO_CHECK_STRIDE`] байт выхода, а не по
    /// заголовку. Реальные XML-части жмутся в 10–20 раз; 200 оставляет запас.
    pub max_compression_ratio: u64,
    /// Максимальная глубина вложенности XML.
    pub max_xml_depth: u64,
    /// Максимальное число атрибутов одного элемента.
    pub max_attrs_per_element: u64,
    /// Максимальное число узлов дерева одной части.
    pub max_nodes_per_part: u64,
    /// Максимальная длина имени (элемента, атрибута, части).
    pub max_name_bytes: u64,
    /// Разрешать ли `<!DOCTYPE`.
    ///
    /// Всегда `false` и не должно становиться `true`: DOCTYPE — вектор XXE и
    /// billion-laughs. Поле существует, чтобы намерение было явным в коде, а не
    /// спрятанным в ветке парсера.
    pub allow_doctype: bool,
    /// Сверять ли CRC каждой распакованной записи.
    pub verify_crc_on_read: bool,
}

// Проверки квот определены целиком уже сейчас, чтобы слои можно было писать
// параллельно, не дописывая каждый раз этот файл. Часть из них подключается
// на своих вехах — до тех пор они формально не вызываются.
#[allow(dead_code)]
impl Limits {
    /// Через сколько байт выхода inflate пересчитывает коэффициент сжатия.
    pub const RATIO_CHECK_STRIDE: u64 = 64 * 1024;

    /// Профиль по умолчанию: недоверенный вход, файлы до ~50 МБ.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            max_input_bytes: 64 * 1024 * 1024,
            max_entries: 8_192,
            max_part_bytes: 128 * 1024 * 1024,
            max_total_uncompressed: 512 * 1024 * 1024,
            max_compression_ratio: 200,
            max_xml_depth: 256,
            max_attrs_per_element: 4_096,
            max_nodes_per_part: 8_000_000,
            max_name_bytes: 1_024,
            allow_doctype: false,
            verify_crc_on_read: true,
        }
    }

    /// Ослабленный профиль для доверенного входа.
    ///
    /// `allow_doctype` остаётся `false` — послаблений по XXE не бывает.
    #[must_use]
    pub const fn permissive() -> Self {
        Self {
            max_input_bytes: 1024 * 1024 * 1024,
            max_entries: 1 << 20,
            max_part_bytes: 1024 * 1024 * 1024,
            max_total_uncompressed: 4 * 1024 * 1024 * 1024,
            max_compression_ratio: 10_000,
            max_xml_depth: 4_096,
            max_attrs_per_element: 65_536,
            max_nodes_per_part: 64_000_000,
            max_name_bytes: 65_536,
            allow_doctype: false,
            verify_crc_on_read: true,
        }
    }

    pub(crate) fn check_input(&self, got: u64) -> Result<()> {
        if got > self.max_input_bytes {
            return Err(LimitError::InputTooLarge {
                got,
                max: self.max_input_bytes,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_entries(&self, got: u64) -> Result<()> {
        if got > self.max_entries {
            return Err(LimitError::TooManyEntries {
                got,
                max: self.max_entries,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_part_size(&self, got: u64) -> Result<()> {
        if got > self.max_part_bytes {
            return Err(LimitError::PartTooLarge {
                got,
                max: self.max_part_bytes,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_total_uncompressed(&self, got: u64) -> Result<()> {
        if got > self.max_total_uncompressed {
            return Err(LimitError::TotalUncompressed {
                got,
                max: self.max_total_uncompressed,
            }
            .into());
        }
        Ok(())
    }

    /// Проверяет коэффициент сжатия по фактически произведённому выходу.
    ///
    /// `compressed` может быть нулём в начале потока — тогда проверка
    /// пропускается, иначе любой поток мгновенно «превысил бы» лимит.
    pub(crate) fn check_ratio(&self, produced: u64, compressed: u64) -> Result<()> {
        if compressed == 0 {
            return Ok(());
        }
        // Делитель проверен на ноль строкой выше — иных side-effect'ов у
        // целочисленного деления нет.
        #[allow(clippy::arithmetic_side_effects)]
        let ratio = produced / compressed;
        if ratio > self.max_compression_ratio {
            return Err(LimitError::CompressionRatio {
                got: ratio,
                max: self.max_compression_ratio,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_depth(&self, got: u64) -> Result<()> {
        if got > self.max_xml_depth {
            return Err(LimitError::XmlDepth {
                got,
                max: self.max_xml_depth,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_attrs(&self, got: u64) -> Result<()> {
        if got > self.max_attrs_per_element {
            return Err(LimitError::TooManyAttrs {
                got,
                max: self.max_attrs_per_element,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_nodes(&self, got: u64) -> Result<()> {
        if got > self.max_nodes_per_part {
            return Err(LimitError::TooManyNodes {
                got,
                max: self.max_nodes_per_part,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) fn check_name_len(&self, got: u64) -> Result<()> {
        if got > self.max_name_bytes {
            return Err(LimitError::NameTooLong {
                got,
                max: self.max_name_bytes,
            }
            .into());
        }
        Ok(())
    }
}

impl Default for Limits {
    /// Строгий профиль. Безопасное поведение должно требовать нулевых усилий.
    fn default() -> Self {
        Self::strict()
    }
}

#[cfg(test)]
mod tests {
    // В тестах паника — это способ сообщить о провале, а не дефект.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::error::{Error, LimitError};

    #[test]
    fn default_is_strict() {
        assert_eq!(Limits::default(), Limits::strict());
    }

    #[test]
    fn doctype_forbidden_in_every_profile() {
        assert!(!Limits::strict().allow_doctype);
        assert!(!Limits::permissive().allow_doctype);
    }

    #[test]
    fn ratio_skipped_at_stream_start() {
        // Пока сжатых байт не потреблено, отношение не определено — проверка
        // не должна срабатывать, иначе любой поток отвергался бы сразу.
        assert!(Limits::strict().check_ratio(1 << 20, 0).is_ok());
    }

    #[test]
    fn ratio_catches_bomb() {
        let l = Limits::strict();
        assert!(l.check_ratio(100 * 1024, 1024).is_ok());
        let err = l.check_ratio(1024 * 1024, 1024).unwrap_err();
        assert!(matches!(
            err,
            Error::Limit(LimitError::CompressionRatio {
                got: 1024,
                max: 200
            })
        ));
    }

    #[test]
    fn boundary_is_inclusive() {
        let l = Limits::strict();
        assert!(l.check_entries(l.max_entries).is_ok());
        assert!(l.check_entries(l.max_entries + 1).is_err());
    }
}
