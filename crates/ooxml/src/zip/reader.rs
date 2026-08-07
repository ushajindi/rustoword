//! Разбор ZIP-контейнера из среза байт.
//!
//! # Что здесь происходит и в каком порядке
//!
//! 1. С конца файла ищется EOCD. Сигнатура `PK\x05\x06` может встретиться в
//!    сжатых данных и в комментарии архива, поэтому кандидатов проверяют на
//!    согласованность, а не берут первый попавшийся.
//! 2. Если EOCD помечен маркерами `0xFFFF`/`0xFFFFFFFF` — рядом обязан лежать
//!    zip64 locator, а по нему zip64 EOCD record.
//! 3. Разбирается центральный каталог: он единственный авторитетный источник
//!    значений, потому что пишется после данных, когда всё уже известно.
//! 4. Для каждой записи каталога читается её локальный заголовок и данные.
//! 5. Записи упорядочиваются по физическому офсету, проверяются на
//!    непересечение, и между ними вычисляются «дыры».
//!
//! # Почему разбор ленив относительно данных
//!
//! Данные записи не распаковываются никогда — ни при разборе, ни при
//! копировании. [`ZipArchive::raw_data`] отдаёт сырой сжатый срез, и веха M3
//! пишет его в выход как есть. Наш deflate не может выдать те же байты, что
//! Office, поэтому единственный способ не потерять ничего — не трогать.
//!
//! # Модель угроз
//!
//! Вход враждебный. Отдельно проверяются: пересечение диапазонов записей
//! (позволяет показать разным читателям разное содержимое), расхождение имён
//! между локальным заголовком и каталогом (то же самое), офсеты за границей
//! буфера, враньё в счётчике записей, шифрование, многотомность. Ни одна
//! ветка не имеет права паниковать: арифметика идёт через `checked_*`,
//! срезы — только через [`Reader`] и [`Span`].

use std::borrow::Cow;

use super::consts::{
    CENTRAL_HEADER_LEN, EOCD_LEN, EOCD_SIG_BYTES, EXTRA_ZIP64, FLAG_DATA_DESCRIPTOR,
    FLAG_ENCRYPTED, FLAG_STRONG_ENCRYPTION, FLAG_UTF8, LOCAL_HEADER_LEN, MAX_COMMENT,
    METHOD_DEFLATE, METHOD_STORE, SIG_CENTRAL, SIG_DATA_DESCRIPTOR, SIG_EOCD, SIG_LOCAL,
    U16_MARKER, U32_MARKER, ZIP64_EOCD_LEN, ZIP64_LOCATOR_LEN,
};
use super::entry::{Descriptor, Eocd, RawEntry, decode_name};
use super::extra::ExtraFields;
use super::zip64::{self, Zip64Layout, Zip64Want};
use crate::bytes::{Reader, Span};
use crate::error::{Error, LimitError, Result, ZipError};
use crate::hash::crc32;
use crate::limits::Limits;

/// Разобранный ZIP-контейнер.
///
/// Держит ссылку на исходный буфер и ни одного скопированного байта данных:
/// вся модель — это офсеты и спаны в чужой памяти.
#[derive(Debug, Clone)]
pub struct ZipArchive<'a> {
    src: &'a [u8],
    prefix: Span,
    entries: Vec<RawEntry>,
    order_by_offset: Vec<u32>,
    eocd: Eocd,
    offset_delta: u64,
    gap_before_cd: Span,
    gap_after_cd: Span,
    trailing: Span,
}

const fn zerr(kind: ZipError, offset: usize) -> Error {
    Error::Zip {
        kind,
        offset: offset as u64,
    }
}

/// Сигнатура на позиции `pos`, если она там читается.
fn sig_at(src: &[u8], pos: usize) -> Option<u32> {
    Reader::at(src, pos)?.le_u32()
}

/// EOCD, прочитанный как есть, без разрешения zip64-маркеров.
#[derive(Debug, Clone, Copy)]
struct RawEocd {
    disk_number: u16,
    cd_start_disk: u16,
    entries_this_disk: u16,
    entries_total: u16,
    cd_size: u32,
    cd_offset: u32,
    comment: Span,
    /// Позиция за последним байтом комментария.
    end: usize,
}

fn read_eocd(src: &[u8], pos: usize) -> Option<RawEocd> {
    let mut r = Reader::at(src, pos)?;
    r.expect_sig(SIG_EOCD)?;
    let disk_number = r.le_u16()?;
    let cd_start_disk = r.le_u16()?;
    let entries_this_disk = r.le_u16()?;
    let entries_total = r.le_u16()?;
    let cd_size = r.le_u32()?;
    let cd_offset = r.le_u32()?;
    let comment_len = usize::from(r.le_u16()?);
    let comment = r.take_span(comment_len)?;
    Some(RawEocd {
        disk_number,
        cd_start_disk,
        entries_this_disk,
        entries_total,
        cd_size,
        cd_offset,
        comment,
        end: r.pos(),
    })
}

/// Похож ли кандидат на настоящий EOCD.
///
/// Проверка нужна, потому что `PK\x05\x06` — четыре байта, которые с
/// вероятностью 2^-32 встречаются в любых данных, а в комментарии архива их
/// можно поставить намеренно. Критерий: каталог, на который ссылается
/// кандидат, действительно начинается записью каталога.
fn eocd_plausible(src: &[u8], pos: usize, e: &RawEocd) -> bool {
    // zip64: 32-битные поля здесь заведомо не значат ничего, проверять их
    // бессмысленно — признаком служит локатор вплотную перед EOCD.
    if e.entries_total == U16_MARKER || e.cd_size == U32_MARKER || e.cd_offset == U32_MARKER {
        return pos
            .checked_sub(ZIP64_LOCATOR_LEN)
            .and_then(|p| sig_at(src, p))
            .is_some_and(|s| s == super::consts::SIG_ZIP64_LOCATOR);
    }
    let size = e.cd_size as usize;
    let off = e.cd_offset as usize;
    if e.entries_total == 0 {
        // Пустой архив: каталога нет, но офсет обязан указывать внутрь файла.
        return size == 0 && off <= pos;
    }
    // Обычный случай: офсет каталога абсолютен.
    if sig_at(src, off) == Some(SIG_CENTRAL) && off.checked_add(size).is_some_and(|end| end <= pos)
    {
        return true;
    }
    // Архив с префиксом, у которого офсеты не поправлены: каталог примыкает
    // к EOCD с конца.
    pos.checked_sub(size).and_then(|p| sig_at(src, p)) == Some(SIG_CENTRAL)
}

/// Ищет EOCD, просматривая кандидатов от конца файла к началу.
fn find_eocd(src: &[u8]) -> Result<(usize, RawEocd)> {
    let window = MAX_COMMENT.saturating_add(EOCD_LEN);
    let from = src.len().saturating_sub(window);
    let mut pos = src.len().saturating_sub(EOCD_LEN);
    // Кандидат, у которого комментарий не дотягивает до конца файла, годится
    // только как запасной: за EOCD не должно оставаться неучтённых байт.
    let mut fallback: Option<(usize, RawEocd)> = None;
    loop {
        if pos < from {
            break;
        }
        if src.get(pos..pos.saturating_add(4)) == Some(&EOCD_SIG_BYTES[..])
            && let Some(e) = read_eocd(src, pos)
            && eocd_plausible(src, pos, &e)
        {
            if e.end == src.len() {
                return Ok((pos, e));
            }
            if fallback.is_none() {
                fallback = Some((pos, e));
            }
        }
        match pos.checked_sub(1) {
            Some(p) => pos = p,
            None => break,
        }
    }
    fallback.ok_or_else(|| zerr(ZipError::NoEndOfCentralDirectory, src.len()))
}

/// Запись центрального каталога, прочитанная как есть.
#[derive(Debug, Clone, Copy)]
struct CdRaw {
    version_made_by: u16,
    version_needed: u16,
    flags: u16,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc: u32,
    comp32: u32,
    uncomp32: u32,
    disk16: u16,
    internal_attrs: u16,
    external_attrs: u32,
    local_off32: u32,
    name: Span,
    extra: Span,
    comment: Span,
    record: Span,
    end: usize,
}

fn read_cd_record(src: &[u8], pos: usize) -> Result<CdRaw> {
    // Ранняя проверка фиксированной части: без неё обрыв ровно посреди
    // заголовка давал бы `Truncated` только на пятом-шестом поле, и офсет в
    // ошибке указывал бы не туда, где на самом деле кончились байты.
    if pos
        .checked_add(CENTRAL_HEADER_LEN)
        .is_none_or(|e| e > src.len())
    {
        return Err(zerr(ZipError::Truncated, pos));
    }
    let mut r = Reader::at(src, pos).ok_or_else(|| zerr(ZipError::Truncated, pos))?;
    let trunc = || zerr(ZipError::Truncated, pos);
    if r.expect_sig(SIG_CENTRAL).is_none() {
        return Err(zerr(
            ZipError::BadSignature {
                expected: SIG_CENTRAL,
                found: sig_at(src, pos).unwrap_or(0),
            },
            pos,
        ));
    }
    let version_made_by = r.le_u16().ok_or_else(trunc)?;
    let version_needed = r.le_u16().ok_or_else(trunc)?;
    let flags = r.le_u16().ok_or_else(trunc)?;
    let method = r.le_u16().ok_or_else(trunc)?;
    let dos_time = r.le_u16().ok_or_else(trunc)?;
    let dos_date = r.le_u16().ok_or_else(trunc)?;
    let crc = r.le_u32().ok_or_else(trunc)?;
    let comp32 = r.le_u32().ok_or_else(trunc)?;
    let uncomp32 = r.le_u32().ok_or_else(trunc)?;
    let name_len = usize::from(r.le_u16().ok_or_else(trunc)?);
    let extra_len = usize::from(r.le_u16().ok_or_else(trunc)?);
    let comment_len = usize::from(r.le_u16().ok_or_else(trunc)?);
    let disk16 = r.le_u16().ok_or_else(trunc)?;
    let internal_attrs = r.le_u16().ok_or_else(trunc)?;
    let external_attrs = r.le_u32().ok_or_else(trunc)?;
    let local_off32 = r.le_u32().ok_or_else(trunc)?;
    let name = r.take_span(name_len).ok_or_else(trunc)?;
    let extra = r.take_span(extra_len).ok_or_else(trunc)?;
    let comment = r.take_span(comment_len).ok_or_else(trunc)?;
    let end = r.pos();
    let record = Span::new(
        u32::try_from(pos).map_err(|_| zerr(ZipError::OffsetOverflow, pos))?,
        u32::try_from(end).map_err(|_| zerr(ZipError::OffsetOverflow, end))?,
    )
    .ok_or_else(trunc)?;
    Ok(CdRaw {
        version_made_by,
        version_needed,
        flags,
        method,
        dos_time,
        dos_date,
        crc,
        comp32,
        uncomp32,
        disk16,
        internal_attrs,
        external_attrs,
        local_off32,
        name,
        extra,
        comment,
        record,
        end,
    })
}

/// Локальный заголовок, прочитанный как есть.
#[derive(Debug, Clone, Copy)]
struct LocalRaw {
    version_needed: u16,
    flags: u16,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc: u32,
    comp32: u32,
    uncomp32: u32,
    name: Span,
    extra: Span,
    header: Span,
    data_start: usize,
}

fn read_local_header(src: &[u8], pos: usize) -> Result<LocalRaw> {
    if pos
        .checked_add(LOCAL_HEADER_LEN)
        .is_none_or(|e| e > src.len())
    {
        return Err(zerr(ZipError::Truncated, pos));
    }
    let mut r = Reader::at(src, pos).ok_or_else(|| zerr(ZipError::Truncated, pos))?;
    let trunc = || zerr(ZipError::Truncated, pos);
    if r.expect_sig(SIG_LOCAL).is_none() {
        return Err(zerr(
            ZipError::BadSignature {
                expected: SIG_LOCAL,
                found: sig_at(src, pos).unwrap_or(0),
            },
            pos,
        ));
    }
    let version_needed = r.le_u16().ok_or_else(trunc)?;
    let flags = r.le_u16().ok_or_else(trunc)?;
    let method = r.le_u16().ok_or_else(trunc)?;
    let dos_time = r.le_u16().ok_or_else(trunc)?;
    let dos_date = r.le_u16().ok_or_else(trunc)?;
    let crc = r.le_u32().ok_or_else(trunc)?;
    let comp32 = r.le_u32().ok_or_else(trunc)?;
    let uncomp32 = r.le_u32().ok_or_else(trunc)?;
    let name_len = usize::from(r.le_u16().ok_or_else(trunc)?);
    let extra_len = usize::from(r.le_u16().ok_or_else(trunc)?);
    let name = r.take_span(name_len).ok_or_else(trunc)?;
    let extra = r.take_span(extra_len).ok_or_else(trunc)?;
    let data_start = r.pos();
    let header = Span::new(
        u32::try_from(pos).map_err(|_| zerr(ZipError::OffsetOverflow, pos))?,
        u32::try_from(data_start).map_err(|_| zerr(ZipError::OffsetOverflow, data_start))?,
    )
    .ok_or_else(trunc)?;
    Ok(LocalRaw {
        version_needed,
        flags,
        method,
        dos_time,
        dos_date,
        crc,
        comp32,
        uncomp32,
        name,
        extra,
        header,
        data_start,
    })
}

/// Проверяет одну гипотезу о форме дескриптора.
///
/// Форма считается верной, только если все три значения совпали с
/// каталожными. Это надёжнее любой эвристики по сигнатуре: сигнатура
/// необязательна, а её байты могут случайно оказаться значением crc.
fn descriptor_fits(
    src: &[u8],
    at: usize,
    form: (bool, bool),
    want: (u32, u64, u64),
) -> Option<Span> {
    let (sig, wide) = form;
    let mut r = Reader::at(src, at)?;
    if sig {
        r.expect_sig(SIG_DATA_DESCRIPTOR)?;
    }
    let crc = r.le_u32()?;
    let (comp, uncomp) = if wide {
        (r.le_u64()?, r.le_u64()?)
    } else {
        (u64::from(r.le_u32()?), u64::from(r.le_u32()?))
    };
    if crc != want.0 || comp != want.1 || uncomp != want.2 {
        return None;
    }
    Span::new(u32::try_from(at).ok()?, u32::try_from(r.pos()).ok()?)
}

/// Определяет форму дескриптора за данными записи.
///
/// Перебираются четыре допустимых формы, начиная с наиболее вероятной. Если
/// ни одна не сошлась по значениям (файл испорчен или размеры в каталоге
/// врут), берётся форма по внешнему признаку — так дескриптор всё равно
/// попадёт в спан записи, а не окажется «дырой» перед следующим заголовком.
fn read_descriptor(
    src: &[u8],
    at: usize,
    hint_wide: bool,
    want: (u32, u64, u64),
) -> Option<Descriptor> {
    for &sig in &[true, false] {
        for &wide in &[hint_wide, !hint_wide] {
            if let Some(span) = descriptor_fits(src, at, (sig, wide), want) {
                return Some(Descriptor {
                    has_signature: sig,
                    wide,
                    span,
                });
            }
        }
    }
    let has_signature = sig_at(src, at) == Some(SIG_DATA_DESCRIPTOR);
    let len = Descriptor::len_for(has_signature, hint_wide);
    let end = at.checked_add(len)?;
    if end > src.len() {
        return None;
    }
    Some(Descriptor {
        has_signature,
        wide: hint_wide,
        span: Span::new(u32::try_from(at).ok()?, u32::try_from(end).ok()?)?,
    })
}

impl<'a> ZipArchive<'a> {
    /// Разбирает контейнер.
    ///
    /// # Errors
    /// [`Error::Zip`] на нарушении структуры, [`Error::Limit`] при превышении квоты.
    #[allow(clippy::too_many_lines)]
    pub fn parse(src: &'a [u8], limits: &Limits) -> Result<Self> {
        let total_len = src.len() as u64;
        limits.check_input(total_len)?;
        // Спаны 32-битные; буфер, не помещающийся в u32, пришлось бы резать
        // молча — вместо этого отказ.
        if src.len() > u32::MAX as usize {
            return Err(LimitError::InputTooLarge {
                got: total_len,
                max: u64::from(u32::MAX),
            }
            .into());
        }

        let (eocd_pos, raw) = find_eocd(src)?;
        let needs_zip64 = raw.entries_total == U16_MARKER
            || raw.cd_size == U32_MARKER
            || raw.cd_offset == U32_MARKER
            || raw.disk_number == U16_MARKER
            || raw.cd_start_disk == U16_MARKER
            || raw.entries_this_disk == U16_MARKER;

        // --- zip64 хвост ---
        let locator = eocd_pos
            .checked_sub(ZIP64_LOCATOR_LEN)
            .and_then(|p| zip64::read_locator(src, p));
        let z64 = locator.and_then(|l| {
            // Сначала абсолютный офсет из локатора. Если он не сходится —
            // архив с префиксом: запись лежит вплотную перед локатором.
            usize::try_from(l.eocd_offset)
                .ok()
                .and_then(|p| zip64::read_eocd(src, p))
                .or_else(|| {
                    l.span
                        .start()
                        .checked_sub(ZIP64_EOCD_LEN as u32)
                        .and_then(|p| zip64::read_eocd(src, p as usize))
                })
        });
        if needs_zip64 && z64.is_none() {
            return Err(zerr(ZipError::NoZip64Locator, eocd_pos));
        }

        // --- эффективные значения хвоста ---
        let (entries_total, cd_size, cd_offset, disk_number, cd_start_disk) = match &z64 {
            Some(z) => (
                z.entries_total,
                z.cd_size,
                z.cd_offset,
                z.disk_number,
                z.cd_start_disk,
            ),
            None => (
                u64::from(raw.entries_total),
                u64::from(raw.cd_size),
                u64::from(raw.cd_offset),
                u32::from(raw.disk_number),
                u32::from(raw.cd_start_disk),
            ),
        };
        if disk_number != 0 || cd_start_disk != 0 || locator.is_some_and(|l| l.total_disks > 1) {
            return Err(zerr(ZipError::MultiDisk, eocd_pos));
        }
        limits.check_entries(entries_total)?;

        // Первая структура хвоста — граница, до которой тянется всё остальное.
        let trailer_start = z64.map_or(eocd_pos, |z| z.span.start() as usize);

        let cd_size_us =
            usize::try_from(cd_size).map_err(|_| zerr(ZipError::DirectoryOutOfBounds, eocd_pos))?;
        let cd_off_us = usize::try_from(cd_offset)
            .map_err(|_| zerr(ZipError::DirectoryOutOfBounds, eocd_pos))?;

        // Офсет каталога может быть записан относительно начала zip-части, а
        // не файла: у самораспаковывающихся архивов впереди исполняемый код.
        // Разница запоминается и прибавляется ко всем офсетам заголовков.
        let direct_ok = (entries_total == 0 || sig_at(src, cd_off_us) == Some(SIG_CENTRAL))
            && cd_off_us
                .checked_add(cd_size_us)
                .is_some_and(|end| end <= trailer_start);
        let cd_start_abs = if direct_ok {
            cd_off_us
        } else {
            trailer_start
                .checked_sub(cd_size_us)
                .ok_or_else(|| zerr(ZipError::DirectoryOutOfBounds, eocd_pos))?
        };
        let cd_end = cd_start_abs
            .checked_add(cd_size_us)
            .filter(|&e| e <= trailer_start)
            .ok_or_else(|| zerr(ZipError::DirectoryOutOfBounds, cd_start_abs))?;
        let offset_delta = (cd_start_abs as u64)
            .checked_sub(cd_offset)
            .ok_or_else(|| zerr(ZipError::DirectoryOutOfBounds, cd_start_abs))?;

        // --- центральный каталог ---
        let cap = usize::try_from(entries_total).unwrap_or(0).min(1024);
        let mut entries: Vec<RawEntry> = Vec::with_capacity(cap);
        let mut pos = cd_start_abs;
        let mut total_uncompressed: u64 = 0;
        let mut n = 0u64;
        while n < entries_total {
            if pos >= cd_end {
                return Err(zerr(ZipError::EntryCountMismatch, pos));
            }
            let cd = read_cd_record(src, pos)?;
            if cd.end > cd_end {
                return Err(zerr(ZipError::DirectoryOutOfBounds, pos));
            }
            let entry = Self::build_entry(src, &cd, offset_delta)?;
            limits.check_part_size(entry.uncomp_size)?;
            total_uncompressed = total_uncompressed.saturating_add(entry.uncomp_size);
            limits.check_total_uncompressed(total_uncompressed)?;
            entries.push(entry);
            pos = cd.end;
            n = n.saturating_add(1);
        }
        // Хвост каталога, не покрытый объявленным числом записей, означает,
        // что счётчик врёт — а значит, часть записей была бы невидима.
        if pos != cd_end {
            return Err(zerr(ZipError::EntryCountMismatch, pos));
        }

        // --- физический порядок, дыры и пересечения ---
        let mut by_offset: Vec<(u32, u32)> = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            let idx = u32::try_from(i).map_err(|_| zerr(ZipError::OffsetOverflow, i))?;
            by_offset.push((e.local_header.start(), idx));
        }
        by_offset.sort_unstable();

        // Граница области данных: до неё тянутся записи, за ней начинается
        // каталог. Если пересечь её, читатель каталога прочитал бы данные.
        let cd_boundary = u32::try_from(cd_start_abs).unwrap_or(u32::MAX);
        let mut prev_end: Option<u32> = None;
        for &(start, idx) in &by_offset {
            let gap_from = prev_end.unwrap_or(start);
            if start < gap_from {
                return Err(zerr(ZipError::OverlappingEntries, start as usize));
            }
            let e = entries
                .get_mut(idx as usize)
                .ok_or_else(|| zerr(ZipError::DirectoryOutOfBounds, idx as usize))?;
            e.gap_before = Span::clamped(gap_from, start);
            prev_end = Some(e.verbatim().end());
        }
        let entries_end = prev_end.unwrap_or(cd_boundary);
        if entries_end > cd_boundary {
            return Err(zerr(ZipError::OverlappingEntries, entries_end as usize));
        }
        // Префикс — всё до первого локального заголовка. У архива без записей
        // его границей служит начало каталога.
        let first_start = by_offset.first().map_or(cd_boundary, |&(s, _)| s);
        let prefix = Span::clamped(0, first_start);
        let gap_before_cd = Span::clamped(entries_end, cd_boundary);
        let gap_after_cd = Span::clamped(
            u32::try_from(cd_end).unwrap_or(u32::MAX),
            u32::try_from(trailer_start).unwrap_or(u32::MAX),
        );
        let trailing = Span::clamped(
            u32::try_from(raw.end).unwrap_or(u32::MAX),
            u32::try_from(src.len()).unwrap_or(u32::MAX),
        );

        let order_by_offset = by_offset.iter().map(|&(_, i)| i).collect();
        let eocd_span = Span::new(
            u32::try_from(eocd_pos).unwrap_or(u32::MAX),
            u32::try_from(raw.end).unwrap_or(u32::MAX),
        )
        .ok_or_else(|| zerr(ZipError::Truncated, eocd_pos))?;

        Ok(Self {
            src,
            prefix,
            entries,
            order_by_offset,
            eocd: Eocd {
                span: eocd_span,
                disk_number: raw.disk_number,
                cd_start_disk: raw.cd_start_disk,
                entries_this_disk: raw.entries_this_disk,
                entries_total: raw.entries_total,
                cd_size: raw.cd_size,
                cd_offset: raw.cd_offset,
                comment: raw.comment,
                zip64: z64,
                locator,
                eff_entries_total: entries_total,
                eff_cd_size: cd_size,
                eff_cd_offset: cd_offset,
                cd_start_abs: cd_start_abs as u64,
            },
            offset_delta,
            gap_before_cd,
            gap_after_cd,
            trailing,
        })
    }

    /// Собирает запись из каталожной записи и её локального заголовка.
    fn build_entry(src: &[u8], cd: &CdRaw, offset_delta: u64) -> Result<RawEntry> {
        let at = cd.record.start() as usize;
        if cd.flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0 {
            return Err(zerr(ZipError::Encrypted, at));
        }
        if cd.method != METHOD_STORE && cd.method != METHOD_DEFLATE {
            return Err(zerr(ZipError::UnsupportedMethod(cd.method), at));
        }

        // --- zip64 из extra каталога ---
        let want_cd = Zip64Want::from_cd(cd.uncomp32, cd.comp32, cd.local_off32, cd.disk16);
        let needs_cd_zip64 = want_cd.uncomp || want_cd.comp || want_cd.offset || want_cd.disk;
        let cd_z = ExtraFields::find(src, cd.extra, EXTRA_ZIP64);
        let mut layout: Option<Zip64Layout> = None;
        let (mut uncomp_size, mut comp_size, mut local_off, mut disk_start) = (
            u64::from(cd.uncomp32),
            u64::from(cd.comp32),
            u64::from(cd.local_off32),
            u32::from(cd.disk16),
        );
        if let Some(f) = cd_z {
            let data = f
                .data
                .slice(src)
                .ok_or_else(|| zerr(ZipError::BadExtraField, at))?;
            let (v, mut l) = zip64::read_zip64_extra(data, want_cd);
            l.in_cd = true;
            if want_cd.uncomp {
                uncomp_size = v.uncomp.ok_or_else(|| zerr(ZipError::BadExtraField, at))?;
            }
            if want_cd.comp {
                comp_size = v.comp.ok_or_else(|| zerr(ZipError::BadExtraField, at))?;
            }
            if want_cd.offset {
                local_off = v.offset.ok_or_else(|| zerr(ZipError::BadExtraField, at))?;
            }
            if want_cd.disk {
                disk_start = v.disk.ok_or_else(|| zerr(ZipError::BadExtraField, at))?;
            }
            layout = Some(l);
        } else if needs_cd_zip64 {
            // Маркер стоит, а записи 0x0001 нет — настоящий размер узнать неоткуда.
            return Err(zerr(ZipError::BadExtraField, at));
        }
        if disk_start != 0 {
            return Err(zerr(ZipError::MultiDisk, at));
        }

        // --- локальный заголовок ---
        let local_pos = local_off
            .checked_add(offset_delta)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| zerr(ZipError::DataOutOfBounds, at))?;
        let local = read_local_header(src, local_pos)?;

        // Имя обязано совпадать: расхождение — это способ показать разным
        // читателям (тем, кто идёт по каталогу, и тем, кто идёт по локальным
        // заголовкам) разные файлы.
        let name_cd_bytes = cd
            .name
            .slice(src)
            .ok_or_else(|| zerr(ZipError::Truncated, at))?;
        let name_local_bytes = local
            .name
            .slice(src)
            .ok_or_else(|| zerr(ZipError::Truncated, local_pos))?;
        if name_cd_bytes != name_local_bytes {
            return Err(zerr(ZipError::NameMismatch, local_pos));
        }

        // zip64 в локальном extra: по APPNOTE 4.5.3 там всегда оба размера,
        // независимо от маркеров, — writer резервирует место заранее.
        if let Some(f) = ExtraFields::find(src, local.extra, EXTRA_ZIP64) {
            let data = f
                .data
                .slice(src)
                .ok_or_else(|| zerr(ZipError::BadExtraField, local_pos))?;
            let (_, l_local) = zip64::read_zip64_extra(data, Zip64Want::local());
            layout = Some(match layout {
                Some(mut l) => {
                    l.in_local = true;
                    l
                }
                None => Zip64Layout {
                    in_local: true,
                    ..l_local
                },
            });
        }

        // --- данные ---
        let data_start = local.data_start;
        let data_end = (data_start as u64)
            .checked_add(comp_size)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|&e| e <= src.len())
            .ok_or_else(|| zerr(ZipError::DataOutOfBounds, data_start))?;
        let data = Span::new(
            u32::try_from(data_start).map_err(|_| zerr(ZipError::OffsetOverflow, data_start))?,
            u32::try_from(data_end).map_err(|_| zerr(ZipError::OffsetOverflow, data_end))?,
        )
        .ok_or_else(|| zerr(ZipError::DataOutOfBounds, data_start))?;

        // --- дескриптор ---
        let descriptor = if cd.flags & FLAG_DATA_DESCRIPTOR != 0 {
            let wide = layout.is_some_and(|l| l.in_local || l.in_cd);
            read_descriptor(src, data_end, wide, (cd.crc, comp_size, uncomp_size))
        } else {
            None
        };

        Ok(RawEntry {
            name_cd: cd.name,
            name_local: local.name,
            version_made_by: cd.version_made_by,
            version_needed_cd: cd.version_needed,
            version_needed_local: local.version_needed,
            flags: cd.flags,
            method: cd.method,
            dos_time: cd.dos_time,
            dos_date: cd.dos_date,
            crc32: cd.crc,
            comp_size,
            uncomp_size,
            flags_local: local.flags,
            method_local: local.method,
            dos_time_local: local.dos_time,
            dos_date_local: local.dos_date,
            crc32_local: local.crc,
            comp_size_local: u64::from(local.comp32),
            uncomp_size_local: u64::from(local.uncomp32),
            local_extra: local.extra,
            cd_extra: cd.extra,
            comment: cd.comment,
            internal_attrs: cd.internal_attrs,
            external_attrs: cd.external_attrs,
            disk_start,
            local_header_off: local_off,
            local_header: local.header,
            cd_record: cd.record,
            data,
            descriptor,
            zip64_layout: layout,
            gap_before: Span::empty(local.header.start()),
        })
    }

    /// Число записей.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Запись по индексу в порядке центрального каталога.
    ///
    /// # Errors
    /// [`ZipError::DirectoryOutOfBounds`], если индекс за пределами каталога.
    pub fn entry(&self, i: usize) -> Result<&RawEntry> {
        self.entries
            .get(i)
            .ok_or_else(|| zerr(ZipError::DirectoryOutOfBounds, i))
    }

    /// Все записи в порядке центрального каталога.
    #[must_use]
    pub fn entries(&self) -> &[RawEntry] {
        &self.entries
    }

    /// Индексы записей в порядке возрастания физического офсета.
    ///
    /// Хранится отдельно от [`Self::entries`], а не подменяет их порядок: в
    /// корпусе каталог всегда отсортирован по офсету, но формат этого не
    /// требует, и опираться на совпадение — значит однажды переставить
    /// записи местами при перезаписи.
    #[must_use]
    pub fn order_by_offset(&self) -> &[u32] {
        &self.order_by_offset
    }

    /// Байты до первого локального заголовка (код самораспаковки, если он есть).
    #[must_use]
    pub const fn prefix(&self) -> Span {
        self.prefix
    }

    #[must_use]
    pub const fn eocd(&self) -> &Eocd {
        &self.eocd
    }

    #[must_use]
    pub const fn src(&self) -> &'a [u8] {
        self.src
    }

    /// Поправка, прибавленная к офсетам заголовков из каталога.
    ///
    /// Ноль у всех нормальных архивов. Отлична от нуля у архива с префиксом,
    /// офсеты которого не были пересчитаны при добавлении префикса.
    #[must_use]
    pub const fn offset_delta(&self) -> u64 {
        self.offset_delta
    }

    /// Байты между последней записью и началом центрального каталога.
    #[must_use]
    pub const fn gap_before_cd(&self) -> Span {
        self.gap_before_cd
    }

    /// Байты между концом каталога и первой структурой хвоста.
    #[must_use]
    pub const fn gap_after_cd(&self) -> Span {
        self.gap_after_cd
    }

    /// Байты после EOCD и его комментария.
    ///
    /// У корректного архива пусты. Непустыми бывают, если к файлу что-то
    /// дописали; выбрасывать эти байты при перезаписи нельзя — они часть файла.
    #[must_use]
    pub const fn trailing(&self) -> Span {
        self.trailing
    }

    /// Сырые байты имени записи.
    ///
    /// # Errors
    /// [`ZipError::DirectoryOutOfBounds`] при неверном индексе.
    pub fn name(&self, i: usize) -> Result<&'a [u8]> {
        let e = self.entry(i)?;
        e.name_cd
            .slice(self.src)
            .ok_or_else(|| zerr(ZipError::Truncated, e.name_cd.start() as usize))
    }

    /// Имя записи как текст: UTF-8 при bit 11, иначе CP437.
    ///
    /// # Errors
    /// [`ZipError::DirectoryOutOfBounds`] при неверном индексе.
    pub fn name_str(&self, i: usize) -> Result<Cow<'a, str>> {
        let utf8 = self.entry(i)?.flags & FLAG_UTF8 != 0;
        Ok(decode_name(self.name(i)?, utf8))
    }

    /// Индекс записи с точным совпадением имени.
    ///
    /// Сравнение побайтовое: имена частей OPC — ASCII, а гадать о кодировке
    /// при поиске нельзя, иначе часть «то находится, то нет».
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        let needle = name.as_bytes();
        self.entries
            .iter()
            .position(|e| e.name_cd.slice(self.src) == Some(needle))
    }

    /// Сырые, всё ещё сжатые байты записи.
    ///
    /// # Errors
    /// [`ZipError::DataOutOfBounds`], если спан не лежит в буфере.
    pub fn raw_data(&self, i: usize) -> Result<&'a [u8]> {
        let e = self.entry(i)?;
        e.data
            .slice(self.src)
            .ok_or_else(|| zerr(ZipError::DataOutOfBounds, e.data.start() as usize))
    }

    /// Распаковывает запись и сверяет CRC.
    ///
    /// Вызывается только для частей, которые действительно нужно читать:
    /// нетронутые части копируются сырыми и через эту функцию не проходят.
    ///
    /// # Errors
    /// [`ZipError::CrcMismatch`] или [`ZipError::SizeMismatch`] при
    /// расхождении с заголовком, [`Error::Deflate`] на битом потоке,
    /// [`Error::Limit`] при превышении квоты.
    pub fn decompress(&self, i: usize, limits: &Limits) -> Result<Vec<u8>> {
        let e = self.entry(i)?;
        let at = e.data.start() as usize;
        limits.check_part_size(e.uncomp_size)?;
        let raw = self.raw_data(i)?;
        let out = match e.method {
            METHOD_STORE => raw.to_vec(),
            METHOD_DEFLATE => {
                let mut out = Vec::new();
                crate::deflate::inflate_into(raw, &mut out, limits)?;
                out
            }
            m => return Err(zerr(ZipError::UnsupportedMethod(m), at)),
        };
        let got = out.len() as u64;
        if got != e.uncomp_size {
            return Err(zerr(
                ZipError::SizeMismatch {
                    expected: e.uncomp_size,
                    actual: got,
                },
                at,
            ));
        }
        if limits.verify_crc_on_read {
            let actual = crc32(&out);
            if actual != e.crc32 {
                return Err(zerr(
                    ZipError::CrcMismatch {
                        expected: e.crc32,
                        actual,
                    },
                    at,
                ));
            }
        }
        Ok(out)
    }
}
