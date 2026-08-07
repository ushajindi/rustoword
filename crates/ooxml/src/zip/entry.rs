//! Сырая модель записи архива и хвостовых структур.
//!
//! # Главное правило этого файла
//!
//! Всё, что записано в файле, должно быть здесь представлено — либо значением,
//! либо спаном. Поле, которого нет в модели, при перезаписи архива будет
//! потеряно, и `repack_all_verbatim(f) == f` перестанет выполняться. Поэтому
//! здесь нет «лишних» полей: каждое соответствует конкретным байтам входа.
//!
//! # Почему local и central хранятся раздельно
//!
//! Измерение реального корпуса (43 файла, ~850 записей, три producer'а) даёт
//! факт, который ломает наивные ZIP-writer'ы: у MS Office область extra
//! **локального** заголовка — 520 байт (`0xA220`, Open Packaging Growth Hint),
//! а extra той же записи в **центральном каталоге** — 0 байт. Writer, который
//! копирует каталожный extra в локальный заголовок (а так делает большинство),
//! разойдётся с оригиналом на первой же записи.
//!
//! То же касается имени и `version needed`: формат не требует их совпадения ни
//! по длине, ни по значению. Совпадение имён мы всё же требуем — расхождение
//! там означает не вариацию writer'а, а попытку показать разным читателям
//! разные файлы. А вот `version needed` и extra расходятся штатно.
//!
//! # Почему crc и размеры хранятся «как записано»
//!
//! Нетронутая запись копируется сырым сжатым срезом и никогда не
//! перепаковывается: наш deflate физически не выдаст те же байты, что Office
//! — у него другой match finder и другие деревья Хаффмана. Значит, crc и
//! размеры обязаны браться из исходных заголовков, а не пересчитываться.
//! У записи с bit 3 в локальном заголовке они нулевые — так и хранятся, а
//! настоящие значения лежат в полях каталога.

use std::borrow::Cow;

use super::consts::{FLAG_DATA_DESCRIPTOR, FLAG_ENCRYPTED, FLAG_STRONG_ENCRYPTION, FLAG_UTF8};
use super::zip64::{Zip64Eocd, Zip64Layout, Zip64Locator};
use crate::bytes::Span;

/// Data descriptor, стоящий за данными записи с флагом bit 3.
///
/// Форма дескриптора не выводится из заголовка: сигнатура необязательна
/// (APPNOTE 4.3.9.3), а ширина полей зависит от того, zip64 запись или нет.
/// Поэтому форма определяется по содержимому и запоминается — переизлучить
/// дескриптор «как принято» нельзя, надо переизлучить его как был.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Descriptor {
    /// Стоит ли перед полями сигнатура `PK\x07\x08`.
    pub has_signature: bool,
    /// 64-битные размеры (zip64) вместо 32-битных.
    pub wide: bool,
    /// Весь дескриптор целиком, включая сигнатуру, если она есть.
    pub span: Span,
}

impl Descriptor {
    /// Длина дескриптора в байтах для заданной формы.
    #[must_use]
    pub const fn len_for(has_signature: bool, wide: bool) -> usize {
        // crc всегда 4 байта; в zip64 расширяются только размеры.
        let fields: usize = if wide { 20 } else { 12 };
        if has_signature {
            fields.saturating_add(4)
        } else {
            fields
        }
    }
}

/// Одна запись архива со всеми полями обоих заголовков.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RawEntry {
    // --- имена ---
    /// Имя, как записано в центральном каталоге.
    pub name_cd: Span,
    /// Имя, как записано в локальном заголовке.
    ///
    /// Байты обязаны совпадать с [`Self::name_cd`] — иначе [`ZipError::NameMismatch`]
    /// ещё на разборе. Спан всё равно хранится отдельно: он нужен, чтобы
    /// вырезать локальный заголовок из исходника без пересчёта офсетов.
    ///
    /// [`ZipError::NameMismatch`]: crate::error::ZipError::NameMismatch
    pub name_local: Span,

    // --- версии ---
    /// `version made by`: старший байт — хост-система, младший — версия ZIP.
    /// В корпусе встречаются 20, 23, 45 и 788 (`0x0314` — ZIP 2.0, Unix-хост).
    pub version_made_by: u16,
    /// `version needed to extract` из центрального каталога.
    pub version_needed_cd: u16,
    /// `version needed to extract` из локального заголовка.
    ///
    /// Отдельное поле, потому что формат не требует совпадения с каталожным,
    /// и writer'ы этим пользуются.
    pub version_needed_local: u16,

    // --- поля центрального каталога: авторитетный источник значений ---
    pub flags: u16,
    pub method: u16,
    /// Время модификации в формате DOS. Берётся из файла и никогда из часов —
    /// иначе round-trip перестал бы быть детерминированным.
    pub dos_time: u16,
    /// Дата модификации в формате DOS.
    pub dos_date: u16,
    pub crc32: u32,
    /// Размер сжатых данных. При zip64 — из extra-поля `0x0001`.
    pub comp_size: u64,
    /// Размер распакованных данных. При zip64 — из extra-поля `0x0001`.
    pub uncomp_size: u64,

    // --- те же поля, как они записаны в ЛОКАЛЬНОМ заголовке ---
    // У записи с bit 3 (data descriptor) здесь нули: writer их ещё не знал.
    // Хранятся не для чтения значений, а чтобы M3 мог сверить, что
    // переизлучённый локальный заголовок совпал с исходным байт в байт.
    pub flags_local: u16,
    pub method_local: u16,
    pub dos_time_local: u16,
    pub dos_date_local: u16,
    pub crc32_local: u32,
    pub comp_size_local: u64,
    pub uncomp_size_local: u64,

    // --- extra и комментарий ---
    /// Область extra ЛОКАЛЬНОГО заголовка. В корпусе — 0, 40, 264 и 520 байт.
    pub local_extra: Span,
    /// Область extra ЦЕНТРАЛЬНОГО каталога. У тех же записей — 0 байт.
    pub cd_extra: Span,
    /// Комментарий к записи (есть только в каталоге).
    pub comment: Span,

    // --- атрибуты ---
    pub internal_attrs: u16,
    pub external_attrs: u32,
    /// Номер диска, с которого начинается запись. При zip64 — из `0x0001`.
    pub disk_start: u32,
    /// Офсет локального заголовка **как записан в каталоге**.
    ///
    /// Именно как записан: у архива с префиксом (самораспаковывающегося) он
    /// может быть сдвинут относительно начала файла. Фактическая позиция —
    /// `local_header.start()`, разница — [`ZipArchive::offset_delta`].
    ///
    /// [`ZipArchive::offset_delta`]: super::ZipArchive::offset_delta
    pub local_header_off: u64,

    // --- срезы исходника ---
    /// Локальный заголовок целиком: 30 фиксированных байт + имя + extra.
    ///
    /// Позволяет вехе M3 скопировать нетронутую запись одним
    /// `extend_from_slice`, вообще не собирая заголовок из полей. Собранный
    /// заголовок — это шанс ошибиться; скопированный — нет.
    pub local_header: Span,
    /// Запись центрального каталога целиком: 46 байт + имя + extra + комментарий.
    pub cd_record: Span,
    /// Сырые, всё ещё сжатые байты данных. Никогда не распаковываются при
    /// копировании записи.
    pub data: Span,
    /// Дескриптор за данными, если запись писалась потоком.
    pub descriptor: Option<Descriptor>,
    /// Раскладка zip64-полей, если они есть.
    pub zip64_layout: Option<Zip64Layout>,
    /// Байты между концом предыдущей физической записи и этим заголовком.
    ///
    /// Обычно пуст. Непустым бывает у архивов, которые выравнивают данные
    /// (например, по 4 КиБ для mmap) или из которых удалили запись, не сжимая
    /// файл. Эти байты не принадлежат ни одной записи, но принадлежат файлу —
    /// значит, должны быть сохранены.
    pub gap_before: Span,
}

impl RawEntry {
    /// Запись писалась потоком: crc и размеры вынесены в дескриптор.
    #[must_use]
    pub const fn has_data_descriptor(&self) -> bool {
        self.flags & FLAG_DATA_DESCRIPTOR != 0
    }

    /// Имя и комментарий записи закодированы в UTF-8 (bit 11), а не в CP437.
    #[must_use]
    pub const fn is_utf8(&self) -> bool {
        self.flags & FLAG_UTF8 != 0
    }

    /// Запись зашифрована (слабо или сильно).
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0
    }

    /// Спан всего, что принадлежит записи в области данных: локальный
    /// заголовок, данные и дескриптор.
    ///
    /// Это и есть единица побайтового копирования вехи M3.
    #[must_use]
    pub fn verbatim(&self) -> Span {
        let end = match self.descriptor {
            Some(d) => d.span.end().max(self.data.end()),
            None => self.data.end(),
        };
        Span::clamped(self.local_header.start(), end)
    }

    /// Запись описывает каталог: имя оканчивается на `/`.
    ///
    /// Проверяется по имени, а не по внешним атрибутам: атрибут `0x10`
    /// (FILE_ATTRIBUTE_DIRECTORY) ставят не все, а завершающий слэш — все.
    #[must_use]
    pub fn is_dir(&self, src: &[u8]) -> bool {
        self.name_cd
            .slice(src)
            .is_some_and(|n| n.last() == Some(&b'/'))
    }
}

/// End Of Central Directory со всеми полями.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Eocd {
    /// Вся запись EOCD: 22 байта плюс комментарий архива.
    pub span: Span,
    /// Номер текущего диска.
    pub disk_number: u16,
    /// Номер диска, на котором начинается центральный каталог.
    pub cd_start_disk: u16,
    /// Число записей каталога на этом диске.
    pub entries_this_disk: u16,
    /// Общее число записей каталога.
    ///
    /// Хранится отдельно от [`Self::entries_this_disk`], потому что в
    /// однотомном архиве они обязаны совпадать, но writer'ы ошибаются, и
    /// «починить» число при перезаписи — значит изменить байты файла.
    pub entries_total: u16,
    pub cd_size: u32,
    pub cd_offset: u32,
    /// Комментарий архива. В корпусе пуст у всех 43 файлов.
    pub comment: Span,
    /// zip64 EOCD record, если архив zip64.
    pub zip64: Option<Zip64Eocd>,
    /// zip64 EOCD locator, если он есть.
    pub locator: Option<Zip64Locator>,

    // --- эффективные значения после разрешения маркеров 0xFFFF/0xFFFFFFFF ---
    /// Действительное число записей каталога.
    pub eff_entries_total: u64,
    /// Действительный размер каталога.
    pub eff_cd_size: u64,
    /// Действительный офсет каталога — **как записан**, без поправки на префикс.
    pub eff_cd_offset: u64,
    /// Фактическая позиция начала каталога в буфере.
    pub cd_start_abs: u64,
}

/// Верхняя половина CP437 — кодировки, в которой имена лежат без bit 11.
///
/// Таблица нужна целиком, а не «латиница плюс мусор»: имена вроде
/// `Файл.xml` из старых архивов иначе превращаются в вопросительные знаки, и
/// поиск части по имени перестаёт работать.
#[rustfmt::skip]
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å',
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ',
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»',
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧',
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀',
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// Декодирует имя записи в текст.
///
/// `utf8` — состояние bit 11 флагов. Без него имя по APPNOTE в CP437; на
/// практике встречаются и файлы с UTF-8 без флага, но угадывать кодировку
/// нельзя: угадав неверно, мы вернули бы имя, по которому часть не находится.
/// Возвращается [`Cow`], потому что подавляющее большинство имён — ASCII, и
/// копировать их незачем.
pub(crate) fn decode_name(raw: &[u8], utf8: bool) -> Cow<'_, str> {
    if utf8 || raw.is_ascii() {
        // Битый UTF-8 при выставленном bit 11 — испорченный файл, но не повод
        // отказать в открытии: имя заменяется на U+FFFD, часть просто не
        // найдётся по имени.
        return String::from_utf8_lossy(raw);
    }
    let mut s = String::with_capacity(raw.len());
    for &b in raw {
        if b < 0x80 {
            s.push(char::from(b));
        } else {
            let idx = usize::from(b.wrapping_sub(0x80));
            s.push(CP437_HIGH.get(idx).copied().unwrap_or('\u{fffd}'));
        }
    }
    Cow::Owned(s)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    #[test]
    fn ascii_name_is_borrowed() {
        assert!(matches!(
            decode_name(b"xl/workbook.xml", false),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn cp437_high_half_decoded() {
        // 0x81 = ü, 0xE1 = ß
        assert_eq!(decode_name(&[0x81, 0xE1], false), "üß");
    }

    #[test]
    fn utf8_flag_disables_cp437() {
        let raw = "щ.xml".as_bytes();
        assert_eq!(decode_name(raw, true), "щ.xml");
        // Те же байты без флага читаются как CP437 — и это не «баг», а формат.
        assert_ne!(decode_name(raw, false), "щ.xml");
    }

    #[test]
    fn broken_utf8_does_not_fail_open() {
        assert!(decode_name(&[0xF0, 0x9F], true).contains('\u{fffd}'));
    }

    #[test]
    fn descriptor_lengths_match_appnote() {
        assert_eq!(Descriptor::len_for(true, false), 16);
        assert_eq!(Descriptor::len_for(false, false), 12);
        assert_eq!(Descriptor::len_for(true, true), 24);
        assert_eq!(Descriptor::len_for(false, true), 20);
    }
}
