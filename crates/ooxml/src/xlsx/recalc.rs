//! Пометка «пересчитать всё при загрузке» и снос устаревшей цепочки вычислений.
//!
//! # Зачем это нужно при ЛЮБОЙ правке
//!
//! Значения формул лежат в файле дважды: как текст формулы в `<f>` и как
//! посчитанный кэш в `<v>`. Мы изменили одну ячейку — все формулы, которые на
//! неё ссылались, стали неверны, а их кэш остался прежним. Excel показывает
//! именно кэш и пересчитывать по своей инициативе не обязан: у книги может
//! стоять `calcMode="manual"`, и в корпусе он стоит у одиннадцати книг из
//! двадцати.
//!
//! Поэтому при каждой правке выставляется `fullCalcOnLoad="1"` и обнуляется
//! `calcId`. Обнулённый `calcId` означает «файл записан вычислителем неизвестной
//! версии», и одного этого Excel'ю обычно достаточно; вместе с `fullCalcOnLoad`
//! это надёжно.
//!
//! # `calcChain.xml` — самая частая причина «нечитаемого содержимого»
//!
//! Цепочка вычислений — это список ячеек в порядке, в котором Excel их считал
//! в прошлый раз. Она **производная**: её можно выбросить целиком, и Excel
//! построит новую. Но она обязана быть согласована с листами, и запись в ней на
//! ячейку, которая больше не формула, — ровно тот случай, когда Excel сообщает
//! о нечитаемом содержимом и «восстанавливает» книгу, теряя при этом всё, чего
//! не понял.
//!
//! Выбрасывать её надо целиком и в трёх местах сразу:
//!
//! 1. сама часть `xl/calcChain.xml`;
//! 2. её `Override` в `[Content_Types].xml`;
//! 3. её `<Relationship>` в `xl/_rels/workbook.xml.rels`.
//!
//! Забыть любое из трёх — значит получить пакет, ссылающийся на несуществующую
//! часть, и это хуже, чем не трогать ничего. [`Package::remove_part`] делает все
//! три шага, и именно поэтому здесь нет ручного разбора `.rels`: соблазн
//! «упростить» до одного удаления записи архива приводит к файлу, который Excel
//! откроет с предупреждением.

use crate::dom::{Document, NodeId};
use crate::error::Result;
use crate::opc::{Package, PartName};
use crate::xlsx::worksheet::{find_child_named, first_child_in, prefixed};

/// Тип отношения «цепочка вычислений».
const CALC_CHAIN_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/calcChain";

/// Соглашение об имени части — запасной путь, если отношения нет.
const CALC_CHAIN_PART: &str = "/xl/calcChain.xml";

/// Элементы `<workbook>`, которые по схеме идут ПОСЛЕ `<calcPr>`.
///
/// Порядок детей `<workbook>` схемой зафиксирован (CT_Workbook — это
/// `xsd:sequence`), и `<calcPr>`, приписанный в конец, сделал бы часть
/// невалидной: после него законны только `oleSize`, представления, кэши сводных
/// таблиц, смарт-теги, веб-публикация и `extLst`. Проще всего встать перед
/// первым из них.
const AFTER_CALC_PR: &[&[u8]] = &[
    b"oleSize",
    b"customWorkbookViews",
    b"pivotCaches",
    b"smartTagPr",
    b"smartTagTypes",
    b"webPublishing",
    b"fileRecoveryPr",
    b"webPublishObjects",
    b"extLst",
];

/// Помечает книгу как требующую полного пересчёта и сносит цепочку вычислений.
///
/// # Errors
///
/// Ошибки разбора и правки `xl/workbook.xml`, `[Content_Types].xml` и файлов
/// отношений.
pub(crate) fn mark_full_recalc(pkg: &mut Package<'_>, workbook: &PartName) -> Result<()> {
    // Сначала цепочка: она находится через отношения книги, а правка
    // `xl/workbook.xml` их не затрагивает — порядок нужен только чтобы не
    // держать два заимствования пакета одновременно.
    let _ = drop_calc_chain(pkg, workbook)?;
    mark_full_calc_on_load(pkg, workbook)
}

/// Выставляет `<calcPr calcId="0" fullCalcOnLoad="1"/>`, создавая элемент,
/// если его не было.
///
/// Прочие атрибуты `<calcPr>` не трогаются. `calcMode="manual"` — это выбор
/// пользователя, и отменять его правкой одной ячейки не наше дело;
/// `fullCalcOnLoad` действует независимо от режима.
fn mark_full_calc_on_load(pkg: &mut Package<'_>, workbook: &PartName) -> Result<()> {
    let doc = pkg.dom(workbook)?;
    let root = doc.root_element()?;
    let node = match find_child_named(doc, root, b"calcPr") {
        Some(n) => n,
        None => create_calc_pr(doc, root)?,
    };
    doc.set_attr(node, "calcId", "0")?;
    doc.set_attr(node, "fullCalcOnLoad", "1")
}

/// Создаёт `<calcPr>` на его месте по схеме.
fn create_calc_pr(doc: &mut Document, root: NodeId) -> Result<NodeId> {
    let name = prefixed(doc, root, "calcPr");
    let node = doc.new_element(&name)?;
    match first_child_in(doc, root, AFTER_CALC_PR) {
        Some(anchor) => doc.insert_before(anchor, node)?,
        None => doc.append_child(root, node)?,
    }
    Ok(node)
}

/// Удаляет `xl/calcChain.xml` вместе с его типом и отношением.
///
/// Возвращает `true`, если цепочка была.
fn drop_calc_chain(pkg: &mut Package<'_>, workbook: &PartName) -> Result<bool> {
    let Some(part) = calc_chain_part(pkg, workbook)? else {
        return Ok(false);
    };
    pkg.remove_part(&part)?;
    Ok(true)
}

/// Ищет часть с цепочкой вычислений.
fn calc_chain_part(pkg: &mut Package<'_>, workbook: &PartName) -> Result<Option<PartName>> {
    if pkg.has_rels(workbook) {
        let found = {
            let rels = pkg.rels(workbook)?;
            match rels.by_type(CALC_CHAIN_REL).next() {
                Some(rel) => rels.resolve(rel)?,
                None => None,
            }
        };
        if let Some(part) = found
            && pkg.has(&part)
        {
            return Ok(Some(part));
        }
    }
    // Отношения может не быть, а часть — быть: такой пакет уже невалиден, но
    // оставить в нём цепочку значило бы усугубить.
    let fallback = PartName::new(CALC_CHAIN_PART)?;
    Ok(pkg.has(&fallback).then_some(fallback))
}
