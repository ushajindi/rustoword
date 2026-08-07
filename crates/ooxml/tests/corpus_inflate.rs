//! Корпусный тест распаковщика: каждая сжатая запись каждого файла корпуса
//! распаковывается и сверяется с CRC и размером из центрального каталога.
//!
//! Разбор ZIP здесь свой, минимальный и намеренно наивный — слой `zip` пишется
//! отдельно, и тест распаковщика не должен зависеть от его готовности.
//! Из каталога берутся только метод, CRC, размеры и офсет локального заголовка.
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

use ooxml::deflate::inflate_into;
use ooxml::hash::crc32;
use ooxml::limits::Limits;

/// Путь к корпусу по умолчанию.
const DEFAULT_CORPUS: &str = "/Users/shakh/rustoword/crates/ooxml/tests/corpus";

fn corpus_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OOXML_CORPUS") {
        return PathBuf::from(p);
    }
    // Рядом с крейтом корпус лежит, когда он распакован в рабочем дереве;
    // иначе берётся общий каталог пользователя.
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
    flags: u16,
    crc: u32,
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
        let flags = le_u16(raw, p + 8)?;
        let method = le_u16(raw, p + 10)?;
        let crc = le_u32(raw, p + 16)?;
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
            flags,
            crc,
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

#[derive(Default)]
struct Stats {
    files: usize,
    entries: usize,
    deflated: usize,
    stored: usize,
    descriptors: usize,
    crc_ok: usize,
    size_ok: usize,
    consumed_exact: usize,
    bytes_out: u64,
}

#[test]
fn every_deflated_entry_of_the_corpus_round_trips() {
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

    let limits = Limits::strict();
    let mut st = Stats::default();
    let mut failures: Vec<String> = Vec::new();

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
        st.files += 1;

        for e in &entries {
            st.entries += 1;
            // Флаг 3 — данные пишутся до того, как известны CRC и размеры;
            // в локальном заголовке там нули, и верить можно только каталогу.
            if e.flags & 0x08 != 0 {
                st.descriptors += 1;
            }

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

            let out = match e.method {
                0 => {
                    st.stored += 1;
                    comp.to_vec()
                }
                8 => {
                    st.deflated += 1;
                    let mut buf = Vec::new();
                    match inflate_into(comp, &mut buf, &limits) {
                        Ok(used) => {
                            if used == e.comp_size {
                                st.consumed_exact += 1;
                            }
                            if used > e.comp_size {
                                failures.push(format!(
                                    "{} :: {}: съедено {used} байт при сжатом размере {}",
                                    path.display(),
                                    e.name,
                                    e.comp_size
                                ));
                            }
                        }
                        Err(err) => {
                            failures.push(format!(
                                "{} :: {}: распаковка провалилась: {err}",
                                path.display(),
                                e.name
                            ));
                            continue;
                        }
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

            st.bytes_out += out.len() as u64;
            if out.len() == e.uncomp_size {
                st.size_ok += 1;
            } else {
                failures.push(format!(
                    "{} :: {}: размер {} вместо {}",
                    path.display(),
                    e.name,
                    out.len(),
                    e.uncomp_size
                ));
            }
            let actual = crc32(&out);
            if actual == e.crc {
                st.crc_ok += 1;
            } else {
                failures.push(format!(
                    "{} :: {}: CRC {actual:#010X} вместо {:#010X}",
                    path.display(),
                    e.name,
                    e.crc
                ));
            }
        }
    }

    println!(
        "корпус {}:\n  файлов: {}\n  записей: {} (deflate {}, stored {}, с дескриптором {})\n  \
         CRC совпал: {}/{}\n  размер совпал: {}/{}\n  \
         потреблено ровно comp_size: {}/{}\n  распаковано байт: {}",
        dir.display(),
        st.files,
        st.entries,
        st.deflated,
        st.stored,
        st.descriptors,
        st.crc_ok,
        st.entries,
        st.size_ok,
        st.entries,
        st.consumed_exact,
        st.deflated,
        st.bytes_out
    );

    assert!(
        failures.is_empty(),
        "расхождений: {}\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(st.crc_ok, st.entries, "CRC совпал не у всех записей");
    assert!(
        st.deflated > 0,
        "в корпусе не нашлось ни одной сжатой записи"
    );
}
