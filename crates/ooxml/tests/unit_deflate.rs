//! Known-answer тесты распаковщика DEFLATE и мини-фаззер.
//!
//! Зависимостей у крейта нет, поэтому эталонные потоки собираются здесь же
//! битовым писателем: он умеет ровно то, что нужно тестам, — писать биты
//! LSB-first и коды Хаффмана старшим битом вперёд.

// В тестах паника — это способ сообщить о провале, а не дефект.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::deflate::inflate;
use ooxml::error::{DeflateError, Error};
use ooxml::limits::Limits;

// ---------------------------------------------------------------------------
// Сборка потоков
// ---------------------------------------------------------------------------

/// Писатель битов LSB-first — зеркало ридера распаковщика.
struct Bw {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bw {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            n: 0,
        }
    }

    /// Пишет `n` младших бит `v` в порядке от младшего к старшему.
    fn bits(&mut self, v: u32, n: u32) {
        self.acc |= u64::from(v & ((1u64 << n) - 1) as u32) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Пишет код Хаффмана — он идёт в поток старшим битом кода вперёд.
    fn code(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.bits((code >> i) & 1, 1);
        }
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    fn raw(&mut self, data: &[u8]) {
        self.align();
        self.out.extend_from_slice(data);
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

/// Заголовок и тело stored-блока (BTYPE=00).
fn stored(bw: &mut Bw, bfinal: bool, data: &[u8]) {
    bw.bits(u32::from(bfinal), 1);
    bw.bits(0, 2);
    bw.align();
    let len = u16::try_from(data.len()).unwrap();
    bw.raw(&len.to_le_bytes());
    bw.raw(&(!len).to_le_bytes());
    bw.raw(data);
}

/// Код фиксированного дерева литералов/длин (RFC 1951 §3.2.6).
fn fixed_lit(sym: u16) -> (u32, u32) {
    let s = u32::from(sym);
    match sym {
        0..=143 => (0x30 + s, 8),
        144..=255 => (0x190 + s - 144, 9),
        256..=279 => (s - 256, 7),
        _ => (0xC0 + s - 280, 8),
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// (символ, значение доп. бит, число доп. бит) для длины совпадения.
fn length_code(len: u32) -> (u16, u32, u32) {
    for i in (0..29).rev() {
        let base = u32::from(LENGTH_BASE[i]);
        if len >= base {
            return (257 + i as u16, len - base, u32::from(LENGTH_EXTRA[i]));
        }
    }
    panic!("длина {len} вне диапазона 3..=258");
}

/// (символ, значение доп. бит, число доп. бит) для расстояния.
fn dist_code(dist: u32) -> (u16, u32, u32) {
    for i in (0..30).rev() {
        let base = u32::from(DIST_BASE[i]);
        if dist >= base {
            return (i as u16, dist - base, u32::from(DIST_EXTRA[i]));
        }
    }
    panic!("расстояние {dist} вне диапазона 1..=32768");
}

/// Литерал в фиксированном дереве.
fn fx_lit(bw: &mut Bw, byte: u8) {
    let (c, l) = fixed_lit(u16::from(byte));
    bw.code(c, l);
}

/// Совпадение LZ77 в фиксированном дереве.
fn fx_match(bw: &mut Bw, len: u32, dist: u32) {
    let (sym, ev, eb) = length_code(len);
    let (c, l) = fixed_lit(sym);
    bw.code(c, l);
    bw.bits(ev, eb);
    let (dsym, dev, deb) = dist_code(dist);
    bw.code(u32::from(dsym), 5);
    bw.bits(dev, deb);
}

fn fx_end(bw: &mut Bw) {
    let (c, l) = fixed_lit(256);
    bw.code(c, l);
}

/// Блок с фиксированными деревьями из готового набора литералов.
fn fixed_literals(bfinal: bool, data: &[u8]) -> Vec<u8> {
    let mut bw = Bw::new();
    bw.bits(u32::from(bfinal), 1);
    bw.bits(1, 2);
    for &b in data {
        fx_lit(&mut bw, b);
    }
    fx_end(&mut bw);
    bw.finish()
}

/// Динамический блок, в котором дерево литералов/длин состоит ровно из двух
/// кодов по одному биту (256 и 285), а дерево расстояний — из `ndist` кодов
/// по одному биту.
///
/// Такой блок кодирует одно совпадение длиной 258 всего двумя битами; он же
/// служит и заготовкой zip-бомбы, и проверкой дерева расстояний из одного
/// кода длины 1 (`ndist == 1`) — той самой законной неполной формы.
fn dynamic_rle_block(bw: &mut Bw, bfinal: bool, ndist: u32, matches: u32) {
    bw.bits(u32::from(bfinal), 1);
    bw.bits(2, 2);

    bw.bits(286 - 257, 5); // HLIT
    bw.bits(ndist - 1, 5); // HDIST
    bw.bits(18 - 4, 4); // HCLEN

    // Алфавит длин кодовых длин: символы 1 и 18, оба по одному биту.
    // Позиция 2 в перестановке — символ 18, позиция 17 — символ 1.
    for i in 0..18 {
        let v = u32::from(i == 2 || i == 17);
        bw.bits(v, 3);
    }
    // Канонические коды: 1 → «0», 18 → «1».
    let put_zeros = |bw: &mut Bw, n: u32| {
        bw.code(1, 1);
        bw.bits(n - 11, 7);
    };
    let put_one = |bw: &mut Bw| bw.code(0, 1);

    put_zeros(bw, 138); // символы 0..137
    put_zeros(bw, 118); // символы 138..255
    put_one(bw); // символ 256 — конец блока
    put_zeros(bw, 28); // символы 257..284
    put_one(bw); // символ 285 — длина 258
    for _ in 0..ndist {
        put_one(bw);
    }

    for _ in 0..matches {
        bw.code(1, 1); // символ 285: длина 258, доп. бит нет
        bw.code(0, 1); // расстояние 1, доп. бит нет
    }
    bw.code(0, 1); // конец блока
}

fn ok(stream: &[u8]) -> Vec<u8> {
    match inflate(stream, &Limits::strict()) {
        Ok(v) => v,
        Err(e) => panic!("ожидалась успешная распаковка, получено {e}"),
    }
}

fn kind_of(stream: &[u8]) -> DeflateError {
    match inflate(stream, &Limits::strict()) {
        Ok(v) => panic!("ожидалась ошибка, распаковано {} байт", v.len()),
        Err(Error::Deflate { kind, .. }) => kind,
        Err(e) => panic!("ожидалась ошибка deflate, получено {e}"),
    }
}

// ---------------------------------------------------------------------------
// Stored-блоки
// ---------------------------------------------------------------------------

#[test]
fn stored_empty_block() {
    let stream = [0x01, 0x00, 0x00, 0xFF, 0xFF];
    assert!(ok(&stream).is_empty());
    let mut out = Vec::new();
    assert_eq!(
        ooxml::deflate::inflate_into(&stream, &mut out, &Limits::strict()).unwrap(),
        5
    );
}

#[test]
fn stored_block_with_data() {
    let mut bw = Bw::new();
    stored(&mut bw, true, b"Hello, stored!");
    let stream = bw.finish();
    assert_eq!(ok(&stream), b"Hello, stored!");
    assert_eq!(stream.len(), 1 + 4 + 14);
}

#[test]
fn several_stored_blocks_in_a_row() {
    let mut bw = Bw::new();
    stored(&mut bw, false, b"one ");
    stored(&mut bw, false, b"");
    stored(&mut bw, false, b"two ");
    stored(&mut bw, true, b"three");
    let stream = bw.finish();
    assert_eq!(ok(&stream), b"one two three");
}

#[test]
fn stored_length_complement_must_match() {
    // LEN = 2, NLEN испорчен.
    let stream = [0x01, 0x02, 0x00, 0xFF, 0xFF, 0xAA, 0xBB];
    assert_eq!(kind_of(&stream), DeflateError::BadStoredLength);
}

#[test]
fn stored_data_shorter_than_declared() {
    let stream = [0x01, 0x08, 0x00, 0xF7, 0xFF, 0xAA];
    assert_eq!(kind_of(&stream), DeflateError::UnexpectedEof);
}

#[test]
fn stored_maximum_length_block() {
    let data: Vec<u8> = (0..65535u32).map(|i| (i % 251) as u8).collect();
    let mut bw = Bw::new();
    stored(&mut bw, true, &data);
    assert_eq!(ok(&bw.finish()), data);
}

// ---------------------------------------------------------------------------
// Фиксированные деревья Хаффмана
// ---------------------------------------------------------------------------

#[test]
fn fixed_literal_text() {
    let text = b"The quick brown fox jumps over the lazy dog";
    assert_eq!(ok(&fixed_literals(true, text)), text);
}

#[test]
fn fixed_literals_cover_all_byte_values() {
    // Байты 144..=255 кодируются девятибитными кодами — отдельная ветка
    // канонической нумерации, которую легко потерять.
    let all: Vec<u8> = (0..=255u8).collect();
    assert_eq!(ok(&fixed_literals(true, &all)), all);
}

#[test]
fn fixed_shortest_match() {
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(1, 2);
    for &b in b"abcd" {
        fx_lit(&mut bw, b);
    }
    fx_match(&mut bw, 3, 4); // минимальная длина совпадения
    fx_end(&mut bw);
    assert_eq!(ok(&bw.finish()), b"abcdabc");
}

#[test]
fn fixed_longest_match() {
    let seed: Vec<u8> = (0..258u32).map(|i| (i % 97) as u8).collect();
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(1, 2);
    for &b in &seed {
        fx_lit(&mut bw, b);
    }
    fx_match(&mut bw, 258, 258); // максимальная длина совпадения
    fx_end(&mut bw);

    let mut expect = seed.clone();
    expect.extend_from_slice(&seed);
    assert_eq!(ok(&bw.finish()), expect);
}

#[test]
fn fixed_distance_one_unrolls_as_rle() {
    // distance = 1 при length > 1 — копия читает байты, которые сама же
    // и дописывает. Классический способ закодировать повтор.
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(1, 2);
    fx_lit(&mut bw, b'x');
    fx_match(&mut bw, 258, 1);
    fx_end(&mut bw);
    let out = ok(&bw.finish());
    assert_eq!(out.len(), 259);
    assert!(out.iter().all(|&b| b == b'x'));
}

#[test]
fn fixed_overlapping_copy_of_two_byte_pattern() {
    // distance = 2, length = 7: перекрытие с периодом 2.
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(1, 2);
    fx_lit(&mut bw, b'a');
    fx_lit(&mut bw, b'b');
    fx_match(&mut bw, 7, 2);
    fx_end(&mut bw);
    assert_eq!(ok(&bw.finish()), b"ababababa");
}

#[test]
fn fixed_maximum_distance() {
    // distance = 32768 — верхняя граница окна; требует ровно столько же
    // уже распакованных байт.
    let window: Vec<u8> = (0..32768u32).map(|i| (i % 253) as u8).collect();
    let mut bw = Bw::new();
    stored(&mut bw, false, &window);
    bw.bits(1, 1);
    bw.bits(1, 2);
    fx_match(&mut bw, 3, 32768);
    fx_end(&mut bw);

    let out = ok(&bw.finish());
    assert_eq!(out.len(), 32771);
    assert_eq!(&out[..32768], &window[..]);
    assert_eq!(&out[32768..], &window[..3]);
}

#[test]
fn distance_beyond_produced_output_is_rejected() {
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(1, 2);
    fx_lit(&mut bw, b'a');
    fx_match(&mut bw, 3, 2); // распаковано всего 1 байт
    fx_end(&mut bw);
    assert_eq!(kind_of(&bw.finish()), DeflateError::DistanceTooFar);
}

#[test]
fn distance_does_not_reach_into_preexisting_output() {
    // inflate_into дописывает в непустой вектор, но окно ссылок начинается
    // с начала текущего потока — иначе запись видела бы чужие данные.
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(1, 2);
    fx_match(&mut bw, 3, 1);
    fx_end(&mut bw);
    let stream = bw.finish();

    let mut out = b"previous contents".to_vec();
    let err = ooxml::deflate::inflate_into(&stream, &mut out, &Limits::strict()).unwrap_err();
    assert!(matches!(
        err,
        Error::Deflate {
            kind: DeflateError::DistanceTooFar,
            ..
        }
    ));
}

#[test]
fn inflate_into_appends_and_reports_consumed_bytes() {
    let mut bw = Bw::new();
    stored(&mut bw, true, b"tail");
    let mut stream = bw.finish();
    let stream_len = stream.len();
    // За потоком стоит посторонний мусор — распаковщик обязан остановиться
    // на границе финального блока и сообщить, сколько байт он съел.
    stream.extend_from_slice(b"GARBAGE");

    let mut out = b"head:".to_vec();
    let used = ooxml::deflate::inflate_into(&stream, &mut out, &Limits::strict()).unwrap();
    assert_eq!(used, stream_len);
    assert_eq!(out, b"head:tail");
}

// ---------------------------------------------------------------------------
// Динамические деревья Хаффмана
// ---------------------------------------------------------------------------

/// Реальный поток из корпуса: `xl/documenttasks/documenttask1.xml`
/// файла `gsheets_01.xlsx` (запись метода 8, динамические деревья).
const REAL_DYNAMIC: [u8; 118] = [
    0x1D, 0xCB, 0xCB, 0x0D, 0xC2, 0x30, 0x0C, 0x00, 0xD0, 0x09, 0xD8, 0x21, 0xF2, 0x9D, 0xB8, 0xE5,
    0x04, 0xA8, 0x69, 0x6F, 0x4C, 0x50, 0x06, 0x88, 0x1C, 0x97, 0x46, 0x34, 0x76, 0x55, 0x07, 0x04,
    0xDB, 0xF3, 0xB9, 0x3E, 0xE9, 0x75, 0xC3, 0xAB, 0x2C, 0xEE, 0xC9, 0x9B, 0x65, 0x95, 0x00, 0xAD,
    0x6F, 0xC0, 0xB1, 0x90, 0xA6, 0x2C, 0xB7, 0x00, 0xD7, 0xF1, 0xB2, 0x3F, 0x82, 0xB3, 0x1A, 0x25,
    0xC5, 0x45, 0x85, 0x03, 0xBC, 0xD9, 0x60, 0xE8, 0x77, 0xDD, 0x18, 0xED, 0x6E, 0xEE, 0x9B, 0xC5,
    0x02, 0xCC, 0xB5, 0xAE, 0x67, 0x44, 0xA3, 0x99, 0x4B, 0x34, 0x5F, 0x32, 0x6D, 0x6A, 0x3A, 0x55,
    0x4F, 0x5A, 0x50, 0xA7, 0x29, 0x13, 0x63, 0xFD, 0x05, 0x3C, 0x34, 0xED, 0x09, 0x93, 0xD2, 0xA3,
    0xB0, 0xD4, 0x3F, 0x01, 0xF6, 0x1F,
];

const REAL_DYNAMIC_PLAIN: &[u8] =
    b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\
<Tasks xmlns=\"http://schemas.microsoft.com/office/tasks/2019/documenttasks\"/>";

#[test]
fn dynamic_stream_from_real_file() {
    let out = ok(&REAL_DYNAMIC);
    assert_eq!(out.len(), 133);
    assert_eq!(out, REAL_DYNAMIC_PLAIN);
    assert_eq!(ooxml::hash::crc32(&out), 0xAF21_16A3);
}

#[test]
fn dynamic_distance_tree_of_a_single_one_bit_code() {
    // Единственный код расстояния длиной 1 бит — неполное дерево, которое
    // спецификация допускает и которое встречается на практике.
    let mut bw = Bw::new();
    stored(&mut bw, false, b"z");
    dynamic_rle_block(&mut bw, true, 1, 2);
    let out = ok(&bw.finish());
    assert_eq!(out.len(), 1 + 2 * 258);
    assert!(out.iter().all(|&b| b == b'z'));
}

#[test]
fn dynamic_distance_tree_of_two_codes() {
    let mut bw = Bw::new();
    stored(&mut bw, false, b"z");
    dynamic_rle_block(&mut bw, true, 2, 3);
    let out = ok(&bw.finish());
    assert_eq!(out.len(), 1 + 3 * 258);
    assert!(out.iter().all(|&b| b == b'z'));
}

/// Динамический заголовок с произвольными длинами кодов алфавита длин.
fn dynamic_header_with_clen(clen: &[u8; 19]) -> Vec<u8> {
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(2, 2);
    bw.bits(0, 5); // HLIT = 257
    bw.bits(0, 5); // HDIST = 1
    bw.bits(19 - 4, 4); // HCLEN = 19
    for &v in clen {
        bw.bits(u32::from(v), 3);
    }
    bw.finish()
}

#[test]
fn oversubscribed_code_length_tree_is_rejected() {
    // Три кода по одному биту в двоичное дерево не помещаются.
    let mut clen = [0u8; 19];
    clen[0] = 1;
    clen[1] = 1;
    clen[2] = 1;
    assert_eq!(
        kind_of(&dynamic_header_with_clen(&clen)),
        DeflateError::OversubscribedTree
    );
}

#[test]
fn incomplete_code_length_tree_is_rejected() {
    // Коды длин 1 и 2 покрывают лишь три четверти пространства.
    let mut clen = [0u8; 19];
    clen[0] = 1;
    clen[1] = 2;
    assert_eq!(
        kind_of(&dynamic_header_with_clen(&clen)),
        DeflateError::IncompleteTree
    );
}

#[test]
fn repeat_previous_length_cannot_be_first() {
    // Символ 16 повторяет предыдущую длину; в самом начале повторять нечего.
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(2, 2);
    bw.bits(0, 5); // HLIT = 257
    bw.bits(0, 5); // HDIST = 1
    bw.bits(19 - 4, 4);
    // Алфавит длин: символы 16 и 17, оба по одному биту (полное дерево).
    let mut clen = [0u8; 19];
    clen[0] = 1; // позиция 0 перестановки — символ 16
    clen[1] = 1; // позиция 1 — символ 17
    for &v in &clen {
        bw.bits(u32::from(v), 3);
    }
    // Канонические коды: 16 → «0», 17 → «1». Первым идёт 16.
    bw.code(0, 1);
    bw.bits(0, 2);
    assert_eq!(
        kind_of(&bw.finish()),
        DeflateError::BadCodeLengthRepeat,
        "повтор в самом начале массива длин недопустим"
    );
}

#[test]
fn code_length_repeat_past_end_is_rejected() {
    // HLIT = 257, HDIST = 1 — всего 258 длин. Пятнадцать повторов по 138
    // нулей заведомо вылезают за массив.
    let mut bw = Bw::new();
    bw.bits(1, 1);
    bw.bits(2, 2);
    bw.bits(0, 5);
    bw.bits(0, 5);
    bw.bits(19 - 4, 4);
    let mut clen = [0u8; 19];
    clen[2] = 1; // символ 18
    clen[3] = 1; // символ 0
    for &v in &clen {
        bw.bits(u32::from(v), 3);
    }
    // Канонические коды: символ 0 → «0», символ 18 → «1».
    for _ in 0..3 {
        bw.code(1, 1);
        bw.bits(127, 7); // 138 нулей
    }
    assert_eq!(kind_of(&bw.finish()), DeflateError::BadCodeLengthRepeat);
}

// ---------------------------------------------------------------------------
// Битые входы
// ---------------------------------------------------------------------------

#[test]
fn reserved_block_type_is_rejected() {
    assert_eq!(kind_of(&[0x07, 0x00]), DeflateError::ReservedBlockType);
    // И в нефинальном блоке тоже.
    assert_eq!(kind_of(&[0x06, 0x00]), DeflateError::ReservedBlockType);
}

#[test]
fn empty_input_is_eof_not_success() {
    assert_eq!(kind_of(&[]), DeflateError::UnexpectedEof);
}

#[test]
fn stream_without_final_block_is_eof() {
    // Один нефинальный stored-блок и больше ничего.
    let mut bw = Bw::new();
    stored(&mut bw, false, b"data");
    assert_eq!(kind_of(&bw.finish()), DeflateError::UnexpectedEof);
}

#[test]
fn truncation_at_every_prefix_is_an_error_never_a_panic() {
    let mut bomb = Bw::new();
    stored(&mut bomb, false, b"z");
    dynamic_rle_block(&mut bomb, true, 2, 2);

    let mut fixed = Bw::new();
    fixed.bits(1, 1);
    fixed.bits(1, 2);
    fx_lit(&mut fixed, b'a');
    fx_match(&mut fixed, 20, 1);
    fx_end(&mut fixed);

    let mut stored_stream = Bw::new();
    stored(&mut stored_stream, true, b"stored payload");

    let streams: [Vec<u8>; 4] = [
        REAL_DYNAMIC.to_vec(),
        bomb.finish(),
        fixed.finish(),
        stored_stream.finish(),
    ];

    for (idx, full) in streams.iter().enumerate() {
        // Требование вехи — первые 32 позиции; проверяем заодно все прочие.
        for cut in 0..full.len() {
            let prefix = &full[..cut];
            assert!(
                inflate(prefix, &Limits::strict()).is_err(),
                "поток {idx}, обрыв на {cut} байтах распаковался"
            );
        }
        assert!(inflate(full, &Limits::strict()).is_ok(), "поток {idx}");
    }
}

#[test]
fn corrupting_any_single_byte_never_panics() {
    let full = REAL_DYNAMIC;
    for pos in 0..full.len() {
        for mask in [0x01u8, 0x40, 0xFF] {
            let mut broken = full;
            broken[pos] ^= mask;
            // Результат неважен — важно, что вызов вернулся.
            let _ = inflate(&broken, &Limits::strict());
        }
    }
}

// ---------------------------------------------------------------------------
// Квоты
// ---------------------------------------------------------------------------

#[test]
fn compression_bomb_hits_the_ratio_limit() {
    // Дерево из двух однобитных кодов даёт 258 байт выхода на два бита
    // входа — отношение около 1000 при заявленном пределе 200.
    let mut bw = Bw::new();
    stored(&mut bw, false, b"z");
    dynamic_rle_block(&mut bw, true, 2, 4000);
    let stream = bw.finish();
    assert!(
        stream.len() < 1100,
        "бомба должна быть маленькой, а не {} байт",
        stream.len()
    );

    let err = inflate(&stream, &Limits::strict()).unwrap_err();
    assert!(err.is_limit(), "ожидалась квота, получено {err}");
    assert!(matches!(
        err,
        Error::Limit(ooxml::error::LimitError::CompressionRatio { .. })
    ));
}

#[test]
fn bomb_is_stopped_within_one_check_stride() {
    // Важно не только отвергнуть бомбу, но и не выделить под неё память:
    // проверка обязана сработать в пределах одного шага, а не в конце.
    let mut bw = Bw::new();
    stored(&mut bw, false, b"z");
    dynamic_rle_block(&mut bw, true, 2, 400_000);
    let stream = bw.finish();

    let mut out = Vec::new();
    let err = ooxml::deflate::inflate_into(&stream, &mut out, &Limits::strict()).unwrap_err();
    assert!(err.is_limit());
    assert!(
        out.len() < 2 * Limits::RATIO_CHECK_STRIDE as usize,
        "выделено {} байт — проверка сработала слишком поздно",
        out.len()
    );
}

#[test]
fn part_size_limit_is_enforced() {
    let mut limits = Limits::strict();
    limits.max_part_bytes = 1000;
    let data: Vec<u8> = vec![b'q'; 4096];
    let mut bw = Bw::new();
    stored(&mut bw, true, &data);
    let err = inflate(&bw.finish(), &limits).unwrap_err();
    assert!(matches!(
        err,
        Error::Limit(ooxml::error::LimitError::PartTooLarge { .. })
    ));
}

#[test]
fn dense_but_small_part_is_not_mistaken_for_a_bomb() {
    // Меньше одного шага проверки высокое отношение ничему не угрожает:
    // отвергнуть такую часть было бы ложной тревогой.
    let mut bw = Bw::new();
    stored(&mut bw, false, b"z");
    dynamic_rle_block(&mut bw, true, 2, 100);
    let out = ok(&bw.finish());
    assert_eq!(out.len(), 1 + 100 * 258);
}

// ---------------------------------------------------------------------------
// Мини-фаззер
// ---------------------------------------------------------------------------

/// SplitMix64 — детерминированный генератор на 64-битном состоянии.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Базовый сид; сид итерации `i` = `BASE ^ i * 0x9E37_79B9_7F4A_7C15`,
/// поэтому упавшую итерацию можно воспроизвести одним числом.
const FUZZ_BASE: u64 = 0x0D15_EA5E_1BAD_C0DE;

fn fuzz_buffer(seed: u64) -> Vec<u8> {
    let mut s = seed;
    let len = (splitmix64(&mut s) % 4096) as usize;
    let mut buf = Vec::with_capacity(len);
    while buf.len() < len {
        buf.extend_from_slice(&splitmix64(&mut s).to_le_bytes());
    }
    buf.truncate(len);
    buf
}

#[test]
fn random_input_never_panics_and_never_exceeds_limits() {
    let iters: u64 = std::env::var("OOXML_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    let limits = Limits::strict();
    let mut decoded = 0u64;
    for i in 0..iters {
        let seed = FUZZ_BASE ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let data = fuzz_buffer(seed);
        match inflate(&data, &limits) {
            // Случайные байты изредка складываются в законный stored-блок
            // (шанс порядка одного на четверть миллиона), поэтому утверждать
            // «всегда Err» было бы неверно. Инвариант — не в отказе, а в том,
            // что успех остаётся в границах квот.
            Ok(out) => {
                decoded += 1;
                assert!(
                    out.len() as u64 <= limits.max_part_bytes,
                    "сид {seed:#018x}: выход {} байт превысил квоту",
                    out.len()
                );
            }
            Err(e) => {
                assert!(!matches!(e, Error::Unsupported(_)), "сид {seed:#018x}: {e}");
            }
        }
    }
    // Полезно видеть в логе: ноль здесь означал бы, что генератор выдаёт
    // не тот мусор, что задумано.
    println!("фаззер: {iters} итераций, из них распаковалось {decoded}");
}

#[test]
fn fuzz_prefixes_of_valid_streams_never_panic() {
    // Отдельно от чистого шума: обрезки валидного потока проходят гораздо
    // глубже по коду, чем случайные байты.
    let mut s = FUZZ_BASE;
    let limits = Limits::strict();
    for _ in 0..2_000 {
        let cut = (splitmix64(&mut s) as usize) % (REAL_DYNAMIC.len() + 1);
        let mut buf = REAL_DYNAMIC[..cut].to_vec();
        let extra = (splitmix64(&mut s) % 8) as usize;
        for _ in 0..extra {
            buf.push(splitmix64(&mut s) as u8);
        }
        let _ = inflate(&buf, &limits);
    }
}
