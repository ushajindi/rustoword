//! Правка дерева.
//!
//! # Что делает правка
//!
//! Ставит `SELF_DIRTY` на изменённый узел и `HAS_DIRTY_DESC` вверх до корня.
//! Это O(глубина), а глубина в OOXML — 10–20. Всё остальное дерево флагов не
//! получает, а значит, остаётся под фаст-пасом сериализатора: правка одного
//! атрибута переписывает ровно один регион файла.
//!
//! # Что правка НЕ делает
//!
//! Не трогает чужие байты. Новое значение экранируется правилами
//! [`crate::xml::escape`] и дописывается в арену; исходный спан просто
//! перестаёт использоваться. Пути «взять чужой спан, декодировать,
//! заэкранировать заново» в коде нет — именно поэтому `&#34;`, `&#xA;`,
//! `<a></a>` и CRLF переживают любую правку соседнего узла.
//!
//! # Почему `\r` в новом тексте становится `&#xD;`
//!
//! XML 1.0 §2.11 обязывает парсер заменить литеральный `\r` на `\n` при
//! разборе, а §3.3.3 — заменить `\t`, `\n`, `\r` в значении атрибута на пробел.
//! Числовые ссылки под эти правила не подпадают. Запиши мы литеральный таб в
//! атрибут — первый round-trip прошёл бы (байты те же), а второй разошёлся бы,
//! потому что при повторном разборе значение стало бы другим. Это делает
//! [`crate::xml::escape`], здесь важно лишь то, что другого пути к записи
//! пользовательской строки нет.

use crate::bytes::Span;
use crate::error::{Error, Result, XmlError};
use crate::xml::escape::{escape_attr, escape_text};
use crate::xml::lexer::xml_err;

use super::arena::{Node, NodeFlags, NodeId, NodeKind, link_before, link_last, unlink};
use super::node::{Attr, AttrFlags, ElemFlags, ElementData};
use super::{ARENA_SPACE, Document, NIL};

impl Document {
    /// Задаёт значение атрибута по сырому имени, как оно пишется в файле.
    ///
    /// Атрибут ищется по написанию, а не по паре (URI, локальное имя): вызов
    /// `set_attr(p, "w:val", …)` обязан попасть в тот же атрибут, который
    /// увидит человек в редакторе. Существующий атрибут сохраняет свои кавычки
    /// и пробелы вокруг `=` — меняется только значение.
    ///
    /// Новый атрибут дописывается последним, в двойных кавычках, отделённый
    /// одним пробелом.
    ///
    /// # Errors
    ///
    /// Узел не элемент; имя не является QName; значение содержит символ,
    /// недопустимый в XML 1.0.
    pub fn set_attr(&mut self, n: NodeId, qname: &str, value: &str) -> Result<()> {
        check_qname(qname)?;
        let _ = self.elem(n)?;

        if let Some(i) = self.find_attr_raw(n, qname.as_bytes()) {
            let e = self.elem(n)?;
            let at = (e.attrs_from as usize).saturating_add(i);
            let quote = self.attrs.get(at).map_or(b'"', |a| a.quote);
            let v = self.push_arena(|o| escape_attr(value, quote, o))?;
            if let Some(a) = self.attrs.get_mut(at) {
                a.value = v;
                a.flags.set(AttrFlags::VALUE_IN_ARENA);
            }
            self.mark_self_dirty(n);
            return Ok(());
        }

        let name = self.push_arena(|o| {
            o.extend_from_slice(qname.as_bytes());
            Ok(())
        })?;
        let v = self.push_arena(|o| escape_attr(value, b'"', o))?;
        let mut flags = AttrFlags::default();
        flags.set(AttrFlags::NAME_IN_ARENA);
        flags.set(AttrFlags::VALUE_IN_ARENA);
        flags.set(AttrFlags::WS_IN_ARENA);
        let attr = Attr {
            // Пробел, которым арена засеяна при создании документа.
            pre_ws: ARENA_SPACE,
            name,
            ws_before_eq: NIL,
            ws_after_eq: NIL,
            value: v,
            quote: b'"',
            flags,
        };
        self.relocate_attrs(n, |list| list.push(attr))?;
        self.mark_self_dirty(n);
        Ok(())
    }

    /// Удаляет атрибут по (URI namespace, локальное имя).
    ///
    /// # Errors
    ///
    /// Узел не элемент.
    pub fn remove_attr(&mut self, n: NodeId, ns: Option<&str>, local: &str) -> Result<bool> {
        let Some((_, i)) = self.find_attr(n, ns, local) else {
            let _ = self.elem(n)?;
            return Ok(false);
        };
        self.relocate_attrs(n, |list| {
            if i < list.len() {
                let _ = list.remove(i);
            }
        })?;
        self.mark_self_dirty(n);
        Ok(true)
    }

    /// Задаёт текстовое содержимое.
    ///
    /// Для текстового узла заменяет его целиком. Для элемента заменяет **всех**
    /// детей одним текстовым узлом; самозакрывающийся `<a/>` при этом
    /// становится парой `<a>…</a>` — иначе текст записать некуда.
    ///
    /// # Errors
    ///
    /// Узел не элемент и не текст; строка содержит символ, недопустимый в
    /// XML 1.0.
    pub fn set_text(&mut self, n: NodeId, value: &str) -> Result<()> {
        match self.kind(n) {
            Some(NodeKind::Text) => {
                let span = self.push_arena(|o| escape_text(value, o))?;
                let node = self.node_mut(n)?;
                node.span = span;
                node.flags.set(NodeFlags::IN_ARENA);
                self.mark_self_dirty(n);
                Ok(())
            }
            Some(NodeKind::Element) => {
                let kids: Vec<NodeId> = self.children(n).collect();
                for c in kids {
                    unlink(&mut self.nodes, c);
                }
                let span = self.push_arena(|o| escape_text(value, o))?;
                let mut flags = NodeFlags::default();
                flags.set(NodeFlags::SELF_DIRTY);
                flags.set(NodeFlags::IN_ARENA);
                let text = self.alloc(NodeKind::Text, span, flags)?;
                self.open_for_children(n)?;
                link_last(&mut self.nodes, n, text);
                self.mark_dirty_chain(Some(n));
                Ok(())
            }
            _ => Err(Error::Unsupported(
                "dom: текст задаётся только элементу или текстовому узлу",
            )),
        }
    }

    /// Создаёт элемент, не подключённый к дереву.
    ///
    /// Пока узел не подключён, он ни на один байт вывода не влияет. Namespace
    /// у него не определён: область видимости префикса станет известна только
    /// после подключения.
    ///
    /// # Errors
    ///
    /// Имя не является QName; исчерпана квота узлов.
    pub fn new_element(&mut self, qname: &str) -> Result<NodeId> {
        check_qname(qname)?;
        let name = self.push_arena(|o| {
            o.extend_from_slice(qname.as_bytes());
            Ok(())
        })?;
        let local_len = u32::try_from(super::node::local_of(qname.as_bytes()).len()).unwrap_or(0);
        let local = Span::clamped(name.end().saturating_sub(local_len), name.end());

        let mut flags = NodeFlags::default();
        flags.set(NodeFlags::SELF_DIRTY);
        // Новый элемент рождается пустым и пишется как `<a/>`; появятся дети —
        // `append_child` снимет флаг и включит закрывающий тег.
        flags.set(NodeFlags::EMPTY_TAG);
        let id = self.alloc(NodeKind::Element, NIL, flags)?;

        let mut eflags = ElemFlags::default();
        eflags.set(ElemFlags::QNAME_IN_ARENA);
        let aux = u32::try_from(self.elements.len()).unwrap_or(u32::MAX);
        self.elements.push(ElementData {
            qname: name,
            local,
            pre_close_ws: NIL,
            close_qname: name,
            close_trailing_ws: NIL,
            attrs_from: u32::try_from(self.attrs.len()).unwrap_or(0),
            attrs_len: 0,
            ns_uri: ElementData::NO_NS,
            flags: eflags,
        });
        self.node_mut(id)?.aux = aux;
        self.dirty = true;
        Ok(id)
    }

    /// Делает `child` последним ребёнком `parent`.
    ///
    /// # Errors
    ///
    /// `parent` не может иметь детей; `child` уже подключён; попытка создать
    /// цикл.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<()> {
        self.check_attachable(parent, child)?;
        self.open_for_children(parent)?;
        link_last(&mut self.nodes, parent, child);
        self.mark_dirty_chain(Some(parent));
        Ok(())
    }

    /// Вставляет `child` непосредственно перед `anchor`.
    ///
    /// # Errors
    ///
    /// У `anchor` нет родителя; `child` уже подключён; попытка создать цикл.
    pub fn insert_before(&mut self, anchor: NodeId, child: NodeId) -> Result<()> {
        let parent = self
            .parent(anchor)
            .ok_or(Error::Unsupported("dom: у якоря нет родителя"))?;
        self.check_attachable(parent, child)?;
        link_before(&mut self.nodes, anchor, child);
        self.mark_dirty_chain(Some(parent));
        Ok(())
    }

    /// Отсоединяет узел от дерева.
    ///
    /// Узел не уничтожается: его идентификатор остаётся валидным, и его можно
    /// подключить обратно. Освобождать место в арене узлов незачем — документ
    /// живёт ровно одну сессию правки.
    ///
    /// # Errors
    ///
    /// Узел — корень дерева либо уже отсоединён.
    pub fn remove(&mut self, n: NodeId) -> Result<()> {
        if n == self.root {
            return Err(Error::Unsupported("dom: узел-документ удалить нельзя"));
        }
        let parent = self
            .parent(n)
            .ok_or(Error::Unsupported("dom: узел уже отсоединён"))?;
        unlink(&mut self.nodes, n);
        // Родитель не «грязный сам», но его внутренность изменилась: фаст-пас
        // записал бы старый спан вместе с удалённым ребёнком.
        self.mark_dirty_chain(Some(parent));
        Ok(())
    }

    /// Копирует поддерево. Копия не подключена к дереву.
    ///
    /// Спаны копии указывают в тот же исходник, поэтому чистая копия
    /// сериализуется тем же фаст-пасом и байт в байт совпадает с оригиналом.
    ///
    /// # Errors
    ///
    /// Узла нет; исчерпана квота узлов.
    pub fn clone_subtree(&mut self, n: NodeId) -> Result<NodeId> {
        let root = self.copy_node(n)?;
        // Обход явным стеком, а не рекурсией: глубина дерева, собранного через
        // API, ничем не ограничена, а переполнение стека — это не ошибка,
        // которую можно вернуть.
        let mut work = vec![(n, root)];
        while let Some((src, dst)) = work.pop() {
            let mut cur = self.node(src)?.first_child;
            while let Some(c) = cur {
                let copy = self.copy_node(c)?;
                link_last(&mut self.nodes, dst, copy);
                work.push((c, copy));
                cur = self.node(c)?.next;
            }
        }
        Ok(root)
    }

    // --- внутреннее -------------------------------------------------------

    pub(crate) fn alloc(&mut self, kind: NodeKind, span: Span, flags: NodeFlags) -> Result<NodeId> {
        let n = self.nodes.len() as u64;
        self.limits.check_nodes(n.saturating_add(1))?;
        let id = NodeId::from_index(self.nodes.len())
            .ok_or(Error::Unsupported("dom: исчерпано пространство узлов"))?;
        self.nodes.push(Node::new(kind, span, flags));
        Ok(id)
    }

    /// Копирует один узел без связей.
    fn copy_node(&mut self, src: NodeId) -> Result<NodeId> {
        let old = *self.node(src)?;
        let id = self.alloc(old.kind, old.span, old.flags)?;
        if old.kind == NodeKind::Element {
            let e = *self
                .elements
                .get(old.aux as usize)
                .ok_or(Error::Unsupported(super::BAD_NODE))?;
            // Атрибуты копируются, а не разделяются: правка значения работает
            // по месту, и общий диапазон изменил бы заодно и оригинал.
            let from = u32::try_from(self.attrs.len()).unwrap_or(u32::MAX);
            let range = {
                let a = e.attrs_from as usize;
                let b = a.saturating_add(e.attrs_len as usize);
                self.attrs.get(a..b).unwrap_or(&[]).to_vec()
            };
            self.attrs.extend_from_slice(&range);
            let aux = u32::try_from(self.elements.len()).unwrap_or(u32::MAX);
            self.elements.push(ElementData {
                attrs_from: from,
                ..e
            });
            self.node_mut(id)?.aux = aux;
        }
        Ok(id)
    }

    /// Переносит диапазон атрибутов элемента в конец общего вектора,
    /// применяя к нему `f`.
    ///
    /// Диапазон переезжает, а не правится по месту, потому что вставка или
    /// удаление сдвинули бы атрибуты всех последующих элементов. Старое место
    /// остаётся неиспользованным — за сессию правки это единицы килобайт.
    fn relocate_attrs(&mut self, n: NodeId, f: impl FnOnce(&mut Vec<Attr>)) -> Result<()> {
        let e = *self.elem(n)?;
        let a = e.attrs_from as usize;
        let b = a.saturating_add(e.attrs_len as usize);
        let mut list = self.attrs.get(a..b).unwrap_or(&[]).to_vec();
        f(&mut list);
        let from = u32::try_from(self.attrs.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(list.len()).unwrap_or(0);
        self.limits.check_attrs(u64::from(len))?;
        self.attrs.extend_from_slice(&list);
        let e = self.elem_mut(n)?;
        e.attrs_from = from;
        e.attrs_len = len;
        Ok(())
    }

    /// Готовит элемент к появлению детей: снимает `<a/>` и включает `</a>`.
    fn open_for_children(&mut self, n: NodeId) -> Result<()> {
        if self.kind(n) != Some(NodeKind::Element) {
            return Ok(());
        }
        if !self.node(n)?.flags.has(NodeFlags::EMPTY_TAG) {
            return Ok(());
        }
        let (qname, _) = self.qname_ref(n);
        self.elem_mut(n)?.close_qname = qname;
        let node = self.node_mut(n)?;
        node.flags.clear(NodeFlags::EMPTY_TAG);
        // Форма тега изменилась — фаст-пас для этого узла больше не годится.
        node.flags.set(NodeFlags::SELF_DIRTY);
        self.dirty = true;
        Ok(())
    }

    fn check_attachable(&self, parent: NodeId, child: NodeId) -> Result<()> {
        let p = self.node(parent)?;
        if !p.kind.may_have_children() {
            return Err(Error::Unsupported(
                "dom: у узла этого вида не может быть детей",
            ));
        }
        if child == self.root {
            return Err(Error::Unsupported(
                "dom: узел-документ не может быть ребёнком",
            ));
        }
        if self.node(child)?.parent.is_some() {
            return Err(Error::Unsupported("dom: узел уже подключён к дереву"));
        }
        // Цикл превратил бы сериализацию в бесконечный обход, и поймать его
        // потом было бы нечем: у дерева нет счётчика посещений.
        let mut cur = Some(parent);
        while let Some(id) = cur {
            if id == child {
                return Err(Error::Unsupported("dom: подключение создало бы цикл"));
            }
            cur = self.node(id)?.parent;
        }
        Ok(())
    }

    /// Отмечает узел изменённым и поднимает признак до корня.
    pub(crate) fn mark_self_dirty(&mut self, n: NodeId) {
        if let Some(node) = self.nodes.get_mut(n.index()) {
            node.flags.set(NodeFlags::SELF_DIRTY);
        }
        let parent = self.nodes.get(n.index()).and_then(|x| x.parent);
        self.mark_dirty_chain(parent);
    }

    /// Поднимает `HAS_DIRTY_DESC` от `from` до корня.
    pub(crate) fn mark_dirty_chain(&mut self, from: Option<NodeId>) {
        self.dirty = true;
        let mut cur = from;
        while let Some(id) = cur {
            let Some(node) = self.nodes.get_mut(id.index()) else {
                return;
            };
            // Признак ставится всегда до самого корня, поэтому встреченный
            // флаг означает, что вся цепочка выше уже отмечена.
            if node.flags.has(NodeFlags::HAS_DIRTY_DESC) {
                return;
            }
            node.flags.set(NodeFlags::HAS_DIRTY_DESC);
            cur = node.parent;
        }
    }
}

/// Проверяет, что строка — QName: имя, возможно с одним префиксом.
///
/// Проверка байт намеренно грубая и совпадает с лексером: всё вне ASCII
/// считается допустимым в имени. Отвергать экзотические имена не за что, а
/// точная таблица `NameStartChar` требует разбора UTF-8 на каждом имени.
fn check_qname(s: &str) -> Result<()> {
    let b = s.as_bytes();
    let bad = || xml_err(XmlError::BadName, 0);
    let first = *b.first().ok_or_else(bad)?;
    if !(first.is_ascii_alphabetic() || matches!(first, b'_') || first >= 0x80) {
        return Err(bad());
    }
    let mut colons = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if c == b':' {
            colons = colons.saturating_add(1);
            // Ни `:a`, ни `a:` не имеют смысла: пустой префикс — это его
            // отсутствие, пустая локальная часть — отсутствие имени.
            if i == 0 || i.saturating_add(1) == b.len() {
                return Err(bad());
            }
            continue;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.') || c >= 0x80) {
            return Err(bad());
        }
    }
    if colons > 1 {
        return Err(bad());
    }
    Ok(())
}
