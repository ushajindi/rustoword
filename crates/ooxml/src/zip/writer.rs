//! Сборка ZIP-контейнера с байтовой идентичностью.
//!
//! # Единственный критерий
//!
//! `repack_all_verbatim(f) == f` побайтово. Всё остальное в этом файле — способ
//! его добиться. Спецификация выведена не из APPNOTE, а из перемера реального
//! корпуса и лежит в `docs/zip-fidelity.md`; ссылки вида «§3.3» ниже — на неё.
//!
//! # Почему запись копируется, а не собирается
//!
//! Нетронутая запись выводится одним `extend_from_slice` куска исходника
//! ([`RawEntry::verbatim`]), а её запись каталога — копией [`RawEntry::cd_record`]
//! с пропатченными четырьмя байтами по смещению 42. Собрать 46-байтовый
//! заголовок из полей — значит получить ещё один шанс переставить местами два
//! соседних `u16`; у копии такого шанса нет. Пересборка остаётся только там,
//! где содержимое действительно меняется ([`EntrySource::Replace`],
//! [`EntrySource::New`]).
//!
//! # Что пересчитывается
//!
//! Ровно одно поле записи каталога — `relative offset of local header` (§2.2),
//! плюс размер и офсет самого каталога в EOCD, которые выводятся из длин.
//! Всё прочее — включая `version made by`, обе `version needed`, флаги, DOS-дату
//! (в корпусе у 296 записей она невалидна, §3.4), `internal`/`external attrs`,
//! обе области extra и форму data descriptor'а — копируется.
//!
//! # Два порядка
//!
//! Порядок push — это **порядок каталога**. Физический порядок вывода данных
//! берётся из исходных офсетов локальных заголовков; новые записи дописываются
//! в конец. В корпусе оба порядка совпадают у всех 43 файлов, но формат этого
//! не требует, а сортировка записей ломает 43 файла из 43 (§3.6), поэтому
//! порядки хранятся раздельно и ни один не выводится из другого.

use std::borrow::Cow;

use super::consts::{
    EXTRA_ZIP64, FLAG_DATA_DESCRIPTOR, FLAG_UTF8, METHOD_DEFLATE, METHOD_STORE, SIG_CENTRAL,
    SIG_DATA_DESCRIPTOR, SIG_EOCD, SIG_LOCAL, SIG_ZIP64_EOCD, SIG_ZIP64_LOCATOR, U16_MARKER,
    U32_MARKER,
};
use super::entry::RawEntry;
use super::extra::ExtraFields;
use super::reader::ZipArchive;
use crate::bytes::{Span, Writer};
use crate::deflate::{self, Level};
use crate::error::{Error, Result, ZipError};
use crate::hash::crc32;

/// Смещение поля `relative offset of local header` внутри записи каталога.
///
/// Фиксировано форматом: 4 + 2·6 + 4·3 + 2·3 + 2 + 2 + 4 = 42. Патчится по
/// этому смещению в скопированной записи — см. заголовок модуля.
const CD_OFFSET_FIELD: usize = 42;

/// `version needed to extract` для метода deflate.
const VERSION_DEFLATE: u16 = 20;
/// `version needed to extract` для метода store.
const VERSION_STORE: u16 = 10;
/// `version made by` для записей, которых не было в исходнике: FAT-хост, ZIP 2.0.
/// Самое частое значение корпуса (418 записей из 606).
const VERSION_MADE_BY: u16 = 20;

const fn werr(kind: ZipError, offset: usize) -> Error {
    Error::Zip {
        kind,
        offset: offset as u64,
    }
}

/// Вырезает спан из буфера, превращая выход за границы в ошибку.
fn slice_of(src: &[u8], s: Span) -> Result<&[u8]> {
    s.slice(src)
        .ok_or_else(|| werr(ZipError::DataOutOfBounds, s.start() as usize))
}

/// Пишет `v` в `buf` начиная с `at`.
fn put(buf: &mut [u8], at: usize, v: &[u8]) -> Result<()> {
    let end = at
        .checked_add(v.len())
        .ok_or_else(|| werr(ZipError::OffsetOverflow, at))?;
    buf.get_mut(at..end)
        .ok_or_else(|| werr(ZipError::OffsetOverflow, at))?
        .copy_from_slice(v);
    Ok(())
}

/// Откуда берётся очередная запись выходного архива.
#[derive(Debug)]
#[non_exhaustive]
pub enum EntrySource<'a> {
    /// Копия сжатых байт и обоих заголовков из исходного архива.
    ///
    /// Единственный путь, дающий байтовую идентичность. Данные не
    /// распаковываются, CRC не пересчитывается, deflate не запускается: чужой
    /// deflate-поток воспроизвести невозможно (§1 — 0 совпадений из 450 на
    /// потоках Office и Яндекса даже подбором уровня zlib).
    Verbatim {
        src: &'a ZipArchive<'a>,
        index: usize,
    },
    /// Замена содержимого записи: пересжать и переписать crc и размеры.
    ///
    /// Сохраняется всё, что не зависит от содержимого: `version made by`,
    /// флаги (включая bit 3 и форму дескриптора), DOS-время, обе области
    /// extra, атрибуты, комментарий и физическая позиция записи.
    Replace {
        src: &'a ZipArchive<'a>,
        index: usize,
        data: Vec<u8>,
        level: Level,
    },
    /// Новая запись, которой не было в исходнике.
    ///
    /// `dos` — пара «время, дата» сырыми `u16`. Часов у ядра нет и быть не
    /// может (§3.4): подстановка текущего времени ломает round-trip 43 файлов
    /// из 43 и делает результат недетерминированным.
    New {
        name: String,
        data: Vec<u8>,
        level: Level,
        dos: (u16, u16),
        external_attrs: u32,
    },
}

/// Что делать с байтами, которые не принадлежат ни одной записи.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Сохранять байты до первого локального заголовка (код самораспаковки).
    pub keep_prefix: bool,
    /// Сохранять зазоры: перед записями, перед каталогом и после него.
    pub keep_gaps: bool,
    /// Уровень сжатия по умолчанию. Уровень, указанный в самой
    /// [`EntrySource`], важнее — это поле лишь заготовка для удобных
    /// обёрток, которым не из чего его взять.
    pub level: Level,
}

impl Default for WriteOptions {
    /// Всё сохраняющее. Потерять байты можно только попросив об этом явно.
    fn default() -> Self {
        Self {
            keep_prefix: true,
            keep_gaps: true,
            level: Level::default(),
        }
    }
}

/// Сборщик ZIP-контейнера.
#[derive(Debug)]
pub struct ZipWriter<'a> {
    opts: WriteOptions,
    entries: Vec<EntrySource<'a>>,
    /// Явно заданный префикс; перекрывает исходный.
    prefix: Option<Vec<u8>>,
    /// Архив-шаблон: у него берутся префикс, зазоры и хвостовые структуры.
    ///
    /// Первый архив, на который сослалась хоть одна запись. Из архива без
    /// записей взять шаблон неоткуда, и тогда хвост собирается с нуля.
    template: Option<&'a ZipArchive<'a>>,
}

/// Запись, приведённая к байтам и ждущая только своего офсета.
#[derive(Debug)]
struct Prepared<'a> {
    /// Зазор перед локальным заголовком.
    gap: &'a [u8],
    /// Локальный заголовок + данные + дескриптор одним куском.
    block: Cow<'a, [u8]>,
    /// Запись каталога; байты 42..46 будут перезаписаны настоящим офсетом.
    cd: Vec<u8>,
    /// Позиция 8-байтового офсета внутри `cd`, если он вынесен в zip64-extra.
    z64_offset_at: Option<usize>,
    /// Ключ физического порядка — исходный офсет локального заголовка.
    /// `None` у новых записей: им место в конце.
    phys: Option<u32>,
}

impl<'a> ZipWriter<'a> {
    #[must_use]
    pub const fn new(opts: WriteOptions) -> Self {
        Self {
            opts,
            entries: Vec::new(),
            prefix: None,
            template: None,
        }
    }

    /// Задаёт префикс архива явно.
    ///
    /// Офсеты в этом случае пишутся абсолютными от начала файла: у исходного
    /// префикса могла быть своя система координат (см. [`ZipArchive::offset_delta`]),
    /// но переносить её на чужие байты — гадание.
    pub fn set_prefix(&mut self, bytes: &[u8]) {
        self.prefix = Some(bytes.to_vec());
    }

    /// Добавляет запись. Порядок вызовов — это порядок центрального каталога.
    ///
    /// # Errors
    /// [`ZipError::DirectoryOutOfBounds`], если индекс не существует в исходном
    /// архиве. Проверка здесь, а не в [`Self::finish`]: ошибку надо вернуть в
    /// том месте, где видно, кто её сделал.
    pub fn push(&mut self, e: EntrySource<'a>) -> Result<()> {
        match &e {
            EntrySource::Verbatim { src, index } | EntrySource::Replace { src, index, .. } => {
                src.entry(*index)?;
                if self.template.is_none() {
                    self.template = Some(src);
                }
            }
            EntrySource::New { .. } => {}
        }
        self.entries.push(e);
        Ok(())
    }

    /// Собирает архив.
    ///
    /// # Errors
    /// [`Error::Unsupported`] при выходе за 4 ГиБ без исходных zip64-полей,
    /// [`Error::Zip`] при несогласованных спанах исходника.
    #[allow(clippy::too_many_lines)]
    pub fn finish(self) -> Result<Vec<u8>> {
        let Self {
            opts,
            entries,
            prefix,
            template,
        } = self;
        let count = entries.len() as u64;

        let mut prepared: Vec<Prepared<'a>> = Vec::with_capacity(entries.len());
        let mut cap: usize = 128;
        for e in entries {
            let p = prepare(e)?;
            cap = cap
                .saturating_add(p.gap.len())
                .saturating_add(p.block.len())
                .saturating_add(p.cd.len());
            prepared.push(p);
        }

        // Физический порядок. Сортировка стабильная и по одному ключу, поэтому
        // новые записи (ключ отсутствует) сохраняют порядок push.
        let mut order: Vec<(u8, u32, usize)> = prepared
            .iter()
            .enumerate()
            .map(|(i, p)| p.phys.map_or((1u8, 0u32, i), |x| (0u8, x, i)))
            .collect();
        // Ключ — только первые два поля: индекс в него не входит, иначе
        // стабильность сортировки перестала бы что-либо значить.
        order.sort_by_key(|a| (a.0, a.1));

        let mut w = Writer::with_capacity(cap);

        // --- префикс ---
        // `base` — поправка, превращающая фактический офсет в записываемый.
        // У нормального архива ноль. У самораспаковывающегося офсеты могут
        // отсчитываться от начала zip-части, а не файла; система координат
        // исходника сохраняется, пока сохраняется его префикс.
        let (prefix_bytes, base): (&[u8], u64) = match (&prefix, template) {
            (Some(p), _) => (p.as_slice(), 0),
            (None, Some(t)) if opts.keep_prefix => {
                (slice_of(t.src(), t.prefix())?, t.offset_delta())
            }
            _ => (&[], 0),
        };
        w.bytes(prefix_bytes);

        // --- записи в физическом порядке ---
        let mut offsets: Vec<u64> = vec![0; prepared.len()];
        for &(_, _, i) in &order {
            let p = prepared
                .get(i)
                .ok_or_else(|| werr(ZipError::OffsetOverflow, i))?;
            if opts.keep_gaps {
                w.bytes(p.gap);
            }
            let at = w
                .offset()
                .checked_sub(base)
                .ok_or_else(|| werr(ZipError::OffsetOverflow, w.len()))?;
            *offsets
                .get_mut(i)
                .ok_or_else(|| werr(ZipError::OffsetOverflow, i))? = at;
            w.bytes(&p.block);
        }

        if opts.keep_gaps
            && let Some(t) = template
        {
            w.bytes(slice_of(t.src(), t.gap_before_cd())?);
        }

        // --- центральный каталог, в порядке push ---
        let cd_start = w.offset();
        for (i, p) in prepared.iter_mut().enumerate() {
            let off = offsets
                .get(i)
                .copied()
                .ok_or_else(|| werr(ZipError::OffsetOverflow, i))?;
            patch_offset(p, off)?;
            w.bytes(&p.cd);
        }
        let cd_size = w
            .offset()
            .checked_sub(cd_start)
            .ok_or_else(|| werr(ZipError::OffsetOverflow, w.len()))?;
        let cd_offset = cd_start
            .checked_sub(base)
            .ok_or_else(|| werr(ZipError::OffsetOverflow, w.len()))?;

        if opts.keep_gaps
            && let Some(t) = template
        {
            w.bytes(slice_of(t.src(), t.gap_after_cd())?);
        }

        // --- хвост ---
        let eocd = template.map(ZipArchive::eocd);
        let z64 = eocd.and_then(|e| e.zip64);
        let locator = eocd.and_then(|e| e.locator);
        let has_z64 = z64.is_some();
        // Счётчики копируются как есть, только если число записей не менялось:
        // «на этом диске» и «всего» обязаны совпадать в однотомном архиве, но
        // writer'ы ошибаются, и починка такой ошибки — это изменение байт.
        let keep_counts = eocd.is_some_and(|e| e.eff_entries_total == count);

        let mut z64_pos: Option<u64> = None;
        if let (Some(z), Some(t)) = (z64, template) {
            let pos = w.offset();
            w.le_u32(SIG_ZIP64_EOCD);
            // `record_size` копируется, а не считается: writer вправе объявить
            // запись длиннее фиксированных 44 байт, и хвост лежит в
            // `extensible_data`, который мы переносим целиком.
            w.le_u64(z.record_size);
            w.le_u16(z.version_made_by);
            w.le_u16(z.version_needed);
            w.le_u32(z.disk_number);
            w.le_u32(z.cd_start_disk);
            w.le_u64(if keep_counts {
                z.entries_this_disk
            } else {
                count
            });
            w.le_u64(if keep_counts { z.entries_total } else { count });
            w.le_u64(cd_size);
            w.le_u64(cd_offset);
            w.bytes(slice_of(t.src(), z.extensible_data)?);
            z64_pos = Some(pos);
        }
        if let Some(l) = locator {
            w.le_u32(SIG_ZIP64_LOCATOR);
            w.le_u32(l.eocd_disk);
            let off = match z64_pos {
                Some(p) => p
                    .checked_sub(base)
                    .ok_or_else(|| werr(ZipError::OffsetOverflow, w.len()))?,
                // Локатор без записи, на которую он показывает, — испорченный
                // исходник; своё поле он сохраняет, чтобы не стало хуже.
                None => l.eocd_offset,
            };
            w.le_u64(off);
            w.le_u32(l.total_disks);
        }

        // --- EOCD ---
        w.le_u32(SIG_EOCD);
        w.le_u16(eocd.map_or(0, |e| e.disk_number));
        w.le_u16(eocd.map_or(0, |e| e.cd_start_disk));
        let (this_disk, total) = match eocd {
            Some(e) if keep_counts => (e.entries_this_disk, e.entries_total),
            _ => {
                let v = fit_u16(count, has_z64)?;
                (v, v)
            }
        };
        w.le_u16(this_disk);
        w.le_u16(total);
        w.le_u32(fit_u32(eocd.map(|e| e.cd_size), cd_size, has_z64)?);
        w.le_u32(fit_u32(eocd.map(|e| e.cd_offset), cd_offset, has_z64)?);
        let comment: &[u8] = match template {
            Some(t) => slice_of(t.src(), t.eocd().comment)?,
            None => &[],
        };
        w.le_u16(u16::try_from(comment.len()).map_err(|_| werr(ZipError::OffsetOverflow, 0))?);
        w.bytes(comment);

        // Байты после EOCD — часть файла, а не мусор: выбросить их значит
        // молча потерять данные, которые кто-то туда положил.
        if let Some(t) = template {
            w.bytes(slice_of(t.src(), t.trailing())?);
        }

        Ok(w.finish())
    }
}

/// Пересобирает архив, ничего не меняя.
///
/// Блокирующий критерий вехи M3: результат обязан совпасть с исходным буфером
/// побайтово на всех 43 файлах корпуса.
///
/// # Errors
/// См. [`ZipWriter::finish`].
pub fn repack_all_verbatim<'a>(src: &'a ZipArchive<'a>) -> Result<Vec<u8>> {
    let mut w = ZipWriter::new(WriteOptions::default());
    for index in 0..src.len() {
        w.push(EntrySource::Verbatim { src, index })?;
    }
    w.finish()
}

/// Вписывает в запись каталога пересчитанный офсет локального заголовка.
fn patch_offset(p: &mut Prepared<'_>, off: u64) -> Result<()> {
    if let Some(at) = p.z64_offset_at {
        // Настоящий офсет лежит в записи `0x0001`; в 32-битном поле обязан
        // остаться маркер, иначе читатель возьмёт его вместо zip64-значения.
        put(&mut p.cd, at, &off.to_le_bytes())?;
        return put(&mut p.cd, CD_OFFSET_FIELD, &U32_MARKER.to_le_bytes());
    }
    match u32::try_from(off) {
        Ok(v) => put(&mut p.cd, CD_OFFSET_FIELD, &v.to_le_bytes()),
        // Офсет перешагнул 4 ГиБ, а поля `0x0001` в записи не было. Вставить
        // его — сдвинуть весь каталог и переписать EOCD; записать обрезанный
        // офсет — молча испортить архив. Ни то ни другое не делается тихо.
        Err(_) => Err(Error::Unsupported("zip64 promotion")),
    }
}

/// Приводит фактическое значение к 16-битному полю EOCD.
///
/// Если в исходнике стоял маркер, он и переизлучается: настоящее значение
/// лежит в zip64 EOCD, и «починка» 16-битного поля рассогласовала бы их.
fn fit_u16(actual: u64, zip64: bool) -> Result<u16> {
    match u16::try_from(actual) {
        Ok(v) => Ok(v),
        Err(_) if zip64 => Ok(U16_MARKER),
        Err(_) => Err(Error::Unsupported("zip64 promotion")),
    }
}

/// То же для 32-битного поля. `raw` — значение из исходного EOCD, если оно было.
fn fit_u32(raw: Option<u32>, actual: u64, zip64: bool) -> Result<u32> {
    if raw == Some(U32_MARKER) {
        return Ok(U32_MARKER);
    }
    match u32::try_from(actual) {
        Ok(v) => Ok(v),
        Err(_) if zip64 => Ok(U32_MARKER),
        Err(_) => Err(Error::Unsupported("zip64 promotion")),
    }
}

/// Позиция 8-байтового zip64-офсета внутри записи каталога.
///
/// Запись `0x0001` позиционная: элементы идут в порядке uncompressed,
/// compressed, offset, disk и присутствуют только те, у кого в основном
/// заголовке стоит маркер. Поэтому позицию офсета нельзя взять константой —
/// её надо сложить из того, что перед ним оказалось.
fn cd_zip64_offset_pos(src: &[u8], e: &RawEntry) -> Option<usize> {
    let l = e.zip64_layout?;
    if !l.in_cd || !l.has_offset {
        return None;
    }
    let f = ExtraFields::find(src, e.cd_extra, EXTRA_ZIP64)?;
    let rel = f.data.start().checked_sub(e.cd_record.start())?;
    let mut at = usize::try_from(rel).ok()?;
    if l.has_usize {
        at = at.checked_add(8)?;
    }
    if l.has_csize {
        at = at.checked_add(8)?;
    }
    Some(at)
}

/// Сжимает данные и сообщает метод, которым они сжаты.
///
/// [`Level::Store`] — это не «нулевой уровень deflate», а метод 0: данные
/// пишутся сырыми. Благодаря этому пути [`EntrySource::New`] и
/// [`EntrySource::Replace`] работают уже сейчас, пока `deflate()` — заглушка
/// вехи M4.
fn compress(data: &[u8], level: Level) -> (u16, Cow<'_, [u8]>) {
    match level {
        Level::Store => (METHOD_STORE, Cow::Borrowed(data)),
        l => (METHOD_DEFLATE, Cow::Owned(deflate::deflate(data, l))),
    }
}

/// `version needed to extract`, поднятая до минимума, требуемого методом.
///
/// Не нормализуется в 20: одна запись корпуса объявляет 10, и приведение к
/// константе ломает файл (§3.2). Поднимается только там, где метод сменился
/// на deflate и старое значение стало ложью.
const fn version_for(was: u16, method: u16) -> u16 {
    if method == METHOD_DEFLATE && was < VERSION_DEFLATE {
        VERSION_DEFLATE
    } else {
        was
    }
}

/// Поля записи, которую пришлось собрать заново.
#[derive(Debug)]
struct Rebuilt<'b> {
    version_made_by: u16,
    version_needed_local: u16,
    version_needed_cd: u16,
    flags: u16,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc: u32,
    comp: u32,
    uncomp: u32,
    internal_attrs: u16,
    external_attrs: u32,
    name_local: &'b [u8],
    name_cd: &'b [u8],
    local_extra: &'b [u8],
    cd_extra: &'b [u8],
    comment: &'b [u8],
    /// Ставить ли перед дескриптором сигнатуру `PK\x07\x08`. Значимо только
    /// при bit 3; форма берётся у исходной записи, а не «как принято».
    descriptor_sig: bool,
    /// Уже сжатые данные.
    data: &'b [u8],
}

impl Rebuilt<'_> {
    /// Запись писалась потоком: crc и размеры вынесены в дескриптор.
    const fn streaming(&self) -> bool {
        self.flags & FLAG_DATA_DESCRIPTOR != 0
    }

    /// Локальный заголовок, данные и дескриптор одним куском.
    fn local_block(&self) -> Result<Vec<u8>> {
        let name =
            u16::try_from(self.name_local.len()).map_err(|_| werr(ZipError::OffsetOverflow, 0))?;
        let extra =
            u16::try_from(self.local_extra.len()).map_err(|_| werr(ZipError::OffsetOverflow, 0))?;
        let mut w = Writer::with_capacity(
            30usize
                .saturating_add(self.name_local.len())
                .saturating_add(self.local_extra.len())
                .saturating_add(self.data.len())
                .saturating_add(16),
        );
        w.le_u32(SIG_LOCAL);
        w.le_u16(self.version_needed_local);
        w.le_u16(self.flags);
        w.le_u16(self.method);
        w.le_u16(self.dos_time);
        w.le_u16(self.dos_date);
        // При bit 3 создатель архива не знал ни crc, ни размеров и оставил
        // здесь нули. Вписать настоящие значения — сломать 31 файл корпуса
        // из 43 (§3.3): читатель, доверяющий локальному заголовку, увидит
        // одно, читатель дескриптора — другое.
        let hide = self.streaming();
        w.le_u32(if hide { 0 } else { self.crc });
        w.le_u32(if hide { 0 } else { self.comp });
        w.le_u32(if hide { 0 } else { self.uncomp });
        w.le_u16(name);
        w.le_u16(extra);
        w.bytes(self.name_local);
        // Local extra ≠ cd extra: у 53 записей корпуса здесь 40/264/520 байт
        // «Open Packaging Growth Hint», а в каталоге у тех же записей — ноль.
        w.bytes(self.local_extra);
        w.bytes(self.data);
        if hide {
            if self.descriptor_sig {
                w.le_u32(SIG_DATA_DESCRIPTOR);
            }
            w.le_u32(self.crc);
            w.le_u32(self.comp);
            w.le_u32(self.uncomp);
        }
        Ok(w.finish())
    }

    /// Запись центрального каталога с нулём в поле офсета.
    ///
    /// Офсет вписывается позже, когда станет известна позиция заголовка.
    fn cd_record(&self) -> Result<Vec<u8>> {
        let name =
            u16::try_from(self.name_cd.len()).map_err(|_| werr(ZipError::OffsetOverflow, 0))?;
        let extra =
            u16::try_from(self.cd_extra.len()).map_err(|_| werr(ZipError::OffsetOverflow, 0))?;
        let comment =
            u16::try_from(self.comment.len()).map_err(|_| werr(ZipError::OffsetOverflow, 0))?;
        let mut w = Writer::with_capacity(
            46usize
                .saturating_add(self.name_cd.len())
                .saturating_add(self.cd_extra.len())
                .saturating_add(self.comment.len()),
        );
        w.le_u32(SIG_CENTRAL);
        w.le_u16(self.version_made_by);
        w.le_u16(self.version_needed_cd);
        w.le_u16(self.flags);
        w.le_u16(self.method);
        w.le_u16(self.dos_time);
        w.le_u16(self.dos_date);
        // В каталоге crc и размеры настоящие даже при bit 3.
        w.le_u32(self.crc);
        w.le_u32(self.comp);
        w.le_u32(self.uncomp);
        w.le_u16(name);
        w.le_u16(extra);
        w.le_u16(comment);
        w.le_u16(0);
        w.le_u16(self.internal_attrs);
        w.le_u32(self.external_attrs);
        w.le_u32(0);
        w.bytes(self.name_cd);
        w.bytes(self.cd_extra);
        w.bytes(self.comment);
        Ok(w.finish())
    }
}

/// Приводит источник записи к байтам.
fn prepare<'a>(e: EntrySource<'a>) -> Result<Prepared<'a>> {
    match e {
        EntrySource::Verbatim { src, index } => {
            let raw = src.entry(index)?;
            let buf = src.src();
            Ok(Prepared {
                gap: slice_of(buf, raw.gap_before)?,
                block: Cow::Borrowed(slice_of(buf, raw.verbatim())?),
                cd: slice_of(buf, raw.cd_record)?.to_vec(),
                z64_offset_at: cd_zip64_offset_pos(buf, raw),
                phys: Some(raw.local_header.start()),
            })
        }
        EntrySource::Replace {
            src,
            index,
            data,
            level,
        } => {
            let raw = src.entry(index)?;
            let buf = src.src();
            // Переизлучить zip64-запись с новыми размерами значит переписать
            // позиционное поле `0x0001` в обоих заголовках, а при смене набора
            // элементов — ещё и сдвинуть каталог. Пока таких входов нет
            // (в корпусе zip64 не встречается ни разу), отказ честнее догадок.
            if raw.zip64_layout.is_some() {
                return Err(Error::Unsupported("zip64 replace"));
            }
            let (method, payload) = compress(&data, level);
            let f = Rebuilt {
                version_made_by: raw.version_made_by,
                version_needed_local: version_for(raw.version_needed_local, method),
                version_needed_cd: version_for(raw.version_needed_cd, method),
                flags: raw.flags,
                method,
                dos_time: raw.dos_time,
                dos_date: raw.dos_date,
                crc: crc32(&data),
                comp: u32::try_from(payload.len())
                    .map_err(|_| Error::Unsupported("zip64 promotion"))?,
                uncomp: u32::try_from(data.len())
                    .map_err(|_| Error::Unsupported("zip64 promotion"))?,
                internal_attrs: raw.internal_attrs,
                external_attrs: raw.external_attrs,
                name_local: slice_of(buf, raw.name_local)?,
                name_cd: slice_of(buf, raw.name_cd)?,
                local_extra: slice_of(buf, raw.local_extra)?,
                cd_extra: slice_of(buf, raw.cd_extra)?,
                comment: slice_of(buf, raw.comment)?,
                descriptor_sig: raw.descriptor.is_none_or(|d| d.has_signature),
                data: &payload,
            };
            Ok(Prepared {
                gap: slice_of(buf, raw.gap_before)?,
                block: Cow::Owned(f.local_block()?),
                cd: f.cd_record()?,
                z64_offset_at: None,
                phys: Some(raw.local_header.start()),
            })
        }
        EntrySource::New {
            name,
            data,
            level,
            dos,
            external_attrs,
        } => {
            let (method, payload) = compress(&data, level);
            let name_bytes = name.as_bytes();
            let f = Rebuilt {
                version_made_by: VERSION_MADE_BY,
                version_needed_local: version_for(VERSION_STORE, method),
                version_needed_cd: version_for(VERSION_STORE, method),
                // bit 11 ставится по факту: имя пришло как `String`, то есть
                // заведомо UTF-8. Для ASCII бит не нужен — его отсутствие
                // совместимо с CP437, и лишний бит менял бы байты зря.
                flags: if name.is_ascii() { 0 } else { FLAG_UTF8 },
                method,
                dos_time: dos.0,
                dos_date: dos.1,
                crc: crc32(&data),
                comp: u32::try_from(payload.len())
                    .map_err(|_| Error::Unsupported("zip64 promotion"))?,
                uncomp: u32::try_from(data.len())
                    .map_err(|_| Error::Unsupported("zip64 promotion"))?,
                internal_attrs: 0,
                external_attrs,
                name_local: name_bytes,
                name_cd: name_bytes,
                local_extra: &[],
                cd_extra: &[],
                comment: &[],
                descriptor_sig: false,
                data: &payload,
            };
            Ok(Prepared {
                gap: &[],
                block: Cow::Owned(f.local_block()?),
                cd: f.cd_record()?,
                z64_offset_at: None,
                phys: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    #[test]
    fn offset_field_sits_at_42() {
        // Поле офсета патчится по константе; если раскладка заголовка вдруг
        // разъедется с ней, ломаться должно здесь, а не на корпусе.
        let f = Rebuilt {
            version_made_by: 20,
            version_needed_local: 20,
            version_needed_cd: 20,
            flags: 0,
            method: 0,
            dos_time: 0,
            dos_date: 0,
            crc: 0,
            comp: 0,
            uncomp: 0,
            internal_attrs: 0,
            external_attrs: 0,
            name_local: b"a",
            name_cd: b"a",
            local_extra: &[],
            cd_extra: &[],
            comment: &[],
            descriptor_sig: false,
            data: &[],
        };
        let mut p = Prepared {
            gap: &[],
            block: Cow::Owned(f.local_block().unwrap()),
            cd: f.cd_record().unwrap(),
            z64_offset_at: None,
            phys: None,
        };
        patch_offset(&mut p, 0x1122_3344).unwrap();
        assert_eq!(&p.cd[42..46], &[0x44, 0x33, 0x22, 0x11]);
        assert_eq!(p.cd.len(), 47);
    }

    #[test]
    fn offset_beyond_4gib_without_zip64_is_refused() {
        let f = Rebuilt {
            version_made_by: 20,
            version_needed_local: 20,
            version_needed_cd: 20,
            flags: 0,
            method: 0,
            dos_time: 0,
            dos_date: 0,
            crc: 0,
            comp: 0,
            uncomp: 0,
            internal_attrs: 0,
            external_attrs: 0,
            name_local: b"a",
            name_cd: b"a",
            local_extra: &[],
            cd_extra: &[],
            comment: &[],
            descriptor_sig: false,
            data: &[],
        };
        let mut p = Prepared {
            gap: &[],
            block: Cow::Owned(f.local_block().unwrap()),
            cd: f.cd_record().unwrap(),
            z64_offset_at: None,
            phys: None,
        };
        assert_eq!(
            patch_offset(&mut p, u64::from(u32::MAX) + 1),
            Err(Error::Unsupported("zip64 promotion"))
        );
    }

    #[test]
    fn store_level_uses_method_zero_and_copies_bytes() {
        let (m, out) = compress(b"payload", Level::Store);
        assert_eq!(m, METHOD_STORE);
        assert_eq!(&*out, b"payload");
    }

    #[test]
    fn version_is_raised_only_for_deflate() {
        assert_eq!(version_for(10, METHOD_STORE), 10);
        assert_eq!(version_for(10, METHOD_DEFLATE), 20);
        assert_eq!(version_for(45, METHOD_DEFLATE), 45, "понижать нельзя");
    }

    #[test]
    fn eocd_marker_is_reemitted_not_recomputed() {
        assert_eq!(fit_u32(Some(U32_MARKER), 100, true).unwrap(), U32_MARKER);
        assert_eq!(fit_u32(Some(7), 100, false).unwrap(), 100);
        assert!(fit_u32(None, u64::from(u32::MAX) + 1, false).is_err());
        assert_eq!(
            fit_u32(None, u64::from(u32::MAX) + 1, true).unwrap(),
            U32_MARKER
        );
    }
}
