//! Мост между ядром и браузером.
//!
//! # Почему без wasm-bindgen
//!
//! Ноль внешних зависимостей — жёсткое требование проекта, а `wasm-bindgen`
//! это зависимость с процедурными макросами и своим кодогенератором. Здесь
//! вместо него голый C-ABI: несколько `extern "C"`-функций и договорённость,
//! что сложные ответы отдаются как байты в линейной памяти. JS читает их
//! напрямую из `WebAssembly.Memory`.
//!
//! Цена — ручное управление памятью на границе; выгода — модуль собирается
//! обычным `cargo build --target wasm32-unknown-unknown` без единого
//! инструмента сверх штатного тулчейна.
//!
//! # Договорённость о вызовах
//!
//! 1. JS просит буфер: [`ooxml_alloc`] и пишет туда байты файла.
//! 2. JS зовёт [`ooxml_open`] — ядро разбирает пакет и держит его у себя.
//! 3. Любой запрос данных возвращает **дескриптор** — упакованные в `u64`
//!    указатель и длину готового куска в памяти. JS читает его и обязан
//!    освободить через [`ooxml_free`].
//! 4. Данные для отрисовки отдаются одним плоским блоком, а не по ячейке:
//!    вызов через границу wasm стоит дороже самой работы, и на листе в 7000
//!    ячеек поячеечный обход был бы на порядок медленнее.

// Единственное место проекта, где допустим `unsafe`: границу C-ABI иначе не
// выразить. Крейт намеренно тонкий — вся логика в `ooxml`, где `unsafe`
// по-прежнему запрещён.
#![allow(clippy::missing_safety_doc)]

use core::mem;
use ooxml::Limits;
use ooxml::xlsx::{Appearance, CellValue, Color, HAlign, VAlign, Workbook};

/// Открытая книга живёт между вызовами. Однопоточный wasm, поэтому
/// глобальное состояние здесь безопасно и избавляет JS от работы с
/// «сырыми» указателями на объект.
static mut STATE: Option<State> = None;

struct State {
    /// Байты файла. Держатся живыми: модель ссылается на них спанами.
    bytes: Vec<u8>,
    /// Последний собранный ответ. Хранится, чтобы JS успел его прочитать.
    out: Vec<u8>,
    error: String,
}

/// Упаковывает указатель и длину в одно значение: `(ptr << 32) | len`.
///
/// Возврат структуры через C-ABI в wasm требует скрытого выходного параметра;
/// одно число проще и на стороне JS читается двумя сдвигами.
const fn pack(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | (len as u64)
}

/// Выделяет буфер, которым распоряжается JS.
#[unsafe(no_mangle)]
pub extern "C" fn ooxml_alloc(len: u32) -> u32 {
    let mut v = Vec::<u8>::with_capacity(len as usize);
    let p = v.as_mut_ptr() as u32;
    mem::forget(v);
    p
}

/// Освобождает буфер, выданный [`ooxml_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ooxml_free(ptr: u32, len: u32) {
    if ptr == 0 {
        return;
    }
    // SAFETY: указатель и длина получены из `ooxml_alloc`, вызывающий обязан
    // передать их без изменений и ровно один раз.
    drop(unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, len as usize) });
}

fn state() -> Option<&'static mut State> {
    // SAFETY: wasm однопоточен, одновременных заимствований не возникает.
    unsafe { (&raw mut STATE).as_mut().and_then(Option::as_mut) }
}

fn fail(msg: &str) -> u64 {
    // SAFETY: см. `state`.
    let slot = unsafe { (&raw mut STATE).as_mut() };
    if let Some(s) = slot {
        if let Some(st) = s.as_mut() {
            st.error = msg.to_owned();
        } else {
            *s = Some(State {
                bytes: Vec::new(),
                out: Vec::new(),
                error: msg.to_owned(),
            });
        }
    }
    0
}

/// Принимает файл. `ptr`/`len` — буфер от [`ooxml_alloc`], владение переходит сюда.
///
/// Возвращает 1 при успехе, 0 при ошибке; текст ошибки — через [`ooxml_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ooxml_open(ptr: u32, len: u32) -> u32 {
    // SAFETY: буфер выдан `ooxml_alloc` с той же длиной.
    let bytes = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };

    // SAFETY: см. `state`.
    unsafe {
        (&raw mut STATE).write(Some(State {
            bytes,
            out: Vec::new(),
            error: String::new(),
        }));
    }

    let Some(st) = state() else { return 0 };
    match Workbook::open(&st.bytes) {
        Ok(_) => 1,
        Err(e) => {
            st.error = e.to_string();
            0
        }
    }
}

/// Текст последней ошибки как дескриптор `(ptr<<32)|len`.
#[unsafe(no_mangle)]
pub extern "C" fn ooxml_error() -> u64 {
    let Some(st) = state() else { return 0 };
    st.out = st.error.clone().into_bytes();
    pack(st.out.as_ptr() as u32, st.out.len() as u32)
}

/// Отдаёт лист в виде плоского блока для отрисовки.
///
/// Формат — самоописывающийся бинарный поток, чтобы JS не разбирал JSON на
/// 7000 ячеек. Все числа little-endian.
///
/// ```text
/// u32 rows | u32 cols
/// u32 n_widths  , затем n_widths × f32   — ширины столбцов в пикселях
/// u32 n_heights , затем n_heights × f32  — высоты строк в пикселях
/// u32 n_merges  , затем n_merges × 4×u32 — r0,c0,r1,c1
/// u32 n_cells   , затем n_cells × запись:
///     u32 row | u32 col | u32 style | u8 kind | u32 text_len | text_len байт UTF-8
/// ```
/// `kind`: 0 — пусто, 1 — число, 2 — текст, 3 — логическое, 4 — ошибка.
#[unsafe(no_mangle)]
pub extern "C" fn ooxml_sheet(index: u32) -> u64 {
    let Some(st) = state() else { return 0 };
    let bytes = mem::take(&mut st.bytes);

    let result = build_sheet(&bytes, index as usize);
    st.bytes = bytes;

    match result {
        Ok(blob) => {
            st.out = blob;
            pack(st.out.as_ptr() as u32, st.out.len() as u32)
        }
        Err(e) => fail(&e),
    }
}

fn build_sheet(bytes: &[u8], index: usize) -> Result<Vec<u8>, String> {
    let mut wb = Workbook::open_with_limits(bytes, Limits::strict()).map_err(|e| e.to_string())?;
    let mut sheet = wb.sheet(index).map_err(|e| e.to_string())?;

    let cells = sheet.read_all().map_err(|e| e.to_string())?;
    let merges = sheet.merges().map_err(|e| e.to_string())?;
    let layout = sheet.layout().map_err(|e| e.to_string())?;

    let mut rows = 0u32;
    let mut cols = 0u32;
    for c in &cells {
        // Размах считается и по оформленным пустым ячейкам: обрезав лист по
        // последнему значению, мы отрезали бы правый и нижний край бланка.
        if matches!(c.value, CellValue::Empty) && c.formula.is_none() && c.style.is_none() {
            continue;
        }
        rows = rows.max(c.at.row.saturating_add(1));
        cols = cols.max(c.at.col.saturating_add(1));
    }

    let mut o = Vec::with_capacity(64 * 1024);
    o.extend_from_slice(&rows.to_le_bytes());
    o.extend_from_slice(&cols.to_le_bytes());

    o.extend_from_slice(&cols.to_le_bytes());
    for c in 0..cols {
        let px = layout
            .col_width(c)
            .map_or(64.0, |w| ooxml::xlsx::col_width_px(w, 7.0));
        o.extend_from_slice(&(px as f32).to_le_bytes());
    }

    o.extend_from_slice(&rows.to_le_bytes());
    for r in 0..rows {
        let px = layout
            .row_height(r)
            .map_or(20.0, ooxml::xlsx::row_height_px);
        o.extend_from_slice(&(px as f32).to_le_bytes());
    }

    let visible: Vec<_> = merges
        .iter()
        .filter(|m| m.from.row < rows && m.from.col < cols)
        .collect();
    o.extend_from_slice(&(visible.len() as u32).to_le_bytes());
    for m in visible {
        for v in [m.from.row, m.from.col, m.to.row, m.to.col] {
            o.extend_from_slice(&v.to_le_bytes());
        }
    }

    // Пустая ячейка со стилем — не мусор, а носитель оформления: рамки
    // бланка живут именно в них (`<c r="A20" s="17"/>` без значения). По
    // корпусу непустых лишь 14 617 из 232 140, и отбросив остальные, мы
    // выбросили бы все квадратики формы вместе с ними.
    let filled: Vec<_> = cells
        .iter()
        .filter(|c| !matches!(c.value, CellValue::Empty) || c.style.is_some())
        .collect();
    o.extend_from_slice(&(filled.len() as u32).to_le_bytes());
    for c in filled {
        o.extend_from_slice(&c.at.row.to_le_bytes());
        o.extend_from_slice(&c.at.col.to_le_bytes());
        o.extend_from_slice(&c.style.unwrap_or(0).to_le_bytes());
        let (kind, text) = match &c.value {
            CellValue::Empty => (0u8, String::new()),
            CellValue::Number(v) => (1, format_number(*v)),
            CellValue::Text(s) => (2, s.clone()),
            CellValue::Bool(b) => (3, (if *b { "ИСТИНА" } else { "ЛОЖЬ" }).to_owned()),
            CellValue::Error(e) => (4, format!("{e:?}")),
        };
        o.push(kind);
        o.extend_from_slice(&(text.len() as u32).to_le_bytes());
        o.extend_from_slice(text.as_bytes());
    }
    Ok(o)
}

/// То же округление до 15 значащих цифр, что и в HTML-экспорте: иначе
/// `0.1 + 0.2` показалось бы как `0.30000000000000004`.
fn format_number(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    let v = format!("{v:.14e}").parse::<f64>().unwrap_or(v);
    format!("{v}")
}

/// Отдаёт оформление книги одним блоком: по записи на стиль.
///
/// ```text
/// u32 n_styles, затем n_styles × запись:
///   u8  flags        — бит0 жирный, бит1 курсив, бит2 перенос
///   f32 font_size_px
///   u32 color_rgb    — 0xFF000000 означает «цвет по контексту»
///   u32 fill_rgb     — то же
///   u8  h_align      — 0 обычное, 1 влево, 2 центр, 3 вправо, 4 по ширине
///   u8  v_align      — 0 верх, 1 центр, 2 низ
///   4 × (u8 style, u32 rgb) — левая, правая, верхняя, нижняя грань
///   u8 name_len, затем name_len байт имени шрифта
/// ```
/// Грань: 0 нет, 1 тонкая, 2 средняя, 3 толстая, 4 двойная, 5 пунктир, 6 точки.
#[unsafe(no_mangle)]
pub extern "C" fn ooxml_styles() -> u64 {
    let Some(st) = state() else { return 0 };
    let bytes = mem::take(&mut st.bytes);
    let result = build_styles(&bytes);
    st.bytes = bytes;
    match result {
        Ok(blob) => {
            st.out = blob;
            pack(st.out.as_ptr() as u32, st.out.len() as u32)
        }
        Err(e) => fail(&e),
    }
}

const NO_COLOR: u32 = 0xFF00_0000;

fn rgb_of(c: Option<Color>) -> u32 {
    c.and_then(Color::to_css)
        .and_then(|s| u32::from_str_radix(s.trim_start_matches('#'), 16).ok())
        .unwrap_or(NO_COLOR)
}

fn build_styles(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut wb = Workbook::open_with_limits(bytes, Limits::strict()).map_err(|e| e.to_string())?;
    let ap: Appearance = wb
        .appearance()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    let n = ap.xf_count() as u32;
    let mut o = Vec::with_capacity(n as usize * 40);
    o.extend_from_slice(&n.to_le_bytes());

    for i in 0..n {
        let font = ap.font_of(i);
        let xf = ap.xf(i);

        let mut flags = 0u8;
        if font.is_some_and(|f| f.bold) {
            flags |= 1;
        }
        if font.is_some_and(|f| f.italic) {
            flags |= 2;
        }
        if xf.is_some_and(|x| x.wrap) {
            flags |= 4;
        }
        o.push(flags);

        // Пункты в пиксели: 96 dpi против 72 pt на дюйм.
        let size_px = font.and_then(|f| f.size).unwrap_or(11.0) * 4.0 / 3.0;
        o.extend_from_slice(&(size_px as f32).to_le_bytes());
        o.extend_from_slice(&rgb_of(font.and_then(|f| f.color)).to_le_bytes());
        o.extend_from_slice(&rgb_of(ap.fill_of(i).and_then(|f| f.solid)).to_le_bytes());

        o.push(match xf.map(|x| x.h_align) {
            Some(HAlign::Left) => 1,
            Some(HAlign::Center) => 2,
            Some(HAlign::Right) => 3,
            Some(HAlign::Justify) => 4,
            _ => 0,
        });
        o.push(match xf.map(|x| x.v_align) {
            Some(VAlign::Top) => 0,
            Some(VAlign::Center) => 1,
            _ => 2,
        });

        let b = ap.borders_of(i).copied().unwrap_or_default();
        for e in [b.left, b.right, b.top, b.bottom] {
            o.push(border_code(e.style));
            o.extend_from_slice(&rgb_of(e.color).to_le_bytes());
        }

        let name = font.and_then(|f| f.name.as_deref()).unwrap_or("");
        let name = name.get(..name.len().min(255)).unwrap_or("");
        o.push(name.len() as u8);
        o.extend_from_slice(name.as_bytes());
    }
    Ok(o)
}

fn border_code(s: ooxml::xlsx::BorderStyle) -> u8 {
    use ooxml::xlsx::BorderStyle as B;
    match s {
        B::None => 0,
        B::Hair | B::Thin => 1,
        B::Medium => 2,
        B::Thick => 3,
        B::Double => 4,
        B::Dashed => 5,
        B::Dotted => 6,
    }
}

/// Записывает значение в ячейку и сразу пересобирает файл.
///
/// `ptr`/`len` — введённый текст в UTF-8. Пустая строка очищает ячейку.
/// Текст, разбирающийся как число, пишется числом — так же, как это делает
/// сам Excel при вводе.
///
/// Файл пересобирается на каждую правку целиком. Это выглядит расточительно,
/// но на деле стоит единицы миллисекунд и снимает главный источник ошибок:
/// состояние всегда ровно одно — байты файла. Держать открытую книгу между
/// вызовами нельзя технически (она заимствует эти байты), а копить правки в
/// стороне значило бы завести второй источник истины.
///
/// Возвращает 1 при успехе, 0 при ошибке.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ooxml_set(sheet: u32, row: u32, col: u32, ptr: u32, len: u32) -> u32 {
    let text = if len == 0 {
        String::new()
    } else {
        // SAFETY: буфер выдан `ooxml_alloc`, длина та же.
        let raw = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
        match core::str::from_utf8(raw) {
            Ok(s) => s.to_owned(),
            Err(_) => return fail("значение не UTF-8") as u32,
        }
    };

    let Some(st) = state() else { return 0 };
    let bytes = mem::take(&mut st.bytes);
    match apply_edit(&bytes, sheet as usize, row, col, &text) {
        Ok(next) => {
            st.bytes = next;
            1
        }
        Err(e) => {
            st.bytes = bytes;
            st.error = e;
            0
        }
    }
}

fn apply_edit(
    bytes: &[u8],
    sheet: usize,
    row: u32,
    col: u32,
    text: &str,
) -> Result<Vec<u8>, String> {
    let mut wb = Workbook::open_with_limits(bytes, Limits::strict()).map_err(|e| e.to_string())?;
    let at = ooxml::xlsx::CellRef::checked(row, col).map_err(|e| e.to_string())?;
    {
        let mut sh = wb.sheet(sheet).map_err(|e| e.to_string())?;
        let t = text.trim();
        if t.is_empty() {
            sh.clear(at).map_err(|e| e.to_string())?;
        } else if let Ok(v) = t.parse::<f64>() {
            // `parse::<f64>` принимает "inf" и "nan"; в таблице это текст.
            if v.is_finite() {
                sh.set_number(at, v).map_err(|e| e.to_string())?;
            } else {
                sh.set_string(at, text).map_err(|e| e.to_string())?;
            }
        } else if t.eq_ignore_ascii_case("истина") || t.eq_ignore_ascii_case("true") {
            sh.set_bool(at, true).map_err(|e| e.to_string())?;
        } else if t.eq_ignore_ascii_case("ложь") || t.eq_ignore_ascii_case("false") {
            sh.set_bool(at, false).map_err(|e| e.to_string())?;
        } else {
            sh.set_string(at, text).map_err(|e| e.to_string())?;
        }
    }
    wb.save().map_err(|e| e.to_string())
}

/// Отдаёт текущие байты файла — для сохранения на диск.
#[unsafe(no_mangle)]
pub extern "C" fn ooxml_save() -> u64 {
    let Some(st) = state() else { return 0 };
    st.out = st.bytes.clone();
    pack(st.out.as_ptr() as u32, st.out.len() as u32)
}
