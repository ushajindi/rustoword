//! Стек namespace и интернер URI.
//!
//! # Что namespace здесь НЕ делает
//!
//! Он не влияет на сериализацию. Атрибуты `xmlns` и `xmlns:*` — обычные
//! атрибуты элемента: они остаются в списке, на своих местах, с исходными
//! кавычками. Причина не в педантизме, а в замерах: в реальных `.xlsx` корень
//! объявляет `xmlns:x14ac`, `xmlns:xr`, `xmlns:xr2`, `xmlns:xr3`, `xmlns:mc`, и
//! рядом лежит `mc:Ignorable="x14ac xr xr2 xr3"`, который ссылается на префиксы
//! **по именам**. Переписать префикс на «канонический» — значит сломать MCE.
//! MCE, к слову, здесь не интерпретируется вовсе: он просто сохраняется.
//!
//! Разрешение префиксов нужно типизированному API — чтобы `w:p` из `.docx`
//! опознавался по URI, а не по написанию префикса, которое документ волен
//! выбрать любым.

use crate::error::{Result, XmlError};
use crate::xml::lexer::xml_err;

/// URI префикса `xml`, предопределённого спецификацией.
pub const XML_URI: &str = "http://www.w3.org/XML/1998/namespace";
/// URI, зарезервированный за самими объявлениями namespace.
pub const XMLNS_URI: &str = "http://www.w3.org/2000/xmlns/";

/// Интернированный URI. Сравнение namespace сводится к сравнению `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NsUri(u32);

#[derive(Debug)]
struct Binding {
    /// Префикс без двоеточия; пустой — объявление по умолчанию.
    prefix: Vec<u8>,
    /// `None` — префикс отвязан (`xmlns=""`).
    uri: Option<NsUri>,
}

/// Стек областей видимости namespace.
///
/// Привязок в реальном документе единицы, поэтому и стек, и интернер — простые
/// векторы с линейным поиском: хеш-таблица здесь проиграла бы на константе.
#[derive(Debug)]
pub struct Namespaces {
    uris: Vec<String>,
    bindings: Vec<Binding>,
    /// Длина `bindings` на момент открытия каждого элемента.
    frames: Vec<usize>,
}

impl Default for Namespaces {
    fn default() -> Self {
        Self::new()
    }
}

impl Namespaces {
    #[must_use]
    pub fn new() -> Self {
        let mut ns = Self {
            uris: vec![XML_URI.to_owned()],
            bindings: Vec::new(),
            frames: Vec::new(),
        };
        // Привязка `xml` лежит ниже всех фреймов, поэтому `pop_frame` её не
        // снимет: она действует во всём документе и объявления не требует.
        ns.bindings.push(Binding {
            prefix: b"xml".to_vec(),
            uri: Some(NsUri(0)),
        });
        ns
    }

    /// Сбрасывает состояние, сохраняя выделенную память.
    pub fn reset(&mut self) {
        self.bindings.truncate(1);
        self.frames.clear();
        self.uris.truncate(1);
    }

    /// Открывает область видимости очередного элемента.
    pub fn push_frame(&mut self) {
        self.frames.push(self.bindings.len());
    }

    /// Закрывает область видимости, снимая объявленные в ней привязки.
    pub fn pop_frame(&mut self) {
        if let Some(n) = self.frames.pop() {
            self.bindings.truncate(n);
        }
    }

    /// Текущая глубина стека областей видимости.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Объявляет привязку в текущей области видимости.
    ///
    /// `prefix` пустой — объявление по умолчанию. Пустой `uri` снимает
    /// привязку. `at` — офсет для сообщения об ошибке.
    pub fn declare(&mut self, prefix: &[u8], uri: &str, at: u32) -> Result<()> {
        // `xmlns` не является префиксом и привязан быть не может; `xml`
        // допускается объявить только к его собственному URI. Оба правила —
        // из Namespaces in XML, и оба нарушаются только намеренно.
        if prefix == b"xmlns" {
            return Err(reserved(at));
        }
        if prefix == b"xml" && uri != XML_URI {
            return Err(reserved(at));
        }
        if uri == XMLNS_URI {
            return Err(reserved(at));
        }
        let bound = if uri.is_empty() {
            None
        } else {
            Some(self.intern(uri))
        };
        self.bindings.push(Binding {
            prefix: prefix.to_vec(),
            uri: bound,
        });
        Ok(())
    }

    /// Разрешает префикс в URI.
    ///
    /// Пустой префикс — namespace по умолчанию; его отсутствие ошибкой не
    /// является. Необъявленный непустой префикс — [`XmlError::UndeclaredPrefix`].
    pub fn resolve(&self, prefix: &[u8], at: u32) -> Result<Option<NsUri>> {
        for b in self.bindings.iter().rev() {
            if b.prefix == prefix {
                return match b.uri {
                    Some(u) => Ok(Some(u)),
                    None if prefix.is_empty() => Ok(None),
                    None => Err(xml_err(XmlError::UndeclaredPrefix, at as usize)),
                };
            }
        }
        if prefix.is_empty() {
            Ok(None)
        } else {
            Err(xml_err(XmlError::UndeclaredPrefix, at as usize))
        }
    }

    /// Namespace по умолчанию в текущей области видимости.
    #[must_use]
    pub fn default_uri(&self) -> Option<NsUri> {
        self.resolve(&[], 0).ok().flatten()
    }

    /// Текст интернированного URI.
    #[must_use]
    pub fn uri(&self, id: NsUri) -> Option<&str> {
        self.uris.get(id.0 as usize).map(String::as_str)
    }

    /// Идентификатор уже интернированного URI, если он встречался.
    #[must_use]
    pub fn lookup(&self, uri: &str) -> Option<NsUri> {
        self.uris
            .iter()
            .position(|u| u == uri)
            .and_then(|i| u32::try_from(i).ok())
            .map(NsUri)
    }

    fn intern(&mut self, uri: &str) -> NsUri {
        if let Some(id) = self.lookup(uri) {
            return id;
        }
        let id = u32::try_from(self.uris.len()).unwrap_or(u32::MAX);
        self.uris.push(uri.to_owned());
        NsUri(id)
    }
}

fn reserved(at: u32) -> crate::error::Error {
    xml_err(XmlError::ReservedPrefix, at as usize)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;
    use crate::error::Error;

    #[test]
    fn xml_prefix_needs_no_declaration() {
        let ns = Namespaces::new();
        let id = ns.resolve(b"xml", 0).unwrap().unwrap();
        assert_eq!(ns.uri(id), Some(XML_URI));
    }

    #[test]
    fn reserved_prefixes_cannot_be_rebound() {
        let mut ns = Namespaces::new();
        ns.push_frame();
        assert!(matches!(
            ns.declare(b"xmlns", "urn:x", 0),
            Err(Error::Xml {
                kind: XmlError::ReservedPrefix,
                ..
            })
        ));
        assert!(matches!(
            ns.declare(b"xml", "urn:x", 0),
            Err(Error::Xml {
                kind: XmlError::ReservedPrefix,
                ..
            })
        ));
        // К собственному URI объявить можно — так делают некоторые генераторы.
        assert!(ns.declare(b"xml", XML_URI, 0).is_ok());
        // Привязать чужой префикс к служебному URI тоже нельзя.
        assert!(ns.declare(b"p", XMLNS_URI, 0).is_err());
    }

    #[test]
    fn frames_restore_shadowed_bindings() {
        let mut ns = Namespaces::new();
        ns.push_frame();
        ns.declare(b"a", "urn:outer", 0).unwrap();
        let outer = ns.resolve(b"a", 0).unwrap().unwrap();

        ns.push_frame();
        ns.declare(b"a", "urn:inner", 0).unwrap();
        let inner = ns.resolve(b"a", 0).unwrap().unwrap();
        assert_ne!(outer, inner);

        ns.pop_frame();
        assert_eq!(ns.resolve(b"a", 0).unwrap(), Some(outer));
    }

    #[test]
    fn undeclared_prefix_is_error_but_missing_default_is_not() {
        let ns = Namespaces::new();
        assert!(matches!(
            ns.resolve(b"nope", 7),
            Err(Error::Xml {
                kind: XmlError::UndeclaredPrefix,
                offset: 7
            })
        ));
        assert_eq!(ns.resolve(b"", 0).unwrap(), None);
    }

    #[test]
    fn default_namespace_can_be_unbound() {
        let mut ns = Namespaces::new();
        ns.push_frame();
        ns.declare(b"", "urn:x", 0).unwrap();
        assert!(ns.default_uri().is_some());
        ns.push_frame();
        ns.declare(b"", "", 0).unwrap();
        assert_eq!(ns.default_uri(), None);
    }

    #[test]
    fn identical_uris_intern_to_one_id() {
        let mut ns = Namespaces::new();
        ns.push_frame();
        ns.declare(b"a", "urn:same", 0).unwrap();
        ns.declare(b"b", "urn:same", 0).unwrap();
        assert_eq!(ns.resolve(b"a", 0).unwrap(), ns.resolve(b"b", 0).unwrap());
    }
}
