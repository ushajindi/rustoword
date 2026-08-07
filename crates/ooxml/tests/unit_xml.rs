//! Проверки XML-слоя: главный инвариант плитования, лексические детали,
//! декодирование, враждебный вход и два фаззера.
//!
//! Главный инвариант формулируется одной строкой: конкатенация спанов всех
//! событий лексера побайтово равна входу. Всё остальное в этом файле — способы
//! найти документ, на котором он не выполняется.

// В тестах паника — это способ сообщить о провале, а не дефект; арифметика в
// генераторе и счётчиках заведомо мала, а переполнение в debug-сборке всё равно
// паникует, то есть сообщит о себе само.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::error::{Error, XmlError};
use ooxml::xml::entity::{decode_attr_to_string, decode_text_to_string};
use ooxml::xml::lexer::{Event, Lexer, retile};
use ooxml::xml::reader::Reader;

// ---------------------------------------------------------------------------
// Вспомогательное
// ---------------------------------------------------------------------------

/// Главный инвариант в исполняемой форме.
#[track_caller]
fn tiles(src: &[u8]) {
    match retile(src) {
        Ok(back) => assert_eq!(
            back,
            src,
            "плитование нарушено на {:?}",
            String::from_utf8_lossy(src)
        ),
        Err(e) => panic!("лексер отверг {:?}: {e}", String::from_utf8_lossy(src)),
    }
}

fn parse(src: &[u8]) -> Result<(), Error> {
    let mut r = Reader::new(src);
    loop {
        if matches!(r.next_event()?, Event::Eof) {
            return Ok(());
        }
    }
}

fn xml_kind(e: &Error) -> Option<XmlError> {
    match e {
        Error::Xml { kind, .. } => Some(*kind),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Инвариант плитования на рукотворных документах
// ---------------------------------------------------------------------------

/// Документы подобраны так, чтобы каждый ловил свою лексическую деталь: если
/// лексер начнёт «нормализовать» пробел, кавычку или перевод строки, ровно один
/// из них перестанет собираться обратно.
const HANDMADE: &[&[u8]] = &[
    b"",
    b"<a/>",
    b"<a></a>",
    b"<a />",
    b"<a  />",
    b"<a\t/>",
    b"<a\r\n/>",
    b"<a></a\t>",
    b"<a></a   >",
    b"<a b='1'/>",
    b"<a b=\"1\"/>",
    b"<a b = '1'/>",
    b"<a\tb\n=\r\n'1'  />",
    b"<a b='1' c=\"2\" d='3'/>",
    b"<a b=''/>",
    b"<a b='&quot;'/>",
    b"<a b='&#34;'/>",
    b"<a b='&#x22;'/>",
    b"<a b=\"it's\"/>",
    b"<a b='say \"hi\"'/>",
    b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<a/>",
    b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a/>",
    b"<?xml version=\"1.0\"?><a/>",
    b"\xEF\xBB\xBF<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<w:p xmlns:w=\"urn:w\"/>",
    b"\xEF\xBB\xBF<a/>",
    b"<a><!-- comment --></a>",
    b"<a><!----></a>",
    b"<a><!-- <b/> & --></a>",
    b"<!-- before --><a/><!-- after -->",
    b"<?pi?><a/><?pi data?>",
    b"<?xml-stylesheet href=\"s.xsl\"?><a/>",
    b"<a><![CDATA[]]></a>",
    b"<a><![CDATA[<b>&amp;]]></a>",
    b"<a>text</a>",
    b"<a>&amp;&lt;&gt;&quot;&apos;</a>",
    b"<a>&#34;&#x22;&#60;&#62;&#xA;&#160;</a>",
    b"<a>\r\n  <b>\r\n    <c/>\r\n  </b>\r\n</a>",
    b"<a>\n  <b/>\n</a>\n",
    b"<a>   </a>",
    b"<a>a<b/>c<d/>e</a>",
    b"<a xmlns=\"urn:d\" xmlns:x=\"urn:x\" x:v=\"1\"/>",
    b"<w:document xmlns:mc=\"urn:mc\" xmlns:x14ac=\"urn:x\" xmlns:xr=\"urn:r\" mc:Ignorable=\"x14ac xr\" xmlns:w=\"urn:w\"/>",
    b"<a>\xD1\x82\xD0\xB5\xD0\xBA\xD1\x81\xD1\x82</a>",
    b"<\xD1\x82\xD0\xB5\xD0\xB3/>",
    b"<a b='\xD0\xB7\xD0\xBD\xD0\xB0\xD1\x87'/>",
    b" <a/> ",
    b"\n\n<a/>\n\n",
    b"<a><b><c><d><e><f/></e></d></c></b></a>",
    b"<a.b-c_d.e-f/>",
    b"<a xml:space=\"preserve\"> </a>",
];

#[test]
fn handmade_documents_tile() {
    for doc in HANDMADE {
        tiles(doc);
    }
}

#[test]
fn handmade_documents_parse() {
    // Пустой документ и одни пробелы корнем не являются — это не тот случай,
    // который здесь проверяется.
    for doc in HANDMADE.iter().filter(|d| !d.is_empty()) {
        assert!(
            parse(doc).is_ok(),
            "не разобрано: {}",
            String::from_utf8_lossy(doc)
        );
    }
}

#[test]
fn three_spellings_of_an_empty_element_stay_distinct() {
    for src in [&b"<a/>"[..], b"<a></a>", b"<a />", b"<a\n/>"] {
        tiles(src);
        assert!(parse(src).is_ok());
    }
    // И различие видно в событии, а не только в байтах.
    let mut lex = Lexer::new(b"<a />");
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
    assert_eq!(pre_close_ws.len(), 1);
}

#[test]
fn no_trailing_newline_is_not_invented() {
    // Все 798 частей корпуса кончаются байтом `>`. Дописанный перевод строки
    // сломал бы round-trip на каждой из них.
    let src = b"<?xml version=\"1.0\"?>\r\n<a><b/></a>";
    let back = retile(src).unwrap();
    assert_eq!(back.last(), Some(&b'>'));
    assert_eq!(back, src);
}

#[test]
fn both_quote_styles_survive() {
    let src = br#"<a x="1" y='2'/>"#;
    let mut r = Reader::new(src);
    let _ = r.next_event().unwrap();
    let _ = r.next_event().unwrap();
    assert_eq!(r.attrs()[0].quote, b'"');
    assert_eq!(r.attrs()[1].quote, b'\'');
    tiles(src);
}

// ---------------------------------------------------------------------------
// Декодирование
// ---------------------------------------------------------------------------

#[test]
fn all_reference_forms_decode_to_the_same_characters() {
    let cases: &[(&[u8], &str)] = &[
        (b"&amp;", "&"),
        (b"&#38;", "&"),
        (b"&#x26;", "&"),
        (b"&lt;", "<"),
        (b"&#60;", "<"),
        (b"&#x3C;", "<"),
        (b"&gt;", ">"),
        (b"&#62;", ">"),
        (b"&#x3E;", ">"),
        (b"&quot;", "\""),
        (b"&#34;", "\""),
        (b"&#x22;", "\""),
        (b"&apos;", "'"),
        (b"&#39;", "'"),
        (b"&#x27;", "'"),
        (b"&#160;", "\u{a0}"),
        (b"&#xA0;", "\u{a0}"),
        (b"&#xA;", "\n"),
        (b"&#10;", "\n"),
    ];
    for (raw, want) in cases {
        assert_eq!(
            &decode_text_to_string(raw).unwrap(),
            want,
            "{:?}",
            core::str::from_utf8(raw)
        );
    }
}

#[test]
fn raw_spans_are_never_normalised_by_the_lexer() {
    // Обе формы кавычки живут в корпусе одновременно; лексер обязан отдать их
    // различимыми, а декодер — свести к одному символу.
    let src = br#"<a p="&quot;" q="&#34;" r="&#x22;"/>"#;
    let mut r = Reader::new(src);
    let _ = r.next_event().unwrap();
    let _ = r.next_event().unwrap();
    let raws: Vec<&[u8]> = r
        .attrs()
        .iter()
        .map(|a| a.value.slice(src).unwrap())
        .collect();
    assert_eq!(raws, vec![&b"&quot;"[..], b"&#34;", b"&#x22;"]);
    for i in 0..3 {
        assert_eq!(r.attr_value(i).unwrap(), "\"");
    }
    tiles(src);
}

#[test]
fn attribute_normalization_only_touches_literal_whitespace() {
    assert_eq!(decode_attr_to_string(b"a\tb").unwrap(), "a b");
    assert_eq!(decode_attr_to_string(b"a&#x9;b").unwrap(), "a\tb");
    assert_eq!(decode_attr_to_string(b"a\r\nb").unwrap(), "a b");
    assert_eq!(decode_attr_to_string(b"a&#xA;b").unwrap(), "a\nb");
}

// ---------------------------------------------------------------------------
// Враждебный вход
// ---------------------------------------------------------------------------

#[test]
fn doctype_is_forbidden_unconditionally() {
    let docs: &[&[u8]] = &[
        b"<!DOCTYPE a><a/>",
        b"<!DOCTYPE a SYSTEM \"http://evil/\"><a/>",
        b"<?xml version=\"1.0\"?><!DOCTYPE a []><a/>",
        b"<!doctype a><a/>",
    ];
    for d in docs {
        let e = parse(d).unwrap_err();
        assert_eq!(
            xml_kind(&e),
            Some(XmlError::DoctypeForbidden),
            "{:?}",
            String::from_utf8_lossy(d)
        );
    }
}

#[test]
fn billion_laughs_dies_at_the_doctype() {
    // Классическая бомба даже не доходит до раскрытия сущностей: DTD запрещён,
    // поэтому определять `&lol9;` негде, а сама конструкция отвергается первой.
    let mut doc = Vec::from(&b"<!DOCTYPE lolz [<!ENTITY lol \"lol\">"[..]);
    for i in 1..10 {
        doc.extend_from_slice(
            format!(
                "<!ENTITY lol{i} \"{}\">",
                format!("&lol{};", i - 1).repeat(10)
            )
            .as_bytes(),
        );
    }
    doc.extend_from_slice(b"]><lolz>&lol9;</lolz>");
    assert_eq!(
        xml_kind(&parse(&doc).unwrap_err()),
        Some(XmlError::DoctypeForbidden)
    );

    // И без DOCTYPE ссылка на неизвестную сущность тоже никуда не раскрывается.
    let mut r = Reader::new(b"<a>&lol9;</a>");
    let mut span = None;
    loop {
        match r.next_event().unwrap() {
            Event::Text { span: s, .. } => span = Some(s),
            Event::Eof => break,
            _ => {}
        }
    }
    assert_eq!(
        xml_kind(&r.text(span.unwrap()).unwrap_err()),
        Some(XmlError::UnknownEntity)
    );
}

#[test]
fn million_deep_nesting_fails_immediately_not_by_timeout() {
    let mut doc = Vec::with_capacity(3_000_000);
    for _ in 0..1_000_000 {
        doc.extend_from_slice(b"<a>");
    }
    let t0 = std::time::Instant::now();
    let e = parse(&doc).unwrap_err();
    let dt = t0.elapsed();
    assert!(e.is_limit(), "{e:?}");
    // Квота глубины — 256, значит отказ обязан прийти на 257-м теге, то есть
    // после разбора долей процента входа.
    assert!(
        dt < std::time::Duration::from_millis(200),
        "отказ занял {dt:?} — похоже, разбирается весь вход"
    );

    // Рекурсии в парсере нет, поэтому и переполнения стека нет: снятие квоты
    // даёт честную работу, а не крах.
    let mut permissive = ooxml::Limits::permissive();
    permissive.max_xml_depth = 4_096;
    let mut r = Reader::with_limits(&doc, permissive);
    let mut err = None;
    loop {
        match r.next_event() {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(err.is_some_and(|e| e.is_limit()));
}

#[test]
fn structural_attacks_are_errors() {
    let cases: &[(&[u8], XmlError)] = &[
        (b"<a>", XmlError::UnbalancedTag),
        (b"<a><b></a></b>", XmlError::MismatchedTag),
        (b"<a></b>", XmlError::MismatchedTag),
        (b"</a>", XmlError::UnbalancedTag),
        (b"<a/><b/>", XmlError::MultipleRoots),
        (b"<a></a><a></a>", XmlError::MultipleRoots),
        (b"", XmlError::NoRoot),
        (b"<!-- only a comment -->", XmlError::NoRoot),
        (b"<a b='<'/>", XmlError::UnexpectedByte(b'<')),
        (b"<a b=1/>", XmlError::UnquotedAttribute),
        (b"<a b=/>", XmlError::UnquotedAttribute),
        (b"<a b/>", XmlError::UnexpectedByte(b'/')),
        (b"<a b='1'c='2'/>", XmlError::UnexpectedByte(b'c')),
        (b"<a x='1' x='2'/>", XmlError::DuplicateAttribute),
        (b"<a b='1", XmlError::UnexpectedEof),
        (b"<a", XmlError::UnexpectedEof),
        (b"<", XmlError::UnexpectedEof),
        (b"<a><!-- unclosed", XmlError::UnexpectedEof),
        (b"<a><![CDATA[unclosed", XmlError::UnexpectedEof),
        (b"<?unclosed", XmlError::UnexpectedEof),
        (b"<a:b:c/>", XmlError::BadName),
        (b"<:a/>", XmlError::BadName),
        (b"<p:a/>", XmlError::UndeclaredPrefix),
        (b"<a p:v='1'/>", XmlError::UndeclaredPrefix),
        (b"<a xmlns:xmlns='urn:x'/>", XmlError::ReservedPrefix),
        (b"<a xmlns:xml='urn:x'/>", XmlError::ReservedPrefix),
        (b"<a>&nope;</a>", XmlError::NoRoot), // текст не декодируется — см. ниже
    ];
    for (src, want) in cases {
        // Последний случай особый: неизвестная сущность в тексте лексером не
        // замечается вовсе, потому что текст не декодируется при разборе.
        if *want == XmlError::NoRoot && !src.is_empty() && src.starts_with(b"<a>&") {
            assert!(
                parse(src).is_ok(),
                "текст не должен декодироваться при разборе"
            );
            continue;
        }
        let e = parse(src).unwrap_err();
        assert_eq!(
            xml_kind(&e),
            Some(*want),
            "{:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn markup_inside_attribute_value_cannot_smuggle_a_tag() {
    assert_eq!(
        xml_kind(&parse(b"<a href='x'><b c='</a><script>'/></a>").unwrap_err()),
        Some(XmlError::UnexpectedByte(b'<'))
    );
}

#[test]
fn invalid_utf8_survives_lexing_and_fails_only_on_decode() {
    // Сырые спаны копируются без проверки: файл с битым байтом в тексте
    // по-прежнему открывается в Word, и переписать его мы обязаны как есть.
    let src = b"<a>\xFF\xFE</a>";
    tiles(src);
    let mut r = Reader::new(src);
    let mut span = None;
    loop {
        match r.next_event().unwrap() {
            Event::Text { span: s, .. } => span = Some(s),
            Event::Eof => break,
            _ => {}
        }
    }
    assert_eq!(
        xml_kind(&r.text(span.unwrap()).unwrap_err()),
        Some(XmlError::NotUtf8)
    );
}

// ---------------------------------------------------------------------------
// Грамматический фаззер
// ---------------------------------------------------------------------------

/// SplitMix64 — детерминированный PRNG в пятнадцать строк.
///
/// Свой, а не из крейта: зависимостей у ядра нет, а тестам нужна
/// воспроизводимость, а не криптостойкость.
struct Rng(u64);

impl Rng {
    const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(Self::GOLDEN);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Равномерно в `[0, n)`. Для `n == 0` возвращает 0.
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }

    fn pick<'t, T>(&mut self, xs: &'t [T]) -> &'t T {
        &xs[self.below(xs.len())]
    }
}

/// Базовый сид. Сид итерации `i` = `BASE ^ i * GOLDEN`, поэтому падение
/// воспроизводится одним числом, а соседние итерации не коррелируют.
const FUZZ_BASE: u64 = 0x0000_5EED_C0FF_EE01;

fn iter_seed(i: u64) -> u64 {
    FUZZ_BASE ^ i.wrapping_mul(Rng::GOLDEN)
}

const ELEM_NAMES: &[&str] = &["x", "y", "z", "a:x", "b:y", "el", "\u{442}\u{435}\u{433}"];
const TEXT_PIECES: &[&str] = &[
    "hello",
    " ",
    "\n",
    "\r\n",
    "\t",
    "&amp;",
    "&#38;",
    "&lt;",
    "&#60;",
    "&gt;",
    "&#62;",
    "&quot;",
    "&#34;",
    "&#x22;",
    "&apos;",
    "&#xA;",
    "&#160;",
    "\u{442}\u{435}\u{43a}\u{441}\u{442}",
    "]]",
    "a > b",
];
const ATTR_VALUES: &[&str] = &[
    "",
    "1",
    "true",
    "&quot;q&quot;",
    "&#34;q&#34;",
    "&#x22;q&#x22;",
    "&amp;",
    "&#xA;",
    "&#x9;",
    "x14ac xr xr2 xr3",
    "\u{437}\u{43d}\u{430}\u{447}",
];

/// Пробельная вставка внутрь тега: генератор обязан порождать всё, что
/// встречается в дикой природе, включая переводы строк посреди атрибутов.
fn ws(rng: &mut Rng, out: &mut String, min: usize) {
    let n = min + rng.below(3);
    for _ in 0..n {
        out.push(*rng.pick(&[' ', '\t', '\n', '\r']));
    }
}

/// Генерирует документ. `max_depth` — предел вложенности.
fn gen_doc(rng: &mut Rng, max_depth: usize) -> Vec<u8> {
    let mut s = String::new();
    if rng.chance(1, 3) {
        s.push('\u{feff}');
    }
    if rng.chance(4, 5) {
        s.push_str("<?xml version=\"1.0\"");
        if rng.chance(3, 4) {
            s.push_str(" encoding=\"UTF-8\"");
        }
        if rng.chance(2, 3) {
            s.push_str(" standalone=\"yes\"");
        }
        s.push_str("?>");
        // У Office после декларации идёт `\r\n`; у прочих генераторов — `\n`
        // либо ничего.
        s.push_str(rng.pick(&["\r\n", "\n", ""]));
    }
    gen_misc(rng, &mut s);

    // Каждый третий документ — «хребет»: цепочка вложенных элементов на всю
    // допустимую глубину. Без него генератор сваливается к деревьям глубиной
    // около десяти, и вложенность в сотни уровней никогда не проверяется.
    let spine = if rng.chance(1, 3) { max_depth } else { 0 };
    let mut budget = 400usize;
    gen_element(rng, &mut s, 1, max_depth, spine, &mut budget, true);

    gen_misc(rng, &mut s);
    s.into_bytes()
}

/// Комментарии, инструкции обработки и пробелы вне корня.
fn gen_misc(rng: &mut Rng, out: &mut String) {
    for _ in 0..rng.below(3) {
        match rng.below(3) {
            0 => {
                out.push_str("<!--");
                out.push_str(rng.pick(&["", " c ", "\r\n", "<a/>", "&amp;", "- -"]));
                out.push_str("-->");
            }
            1 => {
                out.push_str("<?");
                out.push_str(rng.pick(&["pi", "mso-application", "target"]));
                out.push_str(rng.pick(&["", " data", " a=\"b\"", "\r\n x"]));
                out.push_str("?>");
            }
            _ => out.push_str(rng.pick(&["\n", "\r\n", " ", "\t"])),
        }
    }
}

fn gen_element(
    rng: &mut Rng,
    out: &mut String,
    depth: usize,
    max_depth: usize,
    spine: usize,
    budget: &mut usize,
    root: bool,
) {
    let name: &str = if root { "root" } else { rng.pick(ELEM_NAMES) };
    out.push('<');
    out.push_str(name);

    // Корень объявляет все префиксы, которыми пользуются потомки: MCE-подобный
    // набор из реального `.xlsx` тоже воспроизводится.
    if root {
        out.push_str(" xmlns:a=\"urn:a\" xmlns:b=\"urn:b\"");
        if rng.chance(1, 2) {
            out.push_str(" xmlns=\"urn:default\"");
        }
        if rng.chance(1, 3) {
            out.push_str(" xmlns:mc=\"urn:mc\" mc:Ignorable=\"a b\"");
        }
    }
    for i in 0..rng.below(5) {
        ws(rng, out, 1);
        if rng.chance(1, 4) {
            out.push_str("a:");
        }
        out.push_str(&format!("k{i}"));
        if rng.chance(1, 4) {
            ws(rng, out, 0);
        }
        out.push('=');
        if rng.chance(1, 4) {
            ws(rng, out, 0);
        }
        let quote = if rng.chance(1, 2) { '"' } else { '\'' };
        out.push(quote);
        out.push_str(rng.pick(ATTR_VALUES));
        out.push(quote);
    }

    // Пока не пройден хребет, элемент обязан иметь потомка-элемент.
    let on_spine = depth < spine;
    let leaf = !on_spine && (depth >= max_depth || *budget == 0 || rng.chance(1, 3));
    if leaf && rng.chance(1, 2) {
        if rng.chance(1, 3) {
            ws(rng, out, 1);
        }
        out.push_str("/>");
        return;
    }
    if rng.chance(1, 5) {
        ws(rng, out, 1);
    }
    out.push('>');

    if on_spine {
        // Звено хребта бюджет не тратит: иначе цепочка обрывалась бы на
        // середине и глубина снова не достигалась бы.
        gen_element(rng, out, depth + 1, max_depth, spine, budget, false);
    }
    if !leaf {
        for _ in 0..rng.below(3) {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            match rng.below(6) {
                0 => out.push_str(rng.pick(TEXT_PIECES)),
                1 => {
                    out.push_str("<!--");
                    out.push_str(rng.pick(&["", " c ", "\r\n", "&amp;"]));
                    out.push_str("-->");
                }
                2 => {
                    out.push_str("<![CDATA[");
                    out.push_str(rng.pick(&["", "<b>&amp;", "]", "]]", "\r\n"]));
                    out.push_str("]]>");
                }
                3 => {
                    out.push_str("<?pi");
                    out.push_str(rng.pick(&["", " d", "\r\n"]));
                    out.push_str("?>");
                }
                _ => gen_element(rng, out, depth + 1, max_depth, spine, budget, false),
            }
        }
    }

    out.push_str("</");
    out.push_str(name);
    if rng.chance(1, 6) {
        ws(rng, out, 1);
    }
    out.push('>');
}

fn fuzz_iters() -> u64 {
    std::env::var("OOXML_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

#[test]
fn grammar_fuzz_keeps_the_tiling_invariant() {
    let iters = fuzz_iters();
    let mut deepest = 0usize;
    let mut biggest = 0usize;
    for i in 0..iters {
        let seed = iter_seed(i);
        let mut rng = Rng::new(seed);
        // Глубина до 200 — под квотой в 256, чтобы проверялось плитование, а не
        // срабатывание лимита.
        let max_depth = 1 + rng.below(200);
        let doc = gen_doc(&mut rng, max_depth);

        match retile(&doc) {
            Ok(back) => assert!(
                back == doc,
                "сид {seed:#x} (итерация {i}): плитование нарушено\n{}",
                String::from_utf8_lossy(&doc)
            ),
            Err(e) => panic!(
                "сид {seed:#x} (итерация {i}): лексер отверг свой же документ: {e}\n{}",
                String::from_utf8_lossy(&doc)
            ),
        }

        // Сгенерированные документы корректны по построению, поэтому и полный
        // разбор обязан проходить: это ловит ошибки в стеке namespace и в
        // проверке парности, которых плитование не видит.
        let mut limits = ooxml::Limits::strict();
        limits.max_xml_depth = 4_096;
        let mut r = Reader::with_limits(&doc, limits);
        loop {
            match r.next_event() {
                Ok(Event::Eof) => break,
                Ok(_) => deepest = deepest.max(r.depth()),
                Err(e) => panic!(
                    "сид {seed:#x} (итерация {i}): разбор провалился: {e}\n{}",
                    String::from_utf8_lossy(&doc)
                ),
            }
        }
        biggest = biggest.max(doc.len());
    }

    // Сторож самого фаззера: если генератор однажды выродится в «корень и
    // ничего больше», тест продолжит быть зелёным, ничего при этом не проверяя.
    assert!(
        deepest >= 150,
        "генератор перестал строить глубокие документы (максимум {deepest}, ожидалось не меньше 150)"
    );
    assert!(
        biggest >= 1_000,
        "генератор перестал строить объёмные документы (максимум {biggest} байт)"
    );
}

#[test]
fn raw_fuzz_never_panics_and_never_accepts_garbage() {
    let mut accepted = 0usize;
    for i in 0..100_000u64 {
        let seed = iter_seed(i ^ 0xDEAD_BEEF);
        let mut rng = Rng::new(seed);
        let len = rng.below(64);
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }

        // Лексер паниковать не имеет права ни на чём; если он всё же разобрал
        // мусор, плитование обязано выполняться и на нём.
        if let Ok(back) = retile(&buf) {
            assert!(back == buf, "сид {seed:#x}: плитование нарушено на мусоре");
        }
        if parse(&buf).is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(
        accepted, 0,
        "случайные байты не должны разбираться как корректный XML"
    );
}

#[test]
fn raw_fuzz_over_mutated_valid_documents() {
    // Мутация корректного документа доходит до кода, куда чистый шум не
    // добирается: до разбора атрибутов, стека namespace и сравнения имён.
    let base = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="urn:w" xmlns:mc="urn:mc" mc:Ignorable="w"><w:body><w:p w:rsidR="00A"><w:r><w:t xml:space="preserve">text &amp; &#34;more&#34;</w:t></w:r></w:p></w:body></w:document>"#;
    for i in 0..20_000u64 {
        let mut rng = Rng::new(iter_seed(i ^ 0x0BAD_F00D));
        let mut buf = base.to_vec();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(buf.len());
            buf[at] = (rng.next_u64() & 0xFF) as u8;
        }
        if let Ok(back) = retile(&buf) {
            assert!(
                back == buf,
                "сид {:#x}: плитование нарушено на мутации",
                iter_seed(i ^ 0x0BAD_F00D)
            );
        }
        let _ = parse(&buf);
    }
}
