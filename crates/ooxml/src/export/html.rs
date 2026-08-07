//! Экспорт книги в самодостаточный HTML.
//!
//! Результат — одна страница без внешних ресурсов: стили встроены, скриптов
//! нет. Это сделано ради того, чтобы файл можно было открыть откуда угодно и
//! чтобы он не тянул за собой ничего, что могло бы не загрузиться.
//!
//! # Что переносится
//!
//! Листы (каждый своей таблицей), объединённые диапазоны (`colspan`/`rowspan`),
//! типы значений и выравнивание по типу — числа вправо, текст влево, логические
//! и ошибки по центру. Формула показывается подсказкой на ячейке, а в таблице
//! стоит её кэшированное значение: вычислителя у ядра нет и не планируется.
//!
//! Переносятся также **объявленные** ширины столбцов и высоты строк. Это не
//! вёрстка, а перенос чисел из файла, и для бланков он решающий: у формы МВД
//! все 68 столбцов шириной 1,33 символа, и без этих чисел она разъезжается в
//! нечитаемую сетку одинаковых клеток.
//!
//! # Что не переносится
//!
//! Шрифты, заливки, рамки, числовые форматы. Всё это живёт в `xl/styles.xml`,
//! модель которого у нас намеренно минимальна — она отвечает на один вопрос
//! («это дата?»), а не описывает оформление.

use core::fmt::Write as _;

use crate::error::Result;
use crate::xlsx::{Appearance, CellRange, CellValue, HAlign, SheetLayout, VAlign, Workbook};

/// Ширина «нулевой» цифры шрифта по умолчанию, в пикселях.
///
/// Единица ширины столбца в Excel — не пиксель и не миллиметр, а ширина цифры
/// шрифта книги. Для Calibri 11 это 7 px; отсюда и переводной коэффициент.
/// Точное совпадение с Excel недостижимо (у нас нет его шрифтов), но без этого
/// перевода бланк с колонками в 1,33 символа разъезжается в нечитаемую сетку.
const PX_PER_CHAR: f64 = 7.0;
/// Постоянная добавка на поля ячейки и линию сетки.
const CELL_PADDING_PX: f64 = 5.0;

/// Ширина столбца Excel в пикселях.
fn col_px(width: f64) -> f64 {
    if width <= 0.0 {
        return 0.0;
    }
    (width * PX_PER_CHAR + CELL_PADDING_PX).round()
}

/// Высота строки: пункты в пиксели, 96 dpi против 72 pt на дюйм.
fn row_px(points: f64) -> f64 {
    (points * 4.0 / 3.0).round()
}

/// Настройки вывода.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HtmlOptions {
    /// Показывать заголовки строк и столбцов, как в Excel.
    pub headers: bool,
    /// Показывать скрытые листы.
    pub hidden_sheets: bool,
    /// Потолок на число строк одного листа. Защита от того, что лист с
    /// объявленным размахом до миллиона строк развернётся в гигабайтную
    /// страницу; данных там столько нет, а разметка сгенерируется вся.
    pub max_rows: u32,
    /// Потолок на число столбцов одного листа.
    pub max_cols: u32,
    /// Заголовок страницы.
    pub title: String,
}

impl Default for HtmlOptions {
    fn default() -> Self {
        Self {
            headers: true,
            hidden_sheets: false,
            max_rows: 5_000,
            max_cols: 256,
            title: "Книга".to_owned(),
        }
    }
}

/// Экранирует текст для вставки в HTML.
fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn escaped(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    escape(s, &mut o);
    o
}

/// Буква столбца по индексу: 0 → A, 25 → Z, 26 → AA.
fn col_letter(mut col: u32) -> String {
    let mut buf = Vec::new();
    loop {
        buf.push(b'A'.saturating_add((col % 26) as u8));
        if col < 26 {
            break;
        }
        // Схема Excel не позиционная: после Z идёт AA, а не BA. Вычитание
        // единицы и есть эта поправка; `col >= 26` гарантировано проверкой выше.
        col = (col / 26).saturating_sub(1);
    }
    buf.reverse();
    String::from_utf8_lossy(&buf).into_owned()
}

/// Печатает число так, как его показал бы Excel при формате «Общий».
///
/// `f64` по умолчанию печатается либо в экспоненте, либо с хвостом вида
/// `0.30000000000000004`. Ни то, ни другое в таблице видеть не хочется, поэтому
/// берётся кратчайшее представление, которое читается обратно в то же число.
fn number(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    // Excel в формате «Общий» показывает 15 значащих цифр. Без этого
    // округления сумма 0.1 + 0.2 попала бы в таблицу как 0.30000000000000004:
    // значение честное, но в Excel пользователь такого не видит, и таблица
    // выглядела бы сломанной там, где всё правильно.
    let v = format!("{v:.14e}").parse::<f64>().unwrap_or(v);
    let mut s = format!("{v}");
    if s.contains('e') || s.contains('E') {
        s = format!("{v:.10}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Одна ячейка выходной таблицы.
#[derive(Default, Clone)]
struct Slot {
    text: String,
    class: &'static str,
    title: Option<String>,
    span: Option<(u32, u32)>,
    /// Индекс в `cellXfs`. Из него получается класс оформления.
    style: Option<u32>,
    /// Ячейка накрыта объединением сверху или слева — её не выводим вовсе.
    covered: bool,
}

const STYLE: &str = "\
:root{color-scheme:light dark}
*{box-sizing:border-box}
body{margin:0;padding:24px;font:14px/1.45 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;
background:#fbfbfa;color:#1b1b19}
h1{font-size:20px;margin:0 0 4px}
.meta{color:#6b6b66;font-size:13px;margin-bottom:20px}
.sheet{margin-bottom:40px}
.sheet h2{font-size:15px;margin:0 0 8px;font-weight:600}
.sheet h2 .n{color:#6b6b66;font-weight:400;font-size:13px;margin-left:8px}
.wrap{overflow-x:auto;border:1px solid #e0dfda;border-radius:6px;background:#fff}
table{border-collapse:collapse;font-size:13px;table-layout:fixed}
th,td{padding:1px 3px;white-space:nowrap;vertical-align:bottom;position:relative}
/* Excel даёт тексту вытекать в соседние пустые ячейки, и заголовки листа на
   этом держатся. Обрезка превратила бы «Настоящее уведомление...» в «Н». */
td{overflow:visible}
td.ww{overflow:hidden;white-space:normal}
td.clip{overflow:hidden}
table.grid td{border:1px solid #eceae4}
/* Лист — это бумага: у неё свой цвет, заданный документом. Инвертировать её
   в тёмной теме — то же самое, что печатать негатив. */
.wrap{background:#fff;color:#111}
.paper{background:#fff;color:#111}
thead th,tbody th{background:#f5f4f0;color:#6b6b66;font-weight:500;text-align:center;
border:1px solid #eceae4;position:sticky;top:0;z-index:1}
tbody th{position:static;min-width:44px}
/* Типовое выравнивание — только запасной вариант. Селекторы намеренно
   слабее, чем `.xN` со стилями книги: иначе они перебивали бы
   выравнивание, заданное в самом документе. */
.n{text-align:right;font-variant-numeric:tabular-nums}
.s{text-align:left}
.b,.e{text-align:center}
.e{color:#a3372a}
td.ff{background:#fcfbf5}
td.ff::after{content:'ƒ';float:right;color:#b8b096;font-size:10px;margin-left:6px}
.empty{color:#9a9a93;font-style:italic;padding:12px}
@media (prefers-color-scheme:dark){
body{background:#191917;color:#e8e6e1}
.wrap{border-color:#33322e}
.meta,.sheet h2 .n{color:#98968f}
}";

/// Собирает CSS-классы по таблице стилей книги.
///
/// Класс на стиль, а не инлайновый `style` на ячейку: стилей в файле 127, а
/// ячеек 7000, и разница в размере страницы получается кратной.
fn style_classes(ap: &Appearance) -> String {
    let mut css = String::new();
    for i in 0..ap.xf_count() {
        let idx = i as u32;
        let mut decls = String::new();

        if let Some(f) = ap.font_of(idx) {
            if f.bold {
                decls.push_str("font-weight:700;");
            }
            if f.italic {
                decls.push_str("font-style:italic;");
            }
            if f.underline || f.strike {
                let _ = write!(
                    decls,
                    "text-decoration:{}{};",
                    if f.underline { "underline " } else { "" },
                    if f.strike { "line-through" } else { "" }
                );
            }
            if let Some(sz) = f.size {
                let _ = write!(decls, "font-size:{}px;", (sz * 4.0 / 3.0).round());
            }
            if let Some(n) = &f.name {
                let _ = write!(decls, "font-family:'{}',sans-serif;", n.replace('\'', ""));
            }
            if let Some(c) = f.color.and_then(super::super::xlsx::Color::to_css) {
                let _ = write!(decls, "color:{c};");
            }
        }

        if let Some(fill) = ap.fill_of(idx)
            && let Some(c) = fill.solid.and_then(super::super::xlsx::Color::to_css)
        {
            let _ = write!(decls, "background:{c};");
        }

        if let Some(b) = ap.borders_of(idx) {
            for (name, edge) in [
                ("left", b.left),
                ("right", b.right),
                ("top", b.top),
                ("bottom", b.bottom),
            ] {
                if let Some(line) = edge.style.to_css() {
                    let c = edge
                        .color
                        .and_then(super::super::xlsx::Color::to_css)
                        .unwrap_or_else(|| "currentColor".to_owned());
                    let _ = write!(decls, "border-{name}:{line} {c};");
                }
            }
        }

        if let Some(xf) = ap.xf(idx) {
            match xf.h_align {
                HAlign::Left => decls.push_str("text-align:left;"),
                HAlign::Center => decls.push_str("text-align:center;"),
                HAlign::Right => decls.push_str("text-align:right;"),
                HAlign::Justify => decls.push_str("text-align:justify;"),
                HAlign::General => {}
            }
            match xf.v_align {
                VAlign::Top => decls.push_str("vertical-align:top;"),
                VAlign::Center => decls.push_str("vertical-align:middle;"),
                VAlign::Bottom => decls.push_str("vertical-align:bottom;"),
            }
            // Перенос по словам ставится классом `wrap` на самой ячейке:
            // вместе с ним включается и обрезка, иначе перенесённый текст
            // вылезал бы на строки ниже.
        }

        if !decls.is_empty() {
            let _ = writeln!(css, ".x{idx}{{{decls}}}");
        }
    }
    css
}

/// Превращает книгу в самодостаточную HTML-страницу.
///
/// # Errors
///
/// Любая ошибка чтения книги: распаковка, XML, модель.
pub fn workbook_to_html(wb: &mut Workbook<'_>, opts: &HtmlOptions) -> Result<String> {
    let mut out = String::with_capacity(64 * 1024);

    out.push_str("<!doctype html>\n<html lang=\"ru\"><head><meta charset=\"utf-8\">\n");
    out.push_str(
        "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>",
    );
    escape(&opts.title, &mut out);
    out.push_str("</title>\n<style>");
    out.push_str(STYLE);
    let appearance = wb.appearance()?;
    if let Some(ap) = &appearance {
        out.push('\n');
        out.push_str(&style_classes(ap));
    }
    out.push_str("</style>\n</head>\n<body>\n<h1>");
    escape(&opts.title, &mut out);
    out.push_str("</h1>\n");

    let sheet_count = wb.sheets().len();
    let _ = writeln!(out, "<p class=\"meta\">Листов: {sheet_count}</p>");

    for i in 0..sheet_count {
        let (name, visible) = {
            let Some(m) = wb.sheets().get(i) else {
                continue;
            };
            (
                m.name.clone(),
                matches!(m.state, crate::xlsx::SheetState::Visible),
            )
        };
        if !visible && !opts.hidden_sheets {
            continue;
        }

        let mut sheet = wb.sheet(i)?;
        let cells = sheet.read_all()?;
        let merges = sheet.merges()?;
        let layout = sheet.layout()?;
        let grid = sheet.shows_grid_lines()?;

        out.push_str("<section class=\"sheet\">\n<h2>");
        escape(&name, &mut out);
        let _ = writeln!(
            out,
            "<span class=\"n\">{} {}</span></h2>",
            cells.len(),
            plural_cells(cells.len())
        );

        match render_sheet(&cells, &merges, &layout, grid, appearance.as_ref(), opts) {
            Some(table) => out.push_str(&table),
            None => out.push_str("<div class=\"wrap\"><p class=\"empty\">Лист пуст</p></div>\n"),
        }
        out.push_str("</section>\n");
    }

    out.push_str("</body></html>\n");
    Ok(out)
}

fn plural_cells(n: usize) -> &'static str {
    let (d10, d100) = (n % 10, n % 100);
    if d10 == 1 && d100 != 11 {
        "ячейка"
    } else if (2..=4).contains(&d10) && !(12..=14).contains(&d100) {
        "ячейки"
    } else {
        "ячеек"
    }
}

/// Раскладывает ячейки в сетку и печатает таблицу. `None` — данных нет.
fn render_sheet(
    cells: &[crate::xlsx::Cell],
    merges: &[CellRange],
    layout: &SheetLayout,
    show_grid: bool,
    ap: Option<&Appearance>,
    opts: &HtmlOptions,
) -> Option<String> {
    // Размах считается по фактическим данным, а не по объявленному
    // `<dimension>`: он бывает завышен, и тогда страница распухла бы пустотой.
    let mut max_row = 0u32;
    let mut max_col = 0u32;
    let mut any = false;
    for c in cells {
        if matches!(c.value, CellValue::Empty) && c.formula.is_none() {
            continue;
        }
        any = true;
        max_row = max_row.max(c.at.row);
        max_col = max_col.max(c.at.col);
    }
    if !any {
        return None;
    }

    // Лист со снятой сеткой — это свёрстанный документ, а не вид таблицы;
    // буквы столбцов и номера строк в нём такой же посторонний элемент, как
    // линейка поверх страницы.
    let headers = opts.headers && show_grid;

    let rows = (max_row.saturating_add(1)).min(opts.max_rows) as usize;
    let cols = (max_col.saturating_add(1)).min(opts.max_cols) as usize;
    let mut grid = vec![Slot::default(); rows.saturating_mul(cols)];

    let idx = |r: usize, c: usize| r.saturating_mul(cols).saturating_add(c);

    for cell in cells {
        let (r, c) = (cell.at.row as usize, cell.at.col as usize);
        if r >= rows || c >= cols {
            continue;
        }
        let Some(slot) = grid.get_mut(idx(r, c)) else {
            continue;
        };
        let (text, class) = match &cell.value {
            CellValue::Empty => (String::new(), "s"),
            CellValue::Number(v) => (number(*v), "n"),
            CellValue::Text(s) => (s.clone(), "s"),
            CellValue::Bool(b) => ((if *b { "ИСТИНА" } else { "ЛОЖЬ" }).to_owned(), "b"),
            CellValue::Error(e) => (format!("{e:?}"), "e"),
        };
        slot.text = text;
        slot.class = class;
        slot.style = cell.style;
        if let Some(f) = &cell.formula {
            slot.title = Some(format!("={}", f.text));
        }
    }

    // Объединения: верхняя левая ячейка получает span, накрытые помечаются.
    for m in merges {
        let (r0, c0) = (m.from.row as usize, m.from.col as usize);
        let (r1, c1) = (m.to.row as usize, m.to.col as usize);
        if r0 >= rows || c0 >= cols {
            continue;
        }
        let r1 = r1.min(rows.saturating_sub(1));
        let c1 = c1.min(cols.saturating_sub(1));
        if let Some(s) = grid.get_mut(idx(r0, c0)) {
            s.span = Some((
                (r1.saturating_sub(r0).saturating_add(1)) as u32,
                (c1.saturating_sub(c0).saturating_add(1)) as u32,
            ));
        }
        for r in r0..=r1 {
            for c in c0..=c1 {
                if r == r0 && c == c0 {
                    continue;
                }
                if let Some(s) = grid.get_mut(idx(r, c)) {
                    s.covered = true;
                }
            }
        }
    }

    // Excel даёт тексту вытекать вправо только пока соседние ячейки пусты;
    // как только справа появляется содержимое, текст обрезается. Без этого
    // правила подписи налезают друг на друга.
    let mut clip = vec![false; rows.saturating_mul(cols)];
    for r in 0..rows {
        for c in 0..cols {
            let occupied = grid
                .get(idx(r, c))
                .is_some_and(|s| !s.text.is_empty() && !s.covered);
            if !occupied {
                continue;
            }
            let next = c.saturating_add(1);
            if next < cols
                && grid
                    .get(idx(r, next))
                    .is_some_and(|s| !s.text.is_empty() || s.covered)
                && let Some(f) = clip.get_mut(idx(r, c))
            {
                *f = true;
            }
        }
    }

    let mut out = String::with_capacity(rows.saturating_mul(cols).saturating_mul(24));
    // Суммарная ширина обязана быть проставлена явно. `table-layout:fixed`
    // при `width:auto` Chrome применяет непоследовательно: он игнорирует
    // colgroup и раздувает столбцы по содержимому, из-за чего лист в 68 узких
    // колонок уезжает за экран и выглядит совершенно иначе, чем в Excel.
    let head_px = if headers { 46.0 } else { 0.0 };
    let total_px: f64 = head_px
        + (0..cols)
            .map(|c| layout.col_width(c as u32).map_or(64.0, col_px))
            .sum::<f64>();

    // Сетку рисуем только там, где её рисует Excel. Иначе бланк, у которого
    // `showGridLines="false"`, тонет в сплошной сетке, и все его рамки —
    // единственное, что делает форму формой, — перестают читаться.
    let _ = write!(
        out,
        "<div class=\"wrap\"><table class=\"{}\" style=\"width:{total_px}px\">\n",
        if show_grid { "grid" } else { "plain" }
    );

    // Ширины объявлены в файле — переносим их. `table-layout:fixed` нужен,
    // чтобы браузер их слушался: при автоматической раскладке он растянет
    // столбцы по содержимому и бланк перестанет быть бланком.
    out.push_str("<colgroup>");
    if headers {
        out.push_str("<col style=\"width:46px\">");
    }
    for c in 0..cols {
        match layout.col_width(c as u32) {
            Some(w) => {
                let _ = write!(out, "<col style=\"width:{}px\">", col_px(w));
            }
            None => out.push_str("<col>"),
        }
    }
    out.push_str("</colgroup>\n");

    if headers {
        out.push_str("<thead><tr><th></th>");
        for c in 0..cols {
            let _ = write!(out, "<th>{}</th>", col_letter(c as u32));
        }
        out.push_str("</tr></thead>\n");
    }

    out.push_str("<tbody>\n");
    for r in 0..rows {
        match layout.row_height(r as u32) {
            Some(h) => {
                let _ = write!(out, "<tr style=\"height:{}px\">", row_px(h));
            }
            None => out.push_str("<tr>"),
        }
        if headers {
            let _ = write!(out, "<th>{}</th>", r.saturating_add(1));
        }
        for c in 0..cols {
            let Some(slot) = grid.get(idx(r, c)) else {
                continue;
            };
            if slot.covered {
                continue;
            }
            let mut class = slot.class.to_owned();
            if slot.title.is_some() {
                class.push_str(" ff");
            }
            if let Some(sx) = slot.style {
                let _ = write!(class, " x{sx}");
                if ap.is_some_and(|a| a.xf(sx).is_some_and(|x| x.wrap)) {
                    class.push_str(" ww");
                }
            }
            if clip.get(idx(r, c)).copied().unwrap_or(false) {
                class.push_str(" clip");
            }
            out.push_str("<td class=\"");
            out.push_str(&class);
            out.push('"');
            if let Some((rs, cs)) = slot.span {
                if rs > 1 {
                    let _ = write!(out, " rowspan=\"{rs}\"");
                }
                if cs > 1 {
                    let _ = write!(out, " colspan=\"{cs}\"");
                }
            }
            if let Some(t) = &slot.title {
                let _ = write!(out, " title=\"{}\"", escaped(t));
            }
            out.push('>');
            escape(&slot.text, &mut out);
            out.push_str("</td>");
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody></table></div>\n");

    if max_row.saturating_add(1) > opts.max_rows || max_col.saturating_add(1) > opts.max_cols {
        let _ = writeln!(
            out,
            "<p class=\"meta\">Показано {rows}×{cols} из {}×{}</p>",
            max_row.saturating_add(1),
            max_col.saturating_add(1)
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn column_letters_follow_excel() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
        assert_eq!(col_letter(51), "AZ");
        assert_eq!(col_letter(52), "BA");
        assert_eq!(col_letter(16383), "XFD");
    }

    #[test]
    fn numbers_print_without_float_noise() {
        assert_eq!(number(1.0), "1");
        assert_eq!(number(-3.0), "-3");
        assert_eq!(number(1.5), "1.5");
        // Классический артефакт двоичной дроби не должен попасть в таблицу.
        assert_eq!(number(0.1 + 0.2), "0.3");
        assert_eq!(number(1e20), "100000000000000000000");
    }

    #[test]
    fn escaping_closes_the_obvious_holes() {
        let mut o = String::new();
        escape("<b>&\"x\"</b>", &mut o);
        assert_eq!(o, "&lt;b&gt;&amp;&quot;x&quot;&lt;/b&gt;");
    }

    #[test]
    fn plural_matches_russian_rules() {
        assert_eq!(plural_cells(1), "ячейка");
        assert_eq!(plural_cells(2), "ячейки");
        assert_eq!(plural_cells(5), "ячеек");
        assert_eq!(plural_cells(11), "ячеек");
        assert_eq!(plural_cells(21), "ячейка");
        assert_eq!(plural_cells(112), "ячеек");
    }
}
