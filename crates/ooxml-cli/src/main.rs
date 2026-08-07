//! Инструмент отладки ядра.
//!
//! Существует ради `zipdump`: когда repack-identity расходится на одном байте,
//! единственный способ понять почему — разложить оба архива по полям заголовков
//! и сравнить. Разбор аргументов написан вручную — крейт `clap` был бы
//! зависимостью, а их в проекте нет.

// Слой I/O — единственное место в проекте, где допустим `std::fs`.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

const USAGE: &str = "\
ooxml — инструмент отладки ядра OOXML

ИСПОЛЬЗОВАНИЕ:
    ooxml <команда> [аргументы]

КОМАНДЫ:
    info <файл>              сводка по пакету
    zipdump <файл>           все поля обоих ZIP-заголовков каждой записи
    xmldump <файл> <часть>   разбор XML-части
    get <файл> <лист> <A1>   значение ячейки
    set <файл> <лист> <A1> <значение> -o <выход>
    roundtrip <файл>         open -> save, сравнение байт
    fuzz --strategy <s> --iters <n> [--seed <s>]

Команды появляются по мере готовности вех; нереализованные сообщают об этом явно.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{USAGE}");
        return ExitCode::FAILURE;
    };

    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "info" | "zipdump" | "xmldump" | "get" | "set" | "roundtrip" | "fuzz" => {
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
