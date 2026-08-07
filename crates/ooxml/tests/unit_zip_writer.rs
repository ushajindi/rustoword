//! Единичные проверки ZIP-writer'а на рукотворных архивах.
//!
//! Корпус (`corpus_repack.rs`) отвечает на вопрос «работает ли». Здесь ответ
//! на вопрос «что именно сломалось, если не работает»: каждый тест изолирует
//! одну особенность формата, из-за которой байтовая идентичность может
//! потеряться. Все они измерены на реальных файлах и перечислены в
//! `docs/zip-fidelity.md`.
//!
//! Отдельный сюжет — свойства, которых в корпусе нет ни разу: префикс,
//! зазоры, zip64, порядок каталога, отличный от физического. Корпус их не
//! проверит никогда, а формат допускает, и молча потерять их нельзя.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::bytes::Writer;
use ooxml::deflate::Level;
use ooxml::hash::crc32;
use ooxml::zip::{EntrySource, WriteOptions, ZipArchive, ZipWriter, repack_all_verbatim};

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

/// Описание одной записи для сборщика тестовых архивов.
#[derive(Clone, Debug)]
struct Item {
    name: Vec<u8>,
    /// Уже «сжатые» байты; при method = 0 это и есть содержимое.
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
    /// DOS-время и дата сырыми `u16` — в том числе заведомо невалидные.
    dos: (u16, u16),
    descriptor_sig: bool,
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
            // 0x0000/0x0000 — «1980, месяц 0, день 0»: так помечены 296 записей
            // корпуса. Хранится и пишется сырым u16, без календарной арифметики.
            dos: (0, 0),
            descriptor_sig: true,
            gap: Vec::new(),
        }
    }

    fn streaming(mut self, with_sig: bool) -> Self {
        self.flags |= 0x0008;
        self.descriptor_sig = with_sig;
        self
    }
}

#[derive(Debug, Default)]
struct Layout {
    local_offsets: Vec<u32>,
    cd_offsets: Vec<u32>,
    cd_start: u32,
}

fn build(prefix: &[u8], items: &[Item], comment: &[u8]) -> (Vec<u8>, Layout) {
    let mut out = prefix.to_vec();
    let mut lay = Layout::default();

    for it in items {
        out.extend_from_slice(&it.gap);
        lay.local_offsets.push(out.len() as u32);
        let streaming = it.flags & 0x0008 != 0;
        out.extend_from_slice(&SIG_LOCAL);
        out.extend_from_slice(&le16(it.vneed_local));
        out.extend_from_slice(&le16(it.flags));
        out.extend_from_slice(&le16(it.method));
        out.extend_from_slice(&le16(it.dos.0));
        out.extend_from_slice(&le16(it.dos.1));
        out.extend_from_slice(&le32(if streaming { 0 } else { it.crc }));
        out.extend_from_slice(&le32(if streaming { 0 } else { it.data.len() as u32 }));
        out.extend_from_slice(&le32(if streaming { 0 } else { it.uncomp }));
        out.extend_from_slice(&le16(it.name.len() as u16));
        out.extend_from_slice(&le16(it.local_extra.len() as u16));
        out.extend_from_slice(&it.name);
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
        out.extend_from_slice(&le16(it.dos.0));
        out.extend_from_slice(&le16(it.dos.1));
        out.extend_from_slice(&le32(it.crc));
        out.extend_from_slice(&le32(it.data.len() as u32));
        out.extend_from_slice(&le32(it.uncomp));
        out.extend_from_slice(&le16(it.name.len() as u16));
        out.extend_from_slice(&le16(it.cd_extra.len() as u16));
        out.extend_from_slice(&le16(it.comment.len() as u16));
        out.extend_from_slice(&le16(0));
        out.extend_from_slice(&le16(it.internal));
        out.extend_from_slice(&le32(it.external));
        out.extend_from_slice(&le32(off));
        out.extend_from_slice(&it.name);
        out.extend_from_slice(&it.cd_extra);
        out.extend_from_slice(&it.comment);
    }
    let cd_size = out.len() as u32 - lay.cd_start;

    out.extend_from_slice(&SIG_EOCD);
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(0));
    out.extend_from_slice(&le16(items.len() as u16));
    out.extend_from_slice(&le16(items.len() as u16));
    out.extend_from_slice(&le32(cd_size));
    out.extend_from_slice(&le32(lay.cd_start));
    out.extend_from_slice(&le16(comment.len() as u16));
    out.extend_from_slice(comment);
    (out, lay)
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&le32(v));
}

fn strict() -> Limits {
    Limits::strict()
}

/// Позиция и значения первого разошедшегося байта — чтобы провал теста был
/// диагнозом, а не констатацией.
fn assert_identical(want: &[u8], got: &[u8], what: &str) {
    if want == got {
        return;
    }
    let n = want.len().min(got.len());
    match (0..n).find(|&i| want[i] != got[i]) {
        Some(i) => panic!(
            "{what}: байт {i} (0x{i:X}) из {}: было 0x{:02X}, стало 0x{:02X}",
            want.len(),
            want[i],
            got[i]
        ),
        None => panic!(
            "{what}: длина разошлась — было {}, стало {}",
            want.len(),
            got.len()
        ),
    }
}

/// Пересборка исходного буфера через `repack_all_verbatim`.
fn repack(buf: &[u8]) -> Vec<u8> {
    let z = ZipArchive::parse(buf, &strict()).unwrap();
    repack_all_verbatim(&z).unwrap()
}

// ------------------------------------------------------------------ базовое --

#[test]
fn empty_archive_is_a_lone_eocd() {
    let (buf, _) = build(&[], &[], &[]);
    assert_eq!(buf.len(), 22);
    assert_identical(&buf, &repack(&buf), "пустой архив");

    // И собранный с нуля пустой архив — тоже ровно 22 байта.
    let scratch = ZipWriter::new(WriteOptions::default()).finish().unwrap();
    assert_eq!(scratch.len(), 22, "пустой архив — это один EOCD");
    let z = ZipArchive::parse(&scratch, &strict()).unwrap();
    assert_eq!(z.len(), 0);
}

#[test]
fn single_stored_entry_round_trips() {
    let (buf, _) = build(&[], &[Item::stored("a.txt", b"body")], &[]);
    assert_identical(&buf, &repack(&buf), "одна stored-запись");
}

#[test]
fn every_preserved_field_survives_round_trip() {
    // Одна запись, у которой каждое «копируемое» поле отличается от значения
    // по умолчанию: version made by, обе version needed, флаги, атрибуты,
    // комментарий записи, обе области extra и невалидная DOS-дата.
    let mut it = Item::stored("x/y.bin", b"contents");
    it.method = 8;
    it.made_by = 788; // 0x0314 — Unix-хост, ZIP 2.0
    it.vneed_local = 45;
    it.vneed_cd = 20;
    it.flags = 0x0806; // биты 1-2 (подсказка уровня) + бит 11 (UTF-8)
    it.internal = 1;
    it.external = 0x81A4_0000; // Unix 0644
    it.comment = b"note".to_vec();
    it.local_extra = {
        // 0xA220 Open Packaging Growth Hint: 36 байт, как у MS Office.
        let mut v = Vec::new();
        v.extend_from_slice(&le16(0xA220));
        v.extend_from_slice(&le16(32));
        v.extend_from_slice(&le16(0xA028));
        v.extend_from_slice(&le16(32));
        v.extend(std::iter::repeat_n(0u8, 28));
        v
    };
    it.cd_extra = Vec::new(); // ≠ local extra: ровно как в корпусе
    it.dos = (0x307B, 0x5C4A);
    let (buf, _) = build(&[], &[it], &[]);

    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let e = z.entry(0).unwrap();
    assert_eq!(e.local_extra.len(), 36);
    assert!(e.cd_extra.is_empty(), "область extra каталога пуста");
    assert_identical(
        &buf,
        &repack_all_verbatim(&z).unwrap(),
        "запись со всеми полями",
    );
}

#[test]
fn archive_comment_and_trailing_bytes_are_kept() {
    let (mut buf, _) = build(&[], &[Item::stored("a", b"aa")], b"comment here");
    buf.extend_from_slice(b"tail-after-eocd");
    assert_identical(&buf, &repack(&buf), "комментарий и хвост");
}

// ------------------------------------------------------------------ префикс --

#[test]
fn prefix_is_kept_or_dropped_on_demand() {
    let stub = b"MZ fake self-extracting stub";
    let items = [Item::stored("a.txt", b"body")];
    let (with_prefix, _) = build(stub, &items, &[]);
    let (without_prefix, _) = build(&[], &items, &[]);

    let z = ZipArchive::parse(&with_prefix, &strict()).unwrap();
    assert_eq!(z.prefix().len() as usize, stub.len());

    // keep_prefix = true (по умолчанию): байт в байт.
    assert_identical(&with_prefix, &repack(&with_prefix), "префикс сохранён");

    // keep_prefix = false: префикс выброшен, офсеты пересчитаны. Результат
    // обязан совпасть с тем же архивом, собранным без префикса, — иначе
    // «выброшен» означало бы «остались висячие офсеты».
    let mut w = ZipWriter::new(WriteOptions {
        keep_prefix: false,
        ..WriteOptions::default()
    });
    w.push(EntrySource::Verbatim { src: &z, index: 0 }).unwrap();
    assert_identical(&without_prefix, &w.finish().unwrap(), "префикс выброшен");
}

#[test]
fn unadjusted_offsets_of_sfx_archive_stay_unadjusted() {
    // Stub просто дописан в начало файла: офсеты в каталоге остались
    // относительными. Пересчитать их в абсолютные — значит изменить байты
    // архива, который до этого прекрасно открывался.
    let (base, _) = build(&[], &[Item::stored("a.txt", b"body")], &[]);
    let mut buf = vec![0x7Fu8; 64];
    buf.extend_from_slice(&base);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.offset_delta(), 64, "поправка на префикс");
    assert_identical(&buf, &repack_all_verbatim(&z).unwrap(), "SFX с поправкой");
}

#[test]
fn explicit_prefix_replaces_the_original_one() {
    let (buf, _) = build(b"OLD-STUB", &[Item::stored("a", b"aa")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let mut w = ZipWriter::new(WriteOptions::default());
    w.set_prefix(b"NEW-PREFIX-LONGER");
    w.push(EntrySource::Verbatim { src: &z, index: 0 }).unwrap();
    let out = w.finish().unwrap();
    assert!(out.starts_with(b"NEW-PREFIX-LONGER"));

    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    assert_eq!(z2.prefix().len(), 17);
    assert_eq!(z2.raw_data(0).unwrap(), b"aa");
    assert_eq!(z2.offset_delta(), 0, "офсеты стали абсолютными");
}

// -------------------------------------------------------------------- зазоры -

#[test]
fn gaps_between_entries_are_preserved() {
    let a = Item::stored("a.txt", b"aaa");
    let mut b = Item::stored("b.txt", b"bbb");
    b.gap = vec![0xCC; 17];
    let (buf, _) = build(&[], &[a, b], &[]);
    assert_identical(&buf, &repack(&buf), "зазор перед записью");
}

#[test]
fn gaps_are_dropped_only_when_asked() {
    let a = Item::stored("a.txt", b"aaa");
    let mut b = Item::stored("b.txt", b"bbb");
    b.gap = vec![0xCC; 17];
    let (buf, _) = build(&[], &[a.clone(), b], &[]);
    let (tight, _) = build(&[], &[a, Item::stored("b.txt", b"bbb")], &[]);

    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let mut w = ZipWriter::new(WriteOptions {
        keep_gaps: false,
        ..WriteOptions::default()
    });
    for index in 0..z.len() {
        w.push(EntrySource::Verbatim { src: &z, index }).unwrap();
    }
    // Зазор убран — все последующие офсеты и каталог сдвинулись на 17 байт.
    assert_identical(&tight, &w.finish().unwrap(), "зазор выброшен");
}

#[test]
fn gap_before_the_directory_is_preserved() {
    let (mut buf, lay) = build(&[], &[Item::stored("a", b"aaaa")], &[]);
    // Вставляем 9 байт между последней записью и каталогом и чиним EOCD.
    let at = lay.cd_start as usize;
    buf.splice(at..at, std::iter::repeat_n(0xEEu8, 9));
    let eocd = buf.len() - 22;
    put32(&mut buf, eocd + 16, lay.cd_start + 9);

    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.gap_before_cd().len(), 9, "зазор перед каталогом");
    assert_identical(
        &buf,
        &repack_all_verbatim(&z).unwrap(),
        "зазор перед каталогом",
    );
}

// ------------------------------------------------------------- дескрипторы --

#[test]
fn streaming_entry_keeps_zero_header_and_its_descriptor() {
    // Бит 3: crc и размеры в локальном заголовке нулевые и обязаны такими
    // остаться. «Починка» стоит 31 файла корпуса из 43.
    let it = Item::stored("s.bin", b"streamed payload").streaming(true);
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let e = z.entry(0).unwrap();
    assert_eq!(e.crc32_local, 0);
    assert_eq!(e.comp_size_local, 0);
    assert_eq!(e.uncomp_size_local, 0);
    assert_eq!(e.descriptor.unwrap().span.len(), 16);
    assert_identical(
        &buf,
        &repack_all_verbatim(&z).unwrap(),
        "дескриптор с сигнатурой",
    );
}

#[test]
fn descriptor_without_signature_is_not_given_one() {
    // APPNOTE 4.3.9.3 разрешает дескриптор без сигнатуры. Дописать её —
    // сдвинуть на 4 байта всё, что идёт следом.
    let it = Item::stored("s.bin", b"payload").streaming(false);
    let (buf, _) = build(&[], &[it, Item::stored("t.bin", b"next")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let d = z.entry(0).unwrap().descriptor.unwrap();
    assert!(!d.has_signature);
    assert_eq!(d.span.len(), 12);
    assert_identical(
        &buf,
        &repack_all_verbatim(&z).unwrap(),
        "дескриптор без сигнатуры",
    );
}

// -------------------------------------------------------------------- zip64 --

/// Архив с zip64-хвостом и записью `0x0001` в обоих заголовках.
///
/// Маркеры стоят у обоих размеров и у офсета заголовка; номер диска остаётся
/// 16-битным, поэтому в каталожной записи `0x0001` ровно три элемента.
fn build_zip64() -> Vec<u8> {
    let name = b"z.bin";
    let data = b"hello";
    let crc = crc32(data);

    let mut out = Vec::new();
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
fn zip64_layout_is_reemitted_exactly() {
    // Запись 0x0001 позиционная и переменной длины, а офсет заголовка лежит
    // внутри неё, а не в 32-битном поле каталога: там маркер, который обязан
    // остаться маркером. Плюс маркеры в EOCD и пересчёт офсета в локаторе.
    let buf = build_zip64();
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let l = z.entry(0).unwrap().zip64_layout.unwrap();
    assert!(l.in_local && l.in_cd && l.has_offset);
    assert_identical(&buf, &repack_all_verbatim(&z).unwrap(), "zip64");
}

#[test]
fn zip64_archive_with_prefix_keeps_locator_consistent() {
    // Локатор указывает на zip64 EOCD record; при сдвиге всего архива его
    // офсет обязан поехать вместе с записью, иначе хвост станет битым.
    let mut buf = vec![0x11u8; 32];
    buf.extend_from_slice(&build_zip64());
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let out = repack_all_verbatim(&z).unwrap();
    assert_identical(&buf, &out, "zip64 с префиксом");
    // И пересобранный архив по-прежнему разбирается через локатор.
    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    assert_eq!(z2.raw_data(0).unwrap(), b"hello");
}

// ------------------------------------------------------------------ порядок --

#[test]
fn directory_order_differing_from_physical_order_is_preserved() {
    let (mut buf, lay) = build(
        &[],
        &[Item::stored("a", b"aaaa"), Item::stored("b", b"bbbb")],
        &[],
    );
    // Меняем местами офсеты и имена в каталоге: каталог теперь идёт в
    // порядке, обратном физическому. Данные при этом не двигались.
    let (c0, c1) = (lay.cd_offsets[0] as usize, lay.cd_offsets[1] as usize);
    let (o0, o1) = (lay.local_offsets[0], lay.local_offsets[1]);
    put32(&mut buf, c0 + 42, o1);
    put32(&mut buf, c1 + 42, o0);
    buf[c0 + 46] = b'b';
    buf[c1 + 46] = b'a';

    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    assert_eq!(z.order_by_offset(), &[1, 0], "порядки обязаны разойтись");
    let out = repack_all_verbatim(&z).unwrap();
    assert_identical(&buf, &out, "обратный порядок каталога");

    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    assert_eq!(z2.name(0).unwrap(), b"b", "первая запись каталога");
    assert_eq!(
        z2.order_by_offset(),
        &[1, 0],
        "физический порядок не тронут"
    );
}

#[test]
fn pushing_entries_in_a_new_directory_order_moves_only_the_directory() {
    // Порядок push — это порядок каталога. Данные при этом остаются на своих
    // физических местах: их порядок берётся из исходных офсетов.
    let (buf, _) = build(
        &[],
        &[Item::stored("a", b"aaaa"), Item::stored("b", b"bbbb")],
        &[],
    );
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let mut w = ZipWriter::new(WriteOptions::default());
    w.push(EntrySource::Verbatim { src: &z, index: 1 }).unwrap();
    w.push(EntrySource::Verbatim { src: &z, index: 0 }).unwrap();
    let out = w.finish().unwrap();

    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    assert_eq!(z2.name(0).unwrap(), b"b");
    assert_eq!(z2.name(1).unwrap(), b"a");
    assert_eq!(z2.order_by_offset(), &[1, 0], "данные не переставлялись");
    // Область данных не изменилась ни на байт — переехал только каталог.
    let cd = z2.eocd().cd_start_abs as usize;
    assert_eq!(&out[..cd], &buf[..z.eocd().cd_start_abs as usize]);
}

// ------------------------------------------------------- изменённые записи --

#[test]
fn replace_rewrites_only_crc_sizes_and_data() {
    let mut it = Item::stored("a.txt", b"old body");
    it.local_extra = {
        let mut v = Vec::new();
        v.extend_from_slice(&le16(0xA220));
        v.extend_from_slice(&le16(4));
        v.extend_from_slice(&le16(0xA028));
        v.extend_from_slice(&le16(4));
        v
    };
    it.made_by = 788;
    it.external = 0x81A4_0000;
    it.dos = (0x307B, 0x5C4A);
    let (buf, _) = build(&[], &[it, Item::stored("b.txt", b"second")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();

    let mut w = ZipWriter::new(WriteOptions::default());
    w.push(EntrySource::Replace {
        src: &z,
        index: 0,
        data: b"a much longer new body".to_vec(),
        level: Level::Store,
    })
    .unwrap();
    w.push(EntrySource::Verbatim { src: &z, index: 1 }).unwrap();
    let out = w.finish().unwrap();

    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    let e = z2.entry(0).unwrap();
    assert_eq!(z2.raw_data(0).unwrap(), b"a much longer new body");
    assert_eq!(e.crc32, crc32(b"a much longer new body"));
    assert_eq!(e.uncomp_size, 22);
    // Всё, что не зависит от содержимого, осталось прежним.
    assert_eq!(e.version_made_by, 788);
    assert_eq!(e.external_attrs, 0x81A4_0000);
    assert_eq!(e.dos_time, 0x307B);
    assert_eq!(e.dos_date, 0x5C4A);
    assert_eq!(e.local_extra.len(), 8, "growth hint не потерян");
    assert!(e.cd_extra.is_empty(), "и не продублирован в каталог");
    // Соседняя запись сдвинулась, но байт в байт та же.
    assert_eq!(z2.raw_data(1).unwrap(), b"second");
    assert_eq!(z2.entry(1).unwrap().crc32, crc32(b"second"));
}

#[test]
fn replace_keeps_the_data_descriptor_form() {
    // §5: у записи с битом 3 бит остаётся, дескриптор остаётся, а нули в
    // локальном заголовке остаются нулями — меняются только данные.
    let it = Item::stored("s.bin", b"old").streaming(true);
    let (buf, _) = build(&[], &[it], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();

    let mut w = ZipWriter::new(WriteOptions::default());
    w.push(EntrySource::Replace {
        src: &z,
        index: 0,
        data: b"brand new payload".to_vec(),
        level: Level::Store,
    })
    .unwrap();
    let out = w.finish().unwrap();

    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    let e = z2.entry(0).unwrap();
    assert_eq!(e.flags & 0x0008, 0x0008, "бит 3 не снят");
    assert_eq!(e.crc32_local, 0, "локальный crc остался нулём");
    assert_eq!(e.comp_size_local, 0);
    assert_eq!(e.uncomp_size_local, 0);
    let d = e.descriptor.unwrap();
    assert!(d.has_signature);
    assert_eq!(d.span.len(), 16);
    assert_eq!(z2.raw_data(0).unwrap(), b"brand new payload");
}

#[test]
fn new_entry_is_appended_after_the_existing_ones() {
    let (buf, _) = build(&[], &[Item::stored("a.txt", b"aaa")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();

    let mut w = ZipWriter::new(WriteOptions::default());
    w.push(EntrySource::Verbatim { src: &z, index: 0 }).unwrap();
    w.push(EntrySource::New {
        name: "docProps/custom.xml".to_owned(),
        data: b"<x/>".to_vec(),
        level: Level::Store,
        dos: (0x1234, 0x5C4A),
        external_attrs: 0x20,
    })
    .unwrap();
    let out = w.finish().unwrap();

    let z2 = ZipArchive::parse(&out, &strict()).unwrap();
    assert_eq!(z2.len(), 2);
    assert_eq!(z2.name(1).unwrap(), b"docProps/custom.xml");
    assert_eq!(z2.raw_data(1).unwrap(), b"<x/>");
    let e = z2.entry(1).unwrap();
    assert_eq!(
        e.dos_time, 0x1234,
        "DOS-время взято из аргумента, не из часов"
    );
    assert_eq!(e.dos_date, 0x5C4A);
    assert_eq!(e.external_attrs, 0x20);
    assert_eq!(e.method, 0, "Level::Store — это метод 0");
    assert_eq!(z2.order_by_offset(), &[0, 1], "новая запись в конце");
    // Старая запись не сдвинулась ни на байт.
    assert_eq!(
        &out[..z.eocd().cd_start_abs as usize],
        &buf[..z.eocd().cd_start_abs as usize]
    );
}

#[test]
fn non_ascii_new_name_gets_the_utf8_flag() {
    let mut w = ZipWriter::new(WriteOptions::default());
    w.push(EntrySource::New {
        name: "документ.xml".to_owned(),
        data: b"x".to_vec(),
        level: Level::Store,
        dos: (0, 0),
        external_attrs: 0,
    })
    .unwrap();
    let out = w.finish().unwrap();
    let z = ZipArchive::parse(&out, &strict()).unwrap();
    assert!(z.entry(0).unwrap().is_utf8(), "bit 11 обязан стоять");
    assert_eq!(z.name_str(0).unwrap(), "документ.xml");
}

// ------------------------------------------------------------------ отказы --

#[test]
fn pushing_a_nonexistent_index_is_an_error_not_a_panic() {
    let (buf, _) = build(&[], &[Item::stored("a", b"aa")], &[]);
    let z = ZipArchive::parse(&buf, &strict()).unwrap();
    let mut w = ZipWriter::new(WriteOptions::default());
    assert!(w.push(EntrySource::Verbatim { src: &z, index: 7 }).is_err());
}

#[test]
fn foreign_patch_is_rejected_instead_of_corrupting_output() {
    // Отложенные поля адресуются офсетом внутри своего буфера. Патч из
    // чужого `Writer` попал бы не туда — вместо этого ошибка.
    let mut a = Writer::new();
    let mut b = Writer::new();
    a.bytes(&[0u8; 64]);
    let p = a.reserve_u32();
    assert!(b.patch_u32(p, 1).is_err(), "патч из чужого writer'а");
    // И патч неверной ширины тоже.
    let wide = a.reserve_u64();
    assert!(a.patch_u32(wide, 1).is_err(), "патч не той ширины");
}
