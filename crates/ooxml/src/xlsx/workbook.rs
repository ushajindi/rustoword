//! Фасад книги: пакет OPC плюс то, что о листах говорит `xl/workbook.xml`.
//!
//! # Откуда берётся список листов
//!
//! Только из `<sheets>` в `xl/workbook.xml` и только в порядке появления
//! элементов `<sheet>`. Имя части листа получается резолвингом `r:id` через
//! `xl/_rels/workbook.xml.rels`.
//!
//! Соблазн угадать лист по имени файла (`worksheets/sheet1.xml` — первый)
//! существует, и он ошибочен. В корпусе `gsheets_01.xlsx` первый лист книги —
//! это `rId5`, и порядок отношений в `.rels` не совпадает ни с порядком листов,
//! ни с нумерацией файлов. Угадывание там молча открыло бы не тот лист, а файл
//! после правки остался бы валидным — то есть ошибку заметил бы пользователь,
//! а не тест.
//!
//! # Что открытие НЕ делает
//!
//! Не читает листы, не читает таблицу строк, не строит ни одного дерева, кроме
//! `[Content_Types].xml` (его строит [`Package::open`]), `xl/workbook.xml` и его
//! `.rels`. Гарантия «`Workbook::open(f).save() == f` побайтово» держится ровно
//! на этом: разобранное, но не изменённое дерево сериализуется тем же
//! фаст-пасом, а [`Package::save`] всё равно сверяет результат с оригиналом.
//!
//! # Таблица общих строк
//!
//! Загружается лениво — при первом чтении значений или первой записи строки.
//! Части `sharedStrings.xml` может не быть вовсе: книга без единой текстовой
//! ячейки законна. Тогда при первой записи строки в режиме
//! [`StringPolicy::SharedTable`] часть создаётся вместе с её `Override` в
//! `[Content_Types].xml` и отношением от книги.

use crate::dom::Document;
use crate::error::{Error, Result, XlsxError};
use crate::limits::Limits;
use crate::opc::{Package, PartName, TargetMode};
use crate::xlsx::recalc;
use crate::xlsx::sst::SharedStrings;
use crate::xlsx::worksheet::{Sheet, StringPolicy, encode_x_escapes, find_child_named, prefixed};

/// Namespace отношений — там живёт атрибут `r:id` у `<sheet>`.
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Тип отношения «главная часть документа».
const OFFICE_DOCUMENT_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

/// Тип отношения «таблица общих строк».
const SHARED_STRINGS_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";

/// Content type таблицы общих строк.
const SHARED_STRINGS_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml";

/// Пустая таблица общих строк — содержимое вновь созданной части.
///
/// Ни `count`, ни `uniqueCount` не пишутся сознательно: оба необязательны, а
/// поддерживать их точными мы всё равно не сможем (см. [`Workbook::intern_string`]).
const EMPTY_SST: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#;

/// Видимость листа — атрибут `state` у `<sheet>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SheetState {
    /// Лист виден. Значение по умолчанию: атрибута `state` может не быть.
    #[default]
    Visible,
    /// Лист скрыт, но пользователь может его показать.
    Hidden,
    /// Лист скрыт так, что показать его можно только из редактора VBA.
    VeryHidden,
}

impl SheetState {
    /// Разбирает значение атрибута `state`.
    ///
    /// Незнакомое значение считается видимым листом. Отказ здесь стоил бы
    /// дороже пользы: `state` не влияет ни на данные, ни на адресацию, а книга
    /// со странным значением всё равно должна открыться.
    fn parse(s: &str) -> Self {
        match s {
            "hidden" => Self::Hidden,
            "veryHidden" => Self::VeryHidden,
            _ => Self::Visible,
        }
    }
}

/// Лист книги в том виде, в каком о нём говорит `xl/workbook.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetMeta {
    /// Имя листа, как его видит пользователь.
    pub name: String,
    /// Атрибут `sheetId`. К порядку листов отношения не имеет.
    pub sheet_id: u32,
    /// Значение `r:id` — идентификатор отношения, а не имя части.
    pub rel_id: String,
    /// Часть, в которой лежит содержимое листа.
    pub part: PartName,
    /// Видимость.
    pub state: SheetState,
}

/// Состояние таблицы общих строк в рамках сессии правки.
///
/// Хранит разобранную таблицу книги и то, что мы к ней дописали. Индексы
/// сквозные: `i < table.len()` — запись книги, иначе `added[i - table.len()]`.
/// Дописывать можно только в конец, поэтому индекс, однажды выданный ячейке,
/// не меняется никогда.
#[derive(Debug, Default)]
struct SstState {
    /// Часть с таблицей. `None` — в книге её нет.
    part: Option<PartName>,
    /// Разобранная таблица.
    table: SharedStrings,
    /// Строки, добавленные нами и ещё не попавшие в `table`.
    added: Vec<String>,
    /// Индексы записей, пригодных для дедупликации, в порядке значений.
    order: Vec<u32>,
    /// `order` уже построен.
    order_built: bool,
    /// `table` разобрана.
    loaded: bool,
    /// `table` отстала от части: мы дописали в неё строки.
    stale: bool,
}

impl SstState {
    /// Значение записи по сквозному индексу.
    fn text_at(&self, i: u32) -> Option<&str> {
        let i = usize::try_from(i).ok()?;
        let base = self.table.len();
        if i < base {
            self.table.get(u32::try_from(i).ok()?).ok()
        } else {
            self.added.get(i.checked_sub(base)?).map(String::as_str)
        }
    }

    /// Сколько записей в таблице с учётом дописанного.
    fn len(&self) -> usize {
        self.table.len().saturating_add(self.added.len())
    }
}

/// Книга `.xlsx`.
///
/// Держит ссылку на исходный буфер: пакет не копирует содержимое частей, пока
/// его об этом не попросят.
#[derive(Debug)]
pub struct Workbook<'a> {
    pkg: Package<'a>,
    limits: Limits,
    /// Часть с описанием книги.
    workbook_part: PartName,
    sheets: Vec<SheetMeta>,
    /// Политика записи строк, своя у каждого листа.
    policies: Vec<StringPolicy>,
    /// Лист правился: его содержимое надо брать из дерева, а не из архива.
    touched: Vec<bool>,
    sst: SstState,
    /// Хоть одна ячейка была изменена.
    edited: bool,
    /// Пометка «пересчитать всё» уже выставлена.
    recalc_done: bool,
}

impl<'a> Workbook<'a> {
    /// Открывает книгу со строгими квотами.
    ///
    /// # Errors
    ///
    /// Ошибки ZIP, OPC и XML; [`XlsxError::NoWorkbook`], если главной части
    /// книги в пакете нет.
    pub fn open(data: &'a [u8]) -> Result<Self> {
        Self::open_with_limits(data, Limits::strict())
    }

    /// Открывает книгу с заданными квотами.
    ///
    /// # Errors
    ///
    /// См. [`Workbook::open`].
    pub fn open_with_limits(data: &'a [u8], limits: Limits) -> Result<Self> {
        let mut pkg = Package::open(data, limits.clone())?;
        let workbook_part = locate_workbook(&mut pkg)?;
        let sheets = read_sheets(&mut pkg, &workbook_part)?;
        let sst_part = locate_shared_strings(&mut pkg, &workbook_part);

        let n = sheets.len();
        Ok(Self {
            pkg,
            limits,
            workbook_part,
            sheets,
            policies: vec![StringPolicy::SharedTable; n],
            touched: vec![false; n],
            sst: SstState {
                part: sst_part,
                ..SstState::default()
            },
            edited: false,
            recalc_done: false,
        })
    }

    /// Листы книги в порядке `xl/workbook.xml`.
    #[must_use]
    pub fn sheets(&self) -> &[SheetMeta] {
        &self.sheets
    }

    /// Лист по порядковому номеру.
    ///
    /// # Errors
    ///
    /// [`XlsxError::SheetNotFound`], если номер за границей списка.
    pub fn sheet(&mut self, i: usize) -> Result<Sheet<'_, 'a>> {
        if i >= self.sheets.len() {
            return Err(XlsxError::SheetNotFound(i.to_string()).into());
        }
        Ok(Sheet { wb: self, at: i })
    }

    /// Лист по имени. Сравнение точное, включая регистр.
    ///
    /// # Errors
    ///
    /// [`XlsxError::SheetNotFound`], если листа с таким именем нет.
    pub fn sheet_by_name(&mut self, name: &str) -> Result<Sheet<'_, 'a>> {
        let i = self
            .sheets
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| XlsxError::SheetNotFound(name.to_owned()))?;
        Ok(Sheet { wb: self, at: i })
    }

    /// Собирает книгу обратно в байты `.xlsx`.
    ///
    /// Если книгу правили, здесь же — и только здесь — выставляется пометка
    /// «пересчитать всё при загрузке» и выбрасывается устаревшая цепочка
    /// вычислений. Делать это в момент правки было бы неверно: правок бывает
    /// много, а `xl/workbook.xml` надо тронуть один раз.
    ///
    /// # Errors
    ///
    /// Ошибки сериализации XML и сборки ZIP; ошибки правки `xl/workbook.xml`.
    pub fn save(&mut self) -> Result<Vec<u8>> {
        if self.edited && !self.recalc_done {
            recalc::mark_full_recalc(&mut self.pkg, &self.workbook_part)?;
            self.recalc_done = true;
        }
        self.pkg.save()
    }

    // --- внутреннее для фасада листа --------------------------------------

    /// Метаданные листа по индексу.
    pub(crate) fn meta(&self, i: usize) -> Result<&SheetMeta> {
        self.sheets.get(i).ok_or_else(|| bad_index(i))
    }

    /// Действующая политика записи строк.
    pub(crate) fn policy(&self, i: usize) -> StringPolicy {
        self.policies
            .get(i)
            .copied()
            .unwrap_or(StringPolicy::SharedTable)
    }

    /// Меняет политику записи строк.
    pub(crate) fn set_policy(&mut self, i: usize, p: StringPolicy) {
        if let Some(slot) = self.policies.get_mut(i) {
            *slot = p;
        }
    }

    /// Отмечает, что лист правился и его содержимое живёт в дереве.
    pub(crate) fn mark_touched(&mut self, i: usize) {
        if let Some(slot) = self.touched.get_mut(i) {
            *slot = true;
        }
    }

    /// Отмечает, что книга изменена: при сохранении понадобится пересчёт.
    pub(crate) fn mark_edited(&mut self) {
        self.edited = true;
    }

    /// Дерево листа. Строится при первом обращении — то есть при первой правке.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки и разбора XML; [`XlsxError::SheetNotFound`].
    pub(crate) fn sheet_dom(&mut self, i: usize) -> Result<&mut Document> {
        let part = self.meta(i)?.part.clone();
        self.mark_touched(i);
        self.pkg.dom(&part)
    }

    /// Даёт байты листа и актуальную таблицу строк.
    ///
    /// Форма с замыканием выбрана не из любви к ней: байты нетронутого листа
    /// живут в пакете и одалживаются у него, а байты правленого приходится
    /// сериализовать во временный вектор. Вернуть наружу и то и другое одним
    /// типом можно было бы только копией — а копия листа на 1,6 МБ при каждом
    /// чтении и есть та цена, ради отказа от которой существует потоковый
    /// сканер.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки, разбора таблицы строк и сериализации дерева.
    pub(crate) fn with_sheet_bytes<R>(
        &mut self,
        i: usize,
        f: impl FnOnce(&[u8], &SharedStrings, &Limits) -> Result<R>,
    ) -> Result<R> {
        self.ensure_sst()?;
        if self.touched.get(i).copied().unwrap_or(false) {
            let part = self.meta(i)?.part.clone();
            let data = self.pkg.dom(&part)?.serialize()?;
            return f(&data, &self.sst.table, &self.limits);
        }
        // Разбор полей по отдельности: `pkg` одалживается изменяемо, `sheets`,
        // `sst` и `limits` — неизменяемо, и это разные поля.
        let Self {
            pkg,
            sheets,
            sst,
            limits,
            ..
        } = self;
        let part = &sheets.get(i).ok_or_else(|| bad_index(i))?.part;
        let data = pkg.bytes(part)?;
        f(data, &sst.table, limits)
    }

    /// Доводит таблицу общих строк до актуального состояния.
    ///
    /// # Errors
    ///
    /// Ошибки распаковки и разбора части.
    pub(crate) fn ensure_sst(&mut self) -> Result<()> {
        if self.sst.loaded && !self.sst.stale {
            return Ok(());
        }
        let Some(part) = self.sst.part.clone() else {
            self.sst.table = SharedStrings::empty();
            self.sst.loaded = true;
            self.sst.stale = false;
            return Ok(());
        };

        let table = if self.sst.stale {
            // Часть уже правлена: её байты живут в дереве, а не в архиве.
            let data = self.pkg.dom(&part)?.serialize()?;
            SharedStrings::parse(&data, &self.limits)?
        } else {
            let Self { pkg, limits, .. } = self;
            let data = pkg.bytes(&part)?;
            SharedStrings::parse(data, limits)?
        };

        self.sst.table = table;
        // Дописанное теперь в таблице; сквозные индексы не изменились, поэтому
        // `order` остаётся верным.
        self.sst.added.clear();
        self.sst.loaded = true;
        self.sst.stale = false;
        Ok(())
    }

    /// Возвращает индекс строки в общей таблице, добавляя её при необходимости.
    ///
    /// # Дедупликация
    ///
    /// Совпадающая по значению запись переиспользуется. Сравнение идёт по
    /// **значению**, а кандидатами считаются только простые записи вида
    /// `<si><t>…</t></si>`. Запись с посимвольным форматированием (`<si><r>…`)
    /// кандидатом не является: значение у неё то же, но ячейка унаследовала бы
    /// чужое оформление, а это уже порча документа, пусть и незаметная.
    ///
    /// # `count` и `uniqueCount`
    ///
    /// Оба атрибута `<sst>` удаляются при первой же дописанной строке. Оба
    /// необязательны по схеме, и `uniqueCount` мы могли бы держать точным, а вот
    /// `count` — нет: он считает **использования** строк во всех листах книги, а
    /// узнать их число можно только просканировав все листы, чего правка одной
    /// ячейки делать не должна. Врущий атрибут хуже отсутствующего: по нему
    /// читатель может выделить память и оборвать разбор.
    ///
    /// # Errors
    ///
    /// Ошибки разбора и правки `sharedStrings.xml`; невозможность создать часть.
    pub(crate) fn intern_string(&mut self, s: &str) -> Result<u32> {
        self.ensure_sst()?;
        self.ensure_sst_part()?;
        self.ensure_sst_order()?;
        if let Some(i) = self.find_string(s) {
            return Ok(i);
        }
        self.append_string(s)
    }

    /// Создаёт `sharedStrings.xml`, если книга обходилась без него.
    fn ensure_sst_part(&mut self) -> Result<()> {
        if self.sst.part.is_some() {
            return Ok(());
        }
        // Имя строится рядом с книгой, а не прибивается к `/xl/`: главная часть
        // не обязана лежать именно там.
        let part = self.workbook_part.resolve("sharedStrings.xml")?;
        self.pkg
            .add_part(part.clone(), EMPTY_SST.to_vec(), SHARED_STRINGS_CT)?;
        let _ = self.pkg.add_relationship(
            &self.workbook_part,
            SHARED_STRINGS_REL,
            "sharedStrings.xml",
            TargetMode::Internal,
        )?;
        self.sst.part = Some(part);
        self.sst.table = SharedStrings::empty();
        self.sst.loaded = true;
        self.sst.stale = false;
        Ok(())
    }

    /// Строит индекс «значение → сквозной индекс» по дереву части.
    fn ensure_sst_order(&mut self) -> Result<()> {
        if self.sst.order_built {
            return Ok(());
        }
        let part = self
            .sst
            .part
            .clone()
            .ok_or(Error::Unsupported("xlsx: нет части общих строк"))?;
        let mut order = {
            let doc = self.pkg.dom(&part)?;
            simple_si_indices(doc)?
        };
        let sst = &self.sst;
        order.sort_by(|&a, &b| {
            sst.text_at(a)
                .unwrap_or("")
                .cmp(sst.text_at(b).unwrap_or(""))
        });
        self.sst.order = order;
        self.sst.order_built = true;
        Ok(())
    }

    /// Ищет готовую запись с таким значением.
    fn find_string(&self, s: &str) -> Option<u32> {
        let at = self
            .sst
            .order
            .binary_search_by(|&i| self.sst.text_at(i).unwrap_or("").cmp(s))
            .ok()?;
        self.sst.order.get(at).copied()
    }

    /// Дописывает строку в конец таблицы и возвращает её индекс.
    fn append_string(&mut self, s: &str) -> Result<u32> {
        let part = self
            .sst
            .part
            .clone()
            .ok_or(Error::Unsupported("xlsx: нет части общих строк"))?;
        let idx = u32::try_from(self.sst.len())
            .map_err(|_| Error::Unsupported("xlsx: таблица общих строк переполнена"))?;
        let enc = encode_x_escapes(s);

        {
            let doc = self.pkg.dom(&part)?;
            let root = doc.root_element()?;
            let si_name = prefixed(doc, root, "si");
            let si = doc.new_element(&si_name)?;
            let t_name = prefixed(doc, root, "t");
            let t = doc.new_element(&t_name)?;
            if needs_preserve(&enc) {
                doc.set_attr(t, "xml:space", "preserve")?;
            }
            doc.set_text(t, &enc)?;
            doc.append_child(si, t)?;
            doc.append_child(root, si)?;
            let _ = doc.remove_attr(root, None, "count")?;
            let _ = doc.remove_attr(root, None, "uniqueCount")?;
        }

        let pos = self
            .sst
            .order
            .binary_search_by(|&i| self.sst.text_at(i).unwrap_or("").cmp(s))
            .unwrap_or_else(|p| p);
        self.sst.added.push(s.to_owned());
        self.sst.order.insert(pos, idx);
        self.sst.stale = true;
        Ok(idx)
    }
}

/// Нужен ли `<t>` атрибут `xml:space="preserve"`.
///
/// Без него Excel и LibreOffice подрезают пробелы по краям: значение `" итого"`
/// вернулось бы как `"итого"`. Внутренние пробелы не в счёт — их не трогает
/// никто.
fn needs_preserve(s: &str) -> bool {
    s.starts_with(' ') || s.ends_with(' ')
}

/// Индексы записей `<si>`, годных в кандидаты дедупликации.
///
/// Годной считается запись ровно с одним дочерним элементом `<t>`: любая
/// другая форма (`<r>` с оформлением, `<rPh>` с фонетикой) несёт то, что
/// ячейка унаследовала бы вместе со значением.
fn simple_si_indices(doc: &Document) -> Result<Vec<u32>> {
    let root = doc.root_element()?;
    let mut out = Vec::new();
    let mut i: u32 = 0;
    for si in doc.children(root) {
        if doc.local_name(si) != Some(b"si") {
            continue;
        }
        let mut elems = 0usize;
        let mut only_t = true;
        for child in doc.children(si) {
            if doc.kind(child) != Some(crate::dom::NodeKind::Element) {
                continue;
            }
            elems = elems.saturating_add(1);
            if doc.local_name(child) != Some(b"t") {
                only_t = false;
            }
        }
        if elems == 1 && only_t {
            out.push(i);
        }
        i = i.saturating_add(1);
    }
    Ok(out)
}

/// Находит главную часть книги.
///
/// Сначала через отношение уровня пакета — так велит OPC. Соглашение
/// `/xl/workbook.xml` остаётся запасным путём: пакет без корневых отношений
/// невалиден, но открывать его всё же стоит.
fn locate_workbook(pkg: &mut Package<'_>) -> Result<PartName> {
    let root = PartName::root();
    if pkg.has_rels(&root) {
        let found = {
            let rels = pkg.rels(&root)?;
            match rels.by_type(OFFICE_DOCUMENT_REL).next() {
                Some(r) => rels.resolve(r)?,
                None => None,
            }
        };
        if let Some(part) = found
            && pkg.has(&part)
        {
            return Ok(part);
        }
    }
    let fallback = PartName::new(&format!("/{}", crate::xlsx::WORKBOOK_PART))?;
    if pkg.has(&fallback) {
        return Ok(fallback);
    }
    Err(XlsxError::NoWorkbook.into())
}

/// Находит часть с таблицей общих строк. `None` — её в книге нет.
fn locate_shared_strings(pkg: &mut Package<'_>, workbook: &PartName) -> Option<PartName> {
    if pkg.has_rels(workbook) {
        let found = {
            let rels = pkg.rels(workbook).ok()?;
            match rels.by_type(SHARED_STRINGS_REL).next() {
                Some(r) => rels.resolve(r).ok()?,
                None => None,
            }
        };
        if let Some(part) = found
            && pkg.has(&part)
        {
            return Some(part);
        }
    }
    let fallback = PartName::new(&format!("/{}", crate::xlsx::SHARED_STRINGS_PART)).ok()?;
    pkg.has(&fallback).then_some(fallback)
}

/// Читает `<sheets>` и резолвит `r:id` каждого листа.
fn read_sheets(pkg: &mut Package<'_>, workbook: &PartName) -> Result<Vec<SheetMeta>> {
    // Дерево одалживается только на время чтения: резолвинг `r:id` требует
    // пакета изменяемо, а два таких заимствования одновременно невозможны.
    let raw: Vec<(String, u32, String, SheetState)> = {
        let doc = pkg.dom(workbook)?;
        let root = doc.root_element()?;
        let Some(sheets) = find_child_named(doc, root, b"sheets") else {
            // Книга без `<sheets>` невалидна, но её содержимое нам всё ещё
            // доступно побайтово. Пустой список честнее отказа: обращение к
            // листу даст SheetNotFound с понятным сообщением.
            return Ok(Vec::new());
        };
        let mut raw = Vec::new();
        for node in doc.children(sheets) {
            if doc.local_name(node) != Some(b"sheet") {
                continue;
            }
            let name = doc
                .attr(node, None, "name")
                .ok_or(Error::Unsupported("xlsx: у <sheet> нет имени"))?
                .into_owned();
            let rel_id = doc
                .attr(node, Some(R_NS), "id")
                .ok_or(Error::Unsupported("xlsx: у <sheet> нет r:id"))?
                .into_owned();
            let sheet_id = doc
                .attr(node, None, "sheetId")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let state = doc
                .attr(node, None, "state")
                .map_or(SheetState::Visible, |v| SheetState::parse(&v));
            raw.push((name, sheet_id, rel_id, state));
        }
        raw
    };

    let mut out = Vec::with_capacity(raw.len());
    for (name, sheet_id, rel_id, state) in raw {
        let part = pkg.rels(workbook)?.target_of(&rel_id)?;
        out.push(SheetMeta {
            name,
            sheet_id,
            rel_id,
            part,
            state,
        });
    }
    Ok(out)
}

/// Обращение к листу, которого нет.
fn bad_index(i: usize) -> Error {
    XlsxError::SheetNotFound(i.to_string()).into()
}
