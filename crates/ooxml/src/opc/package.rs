//! Пакет OPC: ZIP-контейнер, разложенный на части.
//!
//! # Единственное правило, из которого выводится всё остальное
//!
//! **Нетронутая часть никогда не перепаковывается.** Наш deflate физически не
//! даёт те же байты, что Office (0 совпадений из 450 потоков, `docs/zip-fidelity.md` §1),
//! поэтому «прочитать и записать обратно» — это уже потеря байтовой точности.
//! Часть попадает в [`EntrySource::Replace`] только тогда, когда её новое
//! содержимое **действительно** отличается от старого.
//!
//! Отсюда фолбэк в [`Package::save`]: грязный документ сначала сериализуется и
//! сравнивается с распакованным оригиналом. Правка, вернувшая документ к
//! исходному виду (поставили атрибуту то же значение, добавили и удалили
//! элемент), не должна стоить пакету ни одной перепакованной записи.
//!
//! # Лень
//!
//! [`Package::open`] строит дерево ровно для одной части — `[Content_Types].xml`.
//! [`Package::bytes`] распаковывает, но дерева не строит; [`Package::dom`]
//! строит дерево только для запрошенной части. Причина измеренная: в корпусе
//! есть лист на 1,6 МБ и 70 тыс. узлов, чьё дерево занимает ~13,8 МиБ. Пакет,
//! разбирающий всё сразу, платил бы сотни мегабайт за правку одной ячейки.
//!
//! # Порядок записей
//!
//! Сохраняется исходный. `[Content_Types].xml` — первая запись лишь в 26
//! файлах корпуса из 43, и любая сортировка ломает все 43 (§3.6). Новые части
//! дописываются в конец.
//!
//! # Что в пакете есть, но частью не является
//!
//! Каталожные записи (имя на `/`, нулевой размер) и сам `[Content_Types].xml`.
//! Они не адресуются через [`PartName`], не имеют content type, но обязаны
//! пережить сохранение — поэтому живут в том же списке слотов, просто с
//! `name == None`.

use crate::dom::Document;
use crate::error::{Error, OpcError, Result};
use crate::limits::Limits;
use crate::zip::{EntrySource, WriteOptions, ZipArchive, ZipWriter};

use super::content_types::{CONTENT_TYPES_NAME, ContentTypes};
use super::partname::PartName;
use super::rels::{EMPTY_RELS, Rels, TargetMode};

/// Content type файла отношений. Обычно приходит из `Default Extension="rels"`,
/// но у пакета, где такого `Default` нет, новая часть отношений осталась бы без
/// типа — и пакет стал бы невалидным из-за нашей правки.
const RELS_CONTENT_TYPE: &str = "application/vnd.openxmlformats-package.relationships+xml";

/// DOS-время новых записей: 1980-01-01 00:00:00.
///
/// Часов у ядра нет и быть не может — с ними результат сохранения перестал бы
/// быть детерминированным, а вместе с ним и все тесты байтовой точности.
/// Минимальная дата, представимая в формате, — единственный выбор без часов.
const NEW_DOS: (u16, u16) = (0, 0x0021);

/// Состояние содержимого одной записи.
#[derive(Debug)]
enum Slot {
    /// Не читали. Сжатые байты и оба заголовка копируются как есть.
    Untouched,
    /// Распаковали, дерево не строили.
    Bytes(Vec<u8>),
    /// Разобрали в preserving DOM.
    Dom(Box<Document>),
    /// Часть добавлена нами: в исходном архиве записи нет.
    New(Vec<u8>),
    /// Часть удалена: в выходной архив не попадёт.
    Deleted,
}

/// Одна запись выходного архива вместе с тем, что мы о ней знаем.
#[derive(Debug)]
struct Entry {
    /// Имя части. `None` — запись частью не является (каталог, поток типов).
    name: Option<PartName>,
    /// Индекс записи в исходном архиве. `None` — часть добавлена нами.
    zip: Option<usize>,
    /// Имя записи внутри ZIP, без ведущего слэша.
    zip_name: String,
    slot: Slot,
    /// Разбор, если часть — файл отношений и его уже спрашивали.
    rels: Option<Rels>,
}

/// Пакет OPC поверх ZIP-контейнера.
///
/// Держит ссылку на исходный буфер и не копирует ни одного байта содержимого,
/// пока его не попросили.
#[derive(Debug)]
pub struct Package<'a> {
    zip: ZipArchive<'a>,
    limits: Limits,
    entries: Vec<Entry>,
    /// Отсортированный по ключу индекс имён: (ключ, позиция в `entries`).
    ///
    /// Линейный поиск был бы честнее по объёму кода, но `content_type` и
    /// резолвинг отношений зовутся по разу на часть, и на пакете из тысяч
    /// частей это уже квадрат.
    index: Vec<(String, usize)>,
    content_types: ContentTypes,
    /// Позиция `[Content_Types].xml` в `entries`.
    ct_at: usize,
}

impl<'a> Package<'a> {
    /// Открывает пакет.
    ///
    /// Разбирает ZIP-каталог и строит дерево ровно одной части —
    /// `[Content_Types].xml`. Содержимое остальных не читается.
    ///
    /// # Errors
    ///
    /// [`OpcError::NoContentTypes`], если потока типов нет;
    /// [`OpcError::BadPartName`] на записи, чьё имя не является именем части;
    /// [`OpcError::PartExists`] при двух частях, различающихся только
    /// регистром; ошибки слоёв ZIP и XML.
    pub fn open(data: &'a [u8], limits: Limits) -> Result<Self> {
        let zip = ZipArchive::parse(data, &limits)?;
        let mut entries: Vec<Entry> = Vec::with_capacity(zip.len());
        let mut ct_at: Option<usize> = None;

        for i in 0..zip.len() {
            let zip_name = zip.name_str(i)?.into_owned();
            let raw = zip.entry(i)?;
            // Каталожная запись — не часть, но байты файла: сохраняется как
            // есть и не адресуется по имени.
            let is_dir = raw.is_dir(zip.src());
            let is_ct = zip_name.eq_ignore_ascii_case(CONTENT_TYPES_NAME);
            if is_ct {
                ct_at = Some(entries.len());
            }
            let name = if is_dir || is_ct {
                None
            } else {
                Some(PartName::from_zip_name(&zip_name)?)
            };
            entries.push(Entry {
                name,
                zip: Some(i),
                zip_name,
                slot: Slot::Untouched,
                rels: None,
            });
        }

        let ct_at = ct_at.ok_or(OpcError::NoContentTypes)?;
        let index = build_index(&entries)?;

        // Единственная часть, которую открытие разбирает всегда: без неё
        // невозможно ответить ни на один вопрос о типах, а её размер в корпусе
        // не превышает нескольких килобайт.
        let ct_zip = entries
            .get(ct_at)
            .and_then(|e| e.zip)
            .ok_or(OpcError::NoContentTypes)?;
        let raw = zip.decompress(ct_zip, &limits)?;
        let doc = Document::parse(raw, &limits).map_err(|e| e.in_part(CONTENT_TYPES_NAME))?;
        let content_types = ContentTypes::parse(&doc)?;
        if let Some(e) = entries.get_mut(ct_at) {
            e.slot = Slot::Dom(Box::new(doc));
        }

        Ok(Self {
            zip,
            limits,
            entries,
            index,
            content_types,
            ct_at,
        })
    }

    /// Имена всех частей пакета в порядке записей архива.
    ///
    /// Каталожные записи и `[Content_Types].xml` частями не являются и сюда не
    /// попадают.
    pub fn part_names(&self) -> impl Iterator<Item = &PartName> {
        self.entries
            .iter()
            .filter(|e| !matches!(e.slot, Slot::Deleted))
            .filter_map(|e| e.name.as_ref())
    }

    /// Часть есть в пакете и не удалена.
    #[must_use]
    pub fn has(&self, part: &PartName) -> bool {
        self.find(part).is_some()
    }

    /// Число живых частей.
    #[must_use]
    pub fn part_count(&self) -> usize {
        self.part_names().count()
    }

    /// Content type части. `None` — часть не найдена либо тип не определяется.
    #[must_use]
    pub fn content_type(&self, part: &PartName) -> Option<&str> {
        if !self.has(part) {
            return None;
        }
        self.content_types.resolve(part)
    }

    /// Таблица типов пакета.
    #[must_use]
    pub const fn content_types(&self) -> &ContentTypes {
        &self.content_types
    }

    /// Исходный ZIP-контейнер — для сравнения сырых записей до и после правки.
    #[must_use]
    pub const fn zip(&self) -> &ZipArchive<'a> {
        &self.zip
    }

    /// Распакованные байты части. Дерево не строится.
    ///
    /// Для части, уже открытой как дерево, возвращаются байты, из которых это
    /// дерево построено. Если дерево изменено, байты у него больше не те, и
    /// молча отдать устаревшие — ровно тот класс тихой ошибки, ради борьбы с
    /// которым существует этот проект: тогда возвращается ошибка.
    ///
    /// # Errors
    ///
    /// [`OpcError::PartNotFound`]; ошибки распаковки; попытка получить байты
    /// изменённого дерева.
    pub fn bytes(&mut self, part: &PartName) -> Result<&[u8]> {
        let i = self.must_find(part)?;
        self.materialize(i)?;
        let e = self.entry(i)?;
        match &e.slot {
            Slot::Bytes(b) | Slot::New(b) => Ok(b),
            Slot::Dom(d) if !d.is_dirty() => Ok(d.src()),
            Slot::Dom(_) => Err(Error::Unsupported(
                "opc: часть изменена как дерево, байты берите из dom().serialize()",
            )
            .in_part(part.as_str())),
            Slot::Untouched | Slot::Deleted => {
                Err(OpcError::PartNotFound(part.as_str().to_owned()).into())
            }
        }
    }

    /// Дерево части. Строится при первом обращении.
    ///
    /// # Errors
    ///
    /// [`OpcError::PartNotFound`]; [`OpcError::UnknownContentType`], если тип
    /// части неизвестен; [`Error::Unsupported`], если часть не XML
    /// (`printerSettings/*.bin`, `thumbnail.jpeg`, `vmlDrawing`); ошибки
    /// распаковки и разбора XML — с именем части в контексте.
    pub fn dom(&mut self, part: &PartName) -> Result<&mut Document> {
        let i = self.must_find(part)?;
        self.ensure_dom(i)?;
        match &mut self.entry_mut(i)?.slot {
            Slot::Dom(d) => Ok(d),
            _ => Err(OpcError::PartNotFound(part.as_str().to_owned()).into()),
        }
    }

    /// Отношения части. `owner` — часть-владелец либо [`PartName::root`] для
    /// отношений уровня пакета.
    ///
    /// # Errors
    ///
    /// [`OpcError::PartNotFound`], если файла отношений в пакете нет;
    /// [`OpcError::BadRelationships`] на непонятном содержимом.
    pub fn rels(&mut self, owner: &PartName) -> Result<&Rels> {
        let rp = owner.rels_part()?;
        let i = self
            .find(&rp)
            .ok_or_else(|| OpcError::PartNotFound(rp.as_str().to_owned()))?;
        self.ensure_rels(i, owner.clone())?;
        self.entry(i)?
            .rels
            .as_ref()
            .ok_or_else(|| OpcError::BadRelationships(rp.as_str().to_owned()).into())
    }

    /// Есть ли у части файл отношений.
    #[must_use]
    pub fn has_rels(&self, owner: &PartName) -> bool {
        owner.rels_part().is_ok_and(|rp| self.has(&rp))
    }

    /// Добавляет часть и её тип.
    ///
    /// `Override` в `[Content_Types].xml` дописывается только если
    /// действующий `Default` не даёт нужный тип: лишняя запись испачкала бы
    /// поток типов, а с ним и его запись в архиве.
    ///
    /// # Errors
    ///
    /// [`OpcError::PartExists`]; ошибки правки потока типов.
    pub fn add_part(&mut self, part: PartName, data: Vec<u8>, content_type: &str) -> Result<()> {
        if part.is_root() {
            return Err(OpcError::BadPartName(part.as_str().to_owned()).into());
        }
        if self.has(&part) {
            return Err(OpcError::PartExists(part.as_str().to_owned()).into());
        }
        self.ensure_content_type(&part, content_type)?;

        // Часть с таким именем уже была и была удалена: воскрешаем её на
        // прежнем месте архива. Так сохраняются её позиция, DOS-время и
        // extra-поля — всё, что не зависит от содержимого.
        if let Some(i) = self.find_any(&part) {
            let e = self.entry_mut(i)?;
            e.slot = Slot::New(data);
            e.rels = None;
            return Ok(());
        }

        let at = self.entries.len();
        let key = part.key().to_owned();
        let zip_name = part.zip_name().to_owned();
        self.entries.push(Entry {
            name: Some(part),
            zip: None,
            zip_name,
            slot: Slot::New(data),
            rels: None,
        });
        self.insert_index(key, at);
        Ok(())
    }

    /// Удаляет часть, её файл отношений и все отношения, на неё указывающие.
    ///
    /// Отношение на несуществующую часть делает пакет невалидным. Оставить его
    /// — сломать файл тише, чем это сделал бы отказ, поэтому все `.rels`
    /// пакета просматриваются. Те из них, где ничего не нашлось, остаются
    /// чистыми и копируются побайтово.
    ///
    /// # Errors
    ///
    /// [`OpcError::PartNotFound`]; ошибки разбора просмотренных `.rels`.
    pub fn remove_part(&mut self, part: &PartName) -> Result<()> {
        let i = self.must_find(part)?;
        self.entry_mut(i)?.slot = Slot::Deleted;
        self.drop_override(part)?;

        // Отношения удалённой части больше некому принадлежать.
        if let Ok(rp) = part.rels_part()
            && let Some(j) = self.find(&rp)
        {
            self.entry_mut(j)?.slot = Slot::Deleted;
            self.drop_override(&rp)?;
        }

        for j in 0..self.entries.len() {
            let e = self.entry(j)?;
            if matches!(e.slot, Slot::Deleted) {
                continue;
            }
            let Some(owner) = e.name.as_ref().and_then(PartName::rels_owner) else {
                continue;
            };
            self.ensure_rels(j, owner)?;
            let e = self.entries.get_mut(j).ok_or_else(bad_slot)?;
            let (Slot::Dom(doc), Some(rels)) = (&mut e.slot, e.rels.as_mut()) else {
                continue;
            };
            let _ = rels.remove_targeting(doc, part)?;
        }
        Ok(())
    }

    /// Добавляет отношение от `owner` к `target`, создавая файл отношений,
    /// если его не было.
    ///
    /// Возвращает идентификатор нового отношения.
    ///
    /// # Errors
    ///
    /// [`OpcError::PartNotFound`], если владельца нет в пакете; ошибки правки.
    pub fn add_relationship(
        &mut self,
        owner: &PartName,
        rel_type: &str,
        target: &str,
        mode: TargetMode,
    ) -> Result<String> {
        if !owner.is_root() && !self.has(owner) {
            return Err(OpcError::PartNotFound(owner.as_str().to_owned()).into());
        }
        let rp = owner.rels_part()?;
        if !self.has(&rp) {
            self.add_part(rp.clone(), EMPTY_RELS.to_vec(), RELS_CONTENT_TYPE)?;
        }
        let i = self
            .find(&rp)
            .ok_or_else(|| OpcError::PartNotFound(rp.as_str().to_owned()))?;
        self.ensure_rels(i, owner.clone())?;
        let e = self.entries.get_mut(i).ok_or_else(bad_slot)?;
        let (Slot::Dom(doc), Some(rels)) = (&mut e.slot, e.rels.as_mut()) else {
            return Err(OpcError::BadRelationships(rp.as_str().to_owned()).into());
        };
        rels.add(doc, rel_type, target, mode)
    }

    /// Собирает пакет обратно в ZIP-контейнер.
    ///
    /// Каждая запись проходит один из трёх путей: побайтовая копия, замена
    /// содержимого, новая запись. Первый путь — единственный, дающий байтовую
    /// идентичность, и решение о нём принимается здесь.
    ///
    /// # Errors
    ///
    /// Ошибки сериализации XML, распаковки (нужна для сравнения) и сборки ZIP.
    pub fn save(&mut self) -> Result<Vec<u8>> {
        let zip = &self.zip;
        let limits = &self.limits;
        let mut w = ZipWriter::new(WriteOptions::default());

        for e in &self.entries {
            if matches!(e.slot, Slot::Deleted) {
                continue;
            }
            match e.zip {
                Some(i) => match payload_for(&e.slot, zip, i, limits, &e.zip_name)? {
                    None => w.push(EntrySource::Verbatim { src: zip, index: i })?,
                    Some(data) => w.push(EntrySource::Replace {
                        src: zip,
                        index: i,
                        data,
                        level: crate::deflate::Level::Default,
                    })?,
                },
                None => {
                    let data = match &e.slot {
                        Slot::Bytes(b) | Slot::New(b) => b.clone(),
                        Slot::Dom(d) => d.serialize().map_err(|x| x.in_part(&e.zip_name))?,
                        // Новая запись без содержимого взяться неоткуда:
                        // `Untouched` означает «лежит в исходном архиве».
                        Slot::Untouched | Slot::Deleted => continue,
                    };
                    w.push(EntrySource::New {
                        name: e.zip_name.clone(),
                        data,
                        level: crate::deflate::Level::Default,
                        dos: NEW_DOS,
                        external_attrs: 0,
                    })?;
                }
            }
        }
        w.finish()
    }

    /// Сколько памяти пакет занял сверх входного буфера.
    ///
    /// Считается по `capacity`, а не по `len`: интересен пик. Ровно та же
    /// оговорка, что у [`Document::memory_bytes`], и по той же причине —
    /// внешнего аллокатора-счётчика в ядре быть не может.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.entries.iter().fold(0usize, |acc, e| {
            let own = match &e.slot {
                Slot::Bytes(b) | Slot::New(b) => b.capacity(),
                Slot::Dom(d) => d.memory_bytes(),
                Slot::Untouched | Slot::Deleted => 0,
            };
            acc.saturating_add(own)
                .saturating_add(e.zip_name.capacity())
        })
    }

    // --- внутреннее --------------------------------------------------------

    fn entry(&self, i: usize) -> Result<&Entry> {
        self.entries.get(i).ok_or_else(bad_slot)
    }

    fn entry_mut(&mut self, i: usize) -> Result<&mut Entry> {
        self.entries.get_mut(i).ok_or_else(bad_slot)
    }

    /// Позиция живой части.
    fn find(&self, part: &PartName) -> Option<usize> {
        let i = self.find_any(part)?;
        let e = self.entries.get(i)?;
        if matches!(e.slot, Slot::Deleted) {
            None
        } else {
            Some(i)
        }
    }

    /// Позиция части, включая удалённую.
    fn find_any(&self, part: &PartName) -> Option<usize> {
        let at = self
            .index
            .binary_search_by(|(k, _)| k.as_str().cmp(part.key()))
            .ok()?;
        self.index.get(at).map(|&(_, i)| i)
    }

    fn must_find(&self, part: &PartName) -> Result<usize> {
        self.find(part)
            .ok_or_else(|| OpcError::PartNotFound(part.as_str().to_owned()).into())
    }

    fn insert_index(&mut self, key: String, at: usize) {
        let pos = self
            .index
            .binary_search_by(|(k, _)| k.as_str().cmp(&key))
            .unwrap_or_else(|p| p);
        self.index.insert(pos, (key, at));
    }

    /// Доводит слот до состояния, в котором байты части доступны.
    fn materialize(&mut self, i: usize) -> Result<()> {
        let e = self.entry(i)?;
        if !matches!(e.slot, Slot::Untouched) {
            return Ok(());
        }
        let zi = e.zip.ok_or_else(bad_slot)?;
        let name = e.zip_name.clone();
        let raw = self
            .zip
            .decompress(zi, &self.limits)
            .map_err(|x| x.in_part(&name))?;
        self.entry_mut(i)?.slot = Slot::Bytes(raw);
        Ok(())
    }

    /// Доводит слот до дерева, проверив, что часть вообще XML.
    fn ensure_dom(&mut self, i: usize) -> Result<()> {
        if matches!(self.entry(i)?.slot, Slot::Dom(_)) {
            return Ok(());
        }
        self.require_xml(i)?;
        self.materialize(i)?;
        let e = self.entry(i)?;
        let name = e.zip_name.clone();
        // Копия, а не `mem::take`: `Document::parse` поглощает вектор и на
        // ошибке его не возвращает, а терять содержимое части из-за неудачной
        // попытки её разобрать нельзя.
        let raw = match &e.slot {
            Slot::Bytes(b) | Slot::New(b) => b.clone(),
            _ => return Err(bad_slot()),
        };
        let doc = Document::parse(raw, &self.limits).map_err(|x| x.in_part(&name))?;
        self.entry_mut(i)?.slot = Slot::Dom(Box::new(doc));
        Ok(())
    }

    /// Отказывает, если часть не является XML.
    ///
    /// Решение принимается по content type, а не по расширению: в корпусе
    /// `vmlDrawing1.vml` имеет тип `…officedocument.vmlDrawing` без `+xml`,
    /// `printerSettings1.bin` — `…spreadsheetml.printerSettings`,
    /// `thumbnail.jpeg` — `image/jpeg`. Расширение об этом не говорит ничего.
    fn require_xml(&self, i: usize) -> Result<()> {
        let e = self.entry(i)?;
        let Some(name) = e.name.as_ref() else {
            // Поток типов и каталожные записи через `dom()` не адресуются.
            return Ok(());
        };
        let ct = self
            .content_types
            .resolve(name)
            .ok_or_else(|| OpcError::UnknownContentType(name.as_str().to_owned()))?;
        if is_xml_content_type(ct) {
            Ok(())
        } else {
            Err(Error::Unsupported("opc: часть не является XML").in_part(name.as_str()))
        }
    }

    /// Разбирает файл отношений, если он ещё не разобран.
    fn ensure_rels(&mut self, i: usize, owner: PartName) -> Result<()> {
        if self.entry(i)?.rels.is_some() {
            return Ok(());
        }
        self.ensure_dom(i)?;
        let e = self.entry(i)?;
        let Slot::Dom(doc) = &e.slot else {
            return Err(bad_slot());
        };
        let rels = Rels::parse(owner, doc)?;
        self.entry_mut(i)?.rels = Some(rels);
        Ok(())
    }

    /// Правит поток типов так, чтобы часть разрешалась в нужный тип.
    fn ensure_content_type(&mut self, part: &PartName, content_type: &str) -> Result<()> {
        let at = self.ct_at;
        let Slot::Dom(doc) = &mut self.entries.get_mut(at).ok_or_else(bad_slot)?.slot else {
            return Err(OpcError::NoContentTypes.into());
        };
        let _ = self.content_types.ensure(doc, part, content_type)?;
        Ok(())
    }

    /// Убирает `Override` части из потока типов.
    fn drop_override(&mut self, part: &PartName) -> Result<()> {
        let at = self.ct_at;
        let Slot::Dom(doc) = &mut self.entries.get_mut(at).ok_or_else(bad_slot)?.slot else {
            return Err(OpcError::NoContentTypes.into());
        };
        let _ = self.content_types.remove_override(doc, part)?;
        Ok(())
    }
}

/// Что записать вместо записи, которая была в исходном архиве.
///
/// `None` — «ничего», то есть побайтовая копия. Единственное место, где
/// принимается решение о перепаковке, и единственное, где живёт фолбэк
/// «правка вернула документ к исходному виду».
fn payload_for(
    slot: &Slot,
    zip: &ZipArchive<'_>,
    index: usize,
    limits: &Limits,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    match slot {
        // Читали, но не меняли: distinct от `Untouched` только по памяти.
        Slot::Untouched | Slot::Bytes(_) => Ok(None),
        Slot::Dom(d) if !d.is_dirty() => Ok(None),
        Slot::Dom(d) => {
            let new = d.serialize().map_err(|e| e.in_part(name))?;
            let old = zip.decompress(index, limits).map_err(|e| e.in_part(name))?;
            // Холостая правка: документ вернулся к исходному виду. Перепаковка
            // здесь стоила бы записи её байтовой идентичности ни за что.
            Ok(if new == old { None } else { Some(new) })
        }
        Slot::New(b) => Ok(Some(b.clone())),
        Slot::Deleted => Ok(None),
    }
}

/// Является ли content type разновидностью XML.
///
/// Параметры (`; charset=…`) отбрасываются: они к типу носителя не относятся.
fn is_xml_content_type(ct: &str) -> bool {
    let base = ct.split(';').next().unwrap_or(ct).trim();
    base.eq_ignore_ascii_case("application/xml")
        || base.eq_ignore_ascii_case("text/xml")
        || base.len() > 4 && base.get(base.len().saturating_sub(4)..) == Some("+xml")
}

/// Строит отсортированный индекс имён, попутно ловя дубликаты по регистру.
fn build_index(entries: &[Entry]) -> Result<Vec<(String, usize)>> {
    let mut index: Vec<(String, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.name.as_ref().map(|n| (n.key().to_owned(), i)))
        .collect();
    index.sort_by(|a, b| a.0.cmp(&b.0));
    for pair in index.windows(2) {
        if let [a, b] = pair
            && a.0 == b.0
        {
            // Две части, различающиеся только регистром, — для OPC одна и та
            // же часть. Пакет с ними неоднозначен, и угадывать тут нечего.
            return Err(OpcError::PartExists(a.0.clone()).into());
        }
    }
    Ok(index)
}

/// Слот не в том состоянии, в котором его ожидал вызывающий.
///
/// В `error.rs` нет варианта «внутренняя рассогласованность», и править его
/// эта веха не вправе; [`Error::Unsupported`] — ближайший по смыслу.
fn bad_slot() -> Error {
    Error::Unsupported("opc: слот части в неожиданном состоянии")
}
