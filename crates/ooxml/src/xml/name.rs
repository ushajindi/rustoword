//! Квалифицированные имена.

use crate::bytes::Span;
use crate::error::{Result, XmlError};
use crate::xml::lexer::xml_err;

/// Имя элемента или атрибута, разделённое на префикс и локальную часть.
///
/// Обе половины — спаны исходника: имя нигде не копируется и не приводится к
/// канонической форме. Полное имя восстановимо ([`QName::full`]), потому что
/// именно оно, а не пара (URI, локальное имя), пишется обратно в файл: в OOXML
/// `w:p` и `foo:p` при одном и том же URI — разные байты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QName {
    prefix: Option<Span>,
    local: Span,
}

impl QName {
    /// Заглушка для полей, которые ещё не заполнены первым событием.
    ///
    /// У `QName` нет осмысленного «пустого» состояния, но и обёртка в `Option`
    /// в горячем поле парсера стоила бы ветки на каждом обращении. Читать это
    /// значение бессмысленно, и никакой код его не читает: имя элемента
    /// определено только после `Start` или `End`.
    pub const EMPTY: Self = Self {
        prefix: None,
        local: Span::empty(0),
    };

    /// Разбирает имя, лежащее в `src` по спану `name`.
    ///
    /// Два двоеточия, двоеточие в начале или в конце — [`XmlError::BadName`]:
    /// такое имя не имеет однозначного разбиения, а «догадаться» здесь — значит
    /// разойтись с любым другим парсером.
    pub fn parse(name: Span, src: &[u8]) -> Result<Self> {
        let at = name.start() as usize;
        let bytes = name
            .slice(src)
            .ok_or_else(|| xml_err(XmlError::BadName, at))?;
        if bytes.is_empty() {
            return Err(xml_err(XmlError::BadName, at));
        }

        let mut colon = None;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b':' {
                if colon.is_some() {
                    return Err(xml_err(XmlError::BadName, at));
                }
                colon = Some(i);
            }
        }

        let Some(i) = colon else {
            return Ok(Self {
                prefix: None,
                local: name,
            });
        };
        // Ни `:a`, ни `a:` не имеют смысла: пустой префикс — это отсутствие
        // префикса, а пустая локальная часть — отсутствие имени.
        let last = bytes.len().saturating_sub(1);
        if i == 0 || i == last {
            return Err(xml_err(XmlError::BadName, at));
        }
        let cut = u32::try_from(i)
            .ok()
            .and_then(|i| name.start().checked_add(i))
            .ok_or_else(|| xml_err(XmlError::BadName, at))?;
        let local_start = cut
            .checked_add(1)
            .ok_or_else(|| xml_err(XmlError::BadName, at))?;
        Ok(Self {
            prefix: Span::new(name.start(), cut),
            local: Span::clamped(local_start, name.end()),
        })
    }

    /// Префикс без двоеточия. `None` — имя без префикса.
    #[must_use]
    pub const fn prefix(self) -> Option<Span> {
        self.prefix
    }

    /// Локальная часть.
    #[must_use]
    pub const fn local(self) -> Span {
        self.local
    }

    /// Имя целиком, как оно лежит в файле.
    #[must_use]
    pub const fn full(self) -> Span {
        match self.prefix {
            Some(p) => p.join(self.local),
            None => self.local,
        }
    }

    /// Байты префикса; для имени без префикса — пустой срез.
    ///
    /// Пустой срез, а не `Option`: неснабжённый префиксом элемент участвует в
    /// разрешении namespace наравне с остальными, только под пустым ключом.
    #[must_use]
    pub fn prefix_bytes(self, src: &[u8]) -> &[u8] {
        match self.prefix {
            Some(p) => p.slice(src).unwrap_or(&[]),
            None => &[],
        }
    }

    #[must_use]
    pub fn local_bytes(self, src: &[u8]) -> &[u8] {
        self.local.slice(src).unwrap_or(&[])
    }

    /// Атрибут `xmlns` — объявление namespace по умолчанию.
    #[must_use]
    pub fn is_default_ns_decl(self, src: &[u8]) -> bool {
        self.prefix.is_none() && self.local_bytes(src) == b"xmlns"
    }

    /// Атрибут `xmlns:*` — объявление префикса.
    #[must_use]
    pub fn is_prefixed_ns_decl(self, src: &[u8]) -> bool {
        self.prefix_bytes(src) == b"xmlns"
    }

    /// Любое объявление namespace.
    ///
    /// Такие атрибуты остаются обычными атрибутами элемента и сохраняются в
    /// списке в исходном порядке: в OOXML порядок `xmlns:*` значим для
    /// байт-идентичности, а `mc:Ignorable` ссылается на префиксы по именам.
    #[must_use]
    pub fn is_ns_decl(self, src: &[u8]) -> bool {
        self.is_default_ns_decl(src) || self.is_prefixed_ns_decl(src)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;
    use crate::error::Error;

    fn q(src: &[u8]) -> Result<QName> {
        QName::parse(Span::clamped(0, src.len() as u32), src)
    }

    #[test]
    fn splits_on_the_single_colon() {
        let src = b"w:p";
        let n = q(src).unwrap();
        assert_eq!(n.prefix_bytes(src), b"w");
        assert_eq!(n.local_bytes(src), b"p");
        assert_eq!(n.full(), Span::clamped(0, 3));
    }

    #[test]
    fn unprefixed_name_has_empty_prefix() {
        let src = b"sheetData";
        let n = q(src).unwrap();
        assert!(n.prefix().is_none());
        assert_eq!(n.prefix_bytes(src), b"");
        assert_eq!(n.local_bytes(src), b"sheetData");
    }

    #[test]
    fn malformed_names_rejected() {
        for src in [&b"a:b:c"[..], b":a", b"a:", b"", b":", b"::"] {
            assert!(
                matches!(
                    q(src),
                    Err(Error::Xml {
                        kind: XmlError::BadName,
                        ..
                    })
                ),
                "{:?} должно быть BadName",
                core::str::from_utf8(src)
            );
        }
    }

    #[test]
    fn namespace_declarations_are_recognised_but_stay_attributes() {
        let src = b"xmlns:x14ac";
        assert!(q(src).unwrap().is_prefixed_ns_decl(src));
        assert!(q(src).unwrap().is_ns_decl(src));
        let src = b"xmlns";
        assert!(q(src).unwrap().is_default_ns_decl(src));
        let src = b"mc:Ignorable";
        assert!(!q(src).unwrap().is_ns_decl(src));
    }
}
