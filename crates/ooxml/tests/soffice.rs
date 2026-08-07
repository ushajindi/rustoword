//! Харнесс оракула LibreOffice.
//!
//! # Зачем
//!
//! Своим тестам верить нельзя: они проверяют, что наш парсер согласен с нашим
//! писателем. Оракул отвечает на другой вопрос — согласен ли с нами кто-то
//! посторонний. LibreOffice для этого годится: он читает OOXML независимой
//! реализацией и умеет выводить содержимое в plain-форматы.
//!
//! Харнесс даёт две функции для будущих вех:
//!
//! * [`read_reference_csv`] — референс чтения для **M8**: LibreOffice
//!   конвертирует лист в CSV, мы сравниваем с тем, что вычитали сами.
//! * [`validates`] — валидатор записи для **M9**: произведённые нами байты
//!   скармливаются LibreOffice; если он открывает файл и даёт непустой вывод,
//!   значит мы не сломали контейнер.
//!
//! # Как включить
//!
//! Все тесты помечены `#[ignore]`: LibreOffice есть не на каждой машине, и его
//! запуск занимает секунды, а не миллисекунды.
//!
//! ```text
//! OOXML_SOFFICE=/Applications/LibreOffice.app/Contents/MacOS/soffice \
//!     cargo test --test soffice -- --ignored
//! ```
//!
//! Если `OOXML_SOFFICE` не задана, пробуется известный путь установки на macOS.
//! Если и его нет — тест печатает предупреждение и **проходит**: отсутствие
//! стороннего пакета не является дефектом нашего кода.
//!
//! # Изоляция профиля — не опция
//!
//! Каждый прогон получает собственный `-env:UserInstallation=file:///<tmp>`.
//! Без этого headless-процесс лезет в тот же профиль, что и запущенный GUI
//! (и что соседний тест), упирается в блокировку `.~lock` и либо молча
//! отказывается конвертировать, либо зависает. Симптом — плавающие падения,
//! которые не воспроизводятся поодиночке. Профиль удаляется вместе с рабочим
//! каталогом в [`Workspace::drop`].
//!
//! # Таймаут
//!
//! LibreOffice умеет зависать (диалог восстановления, ожидание X-сервера,
//! повреждённый профиль). Внешних крейтов у нас нет, поэтому ожидание сделано
//! на потоке и канале: рабочий поток запускает процесс и шлёт результат,
//! основной ждёт `recv_timeout`; по истечении срока процесс убивается по pid.

// В тестах паника — способ сообщить о провале, а не дефект.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Путь установки LibreOffice на macOS. Используется, если `OOXML_SOFFICE`
/// не задана: бинаря нет в `PATH`, внутри `.app` его никто не ищет.
const DEFAULT_SOFFICE: &str = "/Applications/LibreOffice.app/Contents/MacOS/soffice";

/// Потолок на один запуск конвертации.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Сколько ждать, пока убитый процесс отдаст управление рабочему потоку.
const REAP_TIMEOUT: Duration = Duration::from_secs(10);

/// Фильтр StarCalc: разделитель полей 44 (`,`), текстовый разделитель 34 (`"`),
/// кодировка 76 (UTF-8). Без явного указания LibreOffice берёт их из локали,
/// и результат перестаёт быть воспроизводимым между машинами.
const CSV_FILTER: &str = "csv:Text - txt - csv (StarCalc):44,34,76";

// ---------------------------------------------------------------------------
// Обнаружение оракула и корпуса
// ---------------------------------------------------------------------------

/// Путь к бинарю LibreOffice или `None`, если оракула на машине нет.
fn soffice_path() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("OOXML_SOFFICE") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
        eprintln!(
            "OOXML_SOFFICE указывает на {}, но такого файла нет",
            path.display()
        );
        return None;
    }
    let fallback = PathBuf::from(DEFAULT_SOFFICE);
    if fallback.is_file() {
        return Some(fallback);
    }
    None
}

/// Оракул или предупреждение. `None` означает «тест надо пропустить».
fn oracle() -> Option<PathBuf> {
    let found = soffice_path();
    if found.is_none() {
        eprintln!(
            "ПРЕДУПРЕЖДЕНИЕ: LibreOffice не найден (ни OOXML_SOFFICE, ни {DEFAULT_SOFFICE}).\n\
             Тесты оракула пропущены."
        );
    }
    found
}

/// Корень корпуса: `OOXML_CORPUS` либо `crates/ooxml/tests/corpus`.
///
/// Корпус — реальные документы пользователя, он не лежит в репозитории
/// (см. `docs/corpus.md`), поэтому его отсутствие — норма, а не провал.
fn corpus_root() -> PathBuf {
    std::env::var_os("OOXML_CORPUS").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("corpus")
        },
        PathBuf::from,
    )
}

/// Первый по имени файл с данным расширением в подкаталоге корпуса.
fn first_corpus_file(subdir: &str, ext: &str) -> Option<PathBuf> {
    let dir = corpus_root().join(subdir);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    found.sort();
    if found.is_empty() {
        eprintln!(
            "ПРЕДУПРЕЖДЕНИЕ: в {} нет файлов *.{ext}; тест пропущен.\n\
             Путь к корпусу задаётся переменной OOXML_CORPUS.",
            dir.display()
        );
    }
    found.into_iter().next()
}

// ---------------------------------------------------------------------------
// Рабочий каталог
// ---------------------------------------------------------------------------

static WORKSPACE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Уникальный временный каталог: профиль LibreOffice + каталог вывода.
///
/// Уникальность собирается из pid, счётчика и наносекунд — параллельные
/// `cargo test` в одном и том же `TMPDIR` не должны пересечься.
#[derive(Debug)]
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(tag: &str) -> Result<Workspace, String> {
        let seq = WORKSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "ooxml-soffice-{tag}-{pid}-{seq}-{nanos}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(root.join("profile"))
            .map_err(|e| format!("не создаётся {}: {e}", root.display()))?;
        std::fs::create_dir_all(root.join("out"))
            .map_err(|e| format!("не создаётся {}: {e}", root.display()))?;
        Ok(Workspace { root })
    }

    /// Значение для `-env:UserInstallation`. Путь абсолютный, поэтому
    /// `file://` + `/tmp/...` даёт корректный `file:///tmp/...`.
    fn profile_url(&self) -> String {
        format!(
            "-env:UserInstallation=file://{}",
            self.root.join("profile").display()
        )
    }

    fn outdir(&self) -> PathBuf {
        self.root.join("out")
    }

    fn scratch(&self) -> PathBuf {
        self.root.clone()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Профиль LibreOffice — десятки мегабайт; оставлять его нельзя.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Запуск с таймаутом
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn kill_process(pid: u32) {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_process(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID"])
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Запускает команду и ждёт не дольше `timeout`. По истечении срока процесс
/// убивается, а вызывающему возвращается ошибка.
///
/// Реализация без внешних крейтов: рабочий поток делает `spawn` и блокирующий
/// `wait_with_output`, основной ждёт результат через `recv_timeout`. Pid
/// передаётся отдельным каналом сразу после `spawn`, чтобы на пути таймаута
/// было кого убивать: `Child` в поток уже уехал и снаружи недоступен.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let (pid_tx, pid_rx) = mpsc::channel::<u32>();
    let (out_tx, out_rx) = mpsc::channel::<Result<Output, String>>();

    let worker = thread::spawn(move || {
        let message = match cmd.spawn() {
            Err(e) => Err(format!("процесс не запустился: {e}")),
            Ok(child) => {
                let _ = pid_tx.send(child.id());
                child
                    .wait_with_output()
                    .map_err(|e| format!("ошибка ожидания процесса: {e}"))
            }
        };
        let _ = out_tx.send(message);
    });

    if let Ok(result) = out_rx.recv_timeout(timeout) {
        let _ = worker.join();
        return result;
    }

    // Таймаут. Убиваем процесс и даём рабочему потоку доложиться, но join'а
    // не ждём: если kill не подействовал, зависнуть должен процесс, а не тест.
    if let Ok(pid) = pid_rx.recv_timeout(Duration::from_millis(500)) {
        kill_process(pid);
    }
    let _ = out_rx.recv_timeout(REAP_TIMEOUT);
    Err(format!(
        "LibreOffice не завершился за {} с, процесс убит",
        timeout.as_secs()
    ))
}

// ---------------------------------------------------------------------------
// Конвертация
// ---------------------------------------------------------------------------

/// Во что конвертируем.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `.xlsx` → CSV фильтром StarCalc.
    Spreadsheet,
    /// `.docx` → plain text.
    Text,
}

impl Kind {
    fn convert_to(self) -> &'static str {
        match self {
            Kind::Spreadsheet => CSV_FILTER,
            Kind::Text => "txt",
        }
    }

    fn out_ext(self) -> &'static str {
        match self {
            Kind::Spreadsheet => "csv",
            Kind::Text => "txt",
        }
    }

    fn in_ext(self) -> &'static str {
        match self {
            Kind::Spreadsheet => "xlsx",
            Kind::Text => "docx",
        }
    }

    /// Определяет тип по сырым байтам пакета: имена записей лежат в локальных
    /// заголовках несжатыми, поэтому достаточно поиска подстроки.
    fn sniff(bytes: &[u8]) -> Kind {
        if find_subslice(bytes, b"word/document.xml").is_some() {
            Kind::Text
        } else {
            Kind::Spreadsheet
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Результат конвертации. Держит рабочий каталог живым: как только структура
/// уничтожена, файл вывода исчезает вместе с каталогом.
#[derive(Debug)]
struct Conversion {
    _workspace: Workspace,
    output: PathBuf,
}

impl Conversion {
    fn output(&self) -> &Path {
        &self.output
    }
}

/// Гоняет файл через LibreOffice и возвращает путь к произведённому выводу.
fn convert(soffice: &Path, input: &Path, kind: Kind) -> Result<Conversion, String> {
    let workspace = Workspace::new("conv")?;
    let outdir = workspace.outdir();

    let mut cmd = Command::new(soffice);
    cmd.arg("--headless")
        .arg(workspace.profile_url())
        .arg("--convert-to")
        .arg(kind.convert_to())
        .arg("--outdir")
        .arg(&outdir)
        .arg(input);

    let output = run_with_timeout(cmd, TIMEOUT)?;
    if !output.status.success() {
        return Err(format!(
            "LibreOffice вернул {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // Имя вывода LibreOffice строит сам; ищем по расширению, а не угадываем.
    let produced = std::fs::read_dir(&outdir)
        .map_err(|e| format!("не читается {}: {e}", outdir.display()))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == kind.out_ext()));

    match produced {
        Some(output_path) => Ok(Conversion {
            _workspace: workspace,
            output: output_path,
        }),
        None => Err(format!(
            "LibreOffice завершился успешно, но не создал *.{} в {}\nstdout: {}",
            kind.out_ext(),
            outdir.display(),
            String::from_utf8_lossy(&output.stdout).trim()
        )),
    }
}

// ---------------------------------------------------------------------------
// Публичная поверхность для будущих вех
// ---------------------------------------------------------------------------

/// Референс чтения для вехи **M8**.
///
/// Читает CSV, произведённый фильтром StarCalc с параметрами `44,34,76`:
/// разделитель — запятая, кавычка — `"`, удвоенная кавычка внутри поля
/// означает саму кавычку, кодировка UTF-8.
///
/// Нечитаемый или отсутствующий файл даёт пустой вектор: вызывающий тест
/// всё равно проверяет непустоту и сообщит понятнее.
fn read_reference_csv(path: &Path) -> Vec<Vec<String>> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    parse_csv(text.strip_prefix('\u{feff}').unwrap_or(&text))
}

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut row_started = false;
    let mut chars = text.chars().peekable();

    // Итератор двигается и внутри тела (заглядывание вперёд после кавычки
    // и после `\r`), поэтому `for` тут не подходит.
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }

        match c {
            '"' => {
                in_quotes = true;
                row_started = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                row_started = true;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                row_started = false;
            }
            _ => {
                field.push(c);
                row_started = true;
            }
        }
    }

    if row_started || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Валидатор записи для вехи **M9**.
///
/// Отдаёт `true`, если LibreOffice открыл наши байты (код возврата 0) и выдал
/// непустой вывод. Пустой вывод считается провалом намеренно: LibreOffice
/// умеет «успешно» конвертировать битый файл в нулевой CSV.
///
/// Без установленного оракула возвращает `false` — вызывать имеет смысл только
/// после проверки, что оракул есть.
fn validates(bytes: &[u8]) -> bool {
    let Some(soffice) = soffice_path() else {
        return false;
    };
    let kind = Kind::sniff(bytes);

    let Ok(workspace) = Workspace::new("valid") else {
        return false;
    };
    let input = workspace
        .scratch()
        .join(format!("candidate.{}", kind.in_ext()));
    if std::fs::write(&input, bytes).is_err() {
        return false;
    }

    match convert(&soffice, &input, kind) {
        Ok(conversion) => std::fs::metadata(conversion.output()).is_ok_and(|m| m.len() > 0),
        Err(e) => {
            eprintln!("validates: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[test]
#[ignore = "требует LibreOffice; включается переменной OOXML_SOFFICE"]
fn smoke_first_xlsx_converts_to_nonempty_csv() {
    let Some(soffice) = oracle() else { return };
    let Some(file) = first_corpus_file("xlsx", "xlsx") else {
        return;
    };

    let conversion = match convert(&soffice, &file, Kind::Spreadsheet) {
        Ok(c) => c,
        Err(e) => panic!("конвертация {} не удалась: {e}", file.display()),
    };

    let size = std::fs::metadata(conversion.output())
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        size > 0,
        "LibreOffice произвёл пустой CSV для {}",
        file.display()
    );

    let rows = read_reference_csv(conversion.output());
    assert!(
        !rows.is_empty(),
        "CSV для {} разобран в ноль строк (размер файла {size} байт)",
        file.display()
    );
    assert!(
        rows.iter().any(|r| r.iter().any(|c| !c.is_empty())),
        "все ячейки CSV для {} пустые",
        file.display()
    );

    eprintln!(
        "оракул: {} → {} строк, {size} байт CSV",
        file.display(),
        rows.len()
    );
}

#[test]
#[ignore = "требует LibreOffice; включается переменной OOXML_SOFFICE"]
fn smoke_first_docx_converts_to_nonempty_txt() {
    let Some(soffice) = oracle() else { return };
    let Some(file) = first_corpus_file("docx", "docx") else {
        return;
    };

    let conversion = match convert(&soffice, &file, Kind::Text) {
        Ok(c) => c,
        Err(e) => panic!("конвертация {} не удалась: {e}", file.display()),
    };

    let size = std::fs::metadata(conversion.output())
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        size > 0,
        "LibreOffice произвёл пустой TXT для {}",
        file.display()
    );
    eprintln!("оракул: {} → {size} байт TXT", file.display());
}

#[test]
#[ignore = "требует LibreOffice; включается переменной OOXML_SOFFICE"]
fn validates_accepts_untouched_corpus_file() {
    if oracle().is_none() {
        return;
    }
    let Some(file) = first_corpus_file("xlsx", "xlsx") else {
        return;
    };
    let Ok(bytes) = std::fs::read(&file) else {
        eprintln!("ПРЕДУПРЕЖДЕНИЕ: не читается {}", file.display());
        return;
    };

    assert_eq!(
        Kind::sniff(&bytes),
        Kind::Spreadsheet,
        "{} опознан не как книга",
        file.display()
    );
    assert!(
        validates(&bytes),
        "оракул отверг нетронутый файл корпуса {} — \
         значит сломан харнесс, а не наш писатель",
        file.display()
    );
}

#[test]
#[ignore = "часть харнесса оракула; гоняется вместе с ним"]
fn validates_rejects_garbage() {
    if oracle().is_none() {
        return;
    }
    // Правдоподобная сигнатура ZIP, дальше мусор: LibreOffice обязан
    // споткнуться, а не выдать пустой, но «успешный» результат.
    let mut garbage = b"PK\x03\x04xl/workbook.xml".to_vec();
    garbage.extend(std::iter::repeat_n(0xA5_u8, 4096));
    assert!(
        !validates(&garbage),
        "оракул принял заведомо битые байты — валидатор M9 бесполезен"
    );
}

#[test]
#[ignore = "часть харнесса оракула; гоняется вместе с ним"]
#[cfg(unix)]
fn timeout_kills_a_hanging_process() {
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");

    let started = SystemTime::now();
    let result = run_with_timeout(cmd, Duration::from_millis(300));
    let elapsed = started.elapsed().unwrap_or(Duration::ZERO);

    assert!(
        result.is_err(),
        "зависший процесс не был прерван: {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "ожидание заняло {elapsed:?} — таймаут не сработал"
    );
}

#[test]
#[ignore = "часть харнесса оракула; гоняется вместе с ним"]
fn workspace_is_unique_and_cleans_up() {
    let first = Workspace::new("selftest").expect("рабочий каталог не создан");
    let second = Workspace::new("selftest").expect("рабочий каталог не создан");
    assert_ne!(
        first.outdir(),
        second.outdir(),
        "два прогона получили один каталог — профили LibreOffice столкнутся"
    );
    assert!(
        first
            .profile_url()
            .starts_with("-env:UserInstallation=file:///")
    );

    let root = second.scratch();
    assert!(root.is_dir());
    drop(second);
    assert!(
        !root.exists(),
        "рабочий каталог {} не удалён",
        root.display()
    );
}

#[test]
#[ignore = "часть харнесса оракула; гоняется вместе с ним"]
fn csv_parser_handles_quotes_and_line_endings() {
    let rows = parse_csv("a,b\r\n\"x,y\",\"он сказал \"\"да\"\"\"\r\n,\n");
    assert_eq!(
        rows,
        vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["x,y".to_owned(), "он сказал \"да\"".to_owned()],
            vec![String::new(), String::new()],
        ]
    );

    // Последняя строка без завершающего перевода строки не теряется.
    assert_eq!(parse_csv("1,2"), vec![vec!["1".to_owned(), "2".to_owned()]]);
    assert!(parse_csv("").is_empty());
}
