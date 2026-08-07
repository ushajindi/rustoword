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
        "roundtrip" | "info" => {
            eprintln!("команде `{cmd}` нужен путь к файлу");
            ExitCode::FAILURE
        }
        "zipdump" | "xmldump" | "get" | "set" => {
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
