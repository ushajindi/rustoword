//! Оформление ячеек: шрифты, заливки, рамки, выравнивание.
//!
//! Отделено от [`Styles`](crate::xlsx::Styles), которая отвечает на единственный
//! вопрос «это дата?» и нужна при чтении значений. Оформление при чтении не
//! нужно вовсе, поэтому оно живёт отдельно и разбирается только теми, кому
//! действительно надо показать документ.
//!
//! # Почему это важнее, чем кажется
//!
//! Бланк выглядит бланком не из-за содержимого, а из-за рамок. Excel рисует
//! границу только там, где так сказано в стиле ячейки; в файле-примере
//! `showGridLines="false"`, то есть сетки нет вообще, и всё, что видит человек
//! — 23 набора рамок из `xl/styles.xml`. Экспорт, рисующий рамку у каждой
//! ячейки, топит форму в сплошной сетке: содержимое верное, документ
//! неузнаваем.
//!
//! # Чего здесь нет
//!
//! Условное форматирование, градиентные заливки, диагональные рамки, отступы,
//! поворот текста. Всё это встречается редко и не меняет узнаваемости
//! документа; при необходимости добавляется без ломки модели.

use crate::error::Result;
use crate::limits::Limits;
use crate::xlsx::scan::{attr_str, in_sml, local_name, raw_attr};
use crate::xml::{Event, Reader};

/// Цвет в терминах OOXML.
///
/// Прямой RGB встречается реже всего: файлы обычно ссылаются на палитру
/// (`indexed`) или на тему (`theme`). Разрешать их приходится нам.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// `rgb="FFRRGGBB"` — старший байт это альфа, она игнорируется.
    Rgb(u32),
    /// `indexed="N"` — номер в устаревшей палитре на 56 цветов.
    Indexed(u8),
    /// `theme="N"` с необязательным `tint`.
    Theme(u8, f64),
    /// `auto="1"` — цвет по контексту.
    Auto,
}

/// Устаревшая палитра Excel. Индексы 0..7 дублируют 8..15 — так в спецификации.
const INDEXED: [u32; 56] = [
    0x00_0000, 0xFF_FFFF, 0xFF_0000, 0x00_FF00, 0x00_00FF, 0xFF_FF00, 0xFF_00FF, 0x00_FFFF,
    0x00_0000, 0xFF_FFFF, 0xFF_0000, 0x00_FF00, 0x00_00FF, 0xFF_FF00, 0xFF_00FF, 0x00_FFFF,
    0x80_0000, 0x00_8000, 0x00_0080, 0x80_8000, 0x80_0080, 0x00_8080, 0xC0_C0C0, 0x80_8080,
    0x99_99FF, 0x99_3366, 0xFF_FFCC, 0xCC_FFFF, 0x66_0066, 0xFF_8080, 0x00_66CC, 0xCC_CCFF,
    0x00_0080, 0xFF_00FF, 0xFF_FF00, 0x00_FFFF, 0x80_0080, 0x80_0000, 0x00_8080, 0x00_00FF,
    0x00_CCFF, 0xCC_FFFF, 0xCC_FFCC, 0xFF_FF99, 0x99_CCFF, 0xFF_99CC, 0xCC_99FF, 0xFF_CC99,
    0x33_66FF, 0x33_CCCC, 0x99_CC00, 0xFF_CC00, 0xFF_9900, 0xFF_6600, 0x66_6699, 0x96_9696,
];

/// Цвета темы Office по умолчанию, в порядке индексов OOXML.
///
/// Читать их из `xl/theme/theme1.xml` было бы точнее, но тема почти всегда
/// стандартная, а разбор ради этого целого DrawingML-документа не окупается.
const THEME: [u32; 12] = [
    0xFF_FFFF, 0x00_0000, 0xE7_E6E6, 0x44_546A, 0x44_72C4, 0xED_7D31, 0xA5_A5A5, 0xFF_C000,
    0x5B_9BD5, 0x70_AD47, 0x05_63C1, 0x95_4F72,
];

impl Color {
    /// Разрешает цвет в `#rrggbb`. `None` — «как в контексте».
    #[must_use]
    pub fn to_css(self) -> Option<String> {
        let rgb = match self {
            Self::Auto => return None,
            Self::Rgb(v) => v & 0x00FF_FFFF,
            Self::Indexed(i) => match INDEXED.get(usize::from(i)) {
                Some(&v) => v,
                // 64 и 65 — «системный текст» и «системный фон»: их значение
                // задаёт тема оформления, а не файл.
                None => return None,
            },
            Self::Theme(i, tint) => {
                let base = match THEME.get(usize::from(i)) {
                    Some(&v) => v,
                    None => return None,
                };
                apply_tint(base, tint)
            }
        };
        Some(format!("#{rgb:06x}"))
    }
}

/// Осветляет или затемняет цвет по правилу OOXML: положительный tint — к белому.
fn apply_tint(rgb: u32, tint: f64) -> u32 {
    if tint == 0.0 {
        return rgb;
    }
    let ch = |shift: u32| {
        let c = f64::from((rgb >> shift) & 0xFF);
        let v = if tint > 0.0 {
            c * (1.0 - tint) + 255.0 * tint
        } else {
            c * (1.0 + tint)
        };
        (v.clamp(0.0, 255.0) as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// Начертание границы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Hair,
    Thin,
    Medium,
    Thick,
    Double,
    Dashed,
    Dotted,
}

impl BorderStyle {
    fn parse(s: &[u8]) -> Self {
        match s {
            b"hair" => Self::Hair,
            b"thin" => Self::Thin,
            b"medium" | b"mediumDashed" | b"mediumDashDot" | b"mediumDashDotDot" => Self::Medium,
            b"thick" => Self::Thick,
            b"double" => Self::Double,
            b"dashed" | b"dashDot" | b"dashDotDot" | b"slantDashDot" => Self::Dashed,
            b"dotted" => Self::Dotted,
            _ => Self::None,
        }
    }

    /// Толщина и тип линии для CSS. `None` — границы нет.
    #[must_use]
    pub fn to_css(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Hair | Self::Thin => Some("1px solid"),
            Self::Medium => Some("2px solid"),
            Self::Thick => Some("3px solid"),
            Self::Double => Some("3px double"),
            Self::Dashed => Some("1px dashed"),
            Self::Dotted => Some("1px dotted"),
        }
    }
}

/// Одна сторона рамки.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edge {
    pub style: BorderStyle,
    pub color: Option<Color>,
}

/// Рамка ячейки целиком.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Borders {
    pub left: Edge,
    pub right: Edge,
    pub top: Edge,
    pub bottom: Edge,
}

impl Borders {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.left.style == BorderStyle::None
            && self.right.style == BorderStyle::None
            && self.top.style == BorderStyle::None
            && self.bottom.style == BorderStyle::None
    }
}

/// Шрифт.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Font {
    pub name: Option<String>,
    /// Размер в пунктах.
    pub size: Option<f64>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Option<Color>,
}

/// Заливка. Хранится только сплошной цвет: градиенты в офисных документах
/// редки и на узнаваемость не влияют.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Fill {
    pub solid: Option<Color>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HAlign {
    #[default]
    General,
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VAlign {
    Top,
    #[default]
    Bottom,
    Center,
}

/// Запись `cellXfs` — то, на что ссылается атрибут `s` ячейки.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Xf {
    pub font_id: u32,
    pub fill_id: u32,
    pub border_id: u32,
    pub h_align: HAlign,
    pub v_align: VAlign,
    pub wrap: bool,
    /// `applyBorder="false"` означает «рамку из этого стиля не применять».
    pub apply_border: bool,
    pub apply_fill: bool,
    pub apply_font: bool,
}

/// Разобранное оформление книги.
#[derive(Debug, Clone, Default)]
pub struct Appearance {
    fonts: Vec<Font>,
    fills: Vec<Fill>,
    borders: Vec<Borders>,
    xfs: Vec<Xf>,
}

impl Appearance {
    #[must_use]
    pub fn xf_count(&self) -> usize {
        self.xfs.len()
    }

    #[must_use]
    pub fn xf(&self, style: u32) -> Option<&Xf> {
        self.xfs.get(style as usize)
    }

    /// Шрифт для стиля с учётом `applyFont`.
    #[must_use]
    pub fn font_of(&self, style: u32) -> Option<&Font> {
        let xf = self.xf(style)?;
        if !xf.apply_font {
            return None;
        }
        self.fonts.get(xf.font_id as usize)
    }

    #[must_use]
    pub fn fill_of(&self, style: u32) -> Option<&Fill> {
        let xf = self.xf(style)?;
        if !xf.apply_fill {
            return None;
        }
        self.fills.get(xf.fill_id as usize)
    }

    #[must_use]
    pub fn borders_of(&self, style: u32) -> Option<&Borders> {
        let xf = self.xf(style)?;
        if !xf.apply_border {
            return None;
        }
        self.borders.get(xf.border_id as usize)
    }

    /// Шрифт книги по умолчанию — нулевой в таблице шрифтов.
    #[must_use]
    pub fn default_font(&self) -> Option<&Font> {
        self.fonts.first()
    }

    /// Разбирает `xl/styles.xml`.
    ///
    /// # Errors
    ///
    /// Ошибки XML.
    pub fn parse(part: &[u8], limits: &Limits) -> Result<Self> {
        let mut rd = Reader::with_limits(part, limits.clone());
        let mut out = Self::default();

        // В каком из четырёх списков мы сейчас находимся. Различать
        // обязательно: элементы `<xf>` встречаются и в `cellStyleXfs`, и в
        // `cellXfs`, а ячейка ссылается только на второй.
        let mut zone = Zone::None;
        let mut font = Font::default();
        let mut fill = Fill::default();
        let mut borders = Borders::default();
        let mut xf = Xf::default();
        let mut in_font = false;
        let mut in_border_edge: Option<u8> = None;

        loop {
            let ev = rd.next_event()?;
            match ev {
                Event::Start { empty, .. } => {
                    if !in_sml(&rd) {
                        continue;
                    }
                    let name = local_name(&rd);
                    match name {
                        b"fonts" => zone = Zone::Fonts,
                        b"fills" => zone = Zone::Fills,
                        b"borders" => zone = Zone::Borders,
                        b"cellStyleXfs" => zone = Zone::StyleXfs,
                        b"cellXfs" => zone = Zone::CellXfs,

                        b"font" if zone == Zone::Fonts => {
                            font = Font::default();
                            in_font = true;
                        }
                        b"b" if in_font => font.bold = flag(&rd),
                        b"i" if in_font => font.italic = flag(&rd),
                        b"u" if in_font => font.underline = flag(&rd),
                        b"strike" if in_font => font.strike = flag(&rd),
                        b"sz" if in_font => font.size = num(&rd, b"val"),
                        b"name" | b"rFont" if in_font => {
                            font.name = attr_str(&rd, b"val")?.map(str::to_owned);
                        }
                        b"color" if in_font => font.color = color(&rd),

                        b"fill" if zone == Zone::Fills => fill = Fill::default(),
                        b"fgColor" if zone == Zone::Fills => fill.solid = color(&rd),
                        b"patternFill" if zone == Zone::Fills => {
                            // `patternType="none"` — заливки нет, что бы ни
                            // стояло в fgColor.
                            if raw_attr(&rd, b"patternType") == Some(b"none") {
                                fill.solid = None;
                            }
                        }

                        b"border" if zone == Zone::Borders => borders = Borders::default(),
                        b"left" | b"right" | b"top" | b"bottom" if zone == Zone::Borders => {
                            let style = raw_attr(&rd, b"style")
                                .map_or(BorderStyle::None, BorderStyle::parse);
                            let side = match name {
                                b"left" => 0,
                                b"right" => 1,
                                b"top" => 2,
                                _ => 3,
                            };
                            edge_mut(&mut borders, side).style = style;
                            in_border_edge = if empty { None } else { Some(side) };
                        }
                        b"color" => {
                            if let Some(side) = in_border_edge {
                                edge_mut(&mut borders, side).color = color(&rd);
                            }
                        }

                        b"xf" if zone == Zone::CellXfs => {
                            xf = Xf {
                                font_id: num(&rd, b"fontId").unwrap_or(0.0) as u32,
                                fill_id: num(&rd, b"fillId").unwrap_or(0.0) as u32,
                                border_id: num(&rd, b"borderId").unwrap_or(0.0) as u32,
                                // Отсутствующий `applyX` означает «применять».
                                apply_border: !is_false(&rd, b"applyBorder"),
                                apply_fill: !is_false(&rd, b"applyFill"),
                                apply_font: !is_false(&rd, b"applyFont"),
                                ..Xf::default()
                            };
                            if empty {
                                out.xfs.push(xf);
                            }
                        }
                        b"alignment" if zone == Zone::CellXfs => {
                            xf.h_align = match raw_attr(&rd, b"horizontal") {
                                Some(b"left") => HAlign::Left,
                                Some(b"center" | b"centerContinuous") => HAlign::Center,
                                Some(b"right") => HAlign::Right,
                                Some(b"justify" | b"distributed") => HAlign::Justify,
                                _ => HAlign::General,
                            };
                            xf.v_align = match raw_attr(&rd, b"vertical") {
                                Some(b"top") => VAlign::Top,
                                Some(b"center" | b"distributed") => VAlign::Center,
                                _ => VAlign::Bottom,
                            };
                            xf.wrap = matches!(raw_attr(&rd, b"wrapText"), Some(b"1" | b"true"));
                        }
                        _ => {}
                    }
                }
                Event::End { .. } => {
                    if !in_sml(&rd) {
                        continue;
                    }
                    match local_name(&rd) {
                        b"font" if in_font => {
                            in_font = false;
                            out.fonts.push(font.clone());
                        }
                        b"fill" if zone == Zone::Fills => out.fills.push(fill),
                        b"border" if zone == Zone::Borders => out.borders.push(borders),
                        b"left" | b"right" | b"top" | b"bottom" => in_border_edge = None,
                        b"xf" if zone == Zone::CellXfs => out.xfs.push(xf),
                        b"fonts" | b"fills" | b"borders" | b"cellStyleXfs" | b"cellXfs" => {
                            zone = Zone::None;
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }
}

fn edge_mut(b: &mut Borders, side: u8) -> &mut Edge {
    match side {
        0 => &mut b.left,
        1 => &mut b.right,
        2 => &mut b.top,
        _ => &mut b.bottom,
    }
}

/// Дочерний элемент-флаг: отсутствие `val` означает истину.
fn flag(rd: &Reader<'_>) -> bool {
    !matches!(raw_attr(rd, b"val"), Some(b"0" | b"false"))
}

fn is_false(rd: &Reader<'_>, name: &[u8]) -> bool {
    matches!(raw_attr(rd, name), Some(b"0" | b"false"))
}

fn num(rd: &Reader<'_>, name: &[u8]) -> Option<f64> {
    raw_attr(rd, name)
        .and_then(|v| core::str::from_utf8(v).ok())
        .and_then(|v| v.trim().parse().ok())
}

fn color(rd: &Reader<'_>) -> Option<Color> {
    if matches!(raw_attr(rd, b"auto"), Some(b"1" | b"true")) {
        return Some(Color::Auto);
    }
    if let Some(v) = raw_attr(rd, b"rgb")
        && let Ok(s) = core::str::from_utf8(v)
        && let Ok(n) = u32::from_str_radix(s.trim(), 16)
    {
        return Some(Color::Rgb(n));
    }
    if let Some(i) = num(rd, b"indexed") {
        return Some(Color::Indexed(i as u8));
    }
    if let Some(t) = num(rd, b"theme") {
        return Some(Color::Theme(t as u8, num(rd, b"tint").unwrap_or(0.0)));
    }
    None
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Zone {
    None,
    Fonts,
    Fills,
    Borders,
    StyleXfs,
    CellXfs,
}

/// Ширина самой широкой цифры шрифта книги, в пикселях при 96 dpi.
///
/// Единица ширины столбца в Excel — не пиксель, а «сколько цифр помещается»,
/// поэтому без этой величины перевод невозможен. Точные значения для типовых
/// шрифтов: Calibri 11 — 7 px, Arial 10 — 6 px; отсюда и коэффициент.
///
/// Своих метрик шрифтов у ядра нет и быть не может, так что это оценка. Ошибка
/// в ней бьёт по всей раскладке сразу: лист в 68 узких столбцов при MDW=7
/// вместо 6 растягивается на 15% и перестаёт совпадать с оригиналом.
#[must_use]
pub fn max_digit_width(font_size_pt: f64) -> f64 {
    if !font_size_pt.is_finite() || font_size_pt <= 0.0 {
        return 7.0;
    }
    (font_size_pt * 0.62).round().max(2.0)
}

/// Переводит ширину столбца из единиц Excel в пиксели.
///
/// Ширина в единицах Excel — это «сколько цифр помещается» плюс поле ячейки.
/// Отсюда `chars * MDW + 5`, где 5 px — два поля по 2 px и линия сетки.
/// Обратное преобразование (пиксели в символы) выглядит иначе, и перепутать их
/// легко: оно даёт для стандартных 8.43 символа 59 px вместо правильных 64.
#[must_use]
pub fn col_width_px(chars: f64, mdw: f64) -> f64 {
    if !chars.is_finite() || chars <= 0.0 || mdw <= 0.0 {
        return 0.0;
    }
    (chars * mdw + 5.0).round()
}

/// Переводит высоту строки из пунктов в пиксели: 96 dpi против 72 pt на дюйм.
#[must_use]
pub fn row_height_px(points: f64) -> f64 {
    if !points.is_finite() || points <= 0.0 {
        return 0.0;
    }
    (points * 4.0 / 3.0).round()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    const STYLES: &[u8] = br#"<?xml version="1.0"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<fonts count="2">
  <font><name val="Arial"/><sz val="10"/></font>
  <font><b/><sz val="14"/><color rgb="FFFF0000"/><name val="Times"/></font>
</fonts>
<fills count="2">
  <fill><patternFill patternType="none"/></fill>
  <fill><patternFill patternType="solid"><fgColor rgb="FFFFFF00"/></patternFill></fill>
</fills>
<borders count="2">
  <border><left/><right/><top/><bottom/></border>
  <border><left style="thin"><color indexed="8"/></left><right style="thick"/><top/><bottom style="double"/></border>
</borders>
<cellStyleXfs count="1"><xf numFmtId="0" fontId="1" fillId="1" borderId="1"/></cellStyleXfs>
<cellXfs count="3">
  <xf fontId="0" fillId="0" borderId="0"/>
  <xf fontId="1" fillId="1" borderId="1"><alignment horizontal="center" vertical="top" wrapText="1"/></xf>
  <xf fontId="1" fillId="1" borderId="1" applyBorder="false" applyFill="false"/>
</cellXfs>
</styleSheet>"#;

    fn parsed() -> Appearance {
        Appearance::parse(STYLES, &Limits::strict()).unwrap()
    }

    #[test]
    fn cell_style_xfs_do_not_leak_into_cell_xfs() {
        // Ячейка ссылается только на cellXfs. Если бы разбор складывал оба
        // списка в один, индексы поехали бы на единицу и весь лист оформился
        // бы чужими стилями.
        assert_eq!(parsed().xf_count(), 3);
    }

    #[test]
    fn font_attributes_are_read() {
        let a = parsed();
        let f = a.font_of(1).unwrap();
        assert!(f.bold);
        assert_eq!(f.size, Some(14.0));
        assert_eq!(f.name.as_deref(), Some("Times"));
        assert_eq!(f.color.unwrap().to_css().as_deref(), Some("#ff0000"));
    }

    #[test]
    fn pattern_none_means_no_fill() {
        let a = parsed();
        assert_eq!(a.fill_of(0).unwrap().solid, None);
        assert_eq!(
            a.fill_of(1).unwrap().solid.unwrap().to_css().as_deref(),
            Some("#ffff00")
        );
    }

    #[test]
    fn borders_keep_each_side_separately() {
        let a = parsed();
        let b = a.borders_of(1).unwrap();
        assert_eq!(b.left.style, BorderStyle::Thin);
        assert_eq!(b.right.style, BorderStyle::Thick);
        assert_eq!(b.top.style, BorderStyle::None);
        assert_eq!(b.bottom.style, BorderStyle::Double);
        assert!(!b.is_empty());
        assert!(a.borders_of(0).unwrap().is_empty());
    }

    #[test]
    fn apply_flags_switch_parts_off() {
        // Стиль 2 ссылается на ту же рамку и заливку, что и стиль 1, но
        // объявляет applyBorder/applyFill=false — значит их применять нельзя.
        let a = parsed();
        assert!(a.borders_of(2).is_none());
        assert!(a.fill_of(2).is_none());
        assert!(a.font_of(2).is_some());
    }

    #[test]
    fn alignment_is_read() {
        let a = parsed();
        let xf = a.xf(1).unwrap();
        assert_eq!(xf.h_align, HAlign::Center);
        assert_eq!(xf.v_align, VAlign::Top);
        assert!(xf.wrap);
    }

    #[test]
    fn column_width_matches_the_spec_not_a_guess() {
        // Опорные точки: стандартная ширина Excel и столбец бланка МВД.
        assert_eq!(max_digit_width(10.0), 6.0);
        assert_eq!(max_digit_width(11.0), 7.0);
        // Стандартная ширина столбца Excel — ровно 64 px, это опорная точка.
        assert_eq!(col_width_px(8.43, 7.0), 64.0);
        // Столбец бланка МВД.
        assert_eq!(col_width_px(1.332_031_25, 7.0), 14.0);
        assert_eq!(col_width_px(1.332_031_25, 6.0), 13.0);
    }

    #[test]
    fn degenerate_inputs_do_not_produce_nonsense() {
        assert_eq!(col_width_px(0.0, 7.0), 0.0);
        assert_eq!(col_width_px(f64::NAN, 7.0), 0.0);
        assert_eq!(row_height_px(-1.0), 0.0);
        assert_eq!(max_digit_width(0.0), 7.0);
    }

    #[test]
    fn tint_lightens_towards_white() {
        let dark = Color::Theme(1, 0.0).to_css().unwrap();
        let light = Color::Theme(1, 0.5).to_css().unwrap();
        assert_eq!(dark, "#000000");
        assert_eq!(light, "#7f7f7f");
    }

    #[test]
    fn unknown_indexed_color_is_context_dependent() {
        // 64 и 65 — «системный текст» и «системный фон»; подставлять вместо
        // них чёрный значило бы врать.
        assert_eq!(Color::Indexed(64).to_css(), None);
        assert_eq!(Color::Indexed(8).to_css().as_deref(), Some("#000000"));
    }
}
