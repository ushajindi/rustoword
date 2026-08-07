//! Разбор ZIP на рукотворных байтовых массивах.
//!
//! Сторонних крейтов в проекте нет, поэтому архивы собираются здесь же, байт
//! за байтом. Это не недостаток, а свойство: тест, который сам укладывает
//! поля, проверяет разбор ровно тех байт, что описаны в APPNOTE, а не то, что
//! случайно сгенерировала чужая библиотека.
//!
//! Распаковки здесь нет ни в одном тесте: `inflate` — заглушка вехи M1, а
//! ценность вехи M2 в разборе заголовков.

// В тестах паника — способ сообщить о провале, а не дефект.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::hash::crc32;
use ooxml::zip::ZipArchive;

// ---------------------------------------------------------------- сборка ---

const SIG_LOCAL: [u8; 4] = [b'P', b'K', 3, 4];
const SIG_CENTRAL: [u8; 4] = [b'P', b'K', 1, 2];
const SIG_EOCD: [u8; 4] = [b'P', b'K', 5, 6];
const SIG_DD: [u8; 4] = [b'P', b'K', 7, 8];
const SIG_Z64_EOCD: [u8; 4] = [b'P', b'K', 6, 6];
const SIG_Z64_LOC: [u8; 4] = [b'P', b'K', 6, 7];

fn le16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}
fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}
fn le64(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

/// Описание одной записи для сборщика.
#[derive(Clone, Debug)]
struct Item {
    name: Vec<u8>,
    /// Уже «сжатые» байты. Для method=0 это и есть содержимое.
    data: Vec<u8>,
    method: u16,
    flags: u16,
    crc: u32,
    uncomp: u32,
    local_extra: Vec<u8>,
    cd_extra: Vec<u8>,
    comment: Vec<u8>,
    made_by: u16,
    vneed_local: u16,
    vneed_cd: u16,
    internal: u16,
    external: u32,
    /// Ставить ли сигнатуру перед дескриптором (только при flags & 8).
    descriptor_sig: bool,
    /// Имя в локальном заголовке, если оно должно отличаться от каталожного.
    local_name: Option<Vec<u8>>,
    /// Мусор, вставляемый перед локальным заголовком.
    gap: Vec<u8>,
}

impl Item {
    fn stored(name: &str, data: &[u8]) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            data: data.to_vec(),
            method: 0,
            flags: 0,
            crc: crc32(data),
            uncomp: data.len() as u32,
            local_extra: Vec::new(),
            cd_extra: Vec::new(),
            comment: Vec::new(),
            made_by: 20,
            vneed_local: 10,
            vneed_cd: 10,
            internal: 0,
            external: 0,
            descriptor_sig: true,
            local_name: None,
            gap: Vec::new(),
        }
    }

    /// «Сжатая» запись: байты данных произвольны — распаковка здесь не нужна.
    fn deflated(name: &str, comp: &[u8], uncomp_len: u32, crc: u32) -> Self {
        let mut it = Self::stored(name, comp);
        it.method = 8;
        it.uncomp = uncomp_len;
        it.crc = crc;
        it.vneed_local = 20;
        it.vneed_cd = 20;
        it
    }

    fn streaming(mut self, with_sig: bool) -> Self {
        self.flags |= 0x0008;
        self.descriptor_sig = with_sig;
        self
    }
}

/// Что сборщик сообщает тесту, чтобы тот мог точечно испортить байты.
#[derive(Debug, Default)]
struct Layout {
    local_offsets: Vec<u32>,
    cd_offsets: Vec<u32>,
    cd_start: u32,
    cd_size: u32,
    eocd: u32,
}

fn build(prefix: &[u8], items: &[Item], comment: &[u8]) -> (Vec<u8>, Layout) {
    let mut out = prefix.to_vec();
    let mut lay = Layout::default();

    for it in items {
        out.extend_from_slice(&it.gap);
        lay.local_offsets.push(out.len() as u32);
        let streaming = it.flags & 0x0008 != 0;
        let lname = it.local_name.as_ref().unwrap_or(&it.name);
        out.extend_from_slice(&SIG_LOCAL);
        out.extend_from_slice(&le16(it.vneed_local));
        out.extend_from_slice(&le16(it.flags));
        out.extend_from_slice(&le16(it.method));
        out.extend_from_slice(&le16(0x4A2B)); // dos time
        out.extend_from_slice(&le16(0x5891)); // dos date
        // При bit 3 writer ещё не знает crc и размеров — в локальном
        // заголовке нули, настоящие значения уйдут в каталог и дескриптор.
        out.extend_from_slice(&le32(if streaming { 0 } else { it.crc }));
        out.extend_from_slice(&le32(if streaming { 0 } else { it.data.len() as u32 }));
        out.extend_from_slice(&le32(if streaming { 0 } else { it.uncomp }));
        out.extend_from_slice(&le16(lname.len() as u16));
        out.extend_from_slice(&le16(it.local_extra.len() as u16));
        out.extend_from_slice(lname);
        out.extend_from_slice(&it.local_extra);
        out.extend_from_slice(&it.data);
        if streaming {
            if it.descriptor_sig {
                out.extend_from_slice(&SIG_DD);
            }
            out.extend_from_slice(&le32(it.crc));
            out.extend_from_slice(&le32(it.data.len() as u32));
            out.extend_from_slice(&le32(it.uncomp));
        }
    }

    lay.cd_start = out.len() as u32;
    for (it, &off) in items.iter().zip(&lay.local_offsets) {
        lay.cd_offsets.push(out.len() as u32);
        out.extend_from_slice(&SIG_CENTRAL);
        out.extend_from_slice(&le16(it.made_by));
        out.extend_from_slice(&le16(it.vneed_cd));
        out.extend_from_slice(&le16(it.flags));
        out.extend_from_slice(&le16(it.method));
        out.extend_from_slice(&le16(0x4A2B));
        out.extend_from_slice(&le16(0x5891));
        out.extend_from_slice(&le32(it.crc));
        out.extend_from_slice(&le32(it.data.len() as u32));
        out.extend_from_slice(&le32(it.uncomp));
        out.extend_from_slice(&le16(it.name.len() as u16));
        out.extend_from_slice(&le16(it.cd_extra.len() as u16));
        out.extend_from_slice(&le16(it.comment.len() as u16));
        out.extend_from_slice(&le16(0)); // disk start
        out.extend_from_slice(&le16(it.internal));
        out.extend_from_slice(&le32(it.external));
        out.extend_from_slice(&le32(off));
        out.extend_from_slice(&it.name);
        out.extend_from_slice(&it.cd_extra);
        out.extend_from_slice(&it.comment);
    }
    lay.cd_size = out.len() as u32 - lay.cd_start;

    lay.eocd = out.len() as u32;
    out.extend_from_slice(&SIG_EOCD);
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(items.len() as u16));
    out.extend_from_slice(&le16(items.len() as u16));
    out.extend_from_slice(&le32(lay.cd_size));
    out.extend_from_slice(&le32(lay.cd_start));
    out.extend_from_slice(&le16(comment.len() as u16));
    out.extend_from_slice(comment);
    (out, lay)
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&le32(v));
}
fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&le16(v));
}

fn strict() -> Limits {
    Limits::strict()
}

/// Спаны модели обязаны замостить весь файл — без дыр и наложений.
///
/// Это предусловие вехи M3: байт, не попавший ни в один спан, будет потерян
/// при перезаписи. Проверка стоит здесь, а не только на корпусе, потому что
/// на рукотворных архивах видно, какая именно структура выпала.
fn assert_tiles_whole_file(z: &ZipArchive<'_>, len: usize) {
    let mut r: Vec<(u32, u32, &str)> = Vec::new();
    let push = |s: ooxml::bytes::Span, tag: &'static str, r: &mut Vec<(u32, u32, &str)>| {
        if !s.is_empty() {
            r.push((s.start(), s.end(), tag));
        }
    };
    push(z.prefix(), "prefix", &mut r);
    for e in z.entries() {
        push(e.gap_before, "gap", &mut r);
        push(e.verbatim(), "entry", &mut r);
        push(e.cd_record, "cd", &mut r);
    }
    push(z.gap_before_cd(), "gap-before-cd", &mut r);
    push(z.gap_after_cd(), "gap-after-cd", &mut r);
    if let Some(zz) = z.eocd().zip64 {
        push(zz.span, "zip64-eocd", &mut r);
    }
    if let Some(l) = z.eocd().locator {
        push(l.span, "zip64-locator", &mut r);
    }
    push(z.eocd().span, "eocd", &mut r);
    push(z.trailing(), "trailing", &mut r);
    r.sort_unstable();
    let mut cursor = 0u32;
    for &(start, end, tag) in &r {
        assert_eq!(start, cursor, "разрыв или наложение перед {tag}: {r:?}");
        cursor = end;
    }
    assert_eq!(cursor as usize, len, "хвост файла не покрыт: {r:?}");
}

// ------------------------------------------------------------ базовые ------

#[test]
fn empty_archive_is_22_bytes() {
    let (buf, _) = build(&[], &[], &[]);
    assert_eq!(buf.len(), 22, "пустой архив — это ровно один EOCD");
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.len(), 0);
    assert!(z.is_empty());
    assert!(z.prefix().is_empty());
    assert!(z.trailing().is_empty());
    assert_eq!(z.eocd().eff_entries_total, 0);
    assert_eq!(z.order_by_offset(), &[] as &[u32]);
}

#[test]
fn single_stored_entry_captures_every_field() {
    let mut it = Item::stored("hello.txt", b"hello world");
    it.made_by = 788;
    it.internal = 1;
    it.external = 0x81a4_0000;
    it.comment = b"note".to_vec();
    let (buf, lay) = build(&[], &[it.clone()], &[]);

    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.len(), 1);
    let e = z.entry(0).unwrap();
    assert_eq!(z.name(0).unwrap(), b"hello.txt");
    assert_eq!(e.method, 0);
    assert_eq!(e.flags, 0);
    assert_eq!(e.version_made_by, 788);
    assert_eq!(e.internal_attrs, 1);
    assert_eq!(e.external_attrs, 0x81a4_0000);
    assert_eq!(e.dos_time, 0x4A2B);
    assert_eq!(e.dos_date, 0x5891);
    assert_eq!(e.crc32, crc32(b"hello world"));
    assert_eq!(e.comp_size, 11);
    assert_eq!(e.uncomp_size, 11);
    assert_eq!(e.comment.slice(&buf), Some(&b"note"[..]));
    assert_eq!(e.local_header_off, u64::from(lay.local_offsets[0]));
    assert_eq!(e.local_header.start(), lay.local_offsets[0]);
    assert_eq!(e.cd_record.start(), lay.cd_offsets[0]);
    assert_eq!(z.raw_data(0).unwrap(), b"hello world");
    assert!(e.descriptor.is_none());
    assert!(e.zip64_layout.is_none());
    assert_eq!(z.index_of("hello.txt"), Some(0));
    assert_eq!(z.index_of("nope.txt"), None);
    assert_eq!(z.offset_delta(), 0);
}

#[test]
fn deflate_entry_is_not_decompressed_on_parse() {
    // Байты «сжатых» данных заведомо не образуют корректного потока deflate.
    // Разбор обязан пройти: распаковка — отдельная и ленивая операция.
    let it = Item::deflated("part.xml", &[0xDE, 0xAD, 0xBE, 0xEF], 999, 0x1234_5678);
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let e = z.entry(0).unwrap();
    assert_eq!(e.method, 8);
    assert_eq!(e.comp_size, 4);
    assert_eq!(e.uncomp_size, 999);
    assert_eq!(z.raw_data(0).unwrap(), &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn local_extra_and_cd_extra_are_independent() {
    // Главная проверка модели: MS Office кладёт 520 байт в локальный extra и
    // ноль — в каталожный. Writer, копирующий одно в другое, сломает
    // байт-идентичность на первой же записи.
    let mut it = Item::stored("[Content_Types].xml", b"<Types/>");
    let mut growth_hint = vec![0x20, 0xA2, 0x14, 0x02]; // id 0xA220, size 0x0214 = 516
    growth_hint.extend(std::iter::repeat_n(0u8, 516));
    assert_eq!(growth_hint.len(), 520);
    it.local_extra = growth_hint;
    it.cd_extra = Vec::new();

    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let e = z.entry(0).unwrap();
    assert_eq!(e.local_extra.len(), 520);
    assert_eq!(e.cd_extra.len(), 0);
    // Спан локального extra обязан указывать внутрь локального заголовка.
    assert!(e.local_extra.end() == e.local_header.end());
}

#[test]
fn version_needed_may_differ_between_headers() {
    let mut it = Item::stored("a.bin", b"x");
    it.vneed_local = 20;
    it.vneed_cd = 45;
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let e = z.entry(0).unwrap();
    assert_eq!(e.version_needed_local, 20);
    assert_eq!(e.version_needed_cd, 45);
}

#[test]
fn directory_entry_parsed() {
    let mut d = Item::stored("word/", b"");
    d.external = 0x0000_0010; // FILE_ATTRIBUTE_DIRECTORY
    let f = Item::stored("word/document.xml", b"<w/>");
    let (buf, _) = build(&[], &[d, f], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.len(), 2);
    let e = z.entry(0).unwrap();
    assert!(e.is_dir(z.src()));
    assert_eq!(e.uncomp_size, 0);
    assert_eq!(e.comp_size, 0);
    assert!(!z.entry(1).unwrap().is_dir(z.src()));
}

// ------------------------------------------------------- data descriptor ---

#[test]
fn data_descriptor_with_signature_is_16_bytes() {
    let it = Item::stored("s.txt", b"streamed").streaming(true);
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let e = z.entry(0).unwrap();
    let d = e.descriptor.expect("bit 3 обязан дать дескриптор");
    assert!(d.has_signature);
    assert!(!d.wide);
    assert_eq!(d.span.len(), 16, "PK\\x07\\x08 + 3×u32");
    assert_eq!(d.span.start(), e.data.end());
    // Значения берутся из каталога: в локальном заголовке они нулевые.
    assert_eq!(e.crc32_local, 0);
    assert_eq!(e.comp_size_local, 0);
    assert_eq!(e.uncomp_size_local, 0);
    assert_eq!(e.crc32, crc32(b"streamed"));
    assert_eq!(e.comp_size, 8);
    assert!(e.has_data_descriptor());
}

#[test]
fn data_descriptor_without_signature_is_12_bytes() {
    let it = Item::stored("s.txt", b"streamed").streaming(false);
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let d = z.entry(0).unwrap().descriptor.unwrap();
    assert!(!d.has_signature);
    assert_eq!(d.span.len(), 12);
}

#[test]
fn descriptor_span_leaves_no_gap_before_next_entry() {
    let a = Item::stored("a", b"aaaa").streaming(true);
    let b = Item::stored("b", b"bb").streaming(false);
    let (buf, _) = build(&[], &[a, b], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    // Если форма дескриптора определена верно, «дыры» перед второй записью нет.
    assert!(z.entry(1).unwrap().gap_before.is_empty());
    assert!(z.gap_before_cd().is_empty());
}

// --------------------------------------------------------------- zip64 ----

/// Собирает zip64-архив с одной stored-записью.
///
/// Маркеры `0xFFFFFFFF` стоят у обоих размеров и у офсета заголовка; номер
/// диска остаётся 16-битным, поэтому в записи `0x0001` каталога ровно три
/// элемента, а не четыре.
fn build_zip64() -> Vec<u8> {
    let name = b"z.bin";
    let data = b"hello";
    let crc = crc32(data);

    let mut out = Vec::new();
    // Локальный extra 0x0001: по APPNOTE 4.5.3 всегда оба размера.
    let mut lextra = Vec::new();
    lextra.extend_from_slice(&le16(0x0001));
    lextra.extend_from_slice(&le16(16));
    lextra.extend_from_slice(&le64(data.len() as u64));
    lextra.extend_from_slice(&le64(data.len() as u64));

    let local_off = out.len() as u64;
    out.extend_from_slice(&SIG_LOCAL);
    out.extend_from_slice(&le16(45));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0x4A2B));
    out.extend_from_slice(&le16(0x5891));
    out.extend_from_slice(&le32(crc));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(&le16(name.len() as u16));
    out.extend_from_slice(&le16(lextra.len() as u16));
    out.extend_from_slice(name);
    out.extend_from_slice(&lextra);
    out.extend_from_slice(data);

    // Каталожный extra 0x0001: uncompressed, compressed, offset — в этом
    // порядке и только те, у кого стоит маркер.
    let mut cextra = Vec::new();
    cextra.extend_from_slice(&le16(0x0001));
    cextra.extend_from_slice(&le16(24));
    cextra.extend_from_slice(&le64(data.len() as u64));
    cextra.extend_from_slice(&le64(data.len() as u64));
    cextra.extend_from_slice(&le64(local_off));

    let cd_start = out.len() as u64;
    out.extend_from_slice(&SIG_CENTRAL);
    out.extend_from_slice(&le16(45));
    out.extend_from_slice(&le16(45));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0x4A2B));
    out.extend_from_slice(&le16(0x5891));
    out.extend_from_slice(&le32(crc));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(&le16(name.len() as u16));
    out.extend_from_slice(&le16(cextra.len() as u16));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le32(0));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(name);
    out.extend_from_slice(&cextra);
    let cd_size = out.len() as u64 - cd_start;

    // zip64 EOCD record: поле size считается от байта после себя, поэтому 44.
    let z64_pos = out.len() as u64;
    out.extend_from_slice(&SIG_Z64_EOCD);
    out.extend_from_slice(&le64(44));
    out.extend_from_slice(&le16(45));
    out.extend_from_slice(&le16(45));
    out.extend_from_slice(&le32(0));
    out.extend_from_slice(&le32(0));
    out.extend_from_slice(&le64(1));
    out.extend_from_slice(&le64(1));
    out.extend_from_slice(&le64(cd_size));
    out.extend_from_slice(&le64(cd_start));

    out.extend_from_slice(&SIG_Z64_LOC);
    out.extend_from_slice(&le32(0));
    out.extend_from_slice(&le64(z64_pos));
    out.extend_from_slice(&le32(1));

    out.extend_from_slice(&SIG_EOCD);
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0xFFFF));
    out.extend_from_slice(&le16(0xFFFF));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(&le32(0xFFFF_FFFF));
    out.extend_from_slice(&le16(0));
    out
}

#[test]
fn zip64_layout_records_which_elements_were_present() {
    let buf = build_zip64();
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.len(), 1);
    let e = z.entry(0).unwrap();
    assert_eq!(e.comp_size, 5);
    assert_eq!(e.uncomp_size, 5);
    assert_eq!(z.raw_data(0).unwrap(), b"hello");

    let l = e.zip64_layout.expect("zip64 обязан быть распознан");
    assert!(l.in_local, "0x0001 есть в локальном заголовке");
    assert!(l.in_cd, "0x0001 есть в каталоге");
    assert!(l.has_usize && l.has_csize && l.has_offset);
    assert!(!l.has_disk, "номер диска не был помечен маркером");
    assert_eq!(l.payload_len(), 24, "раскладка каталожной записи 0x0001");

    let eocd = z.eocd();
    assert_eq!(
        eocd.entries_total, 0xFFFF,
        "в EOCD хранится маркер как есть"
    );
    assert_eq!(eocd.eff_entries_total, 1);
    let z64 = eocd.zip64.expect("zip64 EOCD record");
    assert_eq!(z64.version_made_by, 45);
    assert_eq!(z64.version_needed, 45);
    assert_eq!(z64.record_size, 44);
    assert!(z64.extensible_data.is_empty());
    let loc = eocd.locator.expect("locator");
    assert_eq!(loc.eocd_offset, u64::from(z64.span.start()));
    assert_eq!(loc.total_disks, 1);
    assert!(z.gap_after_cd().is_empty());
}

#[test]
fn zip64_marker_without_extra_field_is_rejected() {
    let mut buf = build_zip64();
    // Стираем запись 0x0001 из каталожного extra, объявив её длину нулевой.
    // Маркеры при этом остаются — настоящие размеры взять неоткуда.
    let cd = buf
        .windows(4)
        .position(|w| w == SIG_CENTRAL)
        .expect("каталог");
    put16(&mut buf, cd + 30, 0); // extra len = 0
    assert!(ZipArchive::parse(&buf, &strict()).is_err());
}

// -------------------------------------------------------------- префикс ---

#[test]
fn prefix_before_first_local_header_is_preserved() {
    // Самораспаковывающийся архив: впереди исполняемый код. Офсеты у таких
    // архивов обычно уже абсолютны — так их пишет `zip -A`.
    let stub = b"MZ this is a fake self-extracting stub, 43 bytes...";
    let (buf, lay) = build(stub, &[Item::stored("a.txt", b"body")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.prefix().start(), 0);
    assert_eq!(z.prefix().len() as usize, stub.len());
    assert_eq!(z.prefix().slice(z.src()), Some(&stub[..]));
    assert_eq!(
        z.entry(0).unwrap().local_header.start(),
        lay.local_offsets[0]
    );
    assert_eq!(z.offset_delta(), 0);
    assert!(z.entry(0).unwrap().gap_before.is_empty());
}

#[test]
fn prefix_with_unadjusted_offsets_is_recovered() {
    // Тот же архив, но офсеты остались относительными: так получается, если
    // stub просто дописали в начало файла, не тронув заголовки.
    let stub = vec![0x7Fu8; 64];
    let (base, _) = build(&[], &[Item::stored("a.txt", b"body")], &[]);
    let mut buf = stub.clone();
    buf.extend_from_slice(&base);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.offset_delta(), 64, "поправка на префикс");
    assert_eq!(z.prefix().len(), 64);
    assert_eq!(z.raw_data(0).unwrap(), b"body");
    // Офсет хранится как записан, а не как исправлен: иначе перезапись
    // изменила бы байты каталога.
    assert_eq!(z.entry(0).unwrap().local_header_off, 0);
    assert_eq!(z.entry(0).unwrap().local_header.start(), 64);
}

// ------------------------------------------------------------ комментарий -

#[test]
fn eocd_signature_inside_archive_comment_does_not_confuse_search() {
    let mut comment = b"AAAA".to_vec();
    comment.extend_from_slice(&SIG_EOCD);
    comment.extend(std::iter::repeat_n(0u8, 40));
    let (buf, lay) = build(&[], &[Item::stored("a.txt", b"body")], &comment);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.eocd().span.start(), lay.eocd);
    assert_eq!(z.eocd().comment.slice(&buf), Some(&comment[..]));
    assert_eq!(z.len(), 1);
    assert!(z.trailing().is_empty());
}

#[test]
fn eocd_signature_inside_stored_data_does_not_confuse_search() {
    let mut payload = b"junk".to_vec();
    payload.extend_from_slice(&SIG_EOCD);
    payload.extend(std::iter::repeat_n(0u8, 32));
    let (buf, lay) = build(&[], &[Item::stored("a.bin", &payload)], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.eocd().span.start(), lay.eocd);
    assert_eq!(z.raw_data(0).unwrap(), &payload[..]);
}

// ------------------------------------------------------------------ дыры --

#[test]
fn gaps_between_entries_are_captured() {
    let a = Item::stored("a.txt", b"aaa");
    let mut b = Item::stored("b.txt", b"bbb");
    b.gap = vec![0xCC; 17];
    let (buf, _) = build(&[], &[a, b], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert!(z.entry(0).unwrap().gap_before.is_empty());
    let g = z.entry(1).unwrap().gap_before;
    assert_eq!(g.len(), 17);
    assert_eq!(g.slice(&buf), Some(&[0xCCu8; 17][..]));
}

#[test]
fn order_by_offset_is_stored_separately_from_directory_order() {
    let (mut buf, lay) = build(
        &[],
        &[Item::stored("a", b"aaaa"), Item::stored("b", b"bbbb")],
        &[],
    );
    // Меняем местами офсеты в каталоге, оставив сами записи на месте: каталог
    // теперь идёт в порядке, обратном физическому.
    let (c0, c1) = (lay.cd_offsets[0] as usize, lay.cd_offsets[1] as usize);
    let (o0, o1) = (lay.local_offsets[0], lay.local_offsets[1]);
    put32(&mut buf, c0 + 42, o1);
    put32(&mut buf, c1 + 42, o0);
    // Имена в каталоге тоже надо поменять местами, иначе сработает NameMismatch.
    buf[c0 + 46] = b'b';
    buf[c1 + 46] = b'a';

    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.name(0).unwrap(), b"b");
    assert_eq!(z.name(1).unwrap(), b"a");
    assert_eq!(z.order_by_offset(), &[1, 0]);
}

// --------------------------------------------------------------- имена ----

#[test]
fn name_str_decodes_cp437_without_bit11() {
    let mut it = Item::stored("x", b"x");
    it.name = vec![0x81, 0xE1, b'.', b't', b'x', b't']; // üß.txt в CP437
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.name_str(0).unwrap(), "üß.txt");
}

#[test]
fn name_str_decodes_utf8_with_bit11() {
    let mut it = Item::stored("x", b"x");
    it.name = "щит.xml".as_bytes().to_vec();
    it.flags = 0x0800;
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.name_str(0).unwrap(), "щит.xml");
    assert!(z.entry(0).unwrap().is_utf8());
}

// ---------------------------------------------------------------- атаки ---

#[test]
fn overlapping_entries_rejected() {
    // Две каталожные записи с одинаковым именем указывают на один и тот же
    // локальный заголовок: диапазоны пересекаются, и читатель, идущий по
    // каталогу, увидел бы файл дважды.
    let (mut buf, lay) = build(
        &[],
        &[Item::stored("same", b"aaaa"), Item::stored("same", b"bbbb")],
        &[],
    );
    put32(
        &mut buf,
        lay.cd_offsets[1] as usize + 42,
        lay.local_offsets[0],
    );
    let err = ZipArchive::parse(&buf, &strict()).unwrap_err();
    assert!(
        format!("{err}").contains("Overlapping"),
        "ожидалось OverlappingEntries, получено {err}"
    );
}

#[test]
fn cd_offset_out_of_bounds_rejected() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put32(&mut buf, lay.eocd as usize + 16, 0x00FF_FFFF);
    assert!(ZipArchive::parse(&buf, &strict()).is_err());
}

#[test]
fn cd_size_out_of_bounds_rejected() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put32(&mut buf, lay.eocd as usize + 12, 0x00FF_FFFF);
    assert!(ZipArchive::parse(&buf, &strict()).is_err());
}

#[test]
fn lying_entry_count_rejected() {
    let (mut buf, lay) = build(
        &[],
        &[Item::stored("a", b"aaaa"), Item::stored("b", b"bbbb")],
        &[],
    );
    // Заявлено больше записей, чем лежит в каталоге.
    put16(&mut buf, lay.eocd as usize + 8, 5);
    put16(&mut buf, lay.eocd as usize + 10, 5);
    assert!(ZipArchive::parse(&buf, &strict()).is_err());

    // И меньше: тогда часть каталога оказалась бы невидимой.
    let (mut buf2, lay2) = build(
        &[],
        &[Item::stored("a", b"aaaa"), Item::stored("b", b"bbbb")],
        &[],
    );
    put16(&mut buf2, lay2.eocd as usize + 8, 1);
    put16(&mut buf2, lay2.eocd as usize + 10, 1);
    let err = ZipArchive::parse(&buf2, &strict()).unwrap_err();
    assert!(
        format!("{err}").contains("EntryCountMismatch"),
        "получено {err}"
    );
}

#[test]
fn entry_count_over_limit_rejected_before_allocation() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put16(&mut buf, lay.eocd as usize + 10, 0xFFFE);
    let mut l = Limits::strict();
    l.max_entries = 4;
    let err = ZipArchive::parse(&buf, &l).unwrap_err();
    assert!(err.is_limit(), "получено {err}");
}

#[test]
fn local_name_differing_from_cd_name_is_fatal() {
    let mut it = Item::stored("innocent.txt", b"payload");
    it.local_name = Some(b"evil.exe\0\0\0\0".to_vec());
    let (buf, _) = build(&[], &[it], &[]);
    let err = ZipArchive::parse(&buf, &strict()).unwrap_err();
    assert!(format!("{err}").contains("NameMismatch"), "получено {err}");
}

#[test]
fn encrypted_entry_rejected() {
    let mut it = Item::stored("a", b"aaaa");
    it.flags = 0x0001;
    let (buf, _) = build(&[], &[it], &[]);
    let err = ZipArchive::parse(&buf, &strict()).unwrap_err();
    assert!(format!("{err}").contains("Encrypted"), "получено {err}");

    let mut it2 = Item::stored("a", b"aaaa");
    it2.flags = 0x0040; // strong encryption
    let (buf2, _) = build(&[], &[it2], &[]);
    assert!(ZipArchive::parse(&buf2, &strict()).is_err());
}

#[test]
fn unsupported_method_rejected() {
    let mut it = Item::stored("a", b"aaaa");
    it.method = 12; // bzip2
    let (buf, _) = build(&[], &[it], &[]);
    let err = ZipArchive::parse(&buf, &strict()).unwrap_err();
    assert!(
        format!("{err}").contains("UnsupportedMethod"),
        "получено {err}"
    );
}

#[test]
fn multi_disk_rejected() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put16(&mut buf, lay.eocd as usize + 4, 1); // number of this disk
    let err = ZipArchive::parse(&buf, &strict()).unwrap_err();
    assert!(format!("{err}").contains("MultiDisk"), "получено {err}");
}

#[test]
fn entry_on_other_disk_rejected() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put16(&mut buf, lay.cd_offsets[0] as usize + 34, 3); // disk start
    let err = ZipArchive::parse(&buf, &strict()).unwrap_err();
    assert!(format!("{err}").contains("MultiDisk"), "получено {err}");
}

#[test]
fn data_beyond_buffer_rejected() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put32(&mut buf, lay.cd_offsets[0] as usize + 20, 0x00FF_FFFF);
    assert!(ZipArchive::parse(&buf, &strict()).is_err());
}

#[test]
fn local_header_offset_beyond_buffer_rejected() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    put32(&mut buf, lay.cd_offsets[0] as usize + 42, 0x00FF_FFFF);
    assert!(ZipArchive::parse(&buf, &strict()).is_err());
}

#[test]
fn truncation_at_every_prefix_length_is_an_error_not_a_panic() {
    let (buf, _) = build(
        &[],
        &[Item::stored("a.txt", b"aaaa"), Item::stored("b.txt", b"bb")],
        &[],
    );
    for len in 0..buf.len() {
        let cut = buf.get(..len).unwrap();
        assert!(
            ZipArchive::parse(cut, &strict()).is_err(),
            "обрезка до {len} байт неожиданно разобралась"
        );
    }
    // И на всякий случай — первые 64 позиции отдельно, как требует веха.
    for len in 0..64.min(buf.len()) {
        assert!(ZipArchive::parse(&buf[..len], &strict()).is_err());
    }
}

#[test]
fn input_larger_than_limit_rejected() {
    let (buf, _) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    let mut l = Limits::strict();
    l.max_input_bytes = 8;
    assert!(ZipArchive::parse(&buf, &l).unwrap_err().is_limit());
}

#[test]
fn part_larger_than_limit_rejected() {
    let (buf, _) = build(&[], &[Item::deflated("big", b"zz", 1 << 30, 0)], &[]);
    let mut l = Limits::strict();
    l.max_part_bytes = 1024;
    assert!(ZipArchive::parse(&buf, &l).unwrap_err().is_limit());
}

#[test]
fn no_eocd_at_all() {
    let err = ZipArchive::parse(b"not a zip file at all", &strict()).unwrap_err();
    assert!(
        format!("{err}").contains("NoEndOfCentralDirectory"),
        "получено {err}"
    );
}

#[test]
fn entry_index_out_of_range_is_error_not_panic() {
    let (buf, _) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert!(z.entry(1).is_err());
    assert!(z.name(99).is_err());
    assert!(z.raw_data(99).is_err());
    assert!(z.decompress(99, &strict()).is_err());
}

#[test]
fn stored_entry_decompresses_without_inflate() {
    // Единственный путь `decompress`, который не упирается в заглушку M1.
    let (buf, _) = build(&[], &[Item::stored("a.txt", b"plain bytes")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.decompress(0, &strict()).unwrap(), b"plain bytes");
}

#[test]
fn stored_entry_with_wrong_crc_is_caught() {
    let (mut buf, lay) = build(&[], &[Item::stored("a.txt", b"plain bytes")], &[]);
    put32(&mut buf, lay.cd_offsets[0] as usize + 16, 0xDEAD_BEEF);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let err = z.decompress(0, &strict()).unwrap_err();
    assert!(format!("{err}").contains("CrcMismatch"), "получено {err}");
}

#[test]
fn model_tiles_every_shape_of_archive() {
    // Каждая форма архива, которую умеет разбирать веха, обязана быть покрыта
    // спанами целиком. Если добавится новая структура (например, «extensible
    // data sector»), а спана для неё не заведут — упадёт здесь, а не в M3.
    let mut office = Item::stored("[Content_Types].xml", b"<Types/>");
    office.local_extra = {
        let mut v = vec![0x20, 0xA2, 0x14, 0x02];
        v.extend(std::iter::repeat_n(0u8, 516));
        v
    };
    let mut gapped = Item::stored("padded.bin", b"pad");
    gapped.gap = vec![0u8; 9];

    let shapes: Vec<(&str, Vec<u8>)> = vec![
        ("пустой", build(&[], &[], &[]).0),
        ("stored", build(&[], &[Item::stored("a", b"aaa")], &[]).0),
        (
            "поток с сигнатурой",
            build(&[], &[Item::stored("a", b"aaa").streaming(true)], &[]).0,
        ),
        (
            "поток без сигнатуры",
            build(&[], &[Item::stored("a", b"aaa").streaming(false)], &[]).0,
        ),
        ("office extra", build(&[], &[office], &[]).0),
        ("дыра", build(&[], &[gapped], &[]).0),
        (
            "префикс",
            build(b"stub bytes here", &[Item::stored("a", b"aaa")], &[]).0,
        ),
        (
            "комментарий",
            build(&[], &[Item::stored("a", b"aaa")], b"hello comment").0,
        ),
        ("zip64", build_zip64()),
    ];
    for (what, buf) in shapes {
        let z = ZipArchive::parse(&buf, &strict())
            .unwrap_or_else(|e| panic!("{what} не разобрался: {e}"));
        assert_tiles_whole_file(&z, buf.len());
    }
}

#[test]
fn trailing_bytes_after_eocd_are_not_lost() {
    // Дописанный в конец мусор — не повод отказать в открытии, но и не повод
    // молча его выбросить: он часть файла.
    let (mut buf, _) = build(&[], &[Item::stored("a", b"aaa")], &[]);
    buf.extend_from_slice(b"appended junk");
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.trailing().slice(&buf), Some(&b"appended junk"[..]));
    assert_tiles_whole_file(&z, buf.len());
}

// --------------------------------------------------------------- фаззер ---

/// SplitMix64 — детерминированный PRNG в пятнадцать строк.
///
/// Свой, потому что зависимостей нет, а `std` не даёт генератора. Качество
/// распределения здесь второстепенно: важно, чтобы прогон был воспроизводим и
/// падение можно было повторить по номеру итерации.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const FUZZ_BASE: u64 = 0x5DEE_CE66_D0DA_1234;

fn fuzz_iters() -> u64 {
    std::env::var("OOXML_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

fn seed_for(i: u64) -> u64 {
    FUZZ_BASE ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn random_bytes(state: &mut u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        v.extend_from_slice(&splitmix64(state).to_le_bytes());
    }
    v.truncate(len);
    v
}

#[test]
fn fuzz_random_buffers_never_parse_and_never_panic() {
    let limits = strict();
    for i in 0..fuzz_iters() {
        let mut s = seed_for(i);
        let len = (splitmix64(&mut s) % 600) as usize;
        let buf = random_bytes(&mut s, len);
        assert!(
            ZipArchive::parse(&buf, &limits).is_err(),
            "случайный буфер разобрался на итерации {i}"
        );
    }
}

#[test]
fn fuzz_mutated_valid_archive_never_panics() {
    // Мутации валидного архива куда опаснее случайного шума: они попадают в
    // ветки разбора, до которых шум не доходит. Здесь допустим любой исход,
    // кроме паники.
    let (base, _) = build(
        &[],
        &[
            Item::stored("a.txt", b"aaaaaaaa"),
            Item::deflated("b.xml", b"\x01\x02\x03\x04", 40, 7),
            Item::stored("c/", b"").streaming(true),
        ],
        b"comment",
    );
    let limits = strict();
    for i in 0..fuzz_iters() {
        let mut s = seed_for(i ^ 0xAAAA_AAAA);
        let mut buf = base.clone();
        let flips = 1 + (splitmix64(&mut s) % 6) as usize;
        for _ in 0..flips {
            let at = (splitmix64(&mut s) % buf.len() as u64) as usize;
            buf[at] ^= (splitmix64(&mut s) & 0xFF) as u8;
        }
        let _ = ZipArchive::parse(&buf, &limits);
    }
}

#[test]
fn fuzz_random_truncations_never_panic() {
    let (base, _) = build(
        &[],
        &[Item::stored("a.txt", b"aaaaaaaa").streaming(true)],
        b"cmt",
    );
    let limits = strict();
    for i in 0..fuzz_iters() {
        let mut s = seed_for(i ^ 0x5555_5555);
        let cut = (splitmix64(&mut s) % (base.len() as u64 + 1)) as usize;
        let _ = ZipArchive::parse(&base[..cut], &limits);
    }
}
