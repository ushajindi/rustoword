//! Файлы отношений `_rels/*.rels`.
//!
//! # Что такое отношение
//!
//! Часть не ссылается на другую часть по имени. Она ссылается на
//! идентификатор (`r:id="rId3"`), а таблица «идентификатор → цель» лежит
//! рядом, в отдельной части. Смысл косвенности в том, что имена частей —
//! деталь упаковки: переименовать `sheet1.xml` можно, не трогая ни одного
//! документа, который на неё ссылается.
//!
//! # Внутренние и внешние цели
//!
//! `TargetMode="External"` означает, что цель — не часть пакета, а URL или
//! путь в файловой системе. Такую цель **нельзя** разрешать в имя части:
//! `https://example.com/a.xml` после наивного резолвинга превратился бы в
//! `/word/https:/example.com/a.xml`, и слой бы «нашёл» часть, которой нет.
//! Поэтому режим — часть типа [`Rel`], а не флаг, о котором можно забыть.
//!
//! # Почему разбор, а не владение документом
//!
//! Как и [`super::ContentTypes`], [`Rels`] хранит только проекцию: сам
//! документ живёт в слоте пакета и подчиняется общему правилу «нетронутое
//! копируется побайтово».

use crate::dom::{Document, NodeId, NodeKind};
use crate::error::{OpcError, Result};

use super::partname::PartName;

/// Namespace файла отношений.
pub const RELATIONSHIPS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Содержимое пустого файла отношений — заготовка для новой части.
///
/// Записано ровно так, как его пишет Office: та же декларация, отсутствие
/// перевода строки, самозакрывающийся корень. Совпадение не обязательно, но
/// диффы пакетов после нашей правки читать легче, когда новые части выглядят
/// как соседние.
pub const EMPTY_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>"#;

/// Куда указывает цель отношения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    /// Часть этого же пакета. Цель разрешается в [`PartName`].
    Internal,
    /// Ресурс вне пакета. Цель остаётся строкой и никогда не резолвится.
    External,
}

/// Одно отношение.
#[derive(Debug, Clone)]
pub struct Rel {
    /// Значение атрибута `Id` — то, на что ссылается `r:id` в документе.
    pub id: String,
    /// Полный URI типа отношения.
    pub rel_type: String,
    /// Цель как записана: относительная, абсолютная или внешняя.
    pub target: String,
    pub mode: TargetMode,
    /// Узел `<Relationship>` в дереве — нужен, чтобы удалить отношение,
    /// не перестраивая часть.
    node: NodeId,
}

/// Разбор одного файла отношений.
#[derive(Debug)]
pub struct Rels {
    /// Часть, которой принадлежат отношения. Для `/_rels/.rels` — корень
    /// пакета: относительные цели там считаются от `/`.
    owner: PartName,
    root: NodeId,
    prefix: String,
    rels: Vec<Rel>,
}

impl Rels {
    /// Разбирает дерево файла отношений.
    ///
    /// # Errors
    ///
    /// [`OpcError::BadRelationships`], если корень — не `Relationships` в
    /// положенном namespace либо у отношения нет `Id`, `Type` или `Target`.
    pub fn parse(owner: PartName, doc: &Document) -> Result<Self> {
        let bad = |what: &str| -> crate::Error {
            let mut m = String::from(owner.as_str());
            m.push_str(": ");
            m.push_str(what);
            OpcError::BadRelationships(m).into()
        };
        let root = doc.root_element()?;
        if doc.local_name(root) != Some(b"Relationships")
            || doc.element_uri(root) != Some(RELATIONSHIPS_NS)
        {
            return Err(bad("корень не Relationships"));
        }
        let prefix = prefix_of(doc, root);

        let mut rels = Vec::new();
        for child in doc.children(root) {
            if doc.kind(child) != Some(NodeKind::Element)
                || doc.local_name(child) != Some(b"Relationship")
            {
                continue;
            }
            let (Some(id), Some(rel_type), Some(target)) = (
                doc.attr(child, None, "Id"),
                doc.attr(child, None, "Type"),
                doc.attr(child, None, "Target"),
            ) else {
                return Err(bad("Relationship без Id, Type или Target"));
            };
            // Любое значение, кроме точного `Internal`, считается внешним
            // только если это точный `External`: незнакомое значение — повод
            // не резолвить, а не повод угадывать.
            let mode = match doc.attr(child, None, "TargetMode").as_deref() {
                Some("External") => TargetMode::External,
                Some("Internal") | None => TargetMode::Internal,
                Some(_) => return Err(bad("неизвестный TargetMode")),
            };
            rels.push(Rel {
                id: id.into_owned(),
                rel_type: rel_type.into_owned(),
                target: target.into_owned(),
                mode,
                node: child,
            });
        }
        Ok(Self {
            owner,
            root,
            prefix,
            rels,
        })
    }

    /// Часть-владелец отношений.
    #[must_use]
    pub const fn owner(&self) -> &PartName {
        &self.owner
    }

    /// Все отношения в порядке файла.
    pub fn iter(&self) -> impl Iterator<Item = &Rel> {
        self.rels.iter()
    }

    /// Число отношений.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rels.len()
    }

    /// Отношений нет.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rels.is_empty()
    }

    /// Отношение по идентификатору.
    #[must_use]
    pub fn by_id(&self, id: &str) -> Option<&Rel> {
        self.rels.iter().find(|r| r.id == id)
    }

    /// Все отношения заданного типа.
    pub fn by_type<'s>(&'s self, rel_type: &'s str) -> impl Iterator<Item = &'s Rel> + 's {
        self.rels.iter().filter(move |r| r.rel_type == rel_type)
    }

    /// Имя части, на которую указывает отношение. `None` — цель внешняя.
    ///
    /// # Errors
    ///
    /// [`OpcError::BadPartName`], если цель не разрешается в имя части.
    pub fn resolve(&self, rel: &Rel) -> Result<Option<PartName>> {
        match rel.mode {
            TargetMode::External => Ok(None),
            TargetMode::Internal => self.owner.resolve(&rel.target).map(Some),
        }
    }

    /// Имя части по идентификатору отношения.
    ///
    /// # Errors
    ///
    /// [`OpcError::RelationshipNotFound`], если идентификатора нет;
    /// [`crate::error::Error::Unsupported`], если цель внешняя.
    pub fn target_of(&self, id: &str) -> Result<PartName> {
        let rel = self
            .by_id(id)
            .ok_or_else(|| OpcError::RelationshipNotFound(id.to_owned()))?;
        self.resolve(rel)?.ok_or(crate::Error::Unsupported(
            "opc: внешняя цель не является частью пакета",
        ))
    }

    /// Свободный идентификатор вида `rIdN`.
    #[must_use]
    pub fn next_id(&self) -> String {
        let mut n = self.rels.len().saturating_add(1);
        loop {
            let candidate = format!("rId{n}");
            if self.by_id(&candidate).is_none() {
                return candidate;
            }
            n = n.saturating_add(1);
        }
    }

    /// Добавляет отношение и возвращает его идентификатор.
    ///
    /// Цель записывается как есть — вызывающий решает, относительная она или
    /// абсолютная. Так делает и Office: `docProps/app.xml` в `/_rels/.rels`
    /// относителен, а `/xl/sharedStrings.xml` в некоторых пакетах — нет.
    ///
    /// # Errors
    ///
    /// Ошибки правки дерева.
    pub fn add(
        &mut self,
        doc: &mut Document,
        rel_type: &str,
        target: &str,
        mode: TargetMode,
    ) -> Result<String> {
        let id = self.next_id();
        let mut qname = String::with_capacity(self.prefix.len().saturating_add(12));
        qname.push_str(&self.prefix);
        qname.push_str("Relationship");
        let node = doc.new_element(&qname)?;
        doc.set_attr(node, "Id", &id)?;
        doc.set_attr(node, "Type", rel_type)?;
        doc.set_attr(node, "Target", target)?;
        if mode == TargetMode::External {
            doc.set_attr(node, "TargetMode", "External")?;
        }
        doc.append_child(self.root, node)?;
        self.rels.push(Rel {
            id: id.clone(),
            rel_type: rel_type.to_owned(),
            target: target.to_owned(),
            mode,
            node,
        });
        Ok(id)
    }

    /// Удаляет отношение по идентификатору.
    ///
    /// # Errors
    ///
    /// Ошибки правки дерева.
    pub fn remove_by_id(&mut self, doc: &mut Document, id: &str) -> Result<bool> {
        let Some(i) = self.rels.iter().position(|r| r.id == id) else {
            return Ok(false);
        };
        let node = self
            .rels
            .get(i)
            .ok_or_else(|| OpcError::RelationshipNotFound(id.to_owned()))?
            .node;
        doc.remove(node)?;
        let _ = self.rels.remove(i);
        Ok(true)
    }

    /// Удаляет все внутренние отношения, указывающие на часть. Возвращает
    /// число удалённых.
    ///
    /// Нужно при удалении части: отношение на несуществующую часть делает
    /// пакет невалидным, и оставить его — значит сломать файл тише, чем это
    /// сделал бы отказ.
    ///
    /// # Errors
    ///
    /// Ошибки правки дерева.
    pub fn remove_targeting(&mut self, doc: &mut Document, part: &PartName) -> Result<usize> {
        let doomed: Vec<String> = self
            .rels
            .iter()
            .filter(|r| {
                r.mode == TargetMode::Internal
                    && self
                        .owner
                        .resolve(&r.target)
                        .is_ok_and(|p| p.key() == part.key())
            })
            .map(|r| r.id.clone())
            .collect();
        let mut n = 0usize;
        for id in &doomed {
            if self.remove_by_id(doc, id)? {
                n = n.saturating_add(1);
            }
        }
        Ok(n)
    }
}

/// Префикс имени элемента вместе с двоеточием.
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
