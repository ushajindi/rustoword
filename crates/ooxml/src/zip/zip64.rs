//! Структуры zip64 (APPNOTE 4.5.3, 4.3.14, 4.3.15).
//!
//! В тестовом корпусе zip64 не встречается ни разу, но вход недоверенный, и
//! zip64-архив надо как минимум корректно разобрать или корректно отвергнуть,
//! а не выйти за границу буфера.
//!
//! # Почему раскладку extra-поля надо запоминать
//!
//! Запись `0x0001` **позиционная и переменной длины**: в ней лежат только те
//! значения, чьи 32-битные поля в основном заголовке равны `0xFFFFFFFF` (для
//! номера диска — `0xFFFF`). Порядок фиксирован: uncompressed, compressed,
//! local header offset, disk number. То есть по одним лишь значениям
//! восстановить байты записи нельзя — нужно знать, какие элементы в ней были.
//! [`Zip64Layout`] хранит именно это: без него веха M3 переизлучила бы поле в
//! другой раскладке и сломала байт-идентичность.
//!
//! # Асимметрия локального заголовка и каталога
//!
//! APPNOTE 4.5.3 требует, чтобы в **локальном** заголовке запись `0x0001`
//! содержала оба размера всегда — даже если маркер стоит только у одного из
//! них. Это сделано ради потоковой записи: writer резервирует место заранее,
//! когда размеры ещё неизвестны. В каталоге такого требования нет. Поэтому
//! локальная и каталожная записи разбираются разными правилами.

use super::consts::{SIG_ZIP64_EOCD, SIG_ZIP64_LOCATOR, U16_MARKER, U32_MARKER, ZIP64_EOCD_FIXED};
use crate::bytes::{Reader, Span};

/// Какие элементы присутствовали в записи `0x0001`.
///
/// `has_*` описывают запись **центрального каталога**, если она есть
/// (`in_cd`), иначе — локальную. Каталожная раскладка важнее: локальная по
/// APPNOTE всегда «оба размера», её можно восстановить без подсказки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Zip64Layout {
    /// Запись `0x0001` есть в extra локального заголовка.
    pub in_local: bool,
    /// Запись `0x0001` есть в extra центрального каталога.
    pub in_cd: bool,
    /// Присутствует поле `original size` (8 байт).
    pub has_usize: bool,
    /// Присутствует поле `compressed size` (8 байт).
    pub has_csize: bool,
    /// Присутствует поле `relative header offset` (8 байт).
    pub has_offset: bool,
    /// Присутствует поле `disk start number` (4 байта).
    pub has_disk: bool,
}

impl Zip64Layout {
    /// Длина, которую занимает такая запись `0x0001` без 4-байтового заголовка.
    ///
    /// Нужна вехе M3 для проверки, что переизлучённое поле совпало по длине с
    /// исходным.
    #[must_use]
    pub const fn payload_len(self) -> usize {
        let mut n = 0usize;
        if self.has_usize {
            n = n.saturating_add(8);
        }
        if self.has_csize {
            n = n.saturating_add(8);
        }
        if self.has_offset {
            n = n.saturating_add(8);
        }
        if self.has_disk {
            n = n.saturating_add(4);
        }
        n
    }
}

/// Значения, извлечённые из записи `0x0001`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Zip64Values {
    pub uncomp: Option<u64>,
    pub comp: Option<u64>,
    pub offset: Option<u64>,
    pub disk: Option<u32>,
}

/// Какие элементы ожидаются в записи — определяется маркерами в заголовке.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Zip64Want {
    pub uncomp: bool,
    pub comp: bool,
    pub offset: bool,
    pub disk: bool,
}

impl Zip64Want {
    /// Что должно лежать в записи `0x0001` центрального каталога.
    pub(crate) const fn from_cd(uncomp: u32, comp: u32, offset: u32, disk: u16) -> Self {
        Self {
            uncomp: uncomp == U32_MARKER,
            comp: comp == U32_MARKER,
            offset: offset == U32_MARKER,
            disk: disk == U16_MARKER,
        }
    }

    /// Что должно лежать в записи `0x0001` локального заголовка.
    ///
    /// Всегда оба размера и никогда офсет/диск — так требует APPNOTE 4.5.3
    /// независимо от того, какие поля помечены маркером.
    pub(crate) const fn local() -> Self {
        Self {
            uncomp: true,
            comp: true,
            offset: false,
            disk: false,
        }
    }
}

/// Разбирает полезную нагрузку записи `0x0001`.
///
/// Читает ровно те элементы, которые запрошены, и ровно в порядке APPNOTE.
/// Если данных не хватило — соответствующий `has_*` останется `false`, и
/// решать, ошибка это или нет, будет вызывающий: для каталога отсутствие
/// запрошенного значения фатально (настоящий размер неизвестен), для
/// локального заголовка — нет (там значения дублирующие).
pub(crate) fn read_zip64_extra(data: &[u8], want: Zip64Want) -> (Zip64Values, Zip64Layout) {
    let mut r = Reader::new(data);
    let mut v = Zip64Values::default();
    let mut l = Zip64Layout::default();
    if want.uncomp
        && let Some(x) = r.le_u64()
    {
        v.uncomp = Some(x);
        l.has_usize = true;
    }
    if want.comp
        && let Some(x) = r.le_u64()
    {
        v.comp = Some(x);
        l.has_csize = true;
    }
    if want.offset
        && let Some(x) = r.le_u64()
    {
        v.offset = Some(x);
        l.has_offset = true;
    }
    if want.disk
        && let Some(x) = r.le_u32()
    {
        v.disk = Some(x);
        l.has_disk = true;
    }
    (v, l)
}

/// zip64 End Of Central Directory record (APPNOTE 4.3.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Zip64Eocd {
    /// Спан всей записи, включая сигнатуру и поле размера.
    pub span: Span,
    /// Поле `size of zip64 end of central directory record`.
    ///
    /// Хранится как записано, а не как вычисленное `44 + extensible.len()`:
    /// writer вправе объявить запись длиннее, и переизлучать надо его число.
    pub record_size: u64,
    pub version_made_by: u16,
    pub version_needed: u16,
    pub disk_number: u32,
    pub cd_start_disk: u32,
    pub entries_this_disk: u64,
    pub entries_total: u64,
    pub cd_size: u64,
    pub cd_offset: u64,
    /// «zip64 extensible data sector» — хвост записи сверх 44 фиксированных
    /// байт. Содержимое непрозрачно и копируется как есть.
    pub extensible_data: Span,
}

/// zip64 End Of Central Directory locator (APPNOTE 4.3.15). Всегда 20 байт.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Zip64Locator {
    pub span: Span,
    /// Номер диска, на котором лежит zip64 EOCD record.
    pub eocd_disk: u32,
    /// Офсет zip64 EOCD record относительно начала его диска.
    pub eocd_offset: u64,
    /// Общее число дисков тома.
    pub total_disks: u32,
}

/// Читает zip64 EOCD record. `None`, если на этой позиции его нет.
pub(crate) fn read_eocd(src: &[u8], pos: usize) -> Option<Zip64Eocd> {
    let mut r = Reader::at(src, pos)?;
    r.expect_sig(SIG_ZIP64_EOCD)?;
    let record_size = r.le_u64()?;
    let content_len = usize::try_from(record_size).ok()?;
    // Меньше 44 байт — запись не может содержать даже обязательных полей.
    let extensible_len = content_len.checked_sub(ZIP64_EOCD_FIXED)?;
    let version_made_by = r.le_u16()?;
    let version_needed = r.le_u16()?;
    let disk_number = r.le_u32()?;
    let cd_start_disk = r.le_u32()?;
    let entries_this_disk = r.le_u64()?;
    let entries_total = r.le_u64()?;
    let cd_size = r.le_u64()?;
    let cd_offset = r.le_u64()?;
    let extensible_data = r.take_span(extensible_len)?;
    let span = Span::new(u32::try_from(pos).ok()?, u32::try_from(r.pos()).ok()?)?;
    Some(Zip64Eocd {
        span,
        record_size,
        version_made_by,
        version_needed,
        disk_number,
        cd_start_disk,
        entries_this_disk,
        entries_total,
        cd_size,
        cd_offset,
        extensible_data,
    })
}

/// Читает zip64 EOCD locator. `None`, если на этой позиции его нет.
pub(crate) fn read_locator(src: &[u8], pos: usize) -> Option<Zip64Locator> {
    let mut r = Reader::at(src, pos)?;
    r.expect_sig(SIG_ZIP64_LOCATOR)?;
    let eocd_disk = r.le_u32()?;
    let eocd_offset = r.le_u64()?;
    let total_disks = r.le_u32()?;
    let span = Span::new(u32::try_from(pos).ok()?, u32::try_from(r.pos()).ok()?)?;
    Some(Zip64Locator {
        span,
        eocd_disk,
        eocd_offset,
        total_disks,
    })
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
    fn cd_layout_follows_markers_not_data_length() {
        // Маркер только у офсета: первые 8 байт записи — это офсет, а не размер.
        let want = Zip64Want::from_cd(10, 20, U32_MARKER, 0);
        let data = 0x1122_3344_5566_7788u64.to_le_bytes();
        let (v, l) = read_zip64_extra(&data, want);
        assert_eq!(v.offset, Some(0x1122_3344_5566_7788));
        assert_eq!(v.uncomp, None);
        assert!(l.has_offset && !l.has_usize && !l.has_csize && !l.has_disk);
        assert_eq!(l.payload_len(), 8);
    }

    #[test]
    fn all_four_elements_are_positional() {
        let want = Zip64Want::from_cd(U32_MARKER, U32_MARKER, U32_MARKER, U16_MARKER);
        let mut data = Vec::new();
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&2u64.to_le_bytes());
        data.extend_from_slice(&3u64.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        let (v, l) = read_zip64_extra(&data, want);
        assert_eq!(
            (v.uncomp, v.comp, v.offset, v.disk),
            (Some(1), Some(2), Some(3), Some(4))
        );
        assert_eq!(l.payload_len(), 28);
    }

    #[test]
    fn missing_bytes_leave_flag_unset_instead_of_panicking() {
        let want = Zip64Want::from_cd(U32_MARKER, U32_MARKER, 0, 0);
        let (v, l) = read_zip64_extra(&[0u8; 8], want);
        assert!(l.has_usize);
        assert!(!l.has_csize);
        assert_eq!(v.comp, None);
    }

    #[test]
    fn local_layout_always_wants_both_sizes() {
        let w = Zip64Want::local();
        assert!(w.uncomp && w.comp && !w.offset && !w.disk);
    }

    #[test]
    fn eocd_shorter_than_fixed_part_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SIG_ZIP64_EOCD.to_le_bytes());
        buf.extend_from_slice(&10u64.to_le_bytes()); // < 44
        buf.extend_from_slice(&[0u8; 64]);
        assert!(read_eocd(&buf, 0).is_none());
    }
}
