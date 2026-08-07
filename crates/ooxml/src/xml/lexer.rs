//! Байтовый токенайзер XML.
//!
//! # Главный инвариант: плитование
//!
//! Конкатенация [`Event::contribution`] всех выданных событий обязана быть
//! **побайтово равна входу**. Ни один байт не теряется, не дублируется и не
//! переупорядочивается.
//!
//! Это не эстетическое требование, а способ поймать потерю лексической детали
//! на самом раннем слое. Замеры реального корпуса (798 XML-частей из 43 файлов)
//! показали, что «незначимого форматирования» в OOXML не бывает: в одном корпусе
//! живут и `&quot;`, и `&#34;`; и `&gt;`, и `&#62;`; BOM есть в docx и нет в
//! xlsx; после декларации у Office идёт `\r\n`; встречается `<a />` с пробелом
//! перед `/>`. Каноникализации, которая свела бы это к одной форме, не
//! существует — значит, всё перечисленное обязано быть событием со спаном, а не
//! мусором, который парсер молча съедает.
//!
//! Отсюда же следует, что **текст и значения атрибутов не декодируются при
//! лексировании**: они остаются сырыми спанами и пишутся обратно verbatim.
//! Декодирование — забота [`crate::xml::entity`], и происходит только когда
//! типизированный API запросил значение.
//!
//! Лексер намеренно не знает ни о парности тегов, ни о квотах, ни о namespace:
//! это работа [`crate::xml::Reader`]. Здесь только байты.

use crate::bytes::{Reader as Cursor, Span};
use crate::error::{Error, LimitError, Result, XmlError};

/// Метка порядка байт UTF-8. В корпусе присутствует в 16 частях (все — docx).
pub const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Событие лексера.
///
/// Все поля — спаны исходного буфера, ни один байт не скопирован и не
/// нормализован.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Начало документа. Выдаётся ровно один раз, до всего остального.
    StartDoc {
        /// Был ли в начале BOM. Его три байта — вклад этого события в плитование.
        bom: bool,
    },
    /// Объявление `<?xml ... ?>`.
    Decl { span: Span },
    /// Инструкция обработки `<?target ... ?>`.
    Pi { span: Span },
    /// Комментарий `<!-- ... -->`.
    Comment { span: Span },
    /// Открывающий или пустой тег.
    Start {
        /// Весь тег, от `<` до `>` включительно.
        span: Span,
        /// Имя элемента как есть, вместе с префиксом.
        name: Span,
        /// Индекс первого атрибута в буфере лексера ([`Lexer::attr_range`]).
        attrs_from: usize,
        /// Число атрибутов.
        attrs_len: usize,
        /// Тег самозакрывающийся (`<a/>`).
        empty: bool,
        /// Пробелы непосредственно перед `>` или `/>`.
        ///
        /// Отдельное поле, потому что `<a/>`, `<a />` и `<a  />` — три разных
        /// файла, и восстановить этот пробел из остальных спанов нельзя.
        pre_close_ws: Span,
    },
    /// Закрывающий тег.
    End {
        span: Span,
        name: Span,
        /// Пробелы между именем и `>`: `</w:p  >` — валидный XML.
        trailing_ws: Span,
    },
    /// Символьные данные между тегами. Сырые, всё ещё экранированные.
    Text {
        span: Span,
        /// Встретился байт `&` — значит, дословный спан и декодированное
        /// значение различаются. Позволяет типизированному API не звать
        /// декодер там, где он не нужен.
        has_refs: bool,
        /// Текст состоит только из пробельных байт. Нужно [`crate::xml::Reader`],
        /// чтобы отличить форматирование в прологе от текста вне корня.
        all_ws: bool,
    },
    /// Секция `<![CDATA[ ... ]]>`.
    CData { span: Span },
    /// Конец документа.
    Eof,
}

impl Event {
    /// Байтовый вклад события в исходный буфер.
    ///
    /// Существует ради главного инварианта: конкатенация вкладов всех событий
    /// обязана быть равна входу. `StartDoc` — единственное событие, спан
    /// которого выводится, а не хранится: BOM по определению лежит в начале
    /// буфера, и хранить в событии константу 0..3 было бы лишним полем.
    /// `Eof` не вносит байт вовсе.
    #[must_use]
    pub const fn contribution(self) -> Option<Span> {
        match self {
            Self::StartDoc { bom } => Some(if bom {
                Span::clamped(0, 3)
            } else {
                Span::empty(0)
            }),
            Self::Decl { span }
            | Self::Pi { span }
            | Self::Comment { span }
            | Self::CData { span }
            | Self::Text { span, .. }
            | Self::Start { span, .. }
            | Self::End { span, .. } => Some(span),
            Self::Eof => None,
        }
    }

    /// Полный спан события, если он у события есть.
    #[must_use]
    pub const fn span(self) -> Option<Span> {
        match self {
            Self::StartDoc { .. } | Self::Eof => None,
            _ => self.contribution(),
        }
    }
}

/// Атрибут, разобранный до лексем, но не декодированный.
///
/// Хранится всё, что различает два байтово разных файла с «одинаковыми»
/// атрибутами: сколько пробелов было до имени, вокруг `=`, каким символом
/// закрыта кавычка. Значение остаётся экранированным — переэкранирование
/// чужого содержимого сломало бы round-trip (`&#34;` вернулся бы как `&quot;`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawAttr {
    /// Пробелы перед именем атрибута. Всегда минимум один байт.
    pub pre_ws: Span,
    /// Имя атрибута вместе с префиксом.
    pub name: Span,
    /// Пробелы между именем и `=`.
    pub ws_before_eq: Span,
    /// Пробелы между `=` и открывающей кавычкой.
    pub ws_after_eq: Span,
    /// Символ кавычки: `"` или `'`. В корпусе встречаются оба.
    pub quote: u8,
    /// Значение БЕЗ кавычек, сырое, всё ещё экранированное.
    pub value: Span,
}

impl RawAttr {
    /// Весь атрибут вместе с ведущими пробелами и обеими кавычками.
    #[must_use]
    pub fn span(self) -> Span {
        // Закрывающая кавычка — ровно один байт за концом значения.
        self.pre_ws.join(Span::clamped(
            self.value.end(),
            self.value.end().saturating_add(1),
        ))
    }
}

/// Токенайзер.
///
/// Атрибуты складываются в общий буфер, а событие ссылается на диапазон в нём:
/// иначе каждый тег стоил бы аллокации вектора, а тегов в листе Excel бывают
/// сотни тысяч.
#[derive(Debug)]
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    attrs: Vec<RawAttr>,
    /// `StartDoc` ещё не выдан.
    at_start: bool,
    /// Вход длиннее, чем адресует `Span`. Ошибка откладывается до `next`,
    /// чтобы конструктор оставался инфаллибельным.
    oversized: bool,
}

impl<'a> Lexer<'a> {
    #[must_use]
    pub fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            pos: 0,
            attrs: Vec::new(),
            at_start: true,
            oversized: src.len() > u32::MAX as usize,
        }
    }

    #[must_use]
    pub const fn src(&self) -> &'a [u8] {
        self.src
    }

    /// Позиция сразу за последним выданным событием.
    #[must_use]
    pub const fn pos(&self) -> usize {
        self.pos
    }

    /// Атрибуты последнего разобранного тега целиком.
    #[must_use]
    pub fn attrs(&self) -> &[RawAttr] {
        &self.attrs
    }

    /// Диапазон атрибутов из [`Event::Start`].
    ///
    /// Выход за границы буфера возвращает пустой срез, а не панику: значения
    /// `attrs_from`/`attrs_len` могли прийти от события другого лексера.
    #[must_use]
    pub fn attr_range(&self, from: usize, len: usize) -> &[RawAttr] {
        let Some(end) = from.checked_add(len) else {
            return &[];
        };
        self.attrs.get(from..end).unwrap_or(&[])
    }

    /// Следующее событие.
    pub fn next_event(&mut self) -> Result<Event> {
        if self.oversized {
            return Err(Error::Limit(LimitError::PartTooLarge {
                got: self.src.len() as u64,
                max: u64::from(u32::MAX),
            }));
        }

        let before = self.pos;
        let ev = self.lex()?;

        // Главный инвариант, проверяемый по индукции: каждое событие покрывает
        // ровно [before, pos), а `pos` монотонно растёт от 0 до длины входа.
        // Отсюда следует, что конкатенация спанов равна входу.
        debug_assert!(
            match ev.contribution() {
                Some(sp) => sp.start() as usize == before && sp.end() as usize == self.pos,
                None => self.pos == before,
            },
            "лексер потерял или продублировал байты: событие {ev:?}, {before}..{}",
            self.pos
        );
        debug_assert!(
            self.pos > before || matches!(ev, Event::Eof | Event::StartDoc { bom: false }),
            "лексер не сдвинулся: {ev:?} на {before}"
        );
        Ok(ev)
    }

    fn lex(&mut self) -> Result<Event> {
        if self.at_start {
            self.at_start = false;
            let bom = self.src.starts_with(&BOM);
            if bom {
                self.pos = BOM.len();
            }
            return Ok(Event::StartDoc { bom });
        }
        if self.pos >= self.src.len() {
            return Ok(Event::Eof);
        }

        let mut cur = Cursor::at(self.src, self.pos)
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, self.pos))?;
        let ev = if cur.peek_at(0) == Some(b'<') {
            match cur.peek_at(1) {
                None => return Err(xml_err(XmlError::UnexpectedEof, self.src.len())),
                Some(b'?') => self.lex_pi(&mut cur)?,
                Some(b'!') => self.lex_bang(&mut cur)?,
                Some(b'/') => self.lex_end(&mut cur)?,
                Some(_) => self.lex_start(&mut cur)?,
            }
        } else {
            self.lex_text(&mut cur)?
        };
        self.pos = cur.pos();
        Ok(ev)
    }

    /// `<?xml ... ?>` или `<?target ... ?>`.
    fn lex_pi(&self, cur: &mut Cursor<'a>) -> Result<Event> {
        let start = cur.pos();
        // Поиск с `start + 2`: `<??>` не должен схлопнуться в пустую инструкцию.
        let body = start.saturating_add(2);
        let close = find_from(self.src, b"?>", body)
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, self.src.len()))?;
        let end = close.saturating_add(2);
        seek(cur, end)?;
        let span = span_of(start, end);

        // Декларацией считается только цель, равная ровно `xml`: `<?xml-stylesheet`
        // — обычная инструкция обработки, и путать их нельзя, потому что
        // декларация допустима лишь в самом начале документа.
        let name_at = body;
        let is_decl = self.src.get(name_at..name_at.saturating_add(3)) == Some(b"xml".as_slice())
            && self
                .src
                .get(name_at.saturating_add(3))
                .is_some_and(|&b| is_ws(b) || b == b'?');
        Ok(if is_decl {
            Event::Decl { span }
        } else {
            Event::Pi { span }
        })
    }

    /// `<!-- ... -->`, `<![CDATA[ ... ]]>` или запрещённый `<!DOCTYPE`.
    fn lex_bang(&self, cur: &mut Cursor<'a>) -> Result<Event> {
        let start = cur.pos();
        let tail = |n: usize| self.src.get(start..start.saturating_add(n));

        if tail(4) == Some(b"<!--".as_slice()) {
            // Поиск с `start + 4`: `<!-->` — не комментарий, а ошибка.
            let from = start.saturating_add(4);
            let close = find_from(self.src, b"-->", from)
                .ok_or_else(|| xml_err(XmlError::UnexpectedEof, self.src.len()))?;
            let end = close.saturating_add(3);
            seek(cur, end)?;
            return Ok(Event::Comment {
                span: span_of(start, end),
            });
        }
        if tail(9) == Some(b"<![CDATA[".as_slice()) {
            let from = start.saturating_add(9);
            let close = find_from(self.src, b"]]>", from)
                .ok_or_else(|| xml_err(XmlError::UnexpectedEof, self.src.len()))?;
            let end = close.saturating_add(3);
            seek(cur, end)?;
            return Ok(Event::CData {
                span: span_of(start, end),
            });
        }
        // Регистронезависимо: `<!doctype` — невалидный XML, но сообщить о нём
        // как о запрещённом DOCTYPE честнее, чем как о случайном байте.
        if tail(9).is_some_and(|t| t.eq_ignore_ascii_case(b"<!DOCTYPE")) {
            return Err(xml_err(XmlError::DoctypeForbidden, start));
        }
        let at = start.saturating_add(2);
        Err(xml_err(
            XmlError::UnexpectedByte(self.src.get(at).copied().unwrap_or(b'!')),
            at,
        ))
    }

    /// `</name  >`.
    fn lex_end(&self, cur: &mut Cursor<'a>) -> Result<Event> {
        let start = cur.pos();
        seek(cur, start.saturating_add(2))?;
        let name = self
            .scan_name(cur)
            .ok_or_else(|| xml_err(XmlError::BadName, cur.pos()))?;
        let trailing_ws = self.skip_ws(cur);
        match cur.u8() {
            Some(b'>') => Ok(Event::End {
                span: span_of(start, cur.pos()),
                name,
                trailing_ws,
            }),
            Some(b) => Err(xml_err(
                XmlError::UnexpectedByte(b),
                cur.pos().saturating_sub(1),
            )),
            None => Err(xml_err(XmlError::UnexpectedEof, self.src.len())),
        }
    }

    /// `<name a="1" b='2' />`.
    fn lex_start(&mut self, cur: &mut Cursor<'a>) -> Result<Event> {
        let start = cur.pos();
        seek(cur, start.saturating_add(1))?;
        let name = self
            .scan_name(cur)
            .ok_or_else(|| xml_err(XmlError::BadName, cur.pos()))?;

        // Буфер переиспользуется между тегами, поэтому аллокация амортизируется.
        // `attrs_from` при этом всегда ноль; поле существует, чтобы построитель
        // дерева более поздней вехи мог переключить буфер в накопительный режим,
        // не меняя тип события.
        self.attrs.clear();
        let attrs_from = 0usize;

        loop {
            let ws = self.skip_ws(cur);
            match cur.peek_at(0) {
                None => return Err(xml_err(XmlError::UnexpectedEof, self.src.len())),
                Some(b'>') => {
                    seek(cur, cur.pos().saturating_add(1))?;
                    return Ok(Event::Start {
                        span: span_of(start, cur.pos()),
                        name,
                        attrs_from,
                        attrs_len: self.attrs.len(),
                        empty: false,
                        pre_close_ws: ws,
                    });
                }
                Some(b'/') => {
                    if cur.peek_at(1) != Some(b'>') {
                        return Err(xml_err(
                            XmlError::UnexpectedByte(cur.peek_at(1).unwrap_or(b'/')),
                            cur.pos().saturating_add(1),
                        ));
                    }
                    seek(cur, cur.pos().saturating_add(2))?;
                    return Ok(Event::Start {
                        span: span_of(start, cur.pos()),
                        name,
                        attrs_from,
                        attrs_len: self.attrs.len(),
                        empty: true,
                        pre_close_ws: ws,
                    });
                }
                Some(b) => {
                    // Атрибут обязан отделяться от предыдущей лексемы пробелом:
                    // `<a b="1"c="2">` — не XML.
                    if ws.is_empty() {
                        return Err(xml_err(XmlError::UnexpectedByte(b), cur.pos()));
                    }
                    let attr = self.lex_attr(cur, ws)?;
                    self.attrs.push(attr);
                }
            }
        }
    }

    fn lex_attr(&self, cur: &mut Cursor<'a>, pre_ws: Span) -> Result<RawAttr> {
        let name = self
            .scan_name(cur)
            .ok_or_else(|| xml_err(XmlError::BadName, cur.pos()))?;
        let ws_before_eq = self.skip_ws(cur);
        match cur.u8() {
            Some(b'=') => {}
            Some(b) => {
                return Err(xml_err(
                    XmlError::UnexpectedByte(b),
                    cur.pos().saturating_sub(1),
                ));
            }
            None => return Err(xml_err(XmlError::UnexpectedEof, self.src.len())),
        }
        let ws_after_eq = self.skip_ws(cur);

        let quote_at = cur.pos();
        let quote = match cur.u8() {
            Some(q @ (b'"' | b'\'')) => q,
            // Значение без кавычек — классический вход, на котором наивные
            // парсеры теряют границу атрибута и начинают читать чужие байты.
            Some(_) | None => return Err(xml_err(XmlError::UnquotedAttribute, quote_at)),
        };

        let value_start = cur.pos();
        let value_end = loop {
            let at = cur.pos();
            match cur.u8() {
                None => return Err(xml_err(XmlError::UnexpectedEof, self.src.len())),
                Some(b) if b == quote => break at,
                // `<` в значении атрибута запрещён спецификацией и является
                // признаком инъекции разметки.
                Some(b'<') => return Err(xml_err(XmlError::UnexpectedByte(b'<'), at)),
                Some(_) => {}
            }
        };

        Ok(RawAttr {
            pre_ws,
            name,
            ws_before_eq,
            ws_after_eq,
            quote,
            value: span_of(value_start, value_end),
        })
    }

    fn lex_text(&self, cur: &mut Cursor<'a>) -> Result<Event> {
        let start = cur.pos();
        let rest = self
            .src
            .get(start..)
            .ok_or_else(|| xml_err(XmlError::UnexpectedEof, start))?;

        // Сканирование идёт по срезу, а не через курсор побайтово: текстовые
        // узлы бывают мегабайтными (sharedStrings), и лишний вызов на байт
        // здесь заметен.
        let mut has_refs = false;
        let mut all_ws = true;
        let mut len = 0usize;
        for &b in rest {
            if b == b'<' {
                break;
            }
            if b == b'&' {
                has_refs = true;
            }
            if !is_ws(b) {
                all_ws = false;
            }
            len = len.saturating_add(1);
        }
        let end = start.saturating_add(len);
        seek(cur, end)?;
        Ok(Event::Text {
            span: span_of(start, end),
            has_refs,
            all_ws,
        })
    }

    /// Имя элемента или атрибута. `None`, если имени нет.
    ///
    /// Проверка байт намеренно грубая: всё, что вне ASCII, считается допустимым
    /// в имени. Точная таблица `NameStartChar` из XML 1.0 требует декодирования
    /// UTF-8 на каждом имени, а отвергать по ней нечего — ни один реальный
    /// документ не выигрывает от того, что мы отклоним экзотическое имя, зато
    /// проигрывает каждый, у кого имя кириллическое.
    fn scan_name(&self, cur: &mut Cursor<'a>) -> Option<Span> {
        let start = cur.pos();
        if !cur.peek_at(0).is_some_and(is_name_start) {
            return None;
        }
        let mut len = 0usize;
        while cur.peek_at(len).is_some_and(is_name_byte) {
            len = len.saturating_add(1);
        }
        let end = start.saturating_add(len);
        cur.seek(end)?;
        Some(span_of(start, end))
    }

    fn skip_ws(&self, cur: &mut Cursor<'a>) -> Span {
        let start = cur.pos();
        let mut len = 0usize;
        while cur.peek_at(len).is_some_and(is_ws) {
            len = len.saturating_add(1);
        }
        let end = start.saturating_add(len);
        // Позиция заведомо в границах: столько байт только что прочитано.
        let _ = cur.seek(end);
        span_of(start, end)
    }
}

/// Собирает вход заново **только** из спанов событий лексера.
///
/// Это исполняемая форма главного инварианта: результат обязан быть равен
/// входу. Проверять так дороже, чем сравнить длины, но зато проверка не может
/// «пройти» на документе, где два спана поменялись местами.
pub fn retile(src: &[u8]) -> Result<Vec<u8>> {
    let mut lex = Lexer::new(src);
    let mut out = Vec::with_capacity(src.len());
    loop {
        let ev = lex.next_event()?;
        if let Some(sp) = ev.contribution() {
            let bytes = sp
                .slice(src)
                .ok_or_else(|| xml_err(XmlError::UnexpectedEof, sp.start() as usize))?;
            out.extend_from_slice(bytes);
        }
        if matches!(ev, Event::Eof) {
            return Ok(out);
        }
    }
}

/// XML 1.0, продукция `S`.
#[must_use]
pub const fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

const fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || matches!(b, b'_' | b':') || b >= 0x80
}

const fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.') || b >= 0x80
}

/// Позиция как `u32`. Длина входа проверена в [`Lexer::new`], поэтому усечения
/// здесь не бывает; `saturating` вместо паники — на случай, если проверку
/// когда-нибудь обойдут.
fn pos32(p: usize) -> u32 {
    u32::try_from(p).unwrap_or(u32::MAX)
}

fn span_of(from: usize, to: usize) -> Span {
    Span::clamped(pos32(from), pos32(to))
}

fn seek(cur: &mut Cursor<'_>, to: usize) -> Result<()> {
    cur.seek(to)
        .ok_or_else(|| xml_err(XmlError::UnexpectedEof, to))
}

pub(crate) fn xml_err(kind: XmlError, at: usize) -> Error {
    Error::Xml {
        kind,
        offset: pos32(at),
    }
}

/// Первое вхождение `needle` начиная с `from`.
fn find_from(src: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let hay = src.get(from..)?;
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len())
        .position(|w| w == needle)
        .and_then(|i| from.checked_add(i))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;

    fn tiles(src: &[u8]) {
        assert_eq!(retile(src).unwrap(), src, "плитование нарушено");
    }

    #[test]
    fn empty_input_tiles() {
        tiles(b"");
    }

    #[test]
    fn bom_is_a_span_not_a_skipped_prefix() {
        let src = b"\xEF\xBB\xBF<a/>";
        let mut lex = Lexer::new(src);
        assert_eq!(lex.next_event().unwrap(), Event::StartDoc { bom: true });
        tiles(src);
    }

    #[test]
    fn space_before_self_close_is_preserved() {
        let src = b"<a />";
        let mut lex = Lexer::new(src);
        let _ = lex.next_event().unwrap();
        let Event::Start {
            empty,
            pre_close_ws,
            ..
        } = lex.next_event().unwrap()
        else {
            panic!("ожидался Start");
        };
        assert!(empty);
        assert_eq!(pre_close_ws.slice(src), Some(&b" "[..]));
        tiles(src);
    }

    #[test]
    fn attribute_keeps_its_quote_and_raw_value() {
        let src = br#"<a x = '&#34;q&#34;' />"#;
        let mut lex = Lexer::new(src);
        let _ = lex.next_event().unwrap();
        let _ = lex.next_event().unwrap();
        let a = lex.attrs()[0];
        assert_eq!(a.quote, b'\'');
        assert_eq!(a.value.slice(src), Some(&b"&#34;q&#34;"[..]));
        assert_eq!(a.ws_before_eq.slice(src), Some(&b" "[..]));
        assert_eq!(a.ws_after_eq.slice(src), Some(&b" "[..]));
        tiles(src);
    }

    #[test]
    fn doctype_is_refused_before_anything_is_expanded() {
        let err = retile(b"<!DOCTYPE a [<!ENTITY x \"y\">]><a/>").unwrap_err();
        assert!(matches!(
            err,
            Error::Xml {
                kind: XmlError::DoctypeForbidden,
                ..
            }
        ));
    }

    #[test]
    fn xml_stylesheet_is_a_pi_not_a_declaration() {
        let src = b"<?xml-stylesheet href=\"a\"?><a/>";
        let mut lex = Lexer::new(src);
        let _ = lex.next_event().unwrap();
        assert!(matches!(lex.next_event().unwrap(), Event::Pi { .. }));
        tiles(src);
    }

    #[test]
    fn empty_comment_open_is_not_a_comment() {
        assert!(retile(b"<!-->").is_err());
    }
}
