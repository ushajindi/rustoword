//! Корпусный тест упаковщика: каждая запись каждого файла корпуса
//! распаковывается готовым `inflate`, сжимается обратно и распаковывается ещё
//! раз — результат обязан совпасть с исходником побайтово.
//!
//! Заодно собирается статистика против того сжатия, которое сделали Office и
//! LibreOffice. Совпадать с ними упаковщик не обязан: у них свой поиск
//! совпадений, свои деревья и свой уровень. Отставание в размере — не провал
//! теста, а данные для решения, нужен ли более умный энкодер, поэтому оно
//! печатается, но ничего не роняет.
//!
//! Разбор ZIP здесь свой, минимальный и намеренно наивный — слой `zip`
//! пишется отдельно, и тест упаковщика не должен зависеть от его готовности.
//!
//! Корпус — реальные файлы пользователя, в репозиторий он не попадает. Путь
//! берётся из `OOXML_CORPUS`; если каталога нет, тест печатает предупреждение
//! и проходит, иначе сборка на чужой машине падала бы без причины.

// В тестах паника — это способ сообщить о провале, а не дефект.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};

use ooxml::deflate::{Level, deflate, inflate_into};
use ooxml::limits::Limits;

/// Путь к корпусу по умолчанию.
const DEFAULT_CORPUS: &str = "/Users/shakh/rustoword/crates/ooxml/tests/corpus";

/// Порог, за которым отставание от чужого сжатия попадает в отчёт.
const REPORT_RATIO: f64 = 1.20;

fn corpus_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OOXML_CORPUS") {
        return PathBuf::from(p);
    }
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus");
    if local.is_dir() {
        return local;
    }
    PathBuf::from(DEFAULT_CORPUS)
}

fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// То, что нужно от записи центрального каталога.
struct Entry {
    name: String,
    method: u16,
    comp_size: usize,
    uncomp_size: usize,
    local_offset: usize,
}

/// Разбирает центральный каталог. `None`, если EOCD не найден.
fn central_directory(raw: &[u8]) -> Option<Vec<Entry>> {
    // EOCD лежит в конце, но за ним может быть комментарий архива, а сама
    // сигнатура может встретиться в данных — берём последнее вхождение.
    let window = raw.len().min(65_557 + 22);
    let from = raw.len() - window;
    let tail = raw.get(from..)?;
    let idx = tail
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .map(|i| from + i)?;

    let count = le_u16(raw, idx + 10)? as usize;
    let cd_off = le_u32(raw, idx + 16)? as usize;

    let mut out = Vec::with_capacity(count);
    let mut p = cd_off;
    for _ in 0..count {
        if raw.get(p..p + 4)? != b"PK\x01\x02" {
            return None;
        }
        let method = le_u16(raw, p + 10)?;
        let comp_size = le_u32(raw, p + 20)? as usize;
        let uncomp_size = le_u32(raw, p + 24)? as usize;
        let name_len = le_u16(raw, p + 28)? as usize;
        let extra_len = le_u16(raw, p + 30)? as usize;
        let comment_len = le_u16(raw, p + 32)? as usize;
        let local_offset = le_u32(raw, p + 42)? as usize;
        let name = String::from_utf8_lossy(raw.get(p + 46..p + 46 + name_len)?).into_owned();

        out.push(Entry {
            name,
            method,
            comp_size,
            uncomp_size,
            local_offset,
        });
        p = p + 46 + name_len + extra_len + comment_len;
    }
    Some(out)
}

/// Начало данных записи: локальный заголовок нужен только ради длин имени и
/// extra — они в нём свои и с каталогом не обязаны совпадать.
fn data_start(raw: &[u8], e: &Entry) -> Option<usize> {
    let p = e.local_offset;
    if raw.get(p..p + 4)? != b"PK\x03\x04" {
        return None;
    }
    let name_len = le_u16(raw, p + 26)? as usize;
    let extra_len = le_u16(raw, p + 28)? as usize;
    Some(p + 30 + name_len + extra_len)
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            files_under(&p, out);
        } else if p.extension().is_some_and(|x| x == "xlsx" || x == "docx") {
            out.push(p);
        }
    }
}

/// Одна строка отчёта: как наше сжатие соотносится с чужим.
struct Sample {
    part: String,
    plain: usize,
    theirs: usize,
    ours: usize,
}

impl Sample {
    fn ratio(&self) -> f64 {
        self.ours as f64 / self.theirs.max(1) as f64
    }
}

#[test]
fn every_corpus_part_survives_our_deflate() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!(
            "ПРЕДУПРЕЖДЕНИЕ: корпус не найден в {}; тест пропущен. \
             Путь задаётся переменной OOXML_CORPUS.",
            dir.display()
        );
        return;
    }

    let mut files = Vec::new();
    files_under(&dir, &mut files);
    if files.is_empty() {
        println!(
            "ПРЕДУПРЕЖДЕНИЕ: в {} нет ни одного .xlsx/.docx; тест пропущен.",
            dir.display()
        );
        return;
    }

    // Квоты ослаблены намеренно: строгий профиль режет отношение сжатия на
    // 200, а сильно повторяющиеся части корпуса жмутся плотнее. Здесь вход
    // доверенный — это наш собственный выход.
    let limits = Limits::permissive();

    let mut failures: Vec<String> = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    let mut n_files = 0usize;
    let mut n_entries = 0usize;
    let mut n_stored = 0usize;
    let mut plain_total = 0u64;
    let mut theirs_total = 0u64;
    let mut ours_total = 0u64;
    let mut ours_store_total = 0u64;
    let mut ours_fast_total = 0u64;
    let mut ours_best_total = 0u64;

    for path in &files {
        let Ok(raw) = std::fs::read(path) else {
            failures.push(format!("{}: файл не читается", path.display()));
            continue;
        };
        let Some(entries) = central_directory(&raw) else {
            failures.push(format!(
                "{}: центральный каталог не разобран",
                path.display()
            ));
            continue;
        };
        n_files += 1;

        for e in &entries {
            let Some(start) = data_start(&raw, e) else {
                failures.push(format!(
                    "{} :: {}: локальный заголовок битый",
                    path.display(),
                    e.name
                ));
                continue;
            };
            let Some(comp) = raw.get(start..start + e.comp_size) else {
                failures.push(format!(
                    "{} :: {}: данные за границей файла",
                    path.display(),
                    e.name
                ));
                continue;
            };

            let plain = match e.method {
                0 => {
                    n_stored += 1;
                    comp.to_vec()
                }
                8 => {
                    let mut buf = Vec::new();
                    if let Err(err) = inflate_into(comp, &mut buf, &limits) {
                        failures.push(format!(
                            "{} :: {}: распаковка оригинала провалилась: {err}",
                            path.display(),
                            e.name
                        ));
                        continue;
                    }
                    buf
                }
                other => {
                    failures.push(format!(
                        "{} :: {}: неизвестный метод {other}",
                        path.display(),
                        e.name
                    ));
                    continue;
                }
            };
            if plain.len() != e.uncomp_size {
                failures.push(format!(
                    "{} :: {}: распакованный размер {} вместо {}",
                    path.display(),
                    e.name,
                    plain.len(),
                    e.uncomp_size
                ));
                continue;
            }
            n_entries += 1;

            // Основное свойство вехи: наш выход распаковывается в наш вход.
            let ours = deflate(&plain, Level::Default);
            let mut back = Vec::new();
            match inflate_into(&ours, &mut back, &limits) {
                Ok(used) => {
                    if used != ours.len() {
                        failures.push(format!(
                            "{} :: {}: распаковщик съел {used} байт из {}",
                            path.display(),
                            e.name,
                            ours.len()
                        ));
                    }
                }
                Err(err) => {
                    failures.push(format!(
                        "{} :: {}: НАШ поток не распаковался: {err}",
                        path.display(),
                        e.name
                    ));
                    continue;
                }
            }
            if back != plain {
                failures.push(format!(
                    "{} :: {}: round-trip разошёлся ({} байт против {})",
                    path.display(),
                    e.name,
                    back.len(),
                    plain.len()
                ));
                continue;
            }

            // Детерминизм проверяем выборочно: полный повтор удвоил бы время
            // прогона, а свойство глобальное и ловится на любой части.
            if n_entries.is_multiple_of(37) {
                assert_eq!(
                    deflate(&plain, Level::Default),
                    ours,
                    "{} :: {}: выход недетерминирован",
                    path.display(),
                    e.name
                );
            }

            plain_total += plain.len() as u64;
            ours_total += ours.len() as u64;
            ours_store_total += deflate(&plain, Level::Store).len() as u64;
            // Остальные уровни считаются ради отчёта: round-trip на них
            // проверяют юнит-тесты, здесь важен только объём.
            ours_fast_total += deflate(&plain, Level::Fast).len() as u64;
            ours_best_total += deflate(&plain, Level::Best).len() as u64;
            if e.method == 8 {
                theirs_total += e.comp_size as u64;
                samples.push(Sample {
                    part: format!(
                        "{} :: {}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        e.name
                    ),
                    plain: plain.len(),
                    theirs: e.comp_size,
                    ours: ours.len(),
                });
            }
        }
    }

    // Распределение отношения «наш размер / их размер».
    let mut buckets = [0usize; 6];
    for s in &samples {
        let r = s.ratio();
        let b = if r < 0.90 {
            0
        } else if r < 1.00 {
            1
        } else if r < 1.05 {
            2
        } else if r < 1.10 {
            3
        } else if r < REPORT_RATIO {
            4
        } else {
            5
        };
        buckets[b] += 1;
    }

    // Худшие случаи считаем по абсолютному перерасходу, а не по отношению:
    // часть в сорок байт с отношением 3.0 стоит нам восемьдесят байт и никого
    // не волнует, а лист на мегабайт с отношением 1.05 — пятьдесят килобайт.
    let mut worst: Vec<&Sample> = samples.iter().collect();
    worst.sort_by_key(|s| s.theirs as i64 - s.ours as i64);

    println!(
        "\nкорпус {}\n  файлов: {n_files}\n  записей: {n_entries} (из них stored в оригинале: \
         {n_stored})\n  распаковано: {plain_total} байт",
        dir.display()
    );
    println!(
        "  их сжатие (deflate-записи): {theirs_total} байт\n  наше сжатие (всё, Default): \
         {ours_total} байт\n  наше Fast: {ours_fast_total}, наше Best: {ours_best_total}\n  \
         наше Store (нижняя граница накладных): {ours_store_total} байт"
    );
    if theirs_total > 0 {
        println!(
            "  наше/их по объёму: {:.4}   их коэффициент: {:.2}x, наш: {:.2}x",
            ours_total as f64 / theirs_total as f64,
            plain_total as f64 / theirs_total.max(1) as f64,
            plain_total as f64 / ours_total.max(1) as f64
        );
    }
    println!(
        "  распределение наше/их: <0.90: {}, 0.90–1.00: {}, 1.00–1.05: {}, \
         1.05–1.10: {}, 1.10–1.20: {}, >=1.20: {}",
        buckets[0], buckets[1], buckets[2], buckets[3], buckets[4], buckets[5]
    );

    println!("  худшие случаи по абсолютному перерасходу:");
    for s in worst.iter().take(8) {
        println!(
            "    {:+8} байт  ({:.3}x)  их {:>8}, наши {:>8}, сырых {:>9}  {}",
            s.ours as i64 - s.theirs as i64,
            s.ratio(),
            s.theirs,
            s.ours,
            s.plain,
            s.part
        );
    }

    let over: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.ratio() >= REPORT_RATIO && s.plain >= 4_096)
        .collect();
    println!(
        "  частей крупнее 4 КБ с отставанием от 20%: {} из {}",
        over.len(),
        samples.len()
    );
    for s in over.iter().take(8) {
        println!(
            "    {:.3}x  их {:>8}, наши {:>8}, сырых {:>9}  {}",
            s.ratio(),
            s.theirs,
            s.ours,
            s.plain,
            s.part
        );
    }

    assert!(
        failures.is_empty(),
        "расхождений: {}\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(n_entries > 0, "в корпусе не нашлось ни одной записи");
}
