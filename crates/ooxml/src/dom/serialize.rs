//! Обратная запись дерева в байты.
//!
//! # Фаст-пас — это и есть весь замысел
//!
//! Узел без флагов `SELF_DIRTY`/`HAS_DIRTY_DESC` пишется одним
//! `extend_from_slice(span)`. Для нетронутого документа под этот случай
//! подпадает сам узел-документ, и вся сериализация вырождается в копирование
//! исходного буфера — побайтовое совпадение получается по построению.
//!
//! Реконструкция по полям (`<` + имя + атрибуты + …) существует только ради
//! изменённых узлов. Каждый её шаг соответствует спану, который лексер выдал
//! отдельно: `pre_ws` атрибута, пробелы вокруг `=`, `pre_close_ws`,
//! `close_trailing_ws`. Ни один из них не «канонизируется»: `<w:rFonts  w:ascii`
//! с двумя пробелами останется с двумя, `w:customStyle = "1"` — с пробелами
//! вокруг `=`, `<a></a>` не превратится в `<a/>`.

use crate::error::{Error, LimitError, Result, XmlError};
use crate::xml::lexer::{BOM, xml_err};

use super::arena::{NodeFlags, NodeId, NodeKind};
use super::node::AttrFlags;
use super::{Document, ElemFlags};
use crate::bytes::Span;

impl Document {
    /// Записывает дерево в байты.
    ///
    /// Для документа без правок результат побайтово равен входу [`Document::parse`].
    ///
    /// # Errors
    ///
    /// Ошибка возможна только при испорченном дереве (спан вне буфера) или
    /// при превышении квоты глубины на дереве, собранном через API.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.src.len().saturating_add(self.arena.len()));
        self.write_node(self.root, &mut out, 0)?;
        Ok(out)
    }

    fn write_node(&self, id: NodeId, out: &mut Vec<u8>, depth: u64) -> Result<()> {
        // Дерево, собранное через API, лексером не проверялось, и его глубина
        // ничем не ограничена — а рекурсия здесь настоящая. Запас в два уровня
        // над квотой: квота считает вложенность элементов, а здесь считаются
        // ещё и узел-документ сверху, и текстовый узел под самым глубоким
        // элементом — документ, прошедший разбор, обязан пройти и запись.
        if depth > self.limits.max_xml_depth.saturating_add(2) {
            return Err(Error::Limit(LimitError::XmlDepth {
                got: depth,
                max: self.limits.max_xml_depth,
            }));
        }
        let n = self.node(id)?;

        if !n.flags.dirty() {
            return self.copy(n.span, n.flags.has(NodeFlags::IN_ARENA), out);
        }

        match n.kind {
            NodeKind::Document => {
                if self.bom {
                    out.extend_from_slice(&BOM);
                }
                self.write_children(id, out, depth)
            }
            NodeKind::Element => self.write_element(id, out, depth),
            // Текст, комментарий, PI, CDATA и декларация даже изменёнными
            // остаются одним куском байт — просто теперь из арены.
            _ => self.copy(n.span, n.flags.has(NodeFlags::IN_ARENA), out),
        }
    }

    fn write_children(&self, id: NodeId, out: &mut Vec<u8>, depth: u64) -> Result<()> {
        let mut cur = self.node(id)?.first_child;
        while let Some(c) = cur {
            self.write_node(c, out, depth.saturating_add(1))?;
            cur = self.node(c)?.next;
        }
        Ok(())
    }

    fn write_element(&self, id: NodeId, out: &mut Vec<u8>, depth: u64) -> Result<()> {
        let n = self.node(id)?;
        let e = self.elem(id)?;
        let name_arena = e.flags.has(ElemFlags::QNAME_IN_ARENA);

        out.push(b'<');
        self.copy(e.qname, name_arena, out)?;

        for a in self.attrs_of(e) {
            let ws = a.flags.has(AttrFlags::WS_IN_ARENA);
            self.copy(a.pre_ws, ws, out)?;
            self.copy(a.name, a.flags.has(AttrFlags::NAME_IN_ARENA), out)?;
            self.copy(a.ws_before_eq, ws, out)?;
            out.push(b'=');
            self.copy(a.ws_after_eq, ws, out)?;
            out.push(a.quote);
            self.copy(a.value, a.flags.has(AttrFlags::VALUE_IN_ARENA), out)?;
            out.push(a.quote);
        }

        // `pre_close_ws` у созданного нами элемента пуст, а пустой спан в
        // нулевой позиции корректен для любого буфера — отдельного флага не
        // нужно.
        self.copy(e.pre_close_ws, false, out)?;

        if n.flags.has(NodeFlags::EMPTY_TAG) {
            out.extend_from_slice(b"/>");
            return Ok(());
        }
        out.push(b'>');
        self.write_children(id, out, depth)?;
        out.extend_from_slice(b"</");
        self.copy(e.close_qname, name_arena, out)?;
        self.copy(e.close_trailing_ws, false, out)?;
        out.push(b'>');
        Ok(())
    }

    fn copy(&self, span: Span, in_arena: bool, out: &mut Vec<u8>) -> Result<()> {
        let bytes = self
            .bytes(span, in_arena)
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, span.start() as usize))?;
        out.extend_from_slice(bytes);
        Ok(())
    }

    /// Дописывает байты в арену и возвращает их спан.
    pub(crate) fn push_arena(
        &mut self,
        f: impl FnOnce(&mut Vec<u8>) -> Result<()>,
    ) -> Result<Span> {
        let start = u32::try_from(self.arena.len()).map_err(|_| arena_overflow())?;
        if let Err(e) = f(&mut self.arena) {
            // Неудачное экранирование не должно оставлять мусор в арене:
            // спана на него никто не выдаст, но пик памяти вырос бы на каждой
            // отвергнутой правке.
            self.arena.truncate(start as usize);
            return Err(e);
        }
        let end = u32::try_from(self.arena.len()).map_err(|_| arena_overflow())?;
        Ok(Span::clamped(start, end))
    }
}

fn arena_overflow() -> Error {
    Error::Limit(LimitError::PartTooLarge {
        got: u64::from(u32::MAX),
        max: u64::from(u32::MAX),
    })
}
