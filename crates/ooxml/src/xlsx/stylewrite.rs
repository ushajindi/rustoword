//! Правка оформления: заведение шрифта и записи формата.
//!
//! # Почему это не «поменять атрибут»
//!
//! Ячейка не хранит своё оформление — она хранит номер записи в `cellXfs`, а та
//! ссылается на шрифт, заливку и рамку по номерам. Чтобы поменять один кегль,
//! нужно завести шрифт с новым размером, завести запись формата, во всём
//! повторяющую прежнюю, но с новым номером шрифта, и переставить номер у
//! ячейки. Прямая правка существующего шрифта задела бы все ячейки, которые на
//! него ссылаются, — а их обычно сотни.
//!
//! # Дедупликация обязательна
//!
//! Без неё каждое нажатие «увеличить кегль» дописывало бы новый шрифт и новую
//! запись формата. Файл, который правят полчаса, распух бы на тысячи записей,
//! а Excel имеет предел в 64 000 стилей. Поэтому перед добавлением ищем
//! совпадение среди уже имеющихся.

use crate::dom::{Document, NodeId};
use crate::error::{Error, Result};

/// Пространство имён SpreadsheetML.
const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// Находит прямого потомка с данным локальным именем.
fn child(doc: &Document, n: NodeId, local: &str) -> Option<NodeId> {
    doc.find_child(n, Some(SML), local)
        .or_else(|| doc.find_child(n, None, local))
}

/// Дети с данным локальным именем, в порядке следования.
fn children_named(doc: &Document, n: NodeId, local: &str) -> Vec<NodeId> {
    doc.children(n)
        .filter(|&c| doc.local_name(c) == Some(local.as_bytes()))
        .collect()
}

/// Приводит поддерево к строке — она служит ключом сравнения при дедупликации.
///
/// Сравнивать разобранные поля пришлось бы поле за полем, и любое, о котором мы
/// не знаем (а в `<font>` их десяток), считалось бы одинаковым у разных
/// шрифтов. Сравнение сериализованного вида таких промахов не допускает.
fn fingerprint(doc: &Document, n: NodeId) -> Result<String> {
    let bytes = doc.serialize_node(n)?;
    String::from_utf8(bytes).map_err(|_| Error::Unsupported("styles: не UTF-8"))
}

/// Обеспечивает наличие шрифта, отличающегося от `font_id` заданными полями.
///
/// `None` означает «оставить как было»: так одной функцией меняется и кегль,
/// и гарнитура, и обе сразу.
///
/// Возвращает номер шрифта: существующего, если такой уже есть, иначе нового.
fn ensure_font(
    doc: &mut Document,
    fonts: NodeId,
    font_id: u32,
    want: &FontEdit<'_>,
) -> Result<u32> {
    let list = children_named(doc, fonts, "font");
    let base = list
        .get(font_id as usize)
        .copied()
        .or_else(|| list.first().copied())
        .ok_or(Error::Unsupported("styles: нет ни одного шрифта"))?;

    let clone = doc.clone_subtree(base)?;

    if let Some(pt) = want.size_pt {
        set_child_val(doc, clone, "sz", &format_size(pt))?;
    }
    // Начертание в OOXML — это наличие элемента, а не его значение. Выключаем
    // удалением: `<b val="0"/>` формату не противоречит, но такого не пишет
    // ни один из генераторов корпуса, и лишний элемент мешал бы дедупликации.
    for (flag, tag) in [(want.bold, "b"), (want.italic, "i"), (want.underline, "u")] {
        let Some(on) = flag else { continue };
        match (on, child(doc, clone, tag)) {
            (true, None) => {
                let n = doc.new_element(tag)?;
                doc.append_child(clone, n)?;
            }
            (false, Some(n)) => doc.remove(n)?,
            _ => {}
        }
    }
    if let Some(n) = want.name {
        // Гарнитура хранится в `<name>`, но у шрифтов из рич-текста тот же
        // смысл несёт `<rFont>`. Правим тот элемент, который есть.
        let tag = if child(doc, clone, "name").is_some() || child(doc, clone, "rFont").is_none() {
            "name"
        } else {
            "rFont"
        };
        set_child_val(doc, clone, tag, n)?;
        // `<scheme val="minor"/>` привязывает шрифт к теме и перебивает имя:
        // Excel подставит шрифт темы, и правка окажется невидимой.
        if let Some(sc) = child(doc, clone, "scheme") {
            doc.remove(sc)?;
        }
    }

    // Присоединяем сразу, а лишнее убираем после сравнения: `remove` работает
    // только с узлом, у которого есть родитель, а `clone_subtree` отдаёт
    // отсоединённый.
    doc.append_child(fonts, clone)?;
    let want = fingerprint(doc, clone)?;
    for (i, &f) in list.iter().enumerate() {
        if fingerprint(doc, f)? == want {
            doc.remove(clone)?;
            return u32::try_from(i)
                .map_err(|_| Error::Unsupported("styles: слишком много шрифтов"));
        }
    }

    bump_count(doc, fonts)?;
    u32::try_from(list.len()).map_err(|_| Error::Unsupported("styles: слишком много шрифтов"))
}

/// Обеспечивает наличие записи формата, повторяющей `xf_id`, но с иным шрифтом.
fn ensure_xf(doc: &mut Document, cell_xfs: NodeId, xf_id: u32, font_id: u32) -> Result<u32> {
    let list = children_named(doc, cell_xfs, "xf");
    let base = list
        .get(xf_id as usize)
        .copied()
        .or_else(|| list.first().copied())
        .ok_or(Error::Unsupported("styles: нет ни одной записи формата"))?;

    let clone = doc.clone_subtree(base)?;
    doc.set_attr(clone, "fontId", &font_id.to_string())?;
    // `applyFont` дописываем, только если там стоит явный отказ. Отсутствие
    // атрибута само по себе означает «применять», а лишняя запись меняет
    // байты и ломает дедупликацию: включив и выключив жирность, мы получали
    // бы не исходный стиль, а его копию.
    if matches!(
        doc.attr(clone, None, "applyFont").as_deref(),
        Some("0" | "false")
    ) {
        doc.set_attr(clone, "applyFont", "1")?;
    }

    doc.append_child(cell_xfs, clone)?;
    let want = fingerprint(doc, clone)?;
    for (i, &x) in list.iter().enumerate() {
        if fingerprint(doc, x)? == want {
            doc.remove(clone)?;
            return u32::try_from(i)
                .map_err(|_| Error::Unsupported("styles: слишком много форматов"));
        }
    }

    bump_count(doc, cell_xfs)?;
    u32::try_from(list.len()).map_err(|_| Error::Unsupported("styles: слишком много форматов"))
}

/// Задаёт `<тег val="…"/>` внутри узла, заводя элемент при необходимости.
fn set_child_val(doc: &mut Document, parent: NodeId, tag: &str, val: &str) -> Result<()> {
    match child(doc, parent, tag) {
        Some(n) => doc.set_attr(n, "val", val),
        None => {
            let n = doc.new_element(tag)?;
            doc.set_attr(n, "val", val)?;
            doc.append_child(parent, n)
        }
    }
}

/// Увеличивает атрибут `count` на единицу, если он есть.
///
/// Атрибут необязателен, но если он есть и врёт, Excel сообщает о повреждении
/// файла. Поэтому либо поддерживаем его точным, либо не трогаем вовсе.
fn bump_count(doc: &mut Document, n: NodeId) -> Result<()> {
    let Some(v) = doc.attr(n, None, "count") else {
        return Ok(());
    };
    let next = v.trim().parse::<u64>().unwrap_or(0).saturating_add(1);
    doc.set_attr(n, "count", &next.to_string())
}

/// Печатает кегль так, как это делает Excel: без хвоста у целых значений.
fn format_size(pt: f64) -> String {
    if pt == pt.trunc() {
        format!("{pt:.0}")
    } else {
        let s = format!("{pt:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// Подбирает номер записи формата с нужным кеглем, заводя недостающее.
///
/// `style` — текущий номер у ячейки (`None` означает нулевой). Заливка, рамки и
/// выравнивание переносятся без изменений: меняется только шрифт.
///
/// # Errors
///
/// Ошибки XML; [`Error::Unsupported`], если структура `styles.xml` неожиданная.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FontEdit<'a> {
    pub size_pt: Option<f64>,
    pub name: Option<&'a str>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
}

pub(crate) fn style_with_font(
    doc: &mut Document,
    style: Option<u32>,
    want: &FontEdit<'_>,
) -> Result<u32> {
    let (size_pt, name) = (want.size_pt, want.name);
    if let Some(pt) = size_pt
        && !(1.0..=409.0).contains(&pt)
    {
        // Пределы самого Excel. Молча зажать значение нельзя: пользователь
        // увидит не то, что ввёл, и решит, что правка не сработала.
        return Err(Error::Unsupported("styles: кегль вне диапазона 1..409"));
    }
    if let Some(n) = name
        && (n.trim().is_empty() || n.len() > 31)
    {
        // 31 символ — предел Excel на имя гарнитуры.
        return Err(Error::Unsupported(
            "styles: имя шрифта пустое или длиннее 31",
        ));
    }

    let root = doc.root_element()?;
    let fonts = child(doc, root, "fonts").ok_or(Error::Unsupported("styles: нет <fonts>"))?;
    let cell_xfs =
        child(doc, root, "cellXfs").ok_or(Error::Unsupported("styles: нет <cellXfs>"))?;

    let xf_id = style.unwrap_or(0);
    let xfs = children_named(doc, cell_xfs, "xf");
    let base_font = xfs
        .get(xf_id as usize)
        .and_then(|&x| doc.attr(x, None, "fontId"))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);

    let font_id = ensure_font(doc, fonts, base_font, want)?;
    ensure_xf(doc, cell_xfs, xf_id, font_id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use crate::limits::Limits;

    const STYLES: &[u8] = br#"<?xml version="1.0"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<fonts count="2"><font><name val="Arial"/><sz val="10"/></font><font><b/><sz val="14"/></font></fonts>
<fills count="1"><fill><patternFill patternType="none"/></fill></fills>
<borders count="1"><border><left/><right/><top/><bottom/></border></borders>
<cellXfs count="2"><xf fontId="0" fillId="0" borderId="0"/><xf fontId="1" fillId="0" borderId="0" applyFont="1"/></cellXfs>
</styleSheet>"#;

    fn doc() -> Document {
        Document::parse(STYLES.to_vec(), &Limits::strict()).unwrap()
    }

    #[test]
    fn new_size_adds_one_font_and_one_format() {
        let mut d = doc();
        let s = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                size_pt: Some(18.0),
                ..FontEdit::default()
            },
        )
        .unwrap();
        assert_eq!(s, 2, "должна появиться третья запись формата");
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert!(out.contains(r#"<sz val="18"/>"#), "{out}");
        assert_eq!(out.matches("<font>").count(), 3);
        assert!(
            out.contains(r#"<fonts count="3""#),
            "счётчик не обновлён: {out}"
        );
    }

    #[test]
    fn repeated_request_reuses_what_it_made() {
        // Без дедупликации каждое нажатие «увеличить кегль» дописывало бы
        // запись; за полчаса правки файл распух бы на тысячи стилей.
        let mut d = doc();
        let a = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                size_pt: Some(18.0),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let b = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                size_pt: Some(18.0),
                ..FontEdit::default()
            },
        )
        .unwrap();
        assert_eq!(a, b);
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert_eq!(
            out.matches("<font>").count(),
            3,
            "шрифт задублировался: {out}"
        );
        assert_eq!(
            out.matches("<xf ").count(),
            3,
            "формат задублировался: {out}"
        );
    }

    #[test]
    fn existing_size_is_found_not_created() {
        // Кегль 14 уже есть у второго шрифта, но с жирным начертанием —
        // значит совпадением он не считается, шрифт всё равно новый.
        let mut d = doc();
        style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                size_pt: Some(14.0),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert_eq!(out.matches("<font>").count(), 3, "{out}");
    }

    #[test]
    fn other_properties_survive() {
        // Меняем кегль у жирного шрифта: жирность обязана остаться.
        let mut d = doc();
        let s = style_with_font(
            &mut d,
            Some(1),
            &FontEdit {
                size_pt: Some(20.0),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert!(out.contains(r#"<b/><sz val="20"/>"#), "{out}");
        assert!(s >= 2);
    }

    #[test]
    fn font_name_is_applied_and_deduplicated() {
        let mut d = doc();
        let a = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                name: Some("Georgia"),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let b = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                name: Some("Georgia"),
                ..FontEdit::default()
            },
        )
        .unwrap();
        assert_eq!(a, b, "повтор той же гарнитуры не должен плодить записи");
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert!(out.contains(r#"<name val="Georgia"/>"#), "{out}");
        assert_eq!(out.matches("<font>").count(), 3, "{out}");
    }

    #[test]
    fn name_and_size_change_together_in_one_font() {
        let mut d = doc();
        style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                size_pt: Some(16.0),
                name: Some("Georgia"),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert!(
            out.contains(r#"<name val="Georgia"/><sz val="16"/>"#),
            "{out}"
        );
        // Ровно один новый шрифт, а не два.
        assert_eq!(out.matches("<font>").count(), 3, "{out}");
    }

    #[test]
    fn theme_scheme_is_dropped_when_the_family_changes() {
        // `<scheme val="minor"/>` привязывает шрифт к теме и перебивает имя:
        // Excel подставил бы шрифт темы, и правка стала бы невидимой.
        const WITH_SCHEME: &[u8] = br#"<?xml version="1.0"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<fonts count="1"><font><sz val="11"/><name val="Calibri"/><scheme val="minor"/></font></fonts>
<cellXfs count="1"><xf fontId="0"/></cellXfs>
</styleSheet>"#;
        let mut d = Document::parse(WITH_SCHEME.to_vec(), &Limits::strict()).unwrap();
        style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                name: Some("Georgia"),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert!(out.contains(r#"<name val="Georgia"/>"#), "{out}");
        assert_eq!(
            out.matches("<scheme").count(),
            1,
            "у нового шрифта scheme остался: {out}"
        );
    }

    #[test]
    fn absurd_name_is_rejected_loudly() {
        let mut d = doc();
        assert!(
            style_with_font(
                &mut d,
                None,
                &FontEdit {
                    name: Some(""),
                    ..FontEdit::default()
                }
            )
            .is_err()
        );
        assert!(
            style_with_font(
                &mut d,
                None,
                &FontEdit {
                    name: Some(&"x".repeat(32)),
                    ..FontEdit::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn weights_are_added_and_removed_as_elements() {
        let mut d = doc();
        // Включаем жирность у обычного шрифта.
        let a = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                bold: Some(true),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert!(out.contains("<b/>"), "{out}");

        // Снимаем жирность у второго шрифта, который был жирным: элемент
        // обязан исчезнуть, а не превратиться в `<b val="0"/>`.
        let mut d2 = doc();
        style_with_font(
            &mut d2,
            Some(1),
            &FontEdit {
                bold: Some(false),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out2 = String::from_utf8(d2.serialize().unwrap()).unwrap();
        assert!(!out2.contains(r#"<b val="0""#), "{out2}");
        assert_eq!(
            out2.matches("<b/>").count(),
            1,
            "жирность снялась не у того шрифта: {out2}"
        );
        assert!(a >= 2);
    }

    #[test]
    fn toggling_back_reuses_the_original_style() {
        // Включили и выключили — должны вернуться к исходному шрифту, а не
        // завести его копию. Иначе жмущий кнопку туда-сюда раздувает файл.
        let mut d = doc();
        let on = style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                bold: Some(true),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let off = style_with_font(
            &mut d,
            Some(on),
            &FontEdit {
                bold: Some(false),
                ..FontEdit::default()
            },
        )
        .unwrap();
        assert_eq!(off, 0, "возврат к исходному стилю не сработал");
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert_eq!(out.matches("<font>").count(), 3, "{out}");
    }

    #[test]
    fn all_three_weights_combine() {
        let mut d = doc();
        style_with_font(
            &mut d,
            Some(0),
            &FontEdit {
                bold: Some(true),
                italic: Some(true),
                underline: Some(true),
                ..FontEdit::default()
            },
        )
        .unwrap();
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        for tag in ["<b/>", "<i/>", "<u/>"] {
            assert!(out.contains(tag), "нет {tag}: {out}");
        }
    }

    #[test]
    fn absurd_size_is_rejected_loudly() {
        let mut d = doc();
        assert!(
            style_with_font(
                &mut d,
                Some(0),
                &FontEdit {
                    size_pt: Some(0.0),
                    ..FontEdit::default()
                }
            )
            .is_err()
        );
        assert!(
            style_with_font(
                &mut d,
                Some(0),
                &FontEdit {
                    size_pt: Some(500.0),
                    ..FontEdit::default()
                }
            )
            .is_err()
        );
        // Отказ не должен ничего дописать.
        let out = String::from_utf8(d.serialize().unwrap()).unwrap();
        assert_eq!(out.matches("<font>").count(), 2, "{out}");
    }
}
