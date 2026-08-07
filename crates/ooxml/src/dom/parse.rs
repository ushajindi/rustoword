//! Построение дерева из байт части.
//!
//! Разбор идёт поверх [`crate::xml::Reader`]: он проверяет парность тегов,
//! единственность корня, глубину, квоты и разрешает namespace. Дерево не
//! добавляет к этому ни одной синтаксической проверки — оно только раскладывает
//! события по узлам, сохраняя спаны как есть.
//!
//! # Ловушка, из-за которой атрибуты копируются немедленно
//!
//! `Reader::attrs()` действителен только до следующего `Start`: буфер атрибутов
//! в лексере переиспользуется между тегами (иначе каждый тег стоил бы
//! аллокации вектора, а тегов в листе Excel сотни тысяч). Поэтому атрибуты
//! копируются в общий вектор документа прямо в обработчике `Start`.

use crate::bytes::Span;
use crate::error::{Error, LimitError, Result, XmlError};
use crate::limits::Limits;
use crate::xml::lexer::{BOM, Event, xml_err};
use crate::xml::ns::NsUri;
use crate::xml::reader::Reader;

use super::arena::{Node, NodeFlags, NodeId, NodeKind, link_last};
use super::node::{Attr, AttrFlags, ElemFlags, ElementData, local_of};
use super::{Document, NIL};

/// Место, где нарушился инвариант покрытия.
///
/// Отдельный тип, а не строка: проверка вызывается из отладочного ассерта на
/// каждом разборе, и аллокация ради сообщения, которого в 99,99 % случаев не
/// будет, здесь неуместна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageBreak {
    /// Узел, внутренность которого покрыта неправильно.
    pub parent: NodeId,
    /// Ребёнок, на котором обнаружен разрыв; `None` — разрыв в самом конце.
    pub child: Option<NodeId>,
    /// Где ожидалась граница.
    pub expected: u32,
    /// Где она оказалась.
    pub found: u32,
}

/// Результат построения, уже без заимствования исходника.
struct Built {
    nodes: Vec<Node>,
    elements: Vec<ElementData>,
    attrs: Vec<Attr>,
    uris: Vec<String>,
    bom: bool,
}

impl Document {
    /// Разбирает XML-часть.
    ///
    /// Байты передаются по значению и остаются жить в документе: спаны узлов
    /// адресуют именно их, копии не делается.
    pub fn parse(bytes: Vec<u8>, limits: &Limits) -> Result<Self> {
        limits.check_part_size(bytes.len() as u64)?;
        // Спан адресует `u32`; часть длиннее — не наш случай, и лучше сказать
        // об этом квотой, чем молча усечь.
        if bytes.len() > u32::MAX as usize {
            return Err(Error::Limit(LimitError::PartTooLarge {
                got: bytes.len() as u64,
                max: u64::from(u32::MAX),
            }));
        }

        let built = build(&bytes, limits)?;
        let doc = Self {
            src: bytes,
            arena: vec![b' '],
            nodes: built.nodes,
            elements: built.elements,
            attrs: built.attrs,
            uris: built.uris,
            root: ROOT,
            bom: built.bom,
            dirty: false,
            limits: limits.clone(),
        };

        // Инвариант покрытия — то, ради чего всё это построено: если он
        // выполнен, побайтовый round-trip следует по построению.
        debug_assert!(
            doc.check_coverage().is_ok(),
            "инвариант покрытия нарушен при разборе: {:?}",
            doc.check_coverage()
        );
        Ok(doc)
    }

    /// Проверяет инвариант покрытия.
    ///
    /// Для изменённого документа проверка пропускается: спаны правленых узлов
    /// намеренно указывают в арену и исходник больше не плитуют.
    ///
    /// # Errors
    ///
    /// Возвращает первое найденное место разрыва.
    pub fn check_coverage(&self) -> core::result::Result<(), CoverageBreak> {
        if self.dirty {
            return Ok(());
        }
        for i in 0..self.nodes.len() {
            let Some(id) = NodeId::from_index(i) else {
                continue;
            };
            let Some(n) = self.nodes.get(i) else { continue };
            if n.first_child.is_none() {
                continue;
            }
            let (lo, hi) = match n.kind {
                // Дети документа покрывают всё после BOM: сам BOM — не узел,
                // а флаг, потому что узлом ему быть нечем (у него нет вида).
                NodeKind::Document => (
                    if self.bom { BOM.len() as u32 } else { 0 },
                    self.src.len() as u32,
                ),
                NodeKind::Element => {
                    let Ok(e) = self.elem(id) else { continue };
                    let empty = n.flags.has(NodeFlags::EMPTY_TAG);
                    (e.open_end(empty), e.close_start())
                }
                _ => {
                    // Текст, комментарий, PI и CDATA детей иметь не могут.
                    return Err(CoverageBreak {
                        parent: id,
                        child: n.first_child,
                        expected: n.span.start(),
                        found: n.span.end(),
                    });
                }
            };

            let mut at = lo;
            let mut cur = n.first_child;
            let mut last = None;
            while let Some(c) = cur {
                let Some(cn) = self.nodes.get(c.index()) else {
                    break;
                };
                if cn.span.start() != at {
                    return Err(CoverageBreak {
                        parent: id,
                        child: Some(c),
                        expected: at,
                        found: cn.span.start(),
                    });
                }
                at = cn.span.end();
                last = Some(c);
                cur = cn.next;
            }
            if at != hi {
                return Err(CoverageBreak {
                    parent: id,
                    child: last,
                    expected: hi,
                    found: at,
                });
            }
        }
        Ok(())
    }
}

/// Идентификатор узла-документа. Он создаётся первым, значит, его индекс — 0.
const ROOT: NodeId = NodeId::FIRST;

fn build(src: &[u8], limits: &Limits) -> Result<Built> {
    let mut b = Builder {
        src,
        limits,
        nodes: Vec::new(),
        elements: Vec::new(),
        attrs: Vec::new(),
        uris: Vec::new(),
        ns_map: Vec::new(),
        stack: Vec::new(),
        bom: false,
    };
    b.run()?;
    // Векторы росли удвоением, и в среднем треть их ёмкости — воздух. Документ
    // живёт всю сессию правки, а этот запас — нет: на самой большой части
    // корпуса усадка снимает около 9 МиБ из 24. Копия при усадке разовая и на
    // фоне разбора незаметна.
    b.nodes.shrink_to_fit();
    b.elements.shrink_to_fit();
    b.attrs.shrink_to_fit();
    b.uris.shrink_to_fit();
    Ok(Built {
        nodes: b.nodes,
        elements: b.elements,
        attrs: b.attrs,
        uris: b.uris,
        bom: b.bom,
    })
}

struct Builder<'a> {
    src: &'a [u8],
    limits: &'a Limits,
    nodes: Vec<Node>,
    elements: Vec<ElementData>,
    attrs: Vec<Attr>,
    uris: Vec<String>,
    /// Соответствие «идентификатор namespace у парсера → индекс в `uris`».
    ///
    /// Ассоциативный список, а не хеш-таблица: разных URI в реальной части
    /// единицы, и линейный поиск по паре `u32` обгонит хеш на константе.
    ns_map: Vec<(NsUri, u32)>,
    stack: Vec<NodeId>,
    bom: bool,
}

impl Builder<'_> {
    fn run(&mut self) -> Result<()> {
        let doc = self.alloc(
            NodeKind::Document,
            Span::clamped(0, self.src.len() as u32),
            NodeFlags::default(),
        )?;
        self.stack.push(doc);

        let mut r = Reader::with_limits(self.src, self.limits.clone());
        loop {
            let ev = r.next_event()?;
            match ev {
                Event::StartDoc { bom } => self.bom = bom,
                Event::Eof => break,
                Event::Decl { span } => self.leaf(NodeKind::Decl, span)?,
                Event::Pi { span } => self.leaf(NodeKind::Pi, span)?,
                Event::Comment { span } => self.leaf(NodeKind::Comment, span)?,
                Event::CData { span } => self.leaf(NodeKind::CData, span)?,
                Event::Text { span, .. } => self.leaf(NodeKind::Text, span)?,
                Event::Start {
                    span,
                    name,
                    empty,
                    pre_close_ws,
                    ..
                } => {
                    let ns = match r.element_ns() {
                        Some(id) => self.intern(id, r.uri(id).unwrap_or("")),
                        None => ElementData::NO_NS,
                    };
                    self.start(span, name, empty, pre_close_ws, ns, r.attrs())?;
                }
                Event::End {
                    span,
                    name,
                    trailing_ws,
                } => self.end(span, name, trailing_ws)?,
            }
        }
        Ok(())
    }

    fn alloc(&mut self, kind: NodeKind, span: Span, flags: NodeFlags) -> Result<NodeId> {
        let n = self.nodes.len() as u64;
        self.limits.check_nodes(n.saturating_add(1))?;
        let id =
            NodeId::from_index(self.nodes.len()).ok_or(Error::Limit(LimitError::TooManyNodes {
                got: n,
                max: self.limits.max_nodes_per_part,
            }))?;
        self.nodes.push(Node::new(kind, span, flags));
        Ok(id)
    }

    /// Узел без детей — текст, комментарий, PI, CDATA, декларация.
    fn leaf(&mut self, kind: NodeKind, span: Span) -> Result<()> {
        let id = self.alloc(kind, span, NodeFlags::default())?;
        self.attach(id)
    }

    fn attach(&mut self, id: NodeId) -> Result<()> {
        let parent = *self
            .stack
            .last()
            .ok_or_else(|| xml_err(XmlError::UnbalancedTag, 0))?;
        link_last(&mut self.nodes, parent, id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        span: Span,
        name: Span,
        empty: bool,
        pre_close_ws: Span,
        ns_uri: u32,
        raw_attrs: &[crate::xml::lexer::RawAttr],
    ) -> Result<()> {
        let mut flags = NodeFlags::default();
        if empty {
            flags.set(NodeFlags::EMPTY_TAG);
        }
        let id = self.alloc(NodeKind::Element, span, flags)?;

        let attrs_from = u32::try_from(self.attrs.len()).unwrap_or(u32::MAX);
        for a in raw_attrs {
            self.attrs.push(Attr {
                pre_ws: a.pre_ws,
                name: a.name,
                ws_before_eq: a.ws_before_eq,
                ws_after_eq: a.ws_after_eq,
                value: a.value,
                quote: a.quote,
                flags: AttrFlags::default(),
            });
        }

        let local_bytes = local_of(name.slice(self.src).unwrap_or(&[]));
        let local_len = u32::try_from(local_bytes.len()).unwrap_or(0);
        let local = Span::clamped(name.end().saturating_sub(local_len), name.end());

        let aux = u32::try_from(self.elements.len()).unwrap_or(u32::MAX);
        self.elements.push(ElementData {
            qname: name,
            local,
            pre_close_ws,
            close_qname: NIL,
            close_trailing_ws: NIL,
            attrs_from,
            attrs_len: u32::try_from(raw_attrs.len()).unwrap_or(0),
            ns_uri,
            flags: ElemFlags::default(),
        });
        if let Some(n) = self.nodes.get_mut(id.index()) {
            n.aux = aux;
        }

        self.attach(id)?;
        if !empty {
            self.stack.push(id);
        }
        Ok(())
    }

    fn end(&mut self, span: Span, name: Span, trailing_ws: Span) -> Result<()> {
        // Первый кадр стека — узел-документ, снимать его нельзя.
        if self.stack.len() <= 1 {
            return Err(xml_err(XmlError::UnbalancedTag, span.start() as usize));
        }
        let id = self
            .stack
            .pop()
            .ok_or_else(|| xml_err(XmlError::UnbalancedTag, span.start() as usize))?;
        let aux = {
            let n = self
                .nodes
                .get_mut(id.index())
                .ok_or_else(|| xml_err(XmlError::UnbalancedTag, span.start() as usize))?;
            // Спан элемента растёт до конца закрывающего тега: узел покрывает
            // элемент целиком, вместе с детьми.
            n.span = Span::clamped(n.span.start(), span.end());
            n.aux as usize
        };
        if let Some(e) = self.elements.get_mut(aux) {
            e.close_qname = name;
            e.close_trailing_ws = trailing_ws;
        }
        Ok(())
    }

    fn intern(&mut self, id: NsUri, uri: &str) -> u32 {
        if let Some(&(_, own)) = self.ns_map.iter().find(|&&(k, _)| k == id) {
            return own;
        }
        let own = match self.uris.iter().position(|u| u == uri) {
            Some(i) => u32::try_from(i).unwrap_or(ElementData::NO_NS),
            None => {
                let i = u32::try_from(self.uris.len()).unwrap_or(ElementData::NO_NS);
                self.uris.push(uri.to_owned());
                i
            }
        };
        self.ns_map.push((id, own));
        own
    }
}
