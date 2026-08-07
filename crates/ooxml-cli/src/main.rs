//! Инструмент отладки ядра.
//!
//! Существует ради `zipdump` и `roundtrip`: когда байтовая идентичность
//! расходится на одном байте, единственный способ понять почему — разложить
//! архив по полям заголовков и сравнить. Разбор аргументов написан вручную:
//! `clap` был бы зависимостью, а их в проекте нет.

// Слой I/O — единственное место в проекте, где допустим `std::fs`.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::export::html::{HtmlOptions, workbook_to_html};
use ooxml::xlsx::Workbook;
use ooxml::zip::{ZipArchive, repack_all_verbatim};
use std::process::ExitCode;

const USAGE: &str = "\
ooxml — инструмент отладки ядра OOXML

ИСПОЛЬЗОВАНИЕ:
    ooxml <команда> [аргументы]

КОМАНДЫ:
    info <файл>              сводка по пакету
    zipdump <файл>           все поля обоих ZIP-заголовков каждой записи
    roundtrip <файл>...      пересобрать без правок и сравнить с исходником
    html <файл> [выход]      показать книгу как самодостаточную HTML-страницу
    xmldump <файл> <часть>   разбор XML-части
    get <файл> <лист> <A1>   значение ячейки
    set <файл> <лист> <A1> <значение> -o <выход>

Команды появляются по мере готовности вех; нереализованные сообщают об этом явно.
";

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

/// Пересобирает архив без единой правки и сравнивает с исходником.
///
/// Это тот же критерий, что и у теста гейта M3, но выполняемый снаружи крейта
/// и над произвольным файлом — в том числе над теми, которых нет в корпусе.
fn cmd_roundtrip(paths: &[String]) -> ExitCode {
    let limits = Limits::strict();
    let (mut ok, mut bad) = (0u32, 0u32);

    for path in paths {
        let data = match read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{e}");
                bad += 1;
                continue;
            }
        };

        let archive = match ZipArchive::parse(&data, &limits) {
            Ok(a) => a,
            Err(e) => {
                println!("РАЗБОР  {path}: {e}");
                bad += 1;
                continue;
            }
        };

        let out = match repack_all_verbatim(&archive) {
            Ok(o) => o,
            Err(e) => {
                println!("СБОРКА  {path}: {e}");
                bad += 1;
                continue;
            }
        };

        if out == data {
            println!(
                "ok      {path} ({} байт, {} записей)",
                data.len(),
                archive.len()
            );
            ok += 1;
        } else {
            let at = data
                .iter()
                .zip(out.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(data.len().min(out.len()));
            println!(
                "РАЗОШЛОСЬ {path}: было {} байт, стало {}; первое различие на офсете {at} \
                 ({:02x?} против {:02x?})",
                data.len(),
                out.len(),
                data.get(at),
                out.get(at),
            );
            bad += 1;
        }
    }

    println!("\nитого: {ok} совпало, {bad} разошлось");
    if bad == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_info(path: &str) -> ExitCode {
    let limits = Limits::strict();
    let Ok(data) = read(path).inspect_err(|e| eprintln!("{e}")) else {
        return ExitCode::FAILURE;
    };
    let archive = match ZipArchive::parse(&data, &limits) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("{path}: {} байт, {} записей", data.len(), archive.len());
    let mut total = 0u64;
    for (i, e) in archive.entries().iter().enumerate() {
        let name = archive
            .name_str(i)
            .map_or_else(|_| "<нечитаемое имя>".into(), std::borrow::Cow::into_owned);
        total += e.uncomp_size;
        println!(
            "  {name:<48} метод {:<2} {:>9} -> {:<9} flags {:#06x} extra {}/{}",
            e.method,
            e.comp_size,
            e.uncomp_size,
            e.flags,
            e.local_extra.len(),
            e.cd_extra.len(),
        );
    }
    println!("  распакованный размер всего: {total}");
    ExitCode::SUCCESS
}

/// Раскладывает оба заголовка каждой записи по полям.
///
/// Когда байтовая идентичность расходится на одном байте, сравнить два таких
/// дампа — единственный способ понять, какое поле разъехалось, не читая hex
/// глазами. Печатаются в том числе поля, которые различаются между локальным
/// заголовком и каталогом: именно они ломают наивные writer'ы.
fn cmd_zipdump(path: &str) -> ExitCode {
    let limits = Limits::strict();
    let Ok(data) = read(path).inspect_err(|e| eprintln!("{e}")) else {
        return ExitCode::FAILURE;
    };
    let archive = match ZipArchive::parse(&data, &limits) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("{path}: {} байт, {} записей", data.len(), archive.len());
    println!("префикс: {} байт", archive.prefix().len());

    for (i, e) in archive.entries().iter().enumerate() {
        let name = archive
            .name_str(i)
            .map_or_else(|_| "<нечитаемое имя>".into(), std::borrow::Cow::into_owned);
        println!("\n[{i}] {name}");
        println!(
            "  made by {:<4} needed local/cd {}/{}{}",
            e.version_made_by,
            e.version_needed_local,
            e.version_needed_cd,
            if e.version_needed_local == e.version_needed_cd {
                ""
            } else {
                "  <-- РАСХОДЯТСЯ"
            }
        );
        println!(
            "  flags {:#06x}  method {}  dos {:#06x}/{:#06x}{}",
            e.flags,
            e.method,
            e.dos_date,
            e.dos_time,
            if e.dos_date == 0 && e.dos_time == 0 {
                "  <-- нулевая дата, не нормализовать"
            } else {
                ""
            }
        );
        println!(
            "  crc {:#010x}  сжато {}  распаковано {}",
            e.crc32, e.comp_size, e.uncomp_size
        );
        if e.flags & 0x0008 != 0 {
            println!(
                "  локально crc/размеры: {:#010x}/{}/{} (нули при bit 3 — так и надо)",
                e.crc32_local, e.comp_size_local, e.uncomp_size_local
            );
        }
        println!(
            "  extra local/cd {}/{} байт{}",
            e.local_extra.len(),
            e.cd_extra.len(),
            if e.local_extra.len() == e.cd_extra.len() {
                ""
            } else {
                "  <-- РАСХОДЯТСЯ, копировать нельзя"
            }
        );
        println!(
            "  attrs internal {:#06x} external {:#010x}  офсет заголовка {}",
            e.internal_attrs, e.external_attrs, e.local_header_off
        );
        if let Some(d) = &e.descriptor {
            println!(
                "  дескриптор: {} байт, сигнатура {}, поля {} бит",
                d.span.len(),
                if d.has_signature {
                    "есть"
                } else {
                    "нет"
                },
                if d.wide { 64 } else { 32 }
            );
        }
        if let Some(z) = &e.zip64_layout {
            println!("  zip64: {z:?}");
        }
    }
    ExitCode::SUCCESS
}

/// Экспортирует книгу в HTML.
fn cmd_html(path: &str, out_path: Option<&str>) -> ExitCode {
    let Ok(data) = read(path).inspect_err(|e| eprintln!("{e}")) else {
        return ExitCode::FAILURE;
    };
    let mut wb = match Workbook::open(&data) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut opts = HtmlOptions::default();
    opts.title = std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned());

    let html = match workbook_to_html(&mut wb, &opts) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match out_path {
        Some(p) => match std::fs::write(p, &html) {
            Ok(()) => {
                println!("{p}: {} байт", html.len());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{p}: {e}");
                ExitCode::FAILURE
            }
        },
        None => {
            print!("{html}");
            ExitCode::SUCCESS
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let rest = args.get(1..).unwrap_or_default();

    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "roundtrip" if !rest.is_empty() => cmd_roundtrip(rest),
        "info" if rest.len() == 1 => rest.first().map_or(ExitCode::FAILURE, |p| cmd_info(p)),
        "html" if rest.len() == 1 || rest.len() == 2 => {
            rest.first().map_or(ExitCode::FAILURE, |p| {
                cmd_html(p, rest.get(1).map(String::as_str))
            })
        }
        "zipdump" if rest.len() == 1 => rest.first().map_or(ExitCode::FAILURE, |p| cmd_zipdump(p)),
        "roundtrip" | "info" | "zipdump" => {
            eprintln!("команде `{cmd}` нужен путь к файлу");
            ExitCode::FAILURE
        }
        "xmldump" | "get" | "set" => {
            eprintln!("команда `{cmd}` появится вместе с соответствующей вехой");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("неизвестная команда: {other}\n");
            print!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
