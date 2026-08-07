//! Pull-парсер поверх лексера.
//!
//! Лексер знает только байты; всё, что требует памяти о прошлом — парность
//! тегов, единственность корня, глубина, квоты, области видимости namespace —
//! живёт здесь.
//!
//! Парсер ничего не переписывает: он выдаёт те же события [`Event`], что и
//! лексер, и лишь добавляет к ним разрешённые имена. Главный инвариант
//! плитования этим не затрагивается — спаны событий проходят насквозь.

use crate::bytes::Span;
use crate::error::{Error, Result, XmlError};
use crate::limits::Limits;
use crate::xml::entity;
use crate::xml::lexer::{Event, Lexer, RawAttr, is_ws, xml_err};
use crate::xml::name::QName;
use crate::xml::ns::{Namespaces, NsUri};

/// Открытый элемент. Хранится только имя — этого достаточно для проверки
/// парности, а всё остальное про элемент уже отдано наружу событием.
#[derive(Debug)]
struct Open {
    name: Span,
}

/// Порог, за которым поиск дубликатов атрибутов переходит на сортировку.
///
/// До него попарное сравнение дешевле сортировки; после — квадратичный рост
/// становится рычагом для атаки, ведь квота допускает 4096 атрибутов на
/// элемент, а это уже восемь миллионов сравнений на один тег.
const DUP_SORT_THRESHOLD: usize = 16;

/// Pull-парсер XML.
#[derive(Debug)]
pub struct Reader<'a> {
    lex: Lexer<'a>,
    src: &'a [u8],
    limits: Limits,
    ns: Namespaces,
    stack: Vec<Open>,
    nodes: u64,
    root_seen: bool,
    /// Декларация допустима только первым событием после `StartDoc`.
    decl_allowed: bool,
    finished: bool,

    // Состояние последнего события `Start` / `End`.
    cur_from: usize,
    cur_len: usize,
    elem: QName,
    elem_ns: Option<NsUri>,
    attr_names: Vec<QName>,
    attr_ns: Vec<Option<NsUri>>,
    /// Переиспользуемые буферы: элементов в листе бывают сотни тысяч, и
    /// аллокация на каждый тег была бы заметнее самого разбора.
    order: Vec<usize>,
    scratch: String,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(src: &'a [u8]) -> Self {
        Self::with_limits(src, Limits::default())
    }

    #[must_use]
    pub fn with_limits(src: &'a [u8], limits: Limits) -> Self {
        Self {
            lex: Lexer::new(src),
            src,
            limits,
            ns: Namespaces::new(),
            stack: Vec::new(),
            nodes: 0,
            root_seen: false,
            decl_allowed: true,
            finished: false,
            cur_from: 0,
            cur_len: 0,
            elem: QName::EMPTY,
            elem_ns: None,
            attr_names: Vec::new(),
            attr_ns: Vec::new(),
            order: Vec::new(),
            scratch: String::new(),
        }
    }

    #[must_use]
    pub const fn src(&self) -> &'a [u8] {
        self.src
    }

    /// Число открытых элементов.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Атрибуты последнего события [`Event::Start`], в исходном порядке.
    ///
    /// Объявления `xmlns` из списка не вычёркиваются: для сериализации они
    /// такие же атрибуты, как остальные, и их порядок значим.
    #[must_use]
    pub fn attrs(&self) -> &[RawAttr] {
        self.lex.attr_range(self.cur_from, self.cur_len)
    }

    /// Разобранное имя элемента последнего [`Event::Start`] или [`Event::End`].
    #[must_use]
    pub const fn element_name(&self) -> QName {
        self.elem
    }

    /// Namespace элемента последнего события.
    #[must_use]
    pub const fn element_ns(&self) -> Option<NsUri> {
        self.elem_ns
    }

    /// Имя атрибута по индексу в [`Self::attrs`].
    #[must_use]
    pub fn attr_name(&self, i: usize) -> Option<QName> {
        self.attr_names.get(i).copied()
    }

    /// Namespace атрибута по индексу в [`Self::attrs`].
    ///
    /// Внешний `None` — атрибута с таким индексом нет; внутренний — атрибут вне
    /// namespace (атрибут без префикса namespace по умолчанию не наследует).
    #[must_use]
    pub fn attr_ns(&self, i: usize) -> Option<Option<NsUri>> {
        self.attr_ns.get(i).copied()
    }

    #[must_use]
    pub fn uri(&self, id: NsUri) -> Option<&str> {
        self.ns.uri(id)
    }

    /// Декодированное значение атрибута по индексу.
    ///
    /// Декодирование происходит здесь и только здесь: сам атрибут остаётся в
    /// файле ровно таким, каким был.
    pub fn attr_value(&self, i: usize) -> Result<String> {
        let a = self
            .attrs()
            .get(i)
            .ok_or_else(|| xml_err(XmlError::BadName, 0))?;
        let raw = a
            .value
            .slice(self.src)
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, a.value.start() as usize))?;
        entity::decode_attr_to_string(raw)
    }

    /// Декодированный текст по спану [`Event::Text`].
    pub fn text(&self, span: Span) -> Result<String> {
        let raw = span
            .slice(self.src)
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, span.start() as usize))?;
        entity::decode_text_to_string(raw)
    }

    /// Следующее событие.
    ///
    /// После успешного [`Event::Eof`] повторные вызовы возвращают `Eof` — так
    /// цикл разбора обходится без флага «уже кончилось».
    pub fn next_event(&mut self) -> Result<Event> {
        if self.finished {
            return Ok(Event::Eof);
        }
        let ev = self.lex.next_event()?;
        self.nodes = self.nodes.saturating_add(1);
        self.limits.check_nodes(self.nodes)?;

        match ev {
            Event::StartDoc { .. } => return Ok(ev),
            Event::Decl { span } => {
                if !self.decl_allowed {
                    // Декларация вне начала документа — не форматирование, а
                    // признак склейки двух файлов.
                    return Err(xml_err(
                        XmlError::UnexpectedByte(b'?'),
                        span.start() as usize,
                    ));
                }
            }
            Event::Pi { .. } | Event::Comment { .. } => {}
            Event::Start {
                name,
                attrs_from,
                attrs_len,
                empty,
                ..
            } => self.on_start(name, attrs_from, attrs_len, empty)?,
            Event::End { span, name, .. } => self.on_end(span, name)?,
            Event::Text { span, all_ws, .. } => {
                // Вне корня допустимы только пробелы: перевод строки после
                // декларации — валидный XML, а буква — уже нет.
                if self.stack.is_empty() && !all_ws {
                    return Err(self.stray_text(span));
                }
            }
            Event::CData { span } => {
                if self.stack.is_empty() {
                    return Err(xml_err(
                        XmlError::UnexpectedByte(b'<'),
                        span.start() as usize,
                    ));
                }
            }
            Event::Eof => {
                if !self.stack.is_empty() {
                    return Err(xml_err(XmlError::UnbalancedTag, self.src.len()));
                }
                if !self.root_seen {
                    return Err(xml_err(XmlError::NoRoot, self.src.len()));
                }
                self.finished = true;
            }
        }
        self.decl_allowed = false;
        Ok(ev)
    }

    fn on_start(&mut self, name: Span, from: usize, len: usize, empty: bool) -> Result<()> {
        let at_root = self.stack.is_empty();
        if at_root && self.root_seen {
            return Err(xml_err(XmlError::MultipleRoots, name.start() as usize));
        }

        self.limits.check_name_len(u64::from(name.len()))?;
        let depth = u64::try_from(self.stack.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        // Проверка до всякой работы над элементом: бомба из миллиона `<a>`
        // обязана отвергаться на 257-м теге, а не по таймауту.
        self.limits.check_depth(depth)?;
        self.limits
            .check_attrs(u64::try_from(len).unwrap_or(u64::MAX))?;

        self.cur_from = from;
        self.cur_len = len;
        self.elem = QName::parse(name, self.src)?;

        self.attr_names.clear();
        for a in self.lex.attr_range(from, len) {
            self.limits.check_name_len(u64::from(a.name.len()))?;
            self.attr_names.push(QName::parse(a.name, self.src)?);
        }
        self.check_duplicate_attrs(from, len)?;

        // Объявления действуют на весь элемент, включая его собственное имя и
        // атрибуты, стоящие в тексте раньше объявления, — поэтому сначала
        // полный проход по `xmlns`, и только потом разрешение префиксов.
        self.ns.push_frame();
        let resolved = self
            .declare_all(from, len)
            .and_then(|()| self.resolve_all(len, name));
        if let Err(e) = resolved {
            self.ns.pop_frame();
            return Err(e);
        }

        if empty {
            self.ns.pop_frame();
        } else {
            self.stack.push(Open { name });
        }
        if at_root {
            self.root_seen = true;
        }
        Ok(())
    }

    fn declare_all(&mut self, from: usize, len: usize) -> Result<()> {
        for (i, a) in self.lex.attr_range(from, len).iter().enumerate() {
            let Some(q) = self.attr_names.get(i).copied() else {
                continue;
            };
            let prefix: &[u8] = if q.is_default_ns_decl(self.src) {
                &[]
            } else if q.is_prefixed_ns_decl(self.src) {
                q.local_bytes(self.src)
            } else {
                continue;
            };
            let raw = a
                .value
                .slice(self.src)
                .ok_or_else(|| xml_err(XmlError::UnexpectedEof, a.value.start() as usize))?;
            // URI сравнивается как строка, поэтому его приходится декодировать:
            // `urn:x&#45;y` и `urn:x-y` — один namespace, хоть и разные байты.
            self.scratch.clear();
            entity::decode_attr(raw, &mut self.scratch)?;
            self.ns.declare(prefix, &self.scratch, a.name.start())?;
        }
        Ok(())
    }

    fn resolve_all(&mut self, len: usize, name: Span) -> Result<()> {
        self.attr_ns.clear();
        for i in 0..len {
            let Some(q) = self.attr_names.get(i).copied() else {
                self.attr_ns.push(None);
                continue;
            };
            let ns = match q.prefix() {
                // Атрибут без префикса namespace по умолчанию НЕ наследует —
                // так требует Namespaces in XML, и от этого зависит, считать ли
                // `val` и `w:val` одним атрибутом.
                None => None,
                // `xmlns:foo` — не атрибут в namespace `xmlns`, а объявление.
                Some(_) if q.is_prefixed_ns_decl(self.src) => None,
                Some(p) => self.ns.resolve(q.prefix_bytes(self.src), p.start())?,
            };
            self.attr_ns.push(ns);
        }
        self.elem_ns = self
            .ns
            .resolve(self.elem.prefix_bytes(self.src), name.start())?;
        Ok(())
    }

    /// Дубликат атрибута ищется по сырому имени.
    ///
    /// Сравниваются именно байты имени, а не пара (URI, локальная часть). Два
    /// разных префикса, ведущих на один URI, спецификация тоже считает
    /// дубликатом, но в OOXML такого не бывает, а разрешение namespace к этому
    /// моменту ещё не выполнено — объявление может стоять правее атрибута.
    fn check_duplicate_attrs(&mut self, from: usize, len: usize) -> Result<()> {
        let attrs = self.lex.attr_range(from, len);
        if len <= DUP_SORT_THRESHOLD {
            for (i, a) in attrs.iter().enumerate() {
                for b in attrs.get(i.saturating_add(1)..).unwrap_or(&[]) {
                    if a.name.slice(self.src) == b.name.slice(self.src) {
                        return Err(xml_err(
                            XmlError::DuplicateAttribute,
                            b.name.start() as usize,
                        ));
                    }
                }
            }
            return Ok(());
        }

        let src = self.src;
        self.order.clear();
        self.order.extend(0..len);
        self.order
            .sort_unstable_by(|&x, &y| match (attrs.get(x), attrs.get(y)) {
                (Some(a), Some(b)) => a.name.slice(src).cmp(&b.name.slice(src)),
                _ => core::cmp::Ordering::Equal,
            });
        for w in self.order.windows(2) {
            let (Some(&x), Some(&y)) = (w.first(), w.get(1)) else {
                continue;
            };
            let (Some(a), Some(b)) = (attrs.get(x), attrs.get(y)) else {
                continue;
            };
            if a.name.slice(src) == b.name.slice(src) {
                return Err(xml_err(
                    XmlError::DuplicateAttribute,
                    b.name.start() as usize,
                ));
            }
        }
        Ok(())
    }

    fn on_end(&mut self, span: Span, name: Span) -> Result<()> {
        let Some(open) = self.stack.pop() else {
            return Err(xml_err(XmlError::UnbalancedTag, span.start() as usize));
        };
        let want = open.name.slice(self.src);
        let got = name.slice(self.src);
        if want.is_none() || want != got {
            return Err(xml_err(XmlError::MismatchedTag, name.start() as usize));
        }
        self.elem = QName::parse(name, self.src)?;
        // Разрешение до снятия фрейма: объявления, сделанные на самом элементе,
        // действуют и на его закрывающий тег.
        self.elem_ns = self
            .ns
            .resolve(self.elem.prefix_bytes(self.src), name.start())?;
        self.ns.pop_frame();
        self.cur_len = 0;
        Ok(())
    }

    /// Непробельный текст вне корневого элемента.
    fn stray_text(&self, span: Span) -> Error {
        let bytes = span.slice(self.src).unwrap_or(&[]);
        let (off, b) = bytes
            .iter()
            .position(|&b| !is_ws(b))
            .and_then(|i| bytes.get(i).map(|&b| (i, b)))
            .unwrap_or((0, b'?'));
        xml_err(
            XmlError::UnexpectedByte(b),
            (span.start() as usize).saturating_add(off),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;

    fn parse(src: &[u8]) -> Result<usize> {
        let mut r = Reader::new(src);
        let mut n = 0usize;
        loop {
            let ev = r.next_event()?;
            n = n.saturating_add(1);
            if matches!(ev, Event::Eof) {
                return Ok(n);
            }
        }
    }

    #[test]
    fn well_formed_document_parses() {
        assert!(parse(b"<?xml version=\"1.0\"?>\r\n<a><b/>t</a>").is_ok());
    }

    #[test]
    fn structural_errors_detected() {
        let cases: &[(&[u8], XmlError)] = &[
            (b"<a>", XmlError::UnbalancedTag),
            (b"<a></b>", XmlError::MismatchedTag),
            (b"</a>", XmlError::UnbalancedTag),
            (b"<a/><b/>", XmlError::MultipleRoots),
            (b"", XmlError::NoRoot),
            (b"   ", XmlError::NoRoot),
            (b"<a x='1' x='2'/>", XmlError::DuplicateAttribute),
            (b"<p:a/>", XmlError::UndeclaredPrefix),
            (b"<a>x</a>y", XmlError::UnexpectedByte(b'y')),
            (b"<a/><![CDATA[x]]>", XmlError::UnexpectedByte(b'<')),
        ];
        for (src, want) in cases {
            match parse(src) {
                Err(Error::Xml { kind, .. }) if kind == *want => {}
                other => panic!(
                    "{:?}: ожидалось {want:?}, получено {other:?}",
                    core::str::from_utf8(src)
                ),
            }
        }
    }

    #[test]
    fn declaration_only_at_the_very_beginning() {
        assert!(matches!(
            parse(b"<a><?xml version=\"1.0\"?></a>"),
            Err(Error::Xml {
                kind: XmlError::UnexpectedByte(b'?'),
                ..
            })
        ));
    }

    #[test]
    fn namespace_declarations_stay_in_the_attribute_list() {
        let src = br#"<w:document xmlns:w="urn:w" xmlns:mc="urn:mc" mc:Ignorable="w"/>"#;
        let mut r = Reader::new(src);
        let _ = r.next_event().unwrap();
        let ev = r.next_event().unwrap();
        assert!(matches!(ev, Event::Start { .. }));
        assert_eq!(r.attrs().len(), 3, "объявления обязаны остаться атрибутами");
        assert_eq!(r.uri(r.element_ns().unwrap()), Some("urn:w"));
        // MCE не интерпретируется — `mc:Ignorable` просто атрибут.
        assert_eq!(r.attr_name(2).unwrap().local_bytes(src), b"Ignorable");
        assert_eq!(r.attr_value(2).unwrap(), "w");
    }

    #[test]
    fn attribute_without_prefix_has_no_namespace() {
        let src = br#"<a xmlns="urn:d" v="1"/>"#;
        let mut r = Reader::new(src);
        let _ = r.next_event().unwrap();
        let _ = r.next_event().unwrap();
        assert_eq!(r.uri(r.element_ns().unwrap()), Some("urn:d"));
        assert_eq!(r.attr_ns(1), Some(None));
    }

    #[test]
    fn deep_nesting_fails_fast_at_the_limit() {
        let mut doc = Vec::new();
        for _ in 0..100_000 {
            doc.extend_from_slice(b"<a>");
        }
        match parse(&doc) {
            Err(e) => assert!(e.is_limit(), "{e:?}"),
            Ok(_) => panic!("глубина должна упереться в квоту"),
        }
    }

    #[test]
    fn many_attributes_do_not_go_quadratic() {
        // Свыше порога включается сортировка; проверяем, что обнаружение
        // дубликата при этом не теряется.
        let mut doc = Vec::from(&b"<a"[..]);
        for i in 0..200 {
            doc.extend_from_slice(format!(" k{i}=\"v\"").as_bytes());
        }
        doc.extend_from_slice(b" k7=\"dup\"/>");
        assert!(matches!(
            parse(&doc),
            Err(Error::Xml {
                kind: XmlError::DuplicateAttribute,
                ..
            })
        ));
    }
}
