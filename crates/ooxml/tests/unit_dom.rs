//! Модульные тесты preserving DOM и детерминированный фаззер.
//!
//! Корпус закрывает не всё: комментариев, CDATA, PI и одинарных кавычек в нём
//! ноль частей из 570 — это дыра в покрытии, а не разрешение их не
//! поддерживать. Её закрывают синтетические случаи ниже и фаззер.
//!
//! # Про сторож глубины у фаззера
//!
//! В вехе M5 обнаружилось, что случайный генератор вырождается в деревья
//! глубиной ≤ 9: вероятность продолжить ветку падает экспоненциально, и
//! зелёный тест доказывал куда меньше, чем казалось. Здесь глубина задаётся
//! явно (цель выбирается до генерации, и спуск до неё обязателен), а
//! достигнутый максимум проверяется ассертом.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::dom::{Document, Node, NodeKind};

fn parse(src: &[u8]) -> Document {
    Document::parse(src.to_vec(), &Limits::strict())
        .unwrap_or_else(|e| panic!("разбор {:?}: {e}", String::from_utf8_lossy(src)))
}

/// Разбор → запись обязаны дать исходные байты, а дерево — покрыть их плотно.
fn round_trip(src: &[u8]) {
    let doc = parse(src);
    if let Err(b) = doc.check_coverage() {
        panic!(
            "инвариант покрытия нарушен на {:?}: {b:?}",
            String::from_utf8_lossy(src)
        );
    }
    let back = doc.serialize().unwrap();
    assert_eq!(
        back,
        src,
        "round-trip разошёлся:\n  вход:  {:?}\n  выход: {:?}",
        String::from_utf8_lossy(src),
        String::from_utf8_lossy(&back)
    );
}

// --- размер узла ----------------------------------------------------------

#[test]
fn node_fits_the_memory_budget() {
    // Бюджет задан снаружи: в корпусе 313 тыс. ячеек, самая большая часть даёт
    // 71 тыс. узлов. Лишний байт узла — это лишний мегабайт на большом пакете.
    assert_eq!(size_of::<Node>(), 36, "узел вырос за бюджет 40 байт");
    assert!(size_of::<Node>() <= 40);
}

// --- инвариант покрытия ---------------------------------------------------

#[test]
fn children_tile_the_interior_of_their_parent() {
    let src = b"<?xml version=\"1.0\"?>\r\n<r>\n  <a/>\n  <b>t</b>\n</r>\n";
    let doc = parse(src);
    doc.check_coverage().unwrap();

    // Отдельно — руками, по публичному API: ассерт внутри `check_coverage`
    // проверяет ту же вещь, но тест обязан быть независим от него.
    let mut stack = vec![doc.document_node()];
    while let Some(n) = stack.pop() {
        let kids: Vec<_> = doc.children(n).collect();
        for w in kids.windows(2) {
            let a = doc.span(w[0]).unwrap();
            let b = doc.span(w[1]).unwrap();
            assert!(
                a.touches(b),
                "между детьми {a:?} и {b:?} узла {n:?} осталась дыра"
            );
        }
        stack.extend(kids);
    }
    // Верхний уровень покрывает весь буфер.
    let top: Vec<_> = doc.children(doc.document_node()).collect();
    assert_eq!(doc.span(top[0]).unwrap().start(), 0);
    assert_eq!(
        doc.span(*top.last().unwrap()).unwrap().end() as usize,
        src.len()
    );
}

#[test]
fn whitespace_between_tags_is_a_node_not_a_discard() {
    let doc = parse(b"<r>\r\n  <a/>\r\n</r>");
    let root = doc.root_element().unwrap();
    let kinds: Vec<_> = doc.children(root).map(|c| doc.kind(c).unwrap()).collect();
    assert_eq!(
        kinds,
        vec![NodeKind::Text, NodeKind::Element, NodeKind::Text],
        "межтеговые пробелы обязаны быть узлами: в корпусе 20 различных форм"
    );
}

// --- лексические детали, измеренные в корпусе ------------------------------

#[test]
fn all_four_declaration_forms_survive() {
    for decl in [
        &b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>"[..], // 351 часть
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>",                         // 60
        b"<?xml version=\"1.0\" ?>",                                           // 156
        b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>",       // 2
    ] {
        let mut src = decl.to_vec();
        src.extend_from_slice(b"\r\n<a/>");
        round_trip(&src);
        let doc = parse(&src);
        let first = doc.children(doc.document_node()).next().unwrap();
        assert_eq!(doc.kind(first), Some(NodeKind::Decl));
    }
}

#[test]
fn empty_element_forms_are_three_different_files() {
    // 185 096 самозакрывающихся тегов, 104 911 пар с пустой внутренностью и
    // 181 тег с пробелом перед `/>` — нормализация к одной форме разошлась бы
    // с исходником в 34 файлах из 43.
    for src in [
        &b"<r><a/></r>"[..],
        b"<r><a></a></r>",
        b"<r><a /></r>",
        b"<r><a  /></r>",
        b"<r><a ></a  ></r>",
        b"<r><a\n/></r>",
    ] {
        round_trip(src);
    }
    assert!(doc_self_closing(b"<r><a/></r>"));
    assert!(!doc_self_closing(b"<r><a></a></r>"));
}

fn doc_self_closing(src: &[u8]) -> bool {
    let doc = parse(src);
    let root = doc.root_element().unwrap();
    let a = doc.find_child(root, None, "a").unwrap();
    doc.is_self_closing(a)
}

#[test]
fn both_quote_styles_survive() {
    round_trip(b"<a x=\"1\" y='2'/>");
    let doc = parse(b"<a x=\"1\" y='2'/>");
    let root = doc.root_element().unwrap();
    assert_eq!(doc.attr(root, None, "y").unwrap(), "2");
}

#[test]
fn spaces_around_equals_and_after_the_name_survive() {
    // 39 вхождений `w:customStyle = "1"` и 31 вхождение двойного пробела после
    // имени тега — все в реальном `.docx`.
    for src in [
        &b"<w:t xmlns:w=\"urn:w\" w:customStyle = \"1\"/>"[..],
        b"<styleSheet  xmlns=\"urn:s\"/>",
        b"<a b\t=\n\"1\"/>",
    ] {
        round_trip(src);
    }
}

#[test]
fn entity_spelling_is_never_canonicalised() {
    // В одном корпусе живут `&quot;` (83) и `&#34;` (44), `&gt;` (1) и
    // `&#62;` (3). Свести их к одной форме — значит изменить файл, который
    // никто не правил.
    let src = b"<a t=\"&quot;&#34;&#x22;\">&gt;&#62;&amp;&#xA;</a>";
    round_trip(src);
    let doc = parse(src);
    let root = doc.root_element().unwrap();
    assert_eq!(
        doc.attr_raw(root, None, "t").unwrap(),
        b"&quot;&#34;&#x22;",
        "сырое значение обязано остаться сырым"
    );
    assert_eq!(doc.attr(root, None, "t").unwrap(), "\"\"\"");
    assert_eq!(doc.text(root).unwrap(), ">>&\n");
}

#[test]
fn bom_survives() {
    let src = b"\xEF\xBB\xBF<?xml version=\"1.0\"?><a/>";
    round_trip(src);
    assert!(parse(src).has_bom());
    assert!(!parse(b"<?xml version=\"1.0\"?><a/>").has_bom());
}

#[test]
fn lone_carriage_return_survives() {
    // 110 частей корпуса используют одиночный `CR` как перевод строки. XML 1.0
    // §2.11 обязывает парсер превратить его в `\n`, поэтому такие части нельзя
    // пропускать через декодированный текст и собирать обратно — только спан.
    let src = b"<?xml version=\"1.0\"?>\r<r>\ra\rb\r</r>";
    round_trip(src);
    let doc = parse(src);
    let root = doc.root_element().unwrap();
    // Читающий API нормализацию применяет — и это правильно.
    assert_eq!(doc.text(root).unwrap(), "\na\nb\n");
    // А байты в файле остались теми же — это и проверил round_trip.
}

#[test]
fn literal_cr_and_lf_inside_an_attribute_value_survive() {
    // Прямое доказательство, что значение атрибута нельзя прочитать в `String`
    // и записать обратно: в корпусе 7 таких `CR` и 7 `LF` (`w:tooltip` у
    // `w:hyperlink`). После разбора их в значении уже не существует.
    let src = b"<a t=\"one\rtwo\nthree\tfour\"/>";
    round_trip(src);
    let doc = parse(src);
    let root = doc.root_element().unwrap();
    assert_eq!(
        doc.attr_raw(root, None, "t").unwrap(),
        b"one\rtwo\nthree\tfour"
    );
    // Нормализация значения атрибута (XML 1.0 §3.3.3) видна только в чтении.
    assert_eq!(doc.attr(root, None, "t").unwrap(), "one two three four");
}

#[test]
fn comments_cdata_and_pis_are_nodes() {
    // Ноль частей корпуса содержат хоть что-то из этого — дыра закрывается
    // здесь.
    let src = b"<?xml version=\"1.0\"?>\n<!-- hi -->\n<?target data?>\n<r><![CDATA[a<b&c]]>x</r>\n<!-- bye -->";
    round_trip(src);
    let doc = parse(src);
    let kinds: Vec<_> = doc
        .children(doc.document_node())
        .map(|c| doc.kind(c).unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec![
            NodeKind::Decl,
            NodeKind::Text,
            NodeKind::Comment,
            NodeKind::Text,
            NodeKind::Pi,
            NodeKind::Text,
            NodeKind::Element,
            NodeKind::Text,
            NodeKind::Comment,
        ]
    );
    let root = doc.root_element().unwrap();
    assert_eq!(doc.text(root).unwrap(), "a<b&cx");
}

#[test]
fn namespaces_resolve_but_prefixes_are_written_as_they_were() {
    let src =
        br#"<w:document xmlns:w="urn:w" xmlns:mc="urn:mc" mc:Ignorable="w"><w:body/></w:document>"#;
    round_trip(src);
    let doc = parse(src);
    let root = doc.root_element().unwrap();
    assert_eq!(doc.qname(root).unwrap(), b"w:document");
    assert_eq!(doc.local_name(root).unwrap(), b"document");
    assert_eq!(doc.element_uri(root), Some("urn:w"));
    assert!(doc.find_child(root, Some("urn:w"), "body").is_some());
    assert_eq!(doc.attr(root, Some("urn:mc"), "Ignorable").unwrap(), "w");
    // Атрибут без префикса namespace по умолчанию не наследует.
    assert!(doc.attr(root, Some("urn:w"), "Ignorable").is_none());
    // Объявления остаются обычными атрибутами.
    assert_eq!(doc.attr_count(root), 3);
}

// --- мутации --------------------------------------------------------------

#[test]
fn set_attr_keeps_the_quote_and_the_spacing_of_the_original() {
    let src = b"<a x = '1' y=\"2\"/>";
    let mut doc = parse(src);
    let root = doc.root_element().unwrap();
    doc.set_attr(root, "x", "it's \"3\"").unwrap();
    // Кавычка та же, пробелы вокруг `=` те же; экранируется только апостроф.
    assert_eq!(
        doc.serialize().unwrap(),
        b"<a x = 'it&apos;s \"3\"' y=\"2\"/>"
    );
    assert!(doc.is_dirty());
}

#[test]
fn new_attribute_is_appended_last_in_double_quotes() {
    let mut doc = parse(b"<a x=\"1\"/>");
    let root = doc.root_element().unwrap();
    doc.set_attr(root, "y", "2").unwrap();
    assert_eq!(doc.serialize().unwrap(), b"<a x=\"1\" y=\"2\"/>");
}

#[test]
fn new_content_escapes_whitespace_that_normalization_would_eat() {
    let mut doc = parse(b"<a/>");
    let root = doc.root_element().unwrap();
    doc.set_attr(root, "t", "a\tb\nc\rd").unwrap();
    let out = doc.serialize().unwrap();
    assert_eq!(out, b"<a t=\"a&#x9;b&#xA;c&#xD;d\"/>");
    // Неподвижная точка достигается с первого раза — это и есть смысл правила.
    let again = Document::parse(out.clone(), &Limits::strict()).unwrap();
    assert_eq!(again.serialize().unwrap(), out);
    assert_eq!(
        again
            .attr(again.root_element().unwrap(), None, "t")
            .unwrap(),
        "a\tb\nc\rd"
    );
}

#[test]
fn remove_attr_matches_by_namespace_not_by_spelling() {
    let src = br#"<r xmlns:w="urn:w" w:val="1" val="2"/>"#;
    let mut doc = parse(src);
    let root = doc.root_element().unwrap();
    assert!(doc.remove_attr(root, Some("urn:w"), "val").unwrap());
    assert_eq!(doc.serialize().unwrap(), br#"<r xmlns:w="urn:w" val="2"/>"#);
    assert!(!doc.remove_attr(root, Some("urn:nope"), "val").unwrap());
    assert!(doc.remove_attr(root, None, "val").unwrap());
    assert_eq!(doc.serialize().unwrap(), br#"<r xmlns:w="urn:w"/>"#);
}

#[test]
fn set_text_turns_a_self_closing_tag_into_a_pair() {
    let mut doc = parse(b"<r><a/></r>");
    let root = doc.root_element().unwrap();
    let a = doc.find_child(root, None, "a").unwrap();
    doc.set_text(a, "x<y&z").unwrap();
    assert_eq!(doc.serialize().unwrap(), b"<r><a>x&lt;y&amp;z</a></r>");
}

#[test]
fn set_text_on_a_text_node_replaces_only_it() {
    let mut doc = parse(b"<r>before<a/>after</r>");
    let root = doc.root_element().unwrap();
    let first = doc.children(root).next().unwrap();
    doc.set_text(first, "AFTER").unwrap();
    assert_eq!(doc.serialize().unwrap(), b"<r>AFTER<a/>after</r>");
}

#[test]
fn new_element_append_insert_and_remove() {
    let mut doc = parse(b"<r><a/><b/></r>");
    let root = doc.root_element().unwrap();

    let c = doc.new_element("c").unwrap();
    doc.set_attr(c, "k", "v").unwrap();
    doc.append_child(root, c).unwrap();
    assert_eq!(doc.serialize().unwrap(), b"<r><a/><b/><c k=\"v\"/></r>");

    let d = doc.new_element("w:d").unwrap();
    let b = doc.find_child(root, None, "b").unwrap();
    doc.insert_before(b, d).unwrap();
    assert_eq!(
        doc.serialize().unwrap(),
        b"<r><a/><w:d/><b/><c k=\"v\"/></r>"
    );

    doc.remove(b).unwrap();
    assert_eq!(doc.serialize().unwrap(), b"<r><a/><w:d/><c k=\"v\"/></r>");

    // Отсоединённый узел можно подключить обратно.
    doc.append_child(root, b).unwrap();
    assert_eq!(
        doc.serialize().unwrap(),
        b"<r><a/><w:d/><c k=\"v\"/><b/></r>"
    );
}

#[test]
fn clone_subtree_copies_bytes_exactly() {
    let src = b"<r><a x = '1'  ><b/>t</a  ></r>";
    let mut doc = parse(src);
    let root = doc.root_element().unwrap();
    let a = doc.find_child(root, None, "a").unwrap();
    let copy = doc.clone_subtree(a).unwrap();
    doc.append_child(root, copy).unwrap();
    // Копия чиста, поэтому пишется тем же фаст-пасом — байт в байт.
    assert_eq!(
        doc.serialize().unwrap(),
        b"<r><a x = '1'  ><b/>t</a  ><a x = '1'  ><b/>t</a  ></r>"
    );
    // Правка копии не задевает оригинал.
    doc.set_attr(copy, "x", "2").unwrap();
    assert_eq!(
        doc.serialize().unwrap(),
        b"<r><a x = '1'  ><b/>t</a  ><a x = '2'  ><b/>t</a  ></r>"
    );
}

#[test]
fn structural_edits_are_refused_when_they_would_break_the_tree() {
    let mut doc = parse(b"<r><a/></r>");
    let root = doc.root_element().unwrap();
    let a = doc.find_child(root, None, "a").unwrap();

    assert!(doc.append_child(root, a).is_err(), "узел уже подключён");
    assert!(doc.remove(doc.document_node()).is_err());
    assert!(doc.set_attr(a, "не имя!", "1").is_err());
    assert!(doc.set_attr(a, "a:b:c", "1").is_err());
    assert!(doc.new_element(":x").is_err());

    // Цикл: попытка сделать предка ребёнком потомка.
    let orphan = doc.new_element("z").unwrap();
    doc.append_child(a, orphan).unwrap();
    doc.remove(a).unwrap();
    assert!(
        doc.append_child(orphan, a).is_err(),
        "цикл обязан отвергаться"
    );
}

#[test]
fn an_untouched_document_stays_clean() {
    let doc = parse(b"<r><a/></r>");
    assert!(!doc.is_dirty());
    let mut doc = doc;
    let root = doc.root_element().unwrap();
    // Чтение не пачкает.
    let _ = doc.attr(root, None, "nope");
    let _ = doc.text(root);
    assert!(!doc.is_dirty());
    doc.set_attr(root, "k", "v").unwrap();
    assert!(doc.is_dirty());
}

#[test]
fn a_second_round_trip_after_an_edit_is_a_fixed_point() {
    let src = b"<?xml version=\"1.0\"?>\r\n<r xmlns=\"urn:d\">\r\n  <a t=\"&#34;\"/>\r\n</r>";
    let mut doc = parse(src);
    let root = doc.root_element().unwrap();
    let a = doc.find_child(root, Some("urn:d"), "a").unwrap();
    doc.set_attr(a, "t", "новое \"значение\"\r\nс переводом")
        .unwrap();
    doc.set_text(a, "текст\rс CR").unwrap();

    let once = doc.serialize().unwrap();
    let twice = Document::parse(once.clone(), &Limits::strict())
        .unwrap()
        .serialize()
        .unwrap();
    assert_eq!(once, twice, "второй round-trip разошёлся с первым");
    // И то, что мы записали, читается обратно ровно тем же.
    let re = Document::parse(once.clone(), &Limits::strict()).unwrap();
    let root = re.root_element().unwrap();
    let a = re.find_child(root, Some("urn:d"), "a").unwrap();
    assert_eq!(
        re.attr(a, None, "t").unwrap(),
        "новое \"значение\"\r\nс переводом"
    );
    assert_eq!(re.text(a).unwrap(), "текст\rс CR");
}

// --- квоты ----------------------------------------------------------------

#[test]
fn node_quota_fires() {
    let mut limits = Limits::strict();
    limits.max_nodes_per_part = 8;
    let mut src = Vec::from(&b"<r>"[..]);
    for _ in 0..50 {
        src.extend_from_slice(b"<a/>");
    }
    src.extend_from_slice(b"</r>");
    let err = Document::parse(src, &limits).unwrap_err();
    assert!(err.is_limit(), "ожидалась квота, получено {err:?}");

    // Правка тоже упирается в квоту, а не только разбор: дерево, полученное из
    // безопасного файла, нельзя раздуть через API до размеров, которые разбор
    // бы не пропустил. `<r/>` — это узел-документ плюс элемент, то есть два
    // узла из трёх разрешённых.
    let mut limits = Limits::strict();
    limits.max_nodes_per_part = 3;
    let mut doc = Document::parse(b"<r/>".to_vec(), &limits).unwrap();
    assert_eq!(doc.node_count(), 2);
    doc.new_element("z").unwrap();
    assert!(doc.new_element("z2").unwrap_err().is_limit());
}

#[test]
fn depth_quota_fires() {
    let mut src = Vec::new();
    for _ in 0..1000 {
        src.extend_from_slice(b"<a>");
    }
    for _ in 0..1000 {
        src.extend_from_slice(b"</a>");
    }
    assert!(
        Document::parse(src, &Limits::strict())
            .unwrap_err()
            .is_limit()
    );
}

#[test]
fn doctype_is_refused_by_the_dom_too() {
    assert!(Document::parse(b"<!DOCTYPE a><a/>".to_vec(), &Limits::strict()).is_err());
}

// --- фаззер ---------------------------------------------------------------

/// SplitMix64. Пятнадцать строк вместо зависимости — и полная
/// воспроизводимость: сид итерации выводится из номера, а не из часов.
struct Rng(u64);

impl Rng {
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(Self::GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

const BASE_SEED: u64 = 0x00C0_FFEE_D0D0_1234;

fn seed_for(i: u64) -> u64 {
    BASE_SEED ^ i.wrapping_mul(Rng::GAMMA)
}

struct Gen {
    rng: Rng,
    out: Vec<u8>,
    /// Глубина, до которой генератор обязан спуститься.
    target: usize,
    reached: usize,
    /// Бюджет узлов вне обязательного спуска — без него ветвление взрывается.
    budget: usize,
}

/// Пробельные вставки внутрь тега: сюда попадает всё, что корпус показал
/// живым — двойной пробел, табуляция, LF и CR внутри тега.
const WS: [&[u8]; 6] = [b"", b" ", b"  ", b"\t", b"\n", b"\r"];
/// Формы записи одного и того же символа. Каноникализации нет — значит,
/// сериализатор обязан вернуть ровно ту, что была.
const REFS: [&[u8]; 7] = [
    b"&quot;", b"&#34;", b"&#x22;", b"&amp;", b"&gt;", b"&#62;", b"&#xA;",
];
const LINE_ENDS: [&[u8]; 4] = [b"", b"\n", b"\r\n", b"\r"];
const DECLS: [&[u8]; 4] = [
    b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
    b"<?xml version=\"1.0\" ?>",
    b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>",
];
const NAMES: [&[u8]; 6] = [b"a", b"b", b"w:p", b"w:t", b"x:v", b"cell"];

impl Gen {
    fn new(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        // Цель выбирается ДО генерации: иначе спуск выродится в глубину 9.
        let target = 1 + rng.below(200) as usize;
        Self {
            rng,
            out: Vec::new(),
            target,
            reached: 0,
            budget: 200,
        }
    }

    fn document(mut self) -> (Vec<u8>, usize) {
        if self.rng.below(4) == 0 {
            self.out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        self.out.extend_from_slice(self.rng.pick(&DECLS));
        let le = *self.rng.pick(&LINE_ENDS);
        self.out.extend_from_slice(le);
        if self.rng.below(3) == 0 {
            self.out.extend_from_slice("<!-- пролог -->".as_bytes());
            self.out.extend_from_slice(self.rng.pick(&LINE_ENDS));
        }
        self.root();
        // После корня допустимы только пробелы.
        self.out.extend_from_slice(self.rng.pick(&LINE_ENDS));
        (self.out, self.reached)
    }

    /// Корень объявляет все префиксы, которыми пользуются потомки.
    fn root(&mut self) {
        self.out
            .extend_from_slice(b"<r xmlns:w=\"urn:w\" xmlns:x=\"urn:x\"");
        if self.rng.below(2) == 0 {
            self.out.extend_from_slice(b" xmlns=\"urn:d\"");
        }
        self.attrs(0);
        self.out.extend_from_slice(self.rng.pick(&WS));
        self.out.push(b'>');
        self.body(1, b"r");
    }

    fn element(&mut self, depth: usize) {
        let name = *self.rng.pick(&NAMES);
        self.out.push(b'<');
        self.out.extend_from_slice(name);
        self.attrs(depth);
        let pre = *self.rng.pick(&WS);

        let must_descend = depth < self.target;
        if !must_descend && self.rng.below(2) == 0 {
            self.out.extend_from_slice(pre);
            self.out.extend_from_slice(b"/>");
            self.reached = self.reached.max(depth);
            return;
        }
        self.out.extend_from_slice(pre);
        self.out.push(b'>');
        self.body(depth, name);
    }

    /// Внутренность элемента и его закрывающий тег.
    fn body(&mut self, depth: usize, name: &[u8]) {
        self.reached = self.reached.max(depth);
        if depth < self.target {
            self.filler(depth);
            self.element(depth + 1);
            self.filler(depth);
        } else {
            let k = self.rng.below(3);
            for _ in 0..k {
                self.child(depth + 1);
            }
        }
        self.out.extend_from_slice(b"</");
        self.out.extend_from_slice(name);
        self.out.extend_from_slice(self.rng.pick(&WS));
        self.out.push(b'>');
    }

    /// Узлы, которые не углубляют дерево.
    fn filler(&mut self, _depth: usize) {
        for _ in 0..self.rng.below(3) {
            self.leaf();
        }
    }

    fn child(&mut self, depth: usize) {
        if self.budget > 0 && self.rng.below(3) == 0 {
            self.budget -= 1;
            self.element(depth);
        } else {
            self.leaf();
        }
    }

    fn leaf(&mut self) {
        match self.rng.below(6) {
            0 => {
                self.out.extend_from_slice(b"<!-- ");
                self.out.extend_from_slice(self.rng.pick(&LINE_ENDS));
                self.out.extend_from_slice("комментарий -->".as_bytes());
            }
            1 => {
                self.out.extend_from_slice("<?tgt данные".as_bytes());
                self.out.extend_from_slice(self.rng.pick(&LINE_ENDS));
                self.out.extend_from_slice(b"?>");
            }
            2 => {
                // Внутри CDATA `<` и `&` законны и не декодируются.
                self.out.extend_from_slice(b"<![CDATA[a<b & c\r\n]]>");
            }
            _ => self.text(),
        }
    }

    fn text(&mut self) {
        let n = 1 + self.rng.below(4);
        for _ in 0..n {
            match self.rng.below(5) {
                0 => self.out.extend_from_slice(self.rng.pick(&REFS)),
                1 => self.out.extend_from_slice(self.rng.pick(&LINE_ENDS)),
                2 => self.out.extend_from_slice("текст".as_bytes()),
                3 => self.out.extend_from_slice(b"  \t "),
                _ => self.out.extend_from_slice(b"value"),
            }
        }
    }

    fn attrs(&mut self, salt: usize) {
        let n = self.rng.below(4);
        for i in 0..n {
            // Имена уникальны внутри элемента: дубликат — ошибка разбора, а не
            // интересный вход.
            self.out
                .extend_from_slice(WS[1 + self.rng.below(2) as usize]);
            let prefixed = self.rng.below(4) == 0;
            if prefixed {
                self.out.extend_from_slice(b"w:");
            }
            self.out
                .extend_from_slice(format!("k{salt}_{i}").as_bytes());
            self.out.extend_from_slice(self.rng.pick(&WS));
            self.out.push(b'=');
            self.out.extend_from_slice(self.rng.pick(&WS));
            let quote = if self.rng.below(3) == 0 { b'\'' } else { b'"' };
            self.out.push(quote);
            self.attr_value(quote);
            self.out.push(quote);
        }
    }

    fn attr_value(&mut self, quote: u8) {
        for _ in 0..self.rng.below(4) {
            match self.rng.below(6) {
                0 => self.out.extend_from_slice(self.rng.pick(&REFS)),
                // Литеральные CR/LF/TAB в значении: в корпусе их 7 и 7, и
                // после разбора их в значении уже не существует — сохранить их
                // может только спан.
                1 => self.out.extend_from_slice(b"\r"),
                2 => self.out.extend_from_slice(b"\n\t"),
                3 => self.out.extend_from_slice("знач".as_bytes()),
                // Противоположная кавычка внутри значения законна.
                4 => self.out.push(if quote == b'"' { b'\'' } else { b'"' }),
                _ => self.out.extend_from_slice(b"1"),
            }
        }
    }
}

/// Случайная правка. Все варианты обязаны оставлять документ разбираемым.
fn random_edit(doc: &mut Document, rng: &mut Rng) {
    let root = doc.root_element().unwrap();
    let mut elems = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        elems.push(n);
        for c in doc.children(n) {
            if doc.kind(c) == Some(NodeKind::Element) {
                stack.push(c);
            }
        }
    }
    let target = elems[rng.below(elems.len() as u64) as usize];

    match rng.below(6) {
        0 => {
            let _ = doc.set_attr(target, "probe", "новое \"знач\"\r\n\tи &");
        }
        1 => {
            let _ = doc.remove_attr(target, None, "k0_0");
        }
        2 => {
            let _ = doc.set_text(target, "заменённый\rтекст & <>");
        }
        3 => {
            if let Ok(e) = doc.new_element("w:new") {
                let _ = doc.set_attr(e, "a", "1");
                let _ = doc.append_child(target, e);
            }
        }
        4 => {
            // Удаляется только то, что не является корнем документа.
            let kids: Vec<_> = doc.children(target).collect();
            if let Some(&k) = kids.first() {
                let _ = doc.remove(k);
            }
        }
        _ => {
            if target != root
                && let Ok(c) = doc.clone_subtree(target)
            {
                let _ = doc.append_child(root, c);
            }
        }
    }
}

#[test]
fn fuzz_round_trip_and_idempotence() {
    let iters: u64 = std::env::var("OOXML_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let limits = Limits::strict();
    let mut max_depth = 0usize;
    let mut edited = 0usize;

    for i in 0..iters {
        let seed = seed_for(i);
        let (src, depth) = Gen::new(seed).document();
        max_depth = max_depth.max(depth);

        // Свойство 1: побайтовый round-trip.
        let doc = Document::parse(src.clone(), &limits).unwrap_or_else(|e| {
            panic!(
                "итерация {i} (сид {seed:#x}): разбор провалился: {e}\n{:?}",
                String::from_utf8_lossy(&src)
            )
        });
        if let Err(b) = doc.check_coverage() {
            panic!("итерация {i} (сид {seed:#x}): покрытие: {b:?}");
        }
        let back = doc.serialize().unwrap();
        if back != src {
            let at = back
                .iter()
                .zip(src.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(src.len().min(back.len()));
            panic!(
                "итерация {i} (сид {seed:#x}): расхождение на байте {at}\n  вход:  {:?}\n  выход: {:?}",
                String::from_utf8_lossy(&src),
                String::from_utf8_lossy(&back)
            );
        }

        // Свойство 2: после случайной правки второй проход — неподвижная точка.
        let mut doc = Document::parse(src.clone(), &limits).unwrap();
        let mut rng = Rng::new(seed ^ 0x5555_5555_5555_5555);
        random_edit(&mut doc, &mut rng);
        let once = doc
            .serialize()
            .unwrap_or_else(|e| panic!("итерация {i} (сид {seed:#x}): запись после правки: {e}"));
        let reparsed = Document::parse(once.clone(), &limits).unwrap_or_else(|e| {
            panic!(
                "итерация {i} (сид {seed:#x}): правленый документ не разбирается: {e}\n{:?}",
                String::from_utf8_lossy(&once)
            )
        });
        let twice = reparsed.serialize().unwrap();
        assert_eq!(
            once, twice,
            "итерация {i} (сид {seed:#x}): второй проход разошёлся с первым"
        );
        if once != src {
            edited += 1;
        }
    }

    eprintln!(
        "фаззер: {iters} итераций, правка изменила байты в {edited}, \
         максимальная достигнутая глубина {max_depth}"
    );
    // Правка, которая ничего не меняет, не проверяет ничего.
    assert!(
        edited * 2 > iters as usize,
        "правки почти не срабатывали: {edited}"
    );
    // Сторож из вехи M5: без него зелёный тест доказывал бы куда меньше.
    assert!(
        max_depth >= 150,
        "генератор выродился: максимальная глубина всего {max_depth}"
    );
}
