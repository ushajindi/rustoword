//! Чтение дерева: навигация, имена, значения.
//!
//! # Где здесь граница миров
//!
//! Всё, что возвращает `&[u8]`, отдаёт **сырые** байты — те же, что уйдут
//! обратно в файл. Всё, что возвращает `Cow<str>`, проходит через
//! [`crate::xml::entity`]: ссылки раскрываются, концы строк нормализуются по
//! XML 1.0. Результат декодирования никогда не возвращается в дерево — иначе
//! `&#34;` из корпуса записался бы как `&quot;`, и файл, который никто не
//! правил, изменился бы (44 вхождения только в одном `.docx`).
//!
//! `Cow` вместо `String` не ради красоты: подавляющее большинство значений в
//! OOXML — `"1"`, `"A1"`, `"s"` — не содержат ни ссылок, ни `\r`, и копировать
//! их незачем.

use std::borrow::Cow;

use crate::error::{Result, XmlError};
use crate::xml::entity;
use crate::xml::lexer::xml_err;

use super::arena::{NodeFlags, NodeId, NodeKind};
use super::node::{Attr, AttrFlags, ElemFlags, ElementData, local_of, prefix_of};
use super::{Document, NIL};

/// URI предопределённого префикса `xml`.
const XML_URI: &str = "http://www.w3.org/XML/1998/namespace";

impl Document {
    /// Корневой элемент части.
    ///
    /// # Errors
    ///
    /// [`XmlError::NoRoot`], если элемента нет — такое возможно только у
    /// дерева, собранного через API.
    pub fn root_element(&self) -> Result<NodeId> {
        self.children(self.root)
            .find(|&c| self.kind(c) == Some(NodeKind::Element))
            .ok_or_else(|| xml_err(XmlError::NoRoot, 0))
    }

    /// Вид узла. `None` — узел не принадлежит документу.
    #[must_use]
    pub fn kind(&self, n: NodeId) -> Option<NodeKind> {
        self.nodes.get(n.index()).map(|x| x.kind)
    }

    /// Родитель узла.
    #[must_use]
    pub fn parent(&self, n: NodeId) -> Option<NodeId> {
        self.nodes.get(n.index()).and_then(|x| x.parent)
    }

    /// Спан узла. Для нетронутого узла — срез исходника; для правленого —
    /// срез арены нового содержимого (см. [`Document::span_in_arena`]).
    #[must_use]
    pub fn span(&self, n: NodeId) -> Option<crate::bytes::Span> {
        self.nodes.get(n.index()).map(|x| x.span)
    }

    /// Указывает ли спан узла в арену, а не в исходник.
    #[must_use]
    pub fn span_in_arena(&self, n: NodeId) -> bool {
        self.nodes
            .get(n.index())
            .is_some_and(|x| x.flags.has(NodeFlags::IN_ARENA))
    }

    /// Элемент записан одним тегом `<a/>`, а не парой `<a></a>`.
    #[must_use]
    pub fn is_self_closing(&self, n: NodeId) -> bool {
        self.nodes
            .get(n.index())
            .is_some_and(|x| x.flags.has(NodeFlags::EMPTY_TAG))
    }

    /// Дети узла в порядке следования.
    pub fn children(&self, n: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        Children {
            doc: self,
            next: self.nodes.get(n.index()).and_then(|x| x.first_child),
        }
    }

    /// Сырое имя элемента вместе с префиксом: `w:p`, а не `p`.
    #[must_use]
    pub fn qname(&self, n: NodeId) -> Option<&[u8]> {
        let e = self.elem(n).ok()?;
        self.bytes(e.qname, e.flags.has(ElemFlags::QNAME_IN_ARENA))
    }

    /// Локальная часть имени элемента.
    ///
    /// Отдельный спан, а не поиск двоеточия при каждом обращении: сравнение
    /// локального имени — самая горячая операция типизированного API, а имя
    /// элемента при жизни документа не меняется.
    #[must_use]
    pub fn local_name(&self, n: NodeId) -> Option<&[u8]> {
        let e = self.elem(n).ok()?;
        self.bytes(e.local, e.flags.has(ElemFlags::QNAME_IN_ARENA))
    }

    /// URI namespace элемента. `None` — элемент вне namespace.
    ///
    /// У элементов, созданных через [`Document::new_element`], namespace не
    /// определён: в момент создания узел ещё не подключён к дереву, и область
    /// видимости префикса неизвестна.
    #[must_use]
    pub fn element_uri(&self, n: NodeId) -> Option<&str> {
        let e = self.elem(n).ok()?;
        if e.ns_uri == ElementData::NO_NS {
            return None;
        }
        self.uris.get(e.ns_uri as usize).map(String::as_str)
    }

    /// Первый ребёнок-элемент с заданным именем.
    ///
    /// `ns == None` означает «элемент **вне** namespace», а не «любой»:
    /// в OOXML безымянный namespace — отдельный случай (`<sheetData>` в
    /// SpreadsheetML лежит в namespace по умолчанию, а `<Relationship>` в
    /// `.rels` — в своём), и «любой» здесь стал бы источником тихих ошибок.
    #[must_use]
    pub fn find_child(&self, n: NodeId, ns: Option<&str>, local: &str) -> Option<NodeId> {
        self.children(n).find(|&c| self.matches(c, ns, local))
    }

    fn matches(&self, n: NodeId, ns: Option<&str>, local: &str) -> bool {
        if self.kind(n) != Some(NodeKind::Element) {
            return false;
        }
        self.local_name(n) == Some(local.as_bytes()) && self.element_uri(n) == ns
    }

    /// Сырое, всё ещё экранированное значение атрибута.
    #[must_use]
    pub fn attr_raw(&self, n: NodeId, ns: Option<&str>, local: &str) -> Option<&[u8]> {
        let (a, _) = self.find_attr(n, ns, local)?;
        self.bytes(a.value, a.flags.has(AttrFlags::VALUE_IN_ARENA))
    }

    /// Декодированное значение атрибута.
    ///
    /// `None` — атрибута нет либо его значение не декодируется (битый UTF-8,
    /// неизвестная сущность). Значение при этом остаётся в файле нетронутым:
    /// невозможность прочитать не даёт права переписать.
    #[must_use]
    pub fn attr(&self, n: NodeId, ns: Option<&str>, local: &str) -> Option<Cow<'_, str>> {
        let raw = self.attr_raw(n, ns, local)?;
        decode_attr_cow(raw).ok()
    }

    /// Сырое имя атрибута по индексу в порядке появления.
    #[must_use]
    pub fn attr_name_at(&self, n: NodeId, i: usize) -> Option<&[u8]> {
        let e = self.elem(n).ok()?;
        let a = self.attrs_of(e).get(i)?;
        self.bytes(a.name, a.flags.has(AttrFlags::NAME_IN_ARENA))
    }

    /// Число атрибутов элемента.
    #[must_use]
    pub fn attr_count(&self, n: NodeId) -> usize {
        self.elem(n).map_or(0, |e| e.attrs_len as usize)
    }

    /// Текстовое содержимое узла.
    ///
    /// Для текстового узла — он сам; для элемента — склейка его
    /// **непосредственных** текстовых детей и секций CDATA. Вложенные элементы
    /// не разворачиваются: `text()` — читающий помощник, а не XPath.
    ///
    /// # Errors
    ///
    /// Содержимое не декодируется (битый UTF-8, неизвестная сущность).
    pub fn text(&self, n: NodeId) -> Result<Cow<'_, str>> {
        let node = self.node(n)?;
        match node.kind {
            NodeKind::Text => {
                let raw = self
                    .bytes(node.span, node.flags.has(NodeFlags::IN_ARENA))
                    .ok_or_else(|| xml_err(XmlError::UnexpectedEof, node.span.start() as usize))?;
                decode_text_cow(raw)
            }
            NodeKind::CData => {
                let raw = self.cdata_body(n)?;
                Ok(String::from_utf8_lossy(raw))
            }
            NodeKind::Element | NodeKind::Document => self.gather_text(n),
            _ => Ok(Cow::Borrowed("")),
        }
    }

    /// Содержимое секции CDATA без обрамления `<![CDATA[` … `]]>`.
    ///
    /// Сущности внутри CDATA не раскрываются по определению формата — здесь
    /// возвращаются именно байты.
    fn cdata_body(&self, n: NodeId) -> Result<&[u8]> {
        let node = self.node(n)?;
        let raw = self
            .bytes(node.span, node.flags.has(NodeFlags::IN_ARENA))
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, node.span.start() as usize))?;
        let body = raw
            .strip_prefix(b"<![CDATA[".as_slice())
            .and_then(|r| r.strip_suffix(b"]]>".as_slice()))
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, node.span.start() as usize))?;
        Ok(body)
    }

    fn gather_text(&self, n: NodeId) -> Result<Cow<'_, str>> {
        let kids: Vec<NodeId> = self
            .children(n)
            .filter(|&c| matches!(self.kind(c), Some(NodeKind::Text | NodeKind::CData)))
            .collect();
        // Один текстовый ребёнок — самый частый случай (`<v>42</v>`), и он
        // обязан обходиться без копии.
        if let (1, Some(&only)) = (kids.len(), kids.first()) {
            return self.text(only);
        }
        let mut out = String::new();
        for c in kids {
            out.push_str(&self.text(c)?);
        }
        Ok(Cow::Owned(out))
    }

    /// Самый глубокий узел, чей спан покрывает офсет исходника.
    ///
    /// Существует ради отладки байтовых расхождений: увидев «первый
    /// разошедшийся байт на позиции 4711», хочется немедленно узнать, чей это
    /// байт, а не листать дамп руками. На изменённом документе бессмысленно —
    /// спаны там уже не плитуют исходник.
    #[must_use]
    pub fn node_at(&self, offset: u32) -> Option<NodeId> {
        let mut best = self.root;
        let mut cur = Some(self.root);
        while let Some(id) = cur {
            best = id;
            cur = self.children(id).find(|&c| {
                self.nodes.get(c.index()).is_some_and(|n| {
                    n.span.start() <= offset && offset < n.span.end() && !n.span.is_empty()
                })
            });
        }
        Some(best)
    }

    /// Путь от корня до узла: `/w:document/w:body/w:p[3]/#text`.
    ///
    /// Индекс в скобках — порядковый номер среди одноимённых братьев, как в
    /// XPath, и по той же причине: без него путь не адресует ничего.
    #[must_use]
    pub fn path(&self, n: NodeId) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut cur = Some(n);
        while let Some(id) = cur {
            let parent = self.parent(id);
            let Some(p) = parent else { break };
            parts.push(self.step(p, id));
            cur = parent;
        }
        parts.reverse();
        if parts.is_empty() {
            return "/".to_owned();
        }
        let mut out = String::new();
        for p in parts {
            out.push('/');
            out.push_str(&p);
        }
        out
    }

    fn step(&self, parent: NodeId, child: NodeId) -> String {
        let label = match self.kind(child) {
            Some(NodeKind::Element) => self
                .qname(child)
                .map(|q| String::from_utf8_lossy(q).into_owned())
                .unwrap_or_else(|| "?".to_owned()),
            Some(NodeKind::Text) => "#text".to_owned(),
            Some(NodeKind::CData) => "#cdata".to_owned(),
            Some(NodeKind::Comment) => "#comment".to_owned(),
            Some(NodeKind::Pi) => "#pi".to_owned(),
            Some(NodeKind::Decl) => "#decl".to_owned(),
            _ => "#?".to_owned(),
        };
        let mut idx = 0usize;
        let mut nth = 0usize;
        for c in self.children(parent) {
            let same = match (self.kind(c), self.kind(child)) {
                (Some(NodeKind::Element), Some(NodeKind::Element)) => {
                    self.qname(c) == self.qname(child)
                }
                (a, b) => a == b,
            };
            if same {
                idx = idx.saturating_add(1);
                if c == child {
                    nth = idx;
                }
            }
        }
        if idx > 1 {
            format!("{label}[{nth}]")
        } else {
            label
        }
    }

    // --- внутреннее -------------------------------------------------------

    /// Находит атрибут по (URI, локальное имя). Возвращает его и индекс.
    pub(crate) fn find_attr(
        &self,
        n: NodeId,
        ns: Option<&str>,
        local: &str,
    ) -> Option<(&Attr, usize)> {
        let e = self.elem(n).ok()?;
        for (i, a) in self.attrs_of(e).iter().enumerate() {
            let name = self.bytes(a.name, a.flags.has(AttrFlags::NAME_IN_ARENA))?;
            if local_of(name) != local.as_bytes() {
                continue;
            }
            if self.attr_uri(n, prefix_of(name)) == ns {
                return Some((a, i));
            }
        }
        None
    }

    /// Находит атрибут по сырому имени, как оно написано.
    pub(crate) fn find_attr_raw(&self, n: NodeId, qname: &[u8]) -> Option<usize> {
        let e = self.elem(n).ok()?;
        self.attrs_of(e)
            .iter()
            .position(|a| self.bytes(a.name, a.flags.has(AttrFlags::NAME_IN_ARENA)) == Some(qname))
    }

    /// URI, в котором находится атрибут с таким префиксом.
    ///
    /// Атрибут без префикса namespace по умолчанию **не** наследует — так
    /// требует Namespaces in XML, и от этого зависит, считать ли `val` и
    /// `w:val` одним атрибутом.
    fn attr_uri(&self, elem: NodeId, prefix: &[u8]) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        if prefix == b"xml" {
            return Some(XML_URI);
        }
        self.resolve_prefix(elem, prefix)
    }

    /// Ищет объявление префикса, поднимаясь от элемента к корню.
    ///
    /// Таблица `uris` заполняется только для элементов, поэтому у атрибутов
    /// разрешение делается по дереву. Это дешевле, чем хранить `u32` на каждом
    /// атрибуте: атрибутов в большой части под миллион, а поиск идёт на
    /// глубину 10–20 и только когда его действительно попросили.
    fn resolve_prefix(&self, from: NodeId, prefix: &[u8]) -> Option<&str> {
        let mut cur = Some(from);
        while let Some(id) = cur {
            if let Ok(e) = self.elem(id) {
                for a in self.attrs_of(e) {
                    let name = self.bytes(a.name, a.flags.has(AttrFlags::NAME_IN_ARENA))?;
                    if prefix_of(name) == b"xmlns" && local_of(name) == prefix {
                        let raw = self.bytes(a.value, a.flags.has(AttrFlags::VALUE_IN_ARENA))?;
                        // Пустое значение — снятие привязки (`xmlns:p=""`).
                        return core::str::from_utf8(raw).ok().filter(|s| !s.is_empty());
                    }
                }
            }
            cur = self.parent(id);
        }
        None
    }

    /// Спан имени элемента вместе с признаком буфера — нужен редактору.
    pub(crate) fn qname_ref(&self, n: NodeId) -> (crate::bytes::Span, bool) {
        self.elem(n).map_or((NIL, false), |e| {
            (e.qname, e.flags.has(ElemFlags::QNAME_IN_ARENA))
        })
    }
}

struct Children<'a> {
    doc: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.doc.nodes.get(cur.index()).and_then(|n| n.next);
        Some(cur)
    }
}

/// Декодирует значение атрибута, копируя только когда без копии нельзя.
fn decode_attr_cow(raw: &[u8]) -> Result<Cow<'_, str>> {
    // Нормализация значения атрибута (XML 1.0 §3.3.3) трогает `\t`, `\n`, `\r`,
    // раскрытие ссылок — `&`. Нет их — значение равно своим байтам.
    if raw
        .iter()
        .any(|&b| matches!(b, b'&' | b'\t' | b'\n' | b'\r'))
    {
        return Ok(Cow::Owned(entity::decode_attr_to_string(raw)?));
    }
    borrow_utf8(raw)
}

fn decode_text_cow(raw: &[u8]) -> Result<Cow<'_, str>> {
    // В тексте нормализуется только `\r` (XML 1.0 §2.11); `\n` и `\t` целы.
    if raw.iter().any(|&b| matches!(b, b'&' | b'\r')) {
        return Ok(Cow::Owned(entity::decode_text_to_string(raw)?));
    }
    borrow_utf8(raw)
}

fn borrow_utf8(raw: &[u8]) -> Result<Cow<'_, str>> {
    core::str::from_utf8(raw)
        .map(Cow::Borrowed)
        .map_err(|e| xml_err(XmlError::NotUtf8, e.valid_up_to()))
}
