//! Арена узлов и связи дерева.
//!
//! Узлы лежат в одном векторе, а ссылки между ними — индексы, а не указатели.
//! Причина утилитарная: лист Excel на 313 тыс. ячеек даёт около миллиона узлов,
//! и миллион отдельных аллокаций (плюс восьмибайтовые указатели) стоил бы
//! дороже самого разбора. Индекс же ещё и переживает `push` вектора, тогда как
//! ссылка — нет.

use core::fmt;
use core::num::NonZeroU32;

use crate::bytes::Span;

/// Идентификатор узла.
///
/// Внутри `NonZeroU32`, чтобы `Option<NodeId>` занимал те же четыре байта, что и
/// сам `NodeId`. Это не микрооптимизация: в [`Node`] пять ссылок, и без ниши
/// узел вырос бы на пять байт — ровно за границу бюджета в 40 байт, ради
/// которого вся эта конструкция и затевалась.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(NonZeroU32);

impl NodeId {
    /// Первый узел арены. Им всегда является узел-документ.
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    /// Идентификатор узла с индексом `i`. `None` — индекс не адресуем `u32`.
    #[must_use]
    pub fn from_index(i: usize) -> Option<Self> {
        u32::try_from(i)
            .ok()
            .and_then(|i| i.checked_add(1))
            .and_then(NonZeroU32::new)
            .map(Self)
    }

    /// Индекс в векторе узлов.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0.get().saturating_sub(1) as usize
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Печатается индекс, а не внутреннее «индекс + 1»: в сообщениях об
        // ошибках нужен номер узла, а не деталь представления.
        write!(f, "#{}", self.index())
    }
}

/// Что за узел. Тег в один байт; полезная нагрузка элемента лежит отдельно.
///
/// Разложить `ElementData` прямо в вариант перечисления не получается: `Box`
/// плюс дискриминант — это уже 16 байт, а вместе с пятью ссылками и спаном узел
/// перевалил бы за 48. Поэтому у узла есть поле `aux` — индекс в таблицу
/// [`crate::dom::ElementData`], и `Node` остаётся плоской структурой.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    /// Корень дерева. Его дети — пролог, корневой элемент и всё, что после.
    Document,
    /// `<?xml ... ?>`. Копируется дословно: форм у неё четыре, и все живые.
    Decl,
    /// Элемент. Данные — в `aux`.
    Element,
    /// Символьные данные. Сырые, всё ещё экранированные.
    Text,
    /// `<![CDATA[ ... ]]>`.
    CData,
    /// `<!-- ... -->`.
    Comment,
    /// Инструкция обработки, кроме декларации.
    Pi,
}

impl NodeKind {
    /// Может ли узел такого вида иметь детей.
    #[must_use]
    pub const fn may_have_children(self) -> bool {
        matches!(self, Self::Document | Self::Element)
    }
}

/// Флаги узла.
///
/// Битовое поле, а не набор `bool`: полей четыре, и каждый лишний байт узла —
/// это лишний мегабайт на большом листе.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeFlags(u8);

impl NodeFlags {
    /// Узел изменён — его `span` больше не описывает то, что нужно записать.
    pub const SELF_DIRTY: u8 = 1 << 0;
    /// Среди потомков есть изменённый или изменился состав детей.
    ///
    /// Отдельный бит от [`Self::SELF_DIRTY`], потому что смысл разный: сам
    /// элемент цел, переписать нужно только его внутренность.
    pub const HAS_DIRTY_DESC: u8 = 1 << 1;
    /// Элемент записан одним тегом: `<a/>`, а не `<a></a>`.
    ///
    /// В корпусе 185 096 самозакрывающихся тегов и 104 911 пар с пустой
    /// внутренностью. Нормализация к одной форме разошлась бы с исходником в
    /// 34 файлах из 43.
    pub const EMPTY_TAG: u8 = 1 << 2;
    /// `span` указывает в арену нового содержимого, а не в исходник.
    pub const IN_ARENA: u8 = 1 << 3;

    #[must_use]
    pub const fn new(bits: u8) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    pub const fn set(&mut self, bit: u8) {
        self.0 |= bit;
    }

    pub const fn clear(&mut self, bit: u8) {
        self.0 &= !bit;
    }

    /// Запрещён ли для узла фаст-пас «скопировать спан целиком».
    #[must_use]
    pub const fn dirty(self) -> bool {
        self.0 & (Self::SELF_DIRTY | Self::HAS_DIRTY_DESC) != 0
    }
}

impl fmt::Debug for NodeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (bit, name) in [
            (Self::SELF_DIRTY, "SELF_DIRTY"),
            (Self::HAS_DIRTY_DESC, "HAS_DIRTY_DESC"),
            (Self::EMPTY_TAG, "EMPTY_TAG"),
            (Self::IN_ARENA, "IN_ARENA"),
        ] {
            if self.has(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first { f.write_str("-") } else { Ok(()) }
    }
}

/// Узел дерева.
///
/// Ровно 36 байт (проверяется тестом `unit_dom.rs`). Бюджет задан снаружи:
/// в корпусе 313 тыс. ячеек, лист на 200 КБ даёт около 30 тыс. узлов, и на
/// самых больших частях счёт идёт на миллионы.
///
/// `span` — срез **исходника** (или арены, если стоит [`NodeFlags::IN_ARENA`]),
/// покрывающий узел целиком: для элемента — от `<` открывающего тега до `>`
/// закрывающего включительно.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub(crate) parent: Option<NodeId>,
    pub(crate) first_child: Option<NodeId>,
    pub(crate) last_child: Option<NodeId>,
    pub(crate) prev: Option<NodeId>,
    pub(crate) next: Option<NodeId>,
    pub(crate) span: Span,
    /// Индекс в таблице `ElementData`; [`Node::NO_AUX`] — данных нет.
    pub(crate) aux: u32,
    pub(crate) kind: NodeKind,
    pub(crate) flags: NodeFlags,
}

impl Node {
    /// «Полезной нагрузки нет».
    pub const NO_AUX: u32 = u32::MAX;

    pub(crate) const fn new(kind: NodeKind, span: Span, flags: NodeFlags) -> Self {
        Self {
            parent: None,
            first_child: None,
            last_child: None,
            prev: None,
            next: None,
            span,
            aux: Self::NO_AUX,
            kind,
            flags,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> NodeKind {
        self.kind
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    #[must_use]
    pub const fn flags(&self) -> NodeFlags {
        self.flags
    }

    #[must_use]
    pub const fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    #[must_use]
    pub const fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    #[must_use]
    pub const fn next_sibling(&self) -> Option<NodeId> {
        self.next
    }
}

// --- операции над связями -------------------------------------------------
//
// Свободные функции над срезом узлов, а не методы `Document`: ими пользуются и
// построитель дерева (у него ещё нет `Document`), и редактор. Все обращения
// идут через `get_mut` — выход за границы означает испорченный идентификатор,
// и он должен привести к «ничего не произошло», а не к панике.

/// Делает `child` последним ребёнком `parent`. Связи `child` считаются пустыми.
pub(crate) fn link_last(nodes: &mut [Node], parent: NodeId, child: NodeId) {
    let prev = nodes.get(parent.index()).and_then(|p| p.last_child);
    if let Some(c) = nodes.get_mut(child.index()) {
        c.parent = Some(parent);
        c.prev = prev;
        c.next = None;
    }
    match prev {
        Some(p) => {
            if let Some(n) = nodes.get_mut(p.index()) {
                n.next = Some(child);
            }
        }
        None => {
            if let Some(p) = nodes.get_mut(parent.index()) {
                p.first_child = Some(child);
            }
        }
    }
    if let Some(p) = nodes.get_mut(parent.index()) {
        p.last_child = Some(child);
    }
}

/// Вставляет `child` непосредственно перед `anchor`.
pub(crate) fn link_before(nodes: &mut [Node], anchor: NodeId, child: NodeId) {
    let Some((parent, prev)) = nodes.get(anchor.index()).map(|a| (a.parent, a.prev)) else {
        return;
    };
    let Some(parent) = parent else { return };
    if let Some(c) = nodes.get_mut(child.index()) {
        c.parent = Some(parent);
        c.prev = prev;
        c.next = Some(anchor);
    }
    if let Some(a) = nodes.get_mut(anchor.index()) {
        a.prev = Some(child);
    }
    match prev {
        Some(p) => {
            if let Some(n) = nodes.get_mut(p.index()) {
                n.next = Some(child);
            }
        }
        None => {
            if let Some(p) = nodes.get_mut(parent.index()) {
                p.first_child = Some(child);
            }
        }
    }
}

/// Вынимает `child` из списка детей его родителя.
pub(crate) fn unlink(nodes: &mut [Node], child: NodeId) {
    let Some((parent, prev, next)) = nodes.get(child.index()).map(|c| (c.parent, c.prev, c.next))
    else {
        return;
    };
    match prev {
        Some(p) => {
            if let Some(n) = nodes.get_mut(p.index()) {
                n.next = next;
            }
        }
        None => {
            if let Some(p) = parent.and_then(|p| nodes.get_mut(p.index())) {
                p.first_child = next;
            }
        }
    }
    match next {
        Some(n) => {
            if let Some(n) = nodes.get_mut(n.index()) {
                n.prev = prev;
            }
        }
        None => {
            if let Some(p) = parent.and_then(|p| nodes.get_mut(p.index())) {
                p.last_child = prev;
            }
        }
    }
    if let Some(c) = nodes.get_mut(child.index()) {
        c.parent = None;
        c.prev = None;
        c.next = None;
    }
}
