//! `[Content_Types].xml` — единственный обязательный поток пакета.
//!
//! # Что здесь моделируется
//!
//! Две таблицы: `Default` по расширению и `Override` по имени части. Обе
//! регистронезависимы, `Override` важнее `Default`. Это весь механизм: тип
//! части нигде больше не хранится, и часть без типа для OPC не существует.
//!
//! # Почему индекс, а не владение документом
//!
//! [`ContentTypes`] хранит только разбор — строки и идентификаторы узлов, — а
//! сам [`Document`] живёт в слоте пакета вместе с остальными частями. Иначе
//! `[Content_Types].xml` пришлось бы вынести из общего пути сохранения, и он
//! стал бы единственной частью, к которой не применяется правило «нетронутое
//! копируется побайтово».
//!
//! Правка добавляет `Override` последним ребёнком `Types`. Всё остальное —
//! порядок существующих записей, их атрибуты, отступы, стиль экранирования —
//! preserving DOM сохраняет сам; здесь достаточно ничего не переписывать.

use crate::dom::{Document, NodeId, NodeKind};
use crate::error::{OpcError, Result};

use super::partname::PartName;

/// Namespace потока типов.
pub const CONTENT_TYPES_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

/// Имя записи потока типов внутри ZIP. Регистр фиксирован спецификацией,
/// но сравнивать всё равно приходится без учёта регистра: встречаются пакеты,
/// собранные инструментами, которые об этом не знали.
pub const CONTENT_TYPES_NAME: &str = "[Content_Types].xml";

/// Тип по умолчанию для расширения.
#[derive(Debug)]
struct DefaultEntry {
    /// Расширение в нижнем регистре, без точки.
    ext: String,
    content_type: String,
}

/// Тип, назначенный конкретной части.
#[derive(Debug)]
struct OverrideEntry {
    /// Ключ имени части (нижний регистр).
    key: String,
    content_type: String,
    node: NodeId,
}

/// Разбор `[Content_Types].xml`.
#[derive(Debug)]
pub struct ContentTypes {
    root: NodeId,
    /// Префикс для новых элементов вместе с двоеточием либо пустая строка.
    ///
    /// Берётся у корня: если пакет объявил namespace через префикс
    /// (`<ct:Types>`), новый `<Override>` без префикса попал бы в другой
    /// namespace и был бы для читателя невидим.
    prefix: String,
    defaults: Vec<DefaultEntry>,
    overrides: Vec<OverrideEntry>,
}

impl ContentTypes {
    /// Разбирает уже построенное дерево потока типов.
    ///
    /// # Errors
    ///
    /// [`OpcError::NoContentTypes`], если корень — не `Types` в положенном
    /// namespace; [`OpcError::UnknownContentType`], если у `Default` или
    /// `Override` нет обязательного атрибута.
    pub fn parse(doc: &Document) -> Result<Self> {
        let root = doc.root_element()?;
        if doc.local_name(root) != Some(b"Types") || doc.element_uri(root) != Some(CONTENT_TYPES_NS)
        {
            return Err(OpcError::NoContentTypes.into());
        }
        let prefix = prefix_of(doc, root);

        let mut defaults = Vec::new();
        let mut overrides = Vec::new();
        for child in doc.children(root) {
            if doc.kind(child) != Some(NodeKind::Element) {
                continue;
            }
            // Атрибуты без префикса namespace не наследуют — отсюда `None`.
            let ct = doc.attr(child, None, "ContentType");
            match doc.local_name(child) {
                Some(b"Default") => {
                    let (Some(ext), Some(ct)) = (doc.attr(child, None, "Extension"), ct) else {
                        return Err(OpcError::UnknownContentType(
                            "Default без Extension или ContentType".to_owned(),
                        )
                        .into());
                    };
                    defaults.push(DefaultEntry {
                        ext: ext.to_ascii_lowercase(),
                        content_type: ct.into_owned(),
                    });
                }
                Some(b"Override") => {
                    let (Some(part), Some(ct)) = (doc.attr(child, None, "PartName"), ct) else {
                        return Err(OpcError::UnknownContentType(
                            "Override без PartName или ContentType".to_owned(),
                        )
                        .into());
                    };
                    overrides.push(OverrideEntry {
                        key: part.to_ascii_lowercase(),
                        content_type: ct.into_owned(),
                        node: child,
                    });
                }
                // Неизвестный элемент — не ошибка: он останется в дереве и
                // будет записан обратно. Молча игнорировать здесь безопасно
                // именно потому, что байты не теряются.
                _ => {}
            }
        }
        Ok(Self {
            root,
            prefix,
            defaults,
            overrides,
        })
    }

    /// Тип части: `Override`, иначе `Default` по расширению.
    #[must_use]
    pub fn resolve(&self, part: &PartName) -> Option<&str> {
        if let Some(o) = self.overrides.iter().find(|o| o.key == part.key()) {
            return Some(&o.content_type);
        }
        let ext = part.extension()?.to_ascii_lowercase();
        self.defaults
            .iter()
            .find(|d| d.ext == ext)
            .map(|d| d.content_type.as_str())
    }

    /// Тип по умолчанию для расширения.
    #[must_use]
    pub fn default_for(&self, ext: &str) -> Option<&str> {
        let ext = ext.to_ascii_lowercase();
        self.defaults
            .iter()
            .find(|d| d.ext == ext)
            .map(|d| d.content_type.as_str())
    }

    /// Число записей `Override`.
    #[must_use]
    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }

    /// Обеспечивает, что часть разрешается в `content_type`.
    ///
    /// Если это уже так — ничего не делает: `Default` по расширению уже мог
    /// дать нужный тип, и лишний `Override` испачкал бы поток типов, а с ним и
    /// его запись в архиве. Правка, которая ничего не меняет, не должна ничего
    /// перепаковывать — это то же правило, что и для остальных частей.
    ///
    /// # Errors
    ///
    /// Ошибки правки дерева (исчерпана квота узлов, недопустимый символ в
    /// значении атрибута).
    pub fn ensure(
        &mut self,
        doc: &mut Document,
        part: &PartName,
        content_type: &str,
    ) -> Result<bool> {
        if self.resolve(part) == Some(content_type) {
            return Ok(false);
        }
        if let Some(o) = self.overrides.iter_mut().find(|o| o.key == part.key()) {
            doc.set_attr(o.node, "ContentType", content_type)?;
            o.content_type = content_type.to_owned();
            return Ok(true);
        }
        let mut qname = String::with_capacity(self.prefix.len().saturating_add(8));
        qname.push_str(&self.prefix);
        qname.push_str("Override");
        let node = doc.new_element(&qname)?;
        doc.set_attr(node, "PartName", part.as_str())?;
        doc.set_attr(node, "ContentType", content_type)?;
        doc.append_child(self.root, node)?;
        self.overrides.push(OverrideEntry {
            key: part.key().to_owned(),
            content_type: content_type.to_owned(),
            node,
        });
        Ok(true)
    }

    /// Убирает `Override` части, если он есть.
    ///
    /// `Default` не трогается: он общий для всех частей с этим расширением.
    ///
    /// # Errors
    ///
    /// Ошибки правки дерева.
    pub fn remove_override(&mut self, doc: &mut Document, part: &PartName) -> Result<bool> {
        let Some(i) = self.overrides.iter().position(|o| o.key == part.key()) else {
            return Ok(false);
        };
        let node = self
            .overrides
            .get(i)
            .ok_or_else(|| OpcError::PartNotFound(part.as_str().to_owned()))?
            .node;
        doc.remove(node)?;
        let _ = self.overrides.remove(i);
        Ok(true)
    }
}

/// Префикс имени элемента вместе с двоеточием: `ct:Types` → `ct:`.
fn prefix_of(doc: &Document, node: NodeId) -> String {
    let Some(q) = doc.qname(node) else {
        return String::new();
    };
    let Some(i) = q.iter().position(|&b| b == b':') else {
        return String::new();
    };
    q.get(..i.saturating_add(1))
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_default()
}
