//! Preserving DOM — дерево, которое хранит исходные байты целиком.
//!
//! # Зачем
//!
//! Проект существует ради одного свойства: правка чужого документа не должна
//! ничего разрушать. Реальный `.docx` содержит сотни элементов ECMA-376 плюс
//! вендорские расширения — смоделировать их все невозможно. Библиотеки на
//! полной типизированной модели теряют при сохранении всё, чего не знают:
//! ревизии, комментарии, нумерацию, макросы.
//!
//! Здесь другой подход. **Дерево хранит исходные байты целиком, узлы ссылаются
//! на них спанами, и всё нетронутое сериализуется одним `extend_from_slice`.**
//! Типизированный API следующих вех — фасад над этим деревом, а не замена ему.
//!
//! # Инвариант покрытия
//!
//! Спаны детей плотно, без пропусков и наложений, покрывают внутренность
//! родителя; спаны верхнего уровня покрывают весь файл (после BOM). Пробелы
//! между тегами, перевод строки после декларации, комментарии, PI — отдельные
//! узлы со своими спанами. Ничего не «выбрасывается как незначимое».
//!
//! Отсюда главное следствие: сериализация нетронутого дерева — это
//! `src[0..len]` **по построению**, а не благодаря аккуратному переписыванию
//! каждой лексемы. Проверяется [`Document::check_coverage`] и отладочным
//! ассертом при разборе.
//!
//! # Два мира байт
//!
//! Байты бывают либо из исходника (спан в `src`), либо написанные нами
//! (спан в `arena`). Переход возможен ровно один: значение, поданное
//! пользователем как `&str`, экранируется правилами [`crate::xml::escape`] и
//! дописывается в арену. Обратного перехода нет: чужой спан невозможно
//! «переэкранировать», потому что для этого его пришлось бы сначала
//! декодировать в `String`, а такой шаг есть только в читающем API и его
//! результат в сериализатор не попадает.
//!
//! Что это спасает на измеренных данных: `&#34;` не станет `&quot;` (44
//! вхождения), `<a></a>` не станет `<a/>` (104 911 вхождений в 34 файлах),
//! CRLF не станет LF (204 части), одиночный CR не станет LF (110 частей).
//!
//! # Пример
//!
//! ```
//! use ooxml::Limits;
//! use ooxml::dom::Document;
//!
//! let src = b"<?xml version=\"1.0\"?>\r\n<w:p xmlns:w=\"urn:w\"><w:t>a</w:t></w:p>";
//! let mut doc = Document::parse(src.to_vec(), &Limits::strict())?;
//! assert_eq!(doc.serialize()?, src);
//!
//! let root = doc.root_element()?;
//! doc.set_attr(root, "w:rsidR", "00A1")?;
//! let out = doc.serialize()?;
//! // Изменился ровно один регион: всё до и после атрибута совпадает побайтово.
//! assert!(out.starts_with(b"<?xml version=\"1.0\"?>\r\n<w:p xmlns:w=\"urn:w\""));
//! assert!(out.ends_with(b"><w:t>a</w:t></w:p>"));
//! # Ok::<(), ooxml::Error>(())
//! ```

mod arena;
mod edit;
mod node;
mod parse;
mod query;
mod serialize;

pub use arena::{Node, NodeFlags, NodeId, NodeKind};
pub use node::{Attr, AttrFlags, ElemFlags, ElementData};
pub use parse::CoverageBreak;

use crate::bytes::Span;
use crate::error::{Error, Result};
use crate::limits::Limits;

/// Сообщение для идентификатора узла, которого в документе нет.
///
/// В `error.rs` нет варианта «неизвестный узел», а править его эта веха не
/// вправе; [`Error::Unsupported`] — ближайший по смыслу: операция, которую
/// ядро выполнить не может.
pub(crate) const BAD_NODE: &str = "dom: узел не принадлежит документу";

/// Разобранная XML-часть.
///
/// Владеет исходными байтами и не отпускает их до конца жизни: спаны узлов
/// адресуют именно этот буфер.
#[derive(Debug)]
pub struct Document {
    /// Исходные байты части целиком, включая BOM.
    src: Vec<u8>,
    /// Байты, написанные нами: заэкранированные значения, имена новых узлов.
    ///
    /// Начинается с одного пробела, чтобы у пустых спанов и у пробела перед
    /// новым атрибутом были фиксированные адреса и не приходилось каждый раз
    /// проверять, не пуста ли арена.
    arena: Vec<u8>,
    nodes: Vec<arena::Node>,
    elements: Vec<node::ElementData>,
    /// Общий вектор атрибутов; элемент ссылается на диапазон в нём.
    attrs: Vec<node::Attr>,
    /// Интернированные URI namespace. Индекс — `ElementData::ns_uri`.
    uris: Vec<String>,
    root: NodeId,
    /// Часть начинается с UTF-8 BOM (16 частей корпуса, все в одном `.docx`).
    bom: bool,
    dirty: bool,
    /// Квоты. Правки обязаны проверяться теми же лимитами, что и разбор:
    /// иначе дерево, полученное из безопасного файла, можно было бы раздуть
    /// через API до размеров, которые разбор бы не пропустил.
    limits: Limits,
}

/// Спан-заглушка: пустой диапазон в нулевой позиции.
///
/// Безопасен для любого буфера, включая пустой: `[]`.get(0..0) == Some(&[]).
pub(crate) const NIL: Span = Span::empty(0);

/// Пробел в арене — им отделяются новые атрибуты от предыдущей лексемы.
pub(crate) const ARENA_SPACE: Span = Span::clamped(0, 1);

impl Document {
    /// Байты исходника целиком.
    #[must_use]
    pub fn src(&self) -> &[u8] {
        &self.src
    }

    /// Узел-корень дерева (вид [`NodeKind::Document`]).
    #[must_use]
    pub const fn document_node(&self) -> NodeId {
        self.root
    }

    /// Есть ли в документе хотя бы одна правка.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Часть начиналась с UTF-8 BOM.
    #[must_use]
    pub const fn has_bom(&self) -> bool {
        self.bom
    }

    /// Число узлов дерева, включая корневой.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Разбивка занятой памяти: (что, байт).
    #[must_use]
    pub fn memory_breakdown(&self) -> [(&'static str, usize); 5] {
        [
            ("исходник", self.src.capacity()),
            (
                "узлы",
                self.nodes
                    .capacity()
                    .saturating_mul(size_of::<arena::Node>()),
            ),
            (
                "элементы",
                self.elements
                    .capacity()
                    .saturating_mul(size_of::<node::ElementData>()),
            ),
            (
                "атрибуты",
                self.attrs
                    .capacity()
                    .saturating_mul(size_of::<node::Attr>()),
            ),
            ("арена", self.arena.capacity()),
        ]
    }

    /// Оценка занятой памяти в байтах.
    ///
    /// Считается по `capacity`, а не по `len`: интересен пик, а не полезная
    /// нагрузка. Внешнего аллокатора-счётчика в ядре нет и быть не может —
    /// это `#![forbid(unsafe_code)]` и `wasm32` без `std::alloc`-хуков.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        let nodes = self
            .nodes
            .capacity()
            .saturating_mul(size_of::<arena::Node>());
        let elems = self
            .elements
            .capacity()
            .saturating_mul(size_of::<node::ElementData>());
        let attrs = self
            .attrs
            .capacity()
            .saturating_mul(size_of::<node::Attr>());
        let uris = self
            .uris
            .iter()
            .fold(0usize, |a, u| a.saturating_add(u.capacity()));
        self.src
            .capacity()
            .saturating_add(self.arena.capacity())
            .saturating_add(nodes)
            .saturating_add(elems)
            .saturating_add(attrs)
            .saturating_add(uris)
    }

    // --- внутренние помощники --------------------------------------------

    pub(crate) fn node(&self, id: NodeId) -> Result<&arena::Node> {
        self.nodes
            .get(id.index())
            .ok_or(Error::Unsupported(BAD_NODE))
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> Result<&mut arena::Node> {
        self.nodes
            .get_mut(id.index())
            .ok_or(Error::Unsupported(BAD_NODE))
    }

    /// Данные элемента по узлу. Ошибка — узел не элемент или таблица порвана.
    pub(crate) fn elem(&self, id: NodeId) -> Result<&node::ElementData> {
        let n = self.node(id)?;
        if n.kind != NodeKind::Element {
            return Err(Error::Unsupported("dom: узел не является элементом"));
        }
        self.elements
            .get(n.aux as usize)
            .ok_or(Error::Unsupported(BAD_NODE))
    }

    pub(crate) fn elem_mut(&mut self, id: NodeId) -> Result<&mut node::ElementData> {
        let n = self.node(id)?;
        if n.kind != NodeKind::Element {
            return Err(Error::Unsupported("dom: узел не является элементом"));
        }
        let aux = n.aux as usize;
        self.elements
            .get_mut(aux)
            .ok_or(Error::Unsupported(BAD_NODE))
    }

    /// Атрибуты элемента в исходном порядке.
    #[must_use]
    pub(crate) fn attrs_of(&self, e: &node::ElementData) -> &[node::Attr] {
        let from = e.attrs_from as usize;
        let to = from.saturating_add(e.attrs_len as usize);
        self.attrs.get(from..to).unwrap_or(&[])
    }

    /// Буфер, в котором лежит спан.
    #[must_use]
    pub(crate) fn buf(&self, in_arena: bool) -> &[u8] {
        if in_arena { &self.arena } else { &self.src }
    }

    /// Байты спана. `None` — спан вне буфера (испорченное дерево).
    #[must_use]
    pub(crate) fn bytes(&self, span: Span, in_arena: bool) -> Option<&[u8]> {
        span.slice(self.buf(in_arena))
    }
}
