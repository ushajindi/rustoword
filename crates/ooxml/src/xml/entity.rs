//! Ленивое декодирование ссылок на сущности и символы.
//!
//! # Почему лениво
//!
//! Лексер не декодирует ничего: он отдаёт сырые спаны. Декодирование вызывается
//! только тогда, когда типизированный API попросил значение — и результат
//! никогда не попадает обратно в файл. Если бы декодирование происходило при
//! разборе, сериализатору пришлось бы экранировать заново, и `&#34;` из корпуса
//! (44 вхождения) превратился бы в `&quot;` (121 вхождение) — round-trip
//! разошёлся бы на файлах, которые никто не правил.
//!
//! # Что поддерживается
//!
//! Ровно пять предопределённых сущностей плюс числовые ссылки. Любая другая
//! сущность — [`XmlError::UnknownEntity`], потому что определить её мог бы
//! только DTD, а DTD запрещён безусловно ([`XmlError::DoctypeForbidden`]).
//! Это же закрывает billion-laughs: рекурсивно раскрывать здесь нечего.
//!
//! # Нормализация концов строк и значений атрибутов
//!
//! XML 1.0 требует, чтобы `\r\n` и одиночный `\r` в содержимом превращались в
//! `\n`, а в значении атрибута любой пробельный байт — в пробел. Здесь это
//! реализовано, и именно поэтому [`crate::xml::escape`] обязан писать новый
//! `\r` как `&#xD;`: ссылки на символы нормализации не подлежат, а литеральные
//! байты подлежат, и без экранирования второй round-trip разошёлся бы с первым.

use crate::error::{Result, XmlError};
use crate::xml::lexer::xml_err;

/// Максимальная длина тела ссылки (между `&` и `;`).
///
/// Самое длинное осмысленное тело — `#x10FFFF` (8 байт). Ограничение нужно не
/// ради строгости, а ради сложности: без него одиночный `&` в мегабайтном
/// тексте заставлял бы искать `;` до конца буфера, и текст из одних амперсандов
/// давал бы квадратичное время.
const MAX_REF_BODY: usize = 32;

/// Декодирует содержимое текстового узла.
///
/// `raw` — спан [`crate::xml::Event::Text`] как есть. Офсеты в ошибках
/// отсчитываются от начала `raw`, а не от начала документа: вызывающий знает,
/// где лежит спан, а декодер — нет.
pub fn decode_text(raw: &[u8], out: &mut String) -> Result<()> {
    decode(raw, out, Mode::Text)
}

/// Декодирует значение атрибута, применяя нормализацию по XML 1.0.
///
/// Литеральные `\t`, `\n`, `\r` становятся пробелом; те же символы, записанные
/// ссылками (`&#x9;`, `&#xA;`, `&#xD;`), сохраняются как есть — так требует
/// спецификация, и на этом различии держится сохранность переносов строк в
/// значениях атрибутов.
pub fn decode_attr(raw: &[u8], out: &mut String) -> Result<()> {
    decode(raw, out, Mode::Attr)
}

/// [`decode_text`] с выделением строки.
pub fn decode_text_to_string(raw: &[u8]) -> Result<String> {
    let mut s = String::with_capacity(raw.len());
    decode_text(raw, &mut s)?;
    Ok(s)
}

/// [`decode_attr`] с выделением строки.
pub fn decode_attr_to_string(raw: &[u8]) -> Result<String> {
    let mut s = String::with_capacity(raw.len());
    decode_attr(raw, &mut s)?;
    Ok(s)
}

/// Допустим ли символ в XML 1.0.
///
/// Управляющие, кроме `\t\n\r`, суррогаты и не-символы недопустимы. Проверка
/// обязательна для числовых ссылок: `&#0;` — валидная по синтаксису запись
/// байта, который сделает документ неразбираемым.
#[must_use]
pub const fn is_xml_char(v: u32) -> bool {
    matches!(v, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x1_0000..=0x10_FFFF)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Attr,
}

fn decode(raw: &[u8], out: &mut String, mode: Mode) -> Result<()> {
    let mut i = 0usize;
    let mut lit = 0usize;
    while let Some(&b) = raw.get(i) {
        match b {
            b'&' => {
                flush(raw, lit, i, out)?;
                i = decode_ref(raw, i, out)?;
                lit = i;
            }
            // Нормализация концов строк: `\r\n` и одиночный `\r` — это `\n`.
            b'\r' => {
                flush(raw, lit, i, out)?;
                i = if raw.get(i.saturating_add(1)) == Some(&b'\n') {
                    i.saturating_add(2)
                } else {
                    i.saturating_add(1)
                };
                out.push(if mode == Mode::Attr { ' ' } else { '\n' });
                lit = i;
            }
            b'\n' | b'\t' if mode == Mode::Attr => {
                flush(raw, lit, i, out)?;
                out.push(' ');
                i = i.saturating_add(1);
                lit = i;
            }
            _ => i = i.saturating_add(1),
        }
    }
    flush(raw, lit, raw.len(), out)
}

/// Переносит литеральный прогон `raw[from..to]`, проверяя UTF-8.
///
/// Проверка именно здесь, а не при лексировании: сырые спаны копируются в
/// вывод без разбора, и требовать от них корректного UTF-8 значило бы
/// отвергать файлы, которые прекрасно открываются в Word.
fn flush(raw: &[u8], from: usize, to: usize, out: &mut String) -> Result<()> {
    if from >= to {
        return Ok(());
    }
    let chunk = raw
        .get(from..to)
        .ok_or_else(|| xml_err(XmlError::NotUtf8, from))?;
    // Границы прогонов всегда падают на ASCII-байты (`&`, `\r`, `\n`, `\t`),
    // поэтому многобайтовая последовательность разрезанной быть не может.
    let s = core::str::from_utf8(chunk)
        .map_err(|e| xml_err(XmlError::NotUtf8, from.saturating_add(e.valid_up_to())))?;
    out.push_str(s);
    Ok(())
}

/// Декодирует одну ссылку, начинающуюся с `&` в позиции `at`.
/// Возвращает индекс первого байта за `;`.
fn decode_ref(raw: &[u8], at: usize, out: &mut String) -> Result<usize> {
    let body_at = at.saturating_add(1);
    let window = raw
        .get(body_at..)
        .map(|r| r.get(..MAX_REF_BODY).unwrap_or(r))
        .unwrap_or(&[]);
    let semi = window
        .iter()
        .position(|&b| b == b';')
        .ok_or_else(|| xml_err(XmlError::UnknownEntity, at))?;
    let body = window
        .get(..semi)
        .ok_or_else(|| xml_err(XmlError::UnknownEntity, at))?;
    let end = body_at.saturating_add(semi).saturating_add(1);

    if let Some(digits) = body.strip_prefix(b"#") {
        out.push(char_ref(digits, at)?);
        return Ok(end);
    }
    let c = match body {
        b"amp" => '&',
        b"lt" => '<',
        b"gt" => '>',
        b"quot" => '"',
        b"apos" => '\'',
        _ => return Err(xml_err(XmlError::UnknownEntity, at)),
    };
    out.push(c);
    Ok(end)
}

fn char_ref(digits: &[u8], at: usize) -> Result<char> {
    // Только строчная `x`: XML 1.0 не допускает `&#X41;`.
    let (radix, ds) = match digits.strip_prefix(b"x") {
        Some(rest) => (16u32, rest),
        None => (10u32, digits),
    };
    if ds.is_empty() {
        return Err(xml_err(XmlError::BadCharRef, at));
    }
    let mut v: u32 = 0;
    for &d in ds {
        let dv = match d {
            b'0'..=b'9' => u32::from(d.saturating_sub(b'0')),
            b'a'..=b'f' if radix == 16 => u32::from(d.saturating_sub(b'a')).saturating_add(10),
            b'A'..=b'F' if radix == 16 => u32::from(d.saturating_sub(b'A')).saturating_add(10),
            _ => return Err(xml_err(XmlError::BadCharRef, at)),
        };
        // Насыщение вместо переполнения: конкретное значение выше 0x10FFFF уже
        // неважно, важно, что оно недопустимо.
        v = v.saturating_mul(radix).saturating_add(dv);
        if v > 0x10_FFFF {
            return Err(xml_err(XmlError::IllegalChar(v), at));
        }
    }
    if !is_xml_char(v) {
        return Err(xml_err(XmlError::IllegalChar(v), at));
    }
    char::from_u32(v).ok_or_else(|| xml_err(XmlError::IllegalChar(v), at))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;
    use crate::error::Error;

    fn t(raw: &[u8]) -> String {
        decode_text_to_string(raw).unwrap()
    }

    #[test]
    fn five_predefined_entities() {
        assert_eq!(t(b"&amp;&lt;&gt;&quot;&apos;"), "&<>\"'");
    }

    #[test]
    fn numeric_forms_agree() {
        // В корпусе живут обе формы кавычки одновременно — декодер обязан
        // сводить их к одному символу, не трогая исходные байты.
        assert_eq!(t(b"&#34;"), "\"");
        assert_eq!(t(b"&#x22;"), "\"");
        assert_eq!(t(b"&quot;"), "\"");
        assert_eq!(t(b"&#160;"), "\u{a0}");
        assert_eq!(t(b"&#x10FFFF;"), "\u{10FFFF}");
    }

    #[test]
    fn unknown_entity_is_error_not_passthrough() {
        assert!(matches!(
            decode_text_to_string(b"&lol1;").unwrap_err(),
            Error::Xml {
                kind: XmlError::UnknownEntity,
                ..
            }
        ));
    }

    #[test]
    fn illegal_chars_rejected() {
        for raw in [
            &b"&#0;"[..],
            b"&#xB;",
            b"&#xD800;",
            b"&#xDFFF;",
            b"&#xFFFE;",
            b"&#xFFFF;",
            b"&#x110000;",
            b"&#99999999999999;",
        ] {
            assert!(
                matches!(
                    decode_text_to_string(raw),
                    Err(Error::Xml {
                        kind: XmlError::IllegalChar(_),
                        ..
                    })
                ),
                "{:?} должно быть IllegalChar",
                core::str::from_utf8(raw)
            );
        }
    }

    #[test]
    fn char_ref_escapes_line_end_normalization() {
        // Литеральный `\r` нормализуется, а `&#xD;` — нет. На этом различии
        // держится сохранность переносов строк.
        assert_eq!(t(b"a\r\nb"), "a\nb");
        assert_eq!(t(b"a\rb"), "a\nb");
        assert_eq!(t(b"a&#xD;b"), "a\rb");
    }

    #[test]
    fn attribute_normalization_differs_from_text() {
        let a = decode_attr_to_string(b"a\tb\nc\r\nd").unwrap();
        assert_eq!(a, "a b c d");
        // Ссылки нормализации не подлежат.
        assert_eq!(decode_attr_to_string(b"a&#x9;b&#xA;c").unwrap(), "a\tb\nc");
    }

    #[test]
    fn bad_utf8_is_reported_only_on_decode() {
        assert!(matches!(
            decode_text_to_string(&[0xFF, 0xFE]).unwrap_err(),
            Error::Xml {
                kind: XmlError::NotUtf8,
                ..
            }
        ));
    }

    #[test]
    fn unterminated_reference_does_not_scan_forever() {
        let mut big = Vec::from(&b"&"[..]);
        big.extend(core::iter::repeat_n(b'a', 100_000));
        assert!(decode_text_to_string(&big).is_err());
    }

    #[test]
    fn uppercase_x_is_not_a_hex_ref() {
        assert!(decode_text_to_string(b"&#X22;").is_err());
    }
}
