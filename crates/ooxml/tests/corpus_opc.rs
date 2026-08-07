//! Гейт вехи M7: `Package::open(f).save() == f` побайтово на всём корпусе.
//!
//! Слой OPC — последний перед типизированной моделью, и он единственный решает,
//! какую запись переписать, а какую скопировать. Ошибка этого решения не видна
//! ни в одном тесте нижних слоёв: и `zip`, и `dom` по отдельности байтово
//! точны, а пакет всё равно может распухнуть, если решит перепаковать часть,
//! которую никто не трогал.
//!
//! Поэтому здесь три теста, а не один:
//!
//! 1. пакет, который не трогали, сохраняется в исходный файл;
//! 2. пакет, который **трогали вхолостую**, сохраняется в исходный файл;
//! 3. пакет, который правда изменили, задевает ровно одну запись.
//!
//! Третий — прямое предусловие вехи M9 (правка ячейки).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::Limits;
use ooxml::dom::NodeKind;
use ooxml::opc::{Package, PartName};
use ooxml::zip::ZipArchive;
use std::path::PathBuf;

fn corpus_files() -> Vec<PathBuf> {
    let root = std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    );
    let mut out = Vec::new();
    for sub in ["xlsx", "docx"] {
        let Ok(entries) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "xlsx" || x == "docx") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn name_of(p: &std::path::Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

/// Первый расходящийся байт и запись, которой он принадлежит.
///
/// Без этого отладка расхождения — это чтение двух дампов по 300 КБ глазами.
fn explain(orig: &[u8], out: &[u8]) -> String {
    let at = orig
        .iter()
        .zip(out.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| orig.len().min(out.len()));
    let limits = Limits::permissive();
    let who = ZipArchive::parse(orig, &limits)
        .ok()
        .and_then(|z| {
            (0..z.len()).find_map(|i| {
                let e = z.entry(i).ok()?;
                let v = e.verbatim();
                (v.start() as usize <= at && at < v.end() as usize)
                    .then(|| z.name_str(i).ok().map(|n| n.into_owned()))?
            })
        })
        .unwrap_or_else(|| "<вне записей: заголовок, каталог или EOCD>".to_owned());
    format!(
        "первое расхождение на байте {at} (было {} байт, стало {}); \
         этот байт принадлежит записи {who}",
        orig.len(),
        out.len()
    )
}

/// Значение атрибута, которое переживает круг «раскодировать → заэкранировать»
/// без изменения байт.
///
/// Нужно тестам холостой правки: подставить обратно декодированное `&#34;`
/// значило бы получить `&quot;` — правку настоящую, а не холостую.
fn escape_stable(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| (0x20..0x7F).contains(&b) && !b"&<>\"'".contains(&b))
}

/// Ищет в части атрибут, пригодный для холостой правки: (узел, имя, значение).
///
/// Берутся только атрибуты **без префикса**: для них `attr_raw(_, None, name)`
/// достаёт сырое, ещё не раскодированное значение, а сравнивать надо именно
/// сырое. Значение вида `&#82;` раскодировалось бы в `R`, и подстановка `R`
/// обратно была бы уже настоящей правкой, а не холостой.
fn victim_attr(doc: &ooxml::dom::Document) -> Option<(ooxml::dom::NodeId, String, String)> {
    let root = doc.root_element().ok()?;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let eprefix = doc
            .qname(n)
            .map(|q| String::from_utf8_lossy(q).into_owned())
            .and_then(|q| q.split(':').next().map(str::to_owned).filter(|p| *p != q))
            .unwrap_or_default();
        for i in 0..doc.attr_count(n) {
            let Some(raw_name) = doc.attr_name_at(n, i) else {
                continue;
            };
            let name = String::from_utf8_lossy(raw_name).into_owned();
            // Объявления namespace не трогаем: у дерева они уже разрешены, и
            // правка объявления рассогласовала бы дерево с байтами.
            if name.starts_with("xmlns") {
                continue;
            }
            // В `.docx` почти каждый атрибут с префиксом (`w:val`), а
            // `attr_raw` спрашивает URI, а не префикс. URI берётся у самого
            // элемента — это верно, когда префиксы совпадают, и этого хватает.
            let (ns, local) = match name.split_once(':') {
                None => (None, name.clone()),
                Some((p, l)) if p == eprefix => match doc.element_uri(n) {
                    Some(u) => (Some(u.to_owned()), l.to_owned()),
                    None => continue,
                },
                Some(_) => continue,
            };
            let Some(raw) = doc.attr_raw(n, ns.as_deref(), &local) else {
                continue;
            };
            let value = String::from_utf8_lossy(raw).into_owned();
            if escape_stable(&value) {
                return Some((n, name, value));
            }
        }
        stack.extend(
            doc.children(n)
                .filter(|&c| doc.kind(c) == Some(NodeKind::Element)),
        );
    }
    None
}

#[test]
fn package_round_trip_is_byte_identical() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен (задайте OOXML_CORPUS)");
        return;
    }

    let mut ok = 0u32;
    let mut peak = (0usize, String::new(), 0usize);
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg_name = name_of(path);
        let mut pkg = Package::open(&data, Limits::strict())
            .unwrap_or_else(|e| panic!("{pkg_name}: открытие: {e}"));
        let mem = pkg.memory_bytes();
        if data.len() > peak.2 {
            peak = (mem, pkg_name.clone(), data.len());
        }
        let out = pkg
            .save()
            .unwrap_or_else(|e| panic!("{pkg_name}: сохранение: {e}"));
        assert!(
            out == data,
            "{pkg_name}: пакет не восстановлен побайтово; {}",
            explain(&data, &out)
        );
        ok += 1;
    }
    println!("побайтовое восстановление пакета: {ok}/{}", files.len());
    println!(
        "память пакета сразу после open() на самом большом файле ({}, {} байт): {} байт",
        peak.1, peak.2, peak.0
    );
    assert_eq!(ok as usize, files.len());
}

#[test]
fn open_is_lazy_and_stays_cheap() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    // Пакет с самой большой частью корпуса: 1,6 МБ и ~70 тыс. узлов. Если
    // открытие когда-нибудь начнёт разбирать всё подряд, сломается именно тут.
    let mut worst: Option<(PathBuf, PartName, u64)> = None;
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg = Package::open(&data, Limits::strict()).unwrap();
        for name in pkg.part_names() {
            let Some(i) = pkg.zip().index_of(name.zip_name()) else {
                continue;
            };
            let size = pkg.zip().entries()[i].uncomp_size;
            if worst.as_ref().is_none_or(|w| size > w.2) {
                worst = Some((path.clone(), name.clone(), size));
            }
        }
    }
    let (path, part, size) = worst.unwrap();
    let data = std::fs::read(&path).unwrap();
    let pkg_name = name_of(&path);

    let mut pkg = Package::open(&data, Limits::strict()).unwrap();
    let after_open = pkg.memory_bytes();
    let _ = pkg.bytes(&part).unwrap();
    let after_bytes = pkg.memory_bytes();
    let _ = pkg.dom(&part).unwrap();
    let after_dom = pkg.memory_bytes();

    println!("самая большая часть корпуса: {pkg_name}!{part} ({size} байт)");
    println!("  память пакета после open(): {after_open}");
    println!("  после bytes(): {after_bytes}");
    println!("  после dom():   {after_dom}");

    assert!(
        (after_open as u64) < size / 10,
        "открытие не должно стоить как разбор: {after_open} против части в {size} байт"
    );
    // `bytes` распаковывает, но дерева не строит — иначе разница исчезла бы.
    assert!(
        after_dom > after_bytes.saturating_mul(3),
        "дерево обязано быть заметно дороже байтов: {after_bytes} против {after_dom}"
    );
    // Сохранение после одного лишь чтения по-прежнему побайтовое.
    assert_eq!(pkg.save().unwrap(), data);
}

#[test]
fn no_op_edit_does_not_repack_anything() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let mut touched = 0u32;
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg_name = name_of(path);
        let mut pkg = Package::open(&data, Limits::strict()).unwrap();

        // Несколько частей на пакет: правка одной могла бы «повезти», правка
        // пяти в пяти разных частях — уже нет.
        let names: Vec<PartName> = pkg
            .part_names()
            .filter(|p| {
                matches!(p.extension(), Some(e) if e.eq_ignore_ascii_case("xml") || e.eq_ignore_ascii_case("rels"))
            })
            .take(5)
            .cloned()
            .collect();

        let mut here = 0u32;
        for name in &names {
            let Ok(doc) = pkg.dom(name) else { continue };
            let Some((node, attr, value)) = victim_attr(doc) else {
                continue;
            };
            // Туда и обратно: сначала настоящая правка, потом возврат.
            doc.set_attr(node, &attr, "___rustoword_probe___").unwrap();
            doc.set_attr(node, &attr, &value).unwrap();
            assert!(
                doc.is_dirty(),
                "{pkg_name}!{name}: документ обязан считать себя грязным — \
                 фолбэк должен ловить именно этот случай, а не отсутствие правки"
            );
            here += 1;
            touched += 1;
        }
        assert!(here > 0, "{pkg_name}: не нашлось ни одной части для правки");

        let out = pkg.save().unwrap();
        assert!(
            out == data,
            "{pkg_name}: холостая правка {here} частей вызвала перепаковку; {}",
            explain(&data, &out)
        );
    }
    println!("частей, правленных вхолостую: {touched}");
}

#[test]
fn real_edit_disturbs_only_its_own_entry() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let limits = Limits::strict();
    let mut checked = 0u32;
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg_name = name_of(path);
        let mut pkg = Package::open(&data, limits.clone()).unwrap();

        let candidates: Vec<PartName> = pkg
            .part_names()
            .filter(|p| matches!(p.extension(), Some(e) if e.eq_ignore_ascii_case("xml")))
            .cloned()
            .collect();

        // Первая часть, где нашёлся атрибут, который можно тронуть.
        let mut edited: Option<String> = None;
        for name in &candidates {
            let Ok(doc) = pkg.dom(name) else { continue };
            let Some((node, attr, value)) = victim_attr(doc) else {
                continue;
            };
            let mut new = value.clone();
            new.push('Z');
            doc.set_attr(node, &attr, &new).unwrap();
            edited = Some(name.zip_name().to_owned());
            break;
        }
        let edited = edited.unwrap_or_else(|| panic!("{pkg_name}: нечего править"));

        let out = pkg.save().unwrap();
        assert_ne!(out, data, "{pkg_name}: настоящая правка ничего не изменила");

        let before = ZipArchive::parse(&data, &limits).unwrap();
        let after = ZipArchive::parse(&out, &limits).unwrap();
        assert_eq!(
            before.len(),
            after.len(),
            "{pkg_name}: изменилось число записей"
        );
        for i in 0..before.len() {
            let n_before = before.name_str(i).unwrap().into_owned();
            let n_after = after.name_str(i).unwrap().into_owned();
            assert_eq!(
                n_before, n_after,
                "{pkg_name}: порядок записей изменился на позиции {i}"
            );
            if n_before == edited {
                continue;
            }
            assert_eq!(
                before.raw_data(i).unwrap(),
                after.raw_data(i).unwrap(),
                "{pkg_name}: правка {edited} задела сжатые данные записи {n_before}"
            );
        }
        checked += 1;
    }
    println!("пакетов, где правка задела ровно свою запись: {checked}");
    assert_eq!(checked as usize, files.len());
}

#[test]
fn every_part_has_a_content_type() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let (mut parts, mut xml_parts) = (0u32, 0u32);
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg_name = name_of(path);
        let pkg = Package::open(&data, Limits::strict()).unwrap();
        for name in pkg.part_names() {
            let ct = pkg
                .content_type(name)
                .unwrap_or_else(|| panic!("{pkg_name}!{name}: тип части не определился"));
            assert!(!ct.is_empty(), "{pkg_name}!{name}: пустой тип");
            parts += 1;
            if ct.ends_with("+xml") || ct == "application/xml" {
                xml_parts += 1;
            }
        }
    }
    println!("частей с определённым типом: {parts}, из них XML: {xml_parts}");
    assert!(parts > 500, "ожидалось ~570 частей, получено {parts}");
}

#[test]
fn relationship_graph_points_at_existing_parts() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }

    let (mut rels_parts, mut links, mut external) = (0u32, 0u32, 0u32);
    let mut dangling: Vec<String> = Vec::new();
    for path in &files {
        let data = std::fs::read(path).unwrap();
        let pkg_name = name_of(path);
        let mut pkg = Package::open(&data, Limits::strict()).unwrap();

        let owners: Vec<PartName> = pkg
            .part_names()
            .filter_map(PartName::rels_owner)
            .collect::<Vec<_>>();

        for owner in &owners {
            // Владельцем может быть корень пакета — его в part_names нет.
            if !owner.is_root() {
                assert!(
                    pkg.has(owner),
                    "{pkg_name}: у отношений {owner}.rels нет части-владельца"
                );
            }
            // Цели снимаются заранее: `rels()` держит пакет занятым.
            let targets: Vec<(String, Option<PartName>)> = {
                let r = pkg
                    .rels(owner)
                    .unwrap_or_else(|e| panic!("{pkg_name}!{owner}: разбор отношений: {e}"));
                r.iter()
                    .map(|rel| (rel.id.clone(), r.resolve(rel).ok().flatten()))
                    .collect()
            };
            rels_parts += 1;
            for (id, target) in targets {
                links += 1;
                let Some(t) = target else {
                    external += 1;
                    continue;
                };
                if !pkg.has(&t) {
                    dangling.push(format!("{pkg_name}!{owner} {id} -> {t}"));
                }
            }
        }
    }
    println!("файлов отношений: {rels_parts}, отношений: {links}, внешних: {external}");
    for d in &dangling {
        println!("  висячая цель: {d}");
    }
    assert!(
        rels_parts > 100,
        "ожидалось ~125 файлов, получено {rels_parts}"
    );

    // Висячие цели — свойство корпуса, а не наше. `40250030_01.docx` собран
    // так, что `Target="word/settings.xml"` записан относительно **пакета**,
    // хотя лежит в `/word/_rels/` и по спецификации отсчитывается от `/word/`.
    // Word такой файл открывает, значит, отказ здесь был бы строже реальности:
    // тест фиксирует измеренное число, чтобы новая висячая цель не проехала
    // молча.
    assert_eq!(
        dangling.len(),
        EXPECTED_DANGLING,
        "изменилось число висячих целей: {dangling:?}"
    );
}

/// Сколько внутренних целей корпуса не указывают ни на одну часть.
const EXPECTED_DANGLING: usize = 1;
