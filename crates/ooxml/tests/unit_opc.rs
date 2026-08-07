//! Слой OPC на сконструированных случаях.
//!
//! Корпус проверяет, что мы не ломаем реальные файлы. Здесь проверяется то,
//! чего в корпусе нет вовсе: цели с `%20` и фрагментом, внешние отношения,
//! коллизия имён по регистру, `Override`, спорящий с `Default`. Всё это
//! встречается в живых пакетах за пределами корпуса, и обнаружить ошибку в
//! таком месте хочется здесь, а не в чужом файле.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::deflate::Level;
use ooxml::error::{Error, OpcError};
use ooxml::opc::{ContentTypes, Package, PartName, TargetMode};
use ooxml::zip::{EntrySource, WriteOptions, ZipWriter};

// --- имена частей ---------------------------------------------------------

#[test]
fn part_name_keeps_spelling_and_compares_without_case() {
    let a = PartName::new("/xl/Worksheets/Sheet1.xml").unwrap();
    let b = PartName::new("/XL/worksheets/sheet1.XML").unwrap();
    assert_eq!(a, b, "имена частей сравниваются без учёта регистра");
    assert_eq!(
        a.as_str(),
        "/xl/Worksheets/Sheet1.xml",
        "написание сохранено"
    );
    assert_eq!(b.as_str(), "/XL/worksheets/sheet1.XML");
    assert_eq!(a.key(), b.key());
    assert_eq!(a.zip_name(), "xl/Worksheets/Sheet1.xml");
}

#[test]
fn part_name_rejects_what_is_not_a_part_name() {
    for bad in [
        "xl/workbook.xml",   // без ведущего слэша
        "/xl/",              // кончается слэшем
        "/xl//workbook.xml", // пустой сегмент
        "/xl/./workbook.xml",
        "/xl/../workbook.xml",
        "/xl/workbook.",     // сегмент кончается точкой
        "/xl\\workbook.xml", // обратный слэш
        "",
    ] {
        let e = PartName::new(bad).unwrap_err();
        assert!(
            matches!(e, Error::Opc(OpcError::BadPartName(_))),
            "{bad:?} должно быть отвергнуто, получено {e}"
        );
    }
}

#[test]
fn extension_is_taken_from_the_name_not_from_a_path_type() {
    // `Path::new("_rels/.rels").extension()` здесь возвращает None, и по одной
    // части на пакет молча теряла бы тип.
    assert_eq!(
        PartName::new("/_rels/.rels").unwrap().extension(),
        Some("rels")
    );
    assert_eq!(
        PartName::new("/xl/workbook.xml").unwrap().extension(),
        Some("xml")
    );
    assert_eq!(
        PartName::new("/xl/printerSettings/printerSettings1.bin")
            .unwrap()
            .extension(),
        Some("bin")
    );
    assert_eq!(PartName::new("/word/media/logo").unwrap().extension(), None);
}

#[test]
fn rels_part_for_root_and_for_a_nested_part() {
    assert_eq!(
        PartName::root().rels_part().unwrap().as_str(),
        "/_rels/.rels"
    );
    assert_eq!(
        PartName::new("/xl/workbook.xml")
            .unwrap()
            .rels_part()
            .unwrap()
            .as_str(),
        "/xl/_rels/workbook.xml.rels"
    );
    assert_eq!(
        PartName::new("/xl/worksheets/sheet1.xml")
            .unwrap()
            .rels_part()
            .unwrap()
            .as_str(),
        "/xl/worksheets/_rels/sheet1.xml.rels"
    );
    // Обратная дорога.
    let rp = PartName::new("/xl/worksheets/_rels/sheet1.xml.rels").unwrap();
    assert!(rp.is_rels_part());
    assert_eq!(
        rp.rels_owner().unwrap().as_str(),
        "/xl/worksheets/sheet1.xml"
    );
    assert!(
        PartName::new("/_rels/.rels")
            .unwrap()
            .rels_owner()
            .unwrap()
            .is_root()
    );
    // Отношений у отношений не бывает.
    assert!(rp.rels_part().is_err());
}

#[test]
fn relative_targets_resolve_against_the_owners_directory() {
    let doc = PartName::new("/word/document.xml").unwrap();
    assert_eq!(
        doc.resolve("../media/image1.png").unwrap().as_str(),
        "/media/image1.png"
    );
    assert_eq!(
        doc.resolve("media/image1.png").unwrap().as_str(),
        "/word/media/image1.png"
    );
    assert_eq!(
        doc.resolve("styles.xml").unwrap().as_str(),
        "/word/styles.xml"
    );
    assert_eq!(
        doc.resolve("./theme/theme1.xml").unwrap().as_str(),
        "/word/theme/theme1.xml"
    );
    // Абсолютная цель базу игнорирует — так записаны 6 отношений корпуса.
    assert_eq!(
        doc.resolve("/xl/sharedStrings.xml").unwrap().as_str(),
        "/xl/sharedStrings.xml"
    );
    // Отношения уровня пакета отсчитываются от корня.
    assert_eq!(
        PartName::root()
            .resolve("docProps/app.xml")
            .unwrap()
            .as_str(),
        "/docProps/app.xml"
    );
    // Выше корня выхода нет.
    assert!(doc.resolve("../../etc/passwd").is_err());
}

#[test]
fn targets_are_iri_references_not_plain_strings() {
    let doc = PartName::new("/word/document.xml").unwrap();
    // Фрагмент адресует место внутри цели, а не другую часть.
    assert_eq!(
        doc.resolve("footnotes.xml#note3").unwrap().as_str(),
        "/word/footnotes.xml"
    );
    // Имя части в архиве записано сырыми байтами, в отношении — закодированным.
    assert_eq!(
        doc.resolve("media/my%20image.png").unwrap().as_str(),
        "/word/media/my image.png"
    );
    assert_eq!(
        doc.resolve("media/%D1%84.png").unwrap().as_str(),
        "/word/media/ф.png"
    );
    // Одиночный процент — байт имени, а не начало escape.
    assert_eq!(
        doc.resolve("media/100%.png").unwrap().as_str(),
        "/word/media/100%.png"
    );
}

// --- сборка пакета для тестов --------------------------------------------

const CT_XML: &str = "application/xml";
const CT_SHEET: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
const CT_RELS: &str = "application/vnd.openxmlformats-package.relationships+xml";
const REL_SHEET: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";

const CONTENT_TYPES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="XML" ContentType="application/xml"/><Default Extension="bin" ContentType="application/octet-stream"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;

const ROOT_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/a.xml" TargetMode="External"/></Relationships>"#;

const WORKBOOK: &[u8] = br#"<?xml version="1.0"?><workbook xmlns="urn:wb"><sheets><sheet name="A" id="1"/></sheets></workbook>"#;
const SHEET1: &[u8] = br#"<?xml version="1.0"?><worksheet xmlns="urn:ws"><sheetData/></worksheet>"#;

fn build(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = ZipWriter::new(WriteOptions::default());
    for (name, data) in parts {
        w.push(EntrySource::New {
            name: (*name).to_owned(),
            data: (*data).to_vec(),
            // Store, а не deflate: тесту нужен предсказуемый контейнер, а не
            // проверка упаковщика — её делают гейты M3 и M4.
            level: Level::Store,
            dos: (0, 0x0021),
            external_attrs: 0,
        })
        .unwrap();
    }
    w.finish().unwrap()
}

fn sample() -> Vec<u8> {
    build(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/worksheets/sheet1.xml", SHEET1),
        ("xl/printerSettings/printerSettings1.bin", b"\x00\x01\x02"),
        ("xl/media/", b""),
    ])
}

fn pn(s: &str) -> PartName {
    PartName::new(s).unwrap()
}

// --- типы частей ----------------------------------------------------------

#[test]
fn override_wins_over_default_and_extensions_ignore_case() {
    let data = sample();
    let pkg = Package::open(&data, Limits::strict()).unwrap();

    // `Default Extension="XML"` записан в верхнем регистре, часть — в нижнем.
    assert_eq!(pkg.content_type(&pn("/xl/workbook.xml")), Some(CT_XML));
    // Та же часть по `Default` была бы `application/xml`; Override сильнее.
    assert_eq!(
        pkg.content_type(&pn("/xl/worksheets/sheet1.xml")),
        Some(CT_SHEET)
    );
    // Имя части в Override тоже сравнивается без учёта регистра.
    assert_eq!(
        pkg.content_type(&pn("/XL/Worksheets/Sheet1.XML")),
        Some(CT_SHEET)
    );
    assert_eq!(pkg.content_type(&pn("/_rels/.rels")), Some(CT_RELS));
    assert_eq!(pkg.content_type(&pn("/nope.xml")), None);
    assert_eq!(pkg.content_types().default_for("xml"), Some(CT_XML));
}

#[test]
fn directories_and_the_types_stream_are_not_parts() {
    let data = sample();
    let pkg = Package::open(&data, Limits::strict()).unwrap();
    let names: Vec<String> = pkg.part_names().map(|p| p.as_str().to_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("Content_Types")));
    assert!(!names.iter().any(|n| n.ends_with('/')));
    assert_eq!(names.len(), 4, "{names:?}");
}

#[test]
fn duplicate_part_names_differing_only_in_case_are_rejected() {
    let data = build(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/Workbook.xml", WORKBOOK),
    ]);
    let e = Package::open(&data, Limits::strict()).unwrap_err();
    assert!(
        matches!(e, Error::Opc(OpcError::PartExists(_))),
        "получено {e}"
    );
}

#[test]
fn a_package_without_content_types_is_not_a_package() {
    let data = build(&[("xl/workbook.xml", WORKBOOK)]);
    let e = Package::open(&data, Limits::strict()).unwrap_err();
    assert!(matches!(e, Error::Opc(OpcError::NoContentTypes)), "{e}");
}

// --- чтение частей --------------------------------------------------------

#[test]
fn bytes_work_on_binary_parts_and_dom_refuses_them() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let bin = pn("/xl/printerSettings/printerSettings1.bin");

    assert_eq!(pkg.bytes(&bin).unwrap(), b"\x00\x01\x02");
    let e = pkg.dom(&bin).unwrap_err();
    assert!(
        e.to_string().contains("не является XML"),
        "ошибка должна называть причину, получено: {e}"
    );
    assert!(
        e.to_string().contains("printerSettings1.bin"),
        "ошибка должна называть часть, получено: {e}"
    );
}

#[test]
fn missing_parts_are_reported_by_name() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let ghost = pn("/xl/worksheets/sheet9.xml");
    assert!(!pkg.has(&ghost));
    for e in [
        pkg.bytes(&ghost).unwrap_err(),
        pkg.dom(&ghost).unwrap_err(),
        pkg.rels(&ghost).unwrap_err(),
    ] {
        assert!(
            matches!(e, Error::Opc(OpcError::PartNotFound(ref n)) if n.contains("sheet9")),
            "получено {e}"
        );
    }
}

// --- отношения ------------------------------------------------------------

#[test]
fn external_targets_are_never_resolved_to_part_names() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let rels = pkg.rels(&PartName::root()).unwrap();

    let internal = rels.by_id("rId1").unwrap();
    assert_eq!(internal.mode, TargetMode::Internal);
    assert_eq!(
        rels.resolve(internal).unwrap().unwrap().as_str(),
        "/xl/workbook.xml"
    );

    let external = rels.by_id("rId9").unwrap();
    assert_eq!(external.mode, TargetMode::External);
    assert_eq!(external.target, "https://example.com/a.xml");
    assert!(
        rels.resolve(external).unwrap().is_none(),
        "внешняя цель не является частью и резолвиться не должна"
    );
    assert!(rels.by_id("rId404").is_none());
    assert!(matches!(
        pkg.rels(&PartName::root())
            .unwrap()
            .target_of("rId404")
            .unwrap_err(),
        Error::Opc(OpcError::RelationshipNotFound(_))
    ));
}

// --- правка пакета --------------------------------------------------------

#[test]
fn add_part_writes_an_override_and_a_relationship() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let sheet2 = pn("/xl/worksheets/sheet2.xml");

    let before = pkg.content_types().override_count();
    pkg.add_part(sheet2.clone(), SHEET1.to_vec(), CT_SHEET)
        .unwrap();
    let id = pkg
        .add_relationship(
            &pn("/xl/workbook.xml"),
            REL_SHEET,
            "worksheets/sheet2.xml",
            TargetMode::Internal,
        )
        .unwrap();

    assert!(pkg.has(&sheet2));
    assert_eq!(pkg.content_type(&sheet2), Some(CT_SHEET));
    assert_eq!(
        pkg.content_types().override_count(),
        before + 1,
        "тип листа не выводится ни из одного Default — нужен Override"
    );

    // Пересобираем и открываем заново: правка обязана пережить сохранение.
    let out = pkg.save().unwrap();
    let mut back = Package::open(&out, Limits::strict()).unwrap();
    assert_eq!(back.content_type(&sheet2), Some(CT_SHEET));
    assert_eq!(back.bytes(&sheet2).unwrap(), SHEET1);
    // Файл отношений книги создан с нуля — его в исходном пакете не было.
    let rels = back.rels(&pn("/xl/workbook.xml")).unwrap();
    let rel = rels.by_id(&id).unwrap();
    assert_eq!(rel.rel_type, REL_SHEET);
    assert_eq!(
        rels.resolve(rel).unwrap().unwrap().as_str(),
        "/xl/worksheets/sheet2.xml"
    );
}

#[test]
fn adding_a_part_that_already_exists_is_an_error() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let e = pkg
        .add_part(pn("/xl/workbook.xml"), WORKBOOK.to_vec(), CT_XML)
        .unwrap_err();
    assert!(
        matches!(e, Error::Opc(OpcError::PartExists(ref n)) if n.contains("workbook")),
        "получено {e}"
    );
}

#[test]
fn add_part_does_not_touch_the_types_stream_when_a_default_already_fits() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let before = pkg.content_types().override_count();
    // `Default Extension="XML"` уже даёт `application/xml`. Лишний Override
    // испачкал бы `[Content_Types].xml`, а с ним и его запись в архиве.
    pkg.add_part(pn("/xl/extra.xml"), WORKBOOK.to_vec(), CT_XML)
        .unwrap();
    assert_eq!(pkg.content_types().override_count(), before);

    let out = pkg.save().unwrap();
    let orig = Package::open(&data, Limits::strict()).unwrap();
    let now = Package::open(&out, Limits::strict()).unwrap();
    assert_eq!(
        orig.content_types().override_count(),
        now.content_types().override_count()
    );
}

#[test]
fn remove_part_drops_its_override_its_rels_and_the_links_to_it() {
    let data = build(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
        ("xl/worksheets/sheet1.xml", SHEET1),
        ("xl/worksheets/_rels/sheet1.xml.rels", EMPTY_SHEET_RELS),
    ]);
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let sheet = pn("/xl/worksheets/sheet1.xml");
    let sheet_rels = pn("/xl/worksheets/_rels/sheet1.xml.rels");
    let before = pkg.content_types().override_count();
    assert_eq!(
        pkg.rels(&pn("/xl/workbook.xml")).unwrap().len(),
        1,
        "исходно книга ссылается на лист"
    );

    pkg.remove_part(&sheet).unwrap();

    assert!(!pkg.has(&sheet));
    assert!(!pkg.has(&sheet_rels), "отношения удалённой части не нужны");
    assert_eq!(pkg.content_types().override_count(), before - 1);
    assert_eq!(pkg.content_type(&sheet), None);
    assert_eq!(
        pkg.rels(&pn("/xl/workbook.xml")).unwrap().len(),
        0,
        "ссылка на удалённую часть делает пакет невалидным"
    );

    let out = pkg.save().unwrap();
    let mut back = Package::open(&out, Limits::strict()).unwrap();
    assert!(!back.has(&sheet));
    assert!(!back.has(&sheet_rels));
    assert_eq!(back.rels(&pn("/xl/workbook.xml")).unwrap().len(), 0);
    assert!(
        back.rels(&PartName::root())
            .unwrap()
            .by_id("rId1")
            .is_some()
    );

    let e = back.remove_part(&sheet).unwrap_err();
    assert!(matches!(e, Error::Opc(OpcError::PartNotFound(_))), "{e}");
}

const WORKBOOK_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;

const EMPTY_SHEET_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>"#;

// --- сохранность ----------------------------------------------------------

#[test]
fn untouched_and_read_only_packages_save_to_the_same_bytes() {
    let data = sample();

    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    assert_eq!(pkg.save().unwrap(), data, "пакет, который не трогали");

    // Чтение — не правка: ни `bytes`, ни `dom` не дают права перепаковать.
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    for name in pkg
        .part_names()
        .map(|p| p.as_str().to_owned())
        .collect::<Vec<_>>()
    {
        let p = pn(&name);
        let _ = pkg.bytes(&p).unwrap();
        let _ = pkg.dom(&p);
    }
    assert_eq!(pkg.save().unwrap(), data, "пакет, который только читали");
}

#[test]
fn an_edit_undone_costs_nothing() {
    let data = sample();
    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let wb = pn("/xl/workbook.xml");
    {
        let doc = pkg.dom(&wb).unwrap();
        let root = doc.root_element().unwrap();
        let sheets = doc.find_child(root, Some("urn:wb"), "sheets").unwrap();
        let sheet = doc.find_child(sheets, Some("urn:wb"), "sheet").unwrap();
        doc.set_attr(sheet, "name", "B").unwrap();
        doc.set_attr(sheet, "name", "A").unwrap();
        assert!(doc.is_dirty());
    }
    assert_eq!(
        pkg.save().unwrap(),
        data,
        "правка, вернувшая документ к исходному виду, не должна ничего перепаковывать"
    );
}

#[test]
fn content_types_parse_rejects_a_foreign_root() {
    let doc = ooxml::dom::Document::parse(
        br#"<Relationships xmlns="urn:x"/>"#.to_vec(),
        &Limits::strict(),
    )
    .unwrap();
    assert!(matches!(
        ContentTypes::parse(&doc).unwrap_err(),
        Error::Opc(OpcError::NoContentTypes)
    ));
}
