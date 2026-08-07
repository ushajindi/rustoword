//! Экранирование **нового** содержимого.
//!
//! Здесь ключевое слово — «нового». Чужие байты никогда не проходят через эти
//! функции: они копируются спанами. Экранируется только то, что пришло из
//! типизированного API в виде `&str`, — иначе `&#34;` из исходника вернулся бы
//! как `&quot;`, и файл, который никто не правил, изменился бы.
//!
//! # Почему `\r`, `\n` и `\t` экранируются числом
//!
//! XML 1.0 нормализует концы строк во всём содержимом (`\r\n` и `\r` → `\n`), а
//! в значениях атрибутов дополнительно превращает любой пробельный байт в
//! пробел. Ссылки на символы нормализации не подлежат. Значит, записанный
//! буквально `\t` в значении атрибута при следующем разборе станет пробелом:
//! первый round-trip пройдёт (байты те же), а второй — уже нет, потому что
//! модель успела измениться. `&#x9;` этого не допускает.
//!
//! Для текстового узла достаточно экранировать `\r`: `\n` и `\t` там переживают
//! нормализацию без изменений.

use crate::error::{Result, XmlError};
use crate::xml::entity::is_xml_char;
use crate::xml::lexer::xml_err;

/// Экранирует текстовое содержимое.
///
/// `>` экранируется всегда, хотя обязателен только внутри `]]>`: так делает и
/// Office (`&gt;` встречается в корпусе), и это избавляет от отдельной проверки
/// на границе склейки строк.
pub fn escape_text(s: &str, out: &mut Vec<u8>) -> Result<()> {
    escape(s, out, 0)
}

/// Экранирует значение атрибута для кавычки `quote`.
///
/// Противоположная кавычка не экранируется — так короче и так пишет Office.
/// Неизвестный `quote` трактуется как «экранировать обе»: это всегда корректно,
/// пусть и многословно.
pub fn escape_attr(s: &str, quote: u8, out: &mut Vec<u8>) -> Result<()> {
    escape(s, out, quote)
}

/// `quote == 0` — режим текстового узла.
fn escape(s: &str, out: &mut Vec<u8>, quote: u8) -> Result<()> {
    let attr = quote != 0;
    let mut lit = 0usize;
    for (i, c) in s.char_indices() {
        let repl: &[u8] = match c {
            '&' => b"&amp;",
            '<' => b"&lt;",
            '>' => b"&gt;",
            '\r' => b"&#xD;",
            '\n' if attr => b"&#xA;",
            '\t' if attr => b"&#x9;",
            '"' if attr && quote != b'\'' => b"&quot;",
            '\'' if attr && quote != b'"' => b"&apos;",
            // Символ, которого в XML 1.0 не бывает: записать его нечем.
            // Тихо выбросить нельзя — вызывающий должен узнать, что его данные
            // непредставимы, а не обнаружить пропажу через год.
            _ if !is_xml_char(c as u32) => {
                return Err(xml_err(XmlError::IllegalChar(c as u32), i));
            }
            _ => continue,
        };
        flush(s, lit, i, out)?;
        out.extend_from_slice(repl);
        lit = i.saturating_add(c.len_utf8());
    }
    flush(s, lit, s.len(), out)
}

fn flush(s: &str, from: usize, to: usize, out: &mut Vec<u8>) -> Result<()> {
    if from >= to {
        return Ok(());
    }
    let part = s
        .get(from..to)
        .ok_or_else(|| xml_err(XmlError::NotUtf8, from))?;
    out.extend_from_slice(part.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;
    use crate::error::Error;
    use crate::xml::entity::{decode_attr_to_string, decode_text_to_string};

    fn esc_text(s: &str) -> String {
        let mut v = Vec::new();
        escape_text(s, &mut v).unwrap();
        String::from_utf8(v).unwrap()
    }

    fn esc_attr(s: &str, q: u8) -> String {
        let mut v = Vec::new();
        escape_attr(s, q, &mut v).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn text_escapes_markup_and_carriage_return() {
        assert_eq!(esc_text("a<b>&c\r\n"), "a&lt;b&gt;&amp;c&#xD;\n");
        // Табуляция в тексте нормализацию переживает — экранировать незачем.
        assert_eq!(esc_text("a\tb"), "a\tb");
    }

    #[test]
    fn attribute_escapes_all_whitespace_that_normalization_would_eat() {
        assert_eq!(esc_attr("a\tb\nc\rd", b'"'), "a&#x9;b&#xA;c&#xD;d");
    }

    #[test]
    fn only_the_used_quote_is_escaped() {
        assert_eq!(esc_attr("he said \"hi\"", b'"'), "he said &quot;hi&quot;");
        assert_eq!(esc_attr("he said \"hi\"", b'\''), "he said \"hi\"");
        assert_eq!(esc_attr("it's", b'\''), "it&apos;s");
        assert_eq!(esc_attr("it's", b'"'), "it's");
    }

    #[test]
    fn round_trip_is_idempotent_after_first_pass() {
        // Свойство, ради которого экранируются `\t` и `\n`: декодирование
        // экранированного значения обязано дать ровно исходную строку, иначе
        // второй round-trip разойдётся с первым.
        for s in ["a\tb\nc\rd", "x", "&<>\"'", "строка\u{a0}", "\u{10FFFF}"] {
            assert_eq!(
                decode_attr_to_string(esc_attr(s, b'"').as_bytes()).unwrap(),
                s
            );
            assert_eq!(
                decode_attr_to_string(esc_attr(s, b'\'').as_bytes()).unwrap(),
                s
            );
        }
        for s in ["a\tb\nc", "&<>", "\u{a0}"] {
            assert_eq!(decode_text_to_string(esc_text(s).as_bytes()).unwrap(), s);
        }
    }

    #[test]
    fn unrepresentable_char_is_an_error() {
        let mut v = Vec::new();
        assert!(matches!(
            escape_text("a\u{1}b", &mut v).unwrap_err(),
            Error::Xml {
                kind: XmlError::IllegalChar(1),
                ..
            }
        ));
    }
}
