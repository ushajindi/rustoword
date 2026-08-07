//! Тесты упаковщика DEFLATE.
//!
//! Оракул один — собственный распаковщик вехи M1, прошедший весь корпус.
//! Совпадать с чужим выходом упаковщик не обязан и не может; проверяются три
//! свойства, на которых держится round-trip: `inflate(deflate(x)) == x` для
//! любого входа, побайтовая воспроизводимость выхода и отсутствие раздувания
//! на коротких данных.

// В тестах паника — это способ сообщить о провале, а не дефект.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::deflate::{Level, deflate, inflate};
use ooxml::limits::Limits;

const LEVELS: [Level; 4] = [Level::Store, Level::Fast, Level::Default, Level::Best];

/// Квоты для тестов: строгий профиль режет отношение сжатия на 200, а прогон
/// из ста тысяч одинаковых байт жмётся в тысячи раз — это не бомба, а
/// нормальный вход упаковщика.
fn limits() -> Limits {
    Limits::permissive()
}

fn round_trip(data: &[u8], level: Level, what: &str) -> Vec<u8> {
    let packed = deflate(data, level);
    assert!(
        !packed.is_empty(),
        "{what} / {level:?}: пустой выход не является потоком DEFLATE"
    );
    match inflate(&packed, &limits()) {
        Ok(back) => {
            assert_eq!(
                back.len(),
                data.len(),
                "{what} / {level:?}: распаковалось {} байт вместо {}",
                back.len(),
                data.len()
            );
            assert!(back == data, "{what} / {level:?}: содержимое разошлось");
        }
        Err(e) => panic!("{what} / {level:?}: собственный поток не распаковался: {e}"),
    }
    packed
}

/// Детерминированный PRNG SplitMix64.
///
/// Свой, а не из крейта: зависимостей у ядра нет, а тестам нужен генератор,
/// который даёт одну и ту же последовательность на любой машине и в любом
/// запуске — иначе упавший случай нельзя воспроизвести.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
}

fn xml_like(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    v.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n");
    let mut i = 0usize;
    while v.len() < n {
        v.extend_from_slice(b"  <w:p w:rsidR=\"00A1B2C3\"><w:r><w:t xml:space=\"preserve\">");
        v.extend_from_slice(format!("значение {i}").as_bytes());
        v.extend_from_slice(b"</w:t></w:r></w:p>\n");
        i += 1;
    }
    v.truncate(n);
    v
}

#[test]
fn round_trips_on_boundary_sizes() {
    // 65535/65536 — граница длины stored-блока, 32768/32769 — граница окна.
    let sizes = [
        0usize, 1, 2, 3, 255, 32_767, 32_768, 32_769, 65_535, 65_536, 65_537,
    ];
    for &n in &sizes {
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        for level in LEVELS {
            round_trip(&data, level, &format!("{n} байт пилообразных"));
        }
    }
}

#[test]
fn round_trips_on_a_single_repeated_byte() {
    // Сто тысяч одинаковых байт — вырожденный случай для хеш-цепочек: все
    // позиции попадают в одну корзину, и без предела длины цепочки поиск
    // становится квадратичным.
    let data = vec![b'\t'; 100_000];
    for level in LEVELS {
        let packed = round_trip(&data, level, "100 КБ одного байта");
        if level != Level::Store {
            assert!(
                packed.len() < 1_000,
                "{level:?}: прогон из одного байта сжался всего до {} байт",
                packed.len()
            );
        }
    }
}

#[test]
fn round_trips_on_random_data() {
    let mut rng = SplitMix64(0x0DDB_1A5E_5BAD_5EED);
    let data: Vec<u8> = (0..200_000).map(|_| rng.byte()).collect();
    for level in LEVELS {
        let packed = round_trip(&data, level, "200 КБ случайных байт");
        // Несжимаемое не обязано сжиматься, но и раздуваться заметно не должно:
        // на такой вход энкодер обязан выбрать stored.
        assert!(
            packed.len() < data.len() + data.len() / 100 + 64,
            "{level:?}: случайные данные раздулись с {} до {}",
            data.len(),
            packed.len()
        );
    }
}

#[test]
fn round_trips_on_xml_like_text() {
    let data = xml_like(300_000);
    for level in LEVELS {
        let packed = round_trip(&data, level, "XML-подобный текст");
        if level != Level::Store {
            assert!(
                packed.len() * 4 < data.len(),
                "{level:?}: XML сжался всего в {:.1} раза",
                data.len() as f64 / packed.len() as f64
            );
        }
    }
}

#[test]
fn empty_input_yields_a_valid_stream() {
    for level in LEVELS {
        let packed = deflate(&[], level);
        assert!(!packed.is_empty(), "{level:?}: пустой Vec — не поток");
        assert!(
            packed.len() <= 8,
            "{level:?}: пустой вход дал {} байт",
            packed.len()
        );
        assert_eq!(inflate(&packed, &limits()).unwrap(), Vec::<u8>::new());
    }
}

#[test]
fn output_is_byte_for_byte_reproducible() {
    // Детерминизм — не украшение: без него round-trip перестаёт быть
    // воспроизводимым, и тесты байтовой идентичности начинают плавать.
    let mut rng = SplitMix64(0x5EED_0F00_D00D_1234);
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"hi".to_vec(),
        xml_like(50_000),
        vec![7u8; 70_000],
        (0..100_000).map(|_| rng.byte()).collect(),
    ];
    for data in &cases {
        for level in LEVELS {
            let a = deflate(data, level);
            let b = deflate(data, level);
            assert_eq!(a, b, "{level:?}: два вызова дали разный выход");
            // Тот же вход, поданный по частям другого происхождения, обязан
            // дать тот же результат — состояние между вызовами не живёт.
            let copy: Vec<u8> = data.to_vec();
            assert_eq!(
                deflate(&copy, level),
                a,
                "{level:?}: выход зависит от буфера"
            );
        }
    }
}

/// Ожидаемый выход `deflate(b"", Best)` — он же служит якорем детерминизма
/// между запусками процесса: если байты изменятся, тест это заметит.
#[test]
fn known_streams_are_stable_across_runs() {
    assert_eq!(deflate(b"", Level::Best), vec![0x03, 0x00]);
    assert_eq!(
        deflate(b"", Level::Store),
        vec![0x01, 0x00, 0x00, 0xFF, 0xFF]
    );
    // Два байта: динамическое дерево стоило бы здесь вдесятеро дороже самих
    // данных, поэтому побеждает фиксированное — четыре байта против семи у
    // stored. Байты зафиксированы как якорь воспроизводимости.
    assert_eq!(deflate(b"hi", Level::Default), vec![0xCB, 0xC8, 0x04, 0x00]);
    // Несжимаемая короткая строка обязана уйти в stored — фиксированное
    // дерево на случайных байтах даёт девять бит на байт.
    let mut rng = SplitMix64(0x1234_5678_9ABC_DEF0);
    let noise: Vec<u8> = (0..200).map(|_| rng.byte()).collect();
    let packed = deflate(&noise, Level::Default);
    assert_eq!(packed.first().copied(), Some(0x01), "ожидался stored-блок");
    assert_eq!(packed.len(), noise.len() + 5);
}

#[test]
fn store_level_uses_only_stored_blocks() {
    for &n in &[0usize, 1, 65_535, 65_536, 200_000] {
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let packed = deflate(&data, Level::Store);
        assert_eq!(inflate(&packed, &limits()).unwrap(), data, "{n} байт");
        // Накладные stored — пять байт на блок, а блок вмещает 65535 байт.
        let blocks = n.div_ceil(65_535).max(1);
        assert_eq!(
            packed.len(),
            n + blocks * 5,
            "{n} байт: ожидались ровно {blocks} stored-блоков"
        );
    }
}

#[test]
fn small_inputs_do_not_blow_up() {
    // Динамическое дерево само по себе стоит десятки байт. Энкодер обязан
    // видеть это по оценке стоимости и уходить в stored.
    for len in 0usize..=64 {
        let data: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        for level in LEVELS {
            let packed = round_trip(&data, level, &format!("{len} байт текста"));
            assert!(
                packed.len() <= len + 8,
                "{level:?}: {len} байт раздулись до {}",
                packed.len()
            );
        }
    }
}

#[test]
fn compression_beats_the_input_on_real_text() {
    let data = xml_like(200_000);
    let store = deflate(&data, Level::Store).len();
    let fast = deflate(&data, Level::Fast).len();
    let default = deflate(&data, Level::Default).len();
    let best = deflate(&data, Level::Best).len();
    println!("XML 200 КБ: store {store}, fast {fast}, default {default}, best {best}");
    assert!(fast < store, "Fast обязан выигрывать у Store");
    assert!(default <= fast, "Default не должен уступать Fast");
    assert!(best <= default, "Best не должен уступать Default");
}

/// Генерирует буфер с управляемой энтропией.
///
/// Чисто случайные данные не сжимаются и потому не проверяют ничего
/// интересного: энкодер уходит в stored, и ни деревья, ни поиск совпадений в
/// работу не вступают. Смеси прогонов, повторов и текста гоняют все ветки.
fn generate(rng: &mut SplitMix64) -> Vec<u8> {
    let target = rng.below(70_000) as usize;
    let mut out: Vec<u8> = Vec::with_capacity(target);
    while out.len() < target {
        match rng.below(6) {
            // Длинный прогон одного байта: вырожденная хеш-цепочка.
            0 => {
                let b = rng.byte();
                let n = rng.below(3_000) as usize;
                out.extend(std::iter::repeat_n(b, n));
            }
            // Повтор уже записанного куска: проверяет дальние расстояния.
            1 if !out.is_empty() => {
                let n = (rng.below(2_000) as usize).min(out.len());
                let from = rng.below(out.len() as u64) as usize;
                let take = n.min(out.len() - from);
                out.extend_from_within(from..from + take);
            }
            // ASCII-текст с узким алфавитом.
            2 => {
                let n = rng.below(2_000) as usize;
                for _ in 0..n {
                    out.push(b' ' + (rng.below(26) as u8));
                }
            }
            // XML-подобное.
            3 => {
                let n = rng.below(3_000) as usize;
                out.extend_from_slice(&xml_like(n));
            }
            // Случайные байты: несжимаемая вставка внутри сжимаемого.
            4 => {
                let n = rng.below(1_500) as usize;
                for _ in 0..n {
                    out.push(rng.byte());
                }
            }
            // Байты из крошечного алфавита — длинные, но неровные совпадения.
            _ => {
                let n = rng.below(2_500) as usize;
                for _ in 0..n {
                    out.push((rng.below(3) as u8) * 0x40);
                }
            }
        }
    }
    out.truncate(target);
    out
}

#[test]
fn fuzz_round_trip() {
    // Сид итерации выводится из номера, чтобы упавший случай воспроизводился
    // одним числом: OOXML_FUZZ_ITERS задаёт длину прогона, а падение печатает
    // и номер, и сид.
    const BASE: u64 = 0x7A6B_5C4D_3E2F_1009;
    let iters: u64 = std::env::var("OOXML_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);

    let lim = limits();
    let mut total_in = 0u64;
    let mut total_out = 0u64;

    for i in 0..iters {
        let seed = BASE ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut rng = SplitMix64(seed);
        let data = generate(&mut rng);
        let level = LEVELS[(i % 4) as usize];

        let packed = deflate(&data, level);
        let back = match inflate(&packed, &lim) {
            Ok(b) => b,
            Err(e) => panic!(
                "итерация {i} (сид {seed:#018x}), уровень {level:?}, {} байт: {e}",
                data.len()
            ),
        };
        assert!(
            back == data,
            "итерация {i} (сид {seed:#018x}), уровень {level:?}: round-trip разошёлся \
             на {} байтах входа",
            data.len()
        );
        assert_eq!(
            deflate(&data, level),
            packed,
            "итерация {i} (сид {seed:#018x}): выход недетерминирован"
        );

        total_in += data.len() as u64;
        total_out += packed.len() as u64;
    }

    println!(
        "фаззер: {iters} итераций, {total_in} байт входа, {total_out} байт выхода \
         (в среднем в {:.2} раза)",
        total_in as f64 / total_out.max(1) as f64
    );
}
