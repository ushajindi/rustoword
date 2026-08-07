//! Имя части пакета и разрешение целей отношений.
//!
//! # Почему имя — отдельный тип, а не `String`
//!
//! Имена частей сравниваются **без учёта регистра** (OPC §9.1.1.1), но
//! записываются в архив ровно так, как были. Пара «исходное написание +
//! ключ сравнения» в одном типе делает невозможной самую дорогую ошибку слоя:
//! найти часть по `/xl/Workbook.xml`, а записать её под именем, которого в
//! `[Content_Types].xml` нет.
//!
//! Ключ считается один раз при создании, а не при каждом сравнении: имя за
//! свою жизнь сравнивается десятки раз (резолвинг отношений, поиск Override,
//! проверка дубликатов), а меняется — ни разу.
//!
//! # Почему расширение берётся из имени, а не через `Path`
//!
//! `Path::new("_rels/.rels").extension()` возвращает `None`: для стандартной
//! библиотеки `.rels` — это файл без расширения с точкой в начале. Для OPC это
//! часть с расширением `rels`, и именно по нему `[Content_Types].xml`
//! сопоставляет ей тип через `Default`. По одной такой части есть в каждом
//! пакете корпуса, и молчаливая потеря типа у них — самый тихий из возможных
//! способов сломать слой. Поэтому расширение здесь — «всё после последней
//! точки последнего сегмента», без исключений для ведущей точки.

use core::fmt;

use crate::error::{OpcError, Result};

/// Имя части пакета: `/xl/worksheets/sheet1.xml`.
///
/// Всегда начинается со слэша, никогда им не кончается — кроме корня пакета
/// [`PartName::root`], который состоит из одного слэша и частью не является:
/// он существует как база для отношений уровня пакета (`/_rels/.rels`).
#[derive(Debug, Clone)]
pub struct PartName {
    /// Написание, как оно лежит в архиве, с добавленным ведущим слэшем.
    text: String,
    /// То же в нижнем ASCII-регистре — ключ сравнения и поиска.
    key: String,
}

impl PartName {
    /// Разбирает имя части, записанное от корня пакета.
    ///
    /// # Errors
    ///
    /// [`OpcError::BadPartName`], если имя не начинается со слэша, кончается
    /// слэшем, содержит пустой сегмент, сегмент `.`/`..`, сегмент,
    /// оканчивающийся точкой, обратный слэш или управляющий символ.
    pub fn new(text: &str) -> Result<Self> {
        validate(text)?;
        Ok(Self::assume_valid(text))
    }

    /// То же для имени, как оно записано в ZIP: без ведущего слэша.
    ///
    /// # Errors
    ///
    /// См. [`PartName::new`].
    pub fn from_zip_name(name: &str) -> Result<Self> {
        let mut text = String::with_capacity(name.len().saturating_add(1));
        text.push('/');
        text.push_str(name);
        Self::new(&text)
    }

    /// Корень пакета. Частью не является; служит базой для `/_rels/.rels`.
    #[must_use]
    pub fn root() -> Self {
        Self::assume_valid("/")
    }

    /// Имя состоит из одного слэша.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.text.len() == 1
    }

    /// Написание с ведущим слэшем.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Ключ сравнения — имя в нижнем ASCII-регистре.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Имя без ведущего слэша — так запись называется внутри ZIP.
    #[must_use]
    pub fn zip_name(&self) -> &str {
        self.text.strip_prefix('/').unwrap_or(&self.text)
    }

    /// Последний сегмент: `sheet1.xml` у `/xl/worksheets/sheet1.xml`.
    #[must_use]
    pub fn last_segment(&self) -> &str {
        self.text.rsplit('/').next().unwrap_or("")
    }

    /// Каталог части вместе с завершающим слэшем: `/xl/worksheets/`.
    ///
    /// Это база для разрешения относительных целей отношений.
    #[must_use]
    pub fn dir(&self) -> &str {
        match self.text.rfind('/') {
            Some(i) => self.text.get(..i.saturating_add(1)).unwrap_or("/"),
            None => "/",
        }
    }

    /// Расширение последнего сегмента без точки. `None` — точки в сегменте нет
    /// либо после неё пусто.
    ///
    /// Ведущая точка расширением быть не мешает: `.rels` → `Some("rels")`.
    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        let seg = self.last_segment();
        let i = seg.rfind('.')?;
        let ext = seg.get(i.saturating_add(1)..)?;
        if ext.is_empty() { None } else { Some(ext) }
    }

    /// Имя части отношений этой части.
    ///
    /// `/` → `/_rels/.rels`, `/xl/workbook.xml` → `/xl/_rels/workbook.xml.rels`.
    ///
    /// # Errors
    ///
    /// [`OpcError::BadPartName`], если результат не является именем части —
    /// например, у самой части отношений (отношения отношений запрещены).
    pub fn rels_part(&self) -> Result<Self> {
        if self.is_rels_part() {
            return Err(OpcError::BadPartName(self.text.clone()).into());
        }
        let mut s = String::with_capacity(self.text.len().saturating_add(13));
        s.push_str(self.dir());
        s.push_str("_rels/");
        s.push_str(self.last_segment());
        s.push_str(".rels");
        Self::new(&s)
    }

    /// Часть является файлом отношений: лежит в каталоге `_rels` и кончается
    /// на `.rels`.
    #[must_use]
    pub fn is_rels_part(&self) -> bool {
        self.key.ends_with(".rels") && self.dir().to_ascii_lowercase().ends_with("/_rels/")
    }

    /// Часть, которой принадлежит этот файл отношений.
    ///
    /// `/_rels/.rels` → корень пакета. `None` — часть файлом отношений не
    /// является.
    #[must_use]
    pub fn rels_owner(&self) -> Option<Self> {
        if !self.is_rels_part() {
            return None;
        }
        let owner_seg = self.last_segment().strip_suffix(".rels")?;
        // Каталог без завершающего `_rels/`: `dir()` — это `…/_rels/`.
        let dir = self.dir();
        let parent = dir
            .get(..dir.len().saturating_sub("_rels/".len()))
            .unwrap_or("/");
        if owner_seg.is_empty() {
            // `/_rels/.rels` — отношения самого пакета.
            return if parent == "/" {
                Some(Self::root())
            } else {
                None
            };
        }
        let mut s = String::with_capacity(parent.len().saturating_add(owner_seg.len()));
        s.push_str(parent);
        s.push_str(owner_seg);
        Self::new(&s).ok()
    }

    /// Разрешает цель отношения относительно этой части.
    ///
    /// База — **каталог** части, а не она сама: `Target="../media/image1.png"`
    /// в `/word/_rels/document.xml.rels` принадлежит части `/word/document.xml`
    /// и указывает на `/media/image1.png`.
    ///
    /// Цель обрабатывается как ссылка IRI: фрагмент (`#…`) и запрос (`?…`)
    /// отбрасываются, `%XX` раскодируются — имя части в архиве записано сырыми
    /// байтами, а в отношении оно закодировано.
    ///
    /// # Errors
    ///
    /// [`OpcError::BadPartName`], если цель выводит выше корня пакета,
    /// разрешается в сам корень или после раскодирования не является UTF-8.
    pub fn resolve(&self, target: &str) -> Result<Self> {
        let head = target
            .split(['#', '?'])
            .next()
            .ok_or_else(|| OpcError::BadPartName(target.to_owned()))?;
        let decoded = percent_decode(head)?;
        let joined = if decoded.starts_with('/') {
            decoded
        } else {
            let base = self.dir();
            let mut s = String::with_capacity(base.len().saturating_add(decoded.len()));
            s.push_str(base);
            s.push_str(&decoded);
            s
        };
        Self::new(&normalize(&joined, target)?)
    }
}

impl PartialEq for PartName {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for PartName {}

impl PartialOrd for PartName {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PartName {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl core::hash::Hash for PartName {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl fmt::Display for PartName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartName {
    /// Собирает пару «написание, ключ» без проверок.
    fn assume_valid(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            key: text.to_ascii_lowercase(),
        }
    }
}

/// Проверяет имя по правилам OPC §9.1.1.1, сведённым к тому, что реально
/// отличает имя части от произвольной строки.
fn validate(text: &str) -> Result<()> {
    let bad = || OpcError::BadPartName(text.to_owned());
    if text == "/" {
        return Ok(());
    }
    let body = text.strip_prefix('/').ok_or_else(bad)?;
    if body.is_empty() || body.ends_with('/') {
        return Err(bad().into());
    }
    for seg in body.split('/') {
        // Пустой сегмент — это `//`; `.`/`..` не имена, а операции над путём,
        // и попасть сюда они могут только из неразрешённой цели отношения.
        if seg.is_empty() || seg == "." || seg == ".." || seg.ends_with('.') {
            return Err(bad().into());
        }
    }
    // Обратный слэш и управляющие символы: в ZIP они допустимы, в имени части —
    // нет. Пропустить их значит согласиться, что одна и та же часть у нас и у
    // Windows называется по-разному.
    if text.bytes().any(|b| b == b'\\' || b < 0x20 || b == 0x7F) {
        return Err(bad().into());
    }
    Ok(())
}

/// Схлопывает `.` и `..`, отбрасывает пустые сегменты.
///
/// `original` — цель как она была записана: она и попадёт в текст ошибки,
/// потому что склеенный путь пользователь в файле не увидит.
fn normalize(path: &str, original: &str) -> Result<String> {
    let bad = || OpcError::BadPartName(original.to_owned());
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // Выход выше корня пакета — не «пустой путь», а испорченная
                // цель: молча схлопнуть её значило бы указать на чужую часть.
                stack.pop().ok_or_else(bad)?;
            }
            s => stack.push(s),
        }
    }
    if stack.is_empty() {
        return Err(bad().into());
    }
    let mut out = String::with_capacity(path.len());
    for seg in stack {
        out.push('/');
        out.push_str(seg);
    }
    Ok(out)
}

/// Раскодирует `%XX`. Одиночный `%` без двух шестнадцатеричных цифр остаётся
/// собой: это не escape, а байт имени.
fn percent_decode(s: &str) -> Result<String> {
    let b = s.as_bytes();
    if !b.contains(&b'%') {
        return Ok(s.to_owned());
    }
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while let Some(&c) = b.get(i) {
        if c == b'%'
            && let (Some(h), Some(l)) = (
                b.get(i.saturating_add(1)).copied().and_then(hex),
                b.get(i.saturating_add(2)).copied().and_then(hex),
            )
        {
            out.push(h.saturating_mul(16).saturating_add(l));
            i = i.saturating_add(3);
            continue;
        }
        out.push(c);
        i = i.saturating_add(1);
    }
    String::from_utf8(out).map_err(|_| OpcError::BadPartName(s.to_owned()).into())
}

const fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b.wrapping_sub(b'0')),
        b'a'..=b'f' => Some(b.wrapping_sub(b'a').wrapping_add(10)),
        b'A'..=b'F' => Some(b.wrapping_sub(b'A').wrapping_add(10)),
        _ => None,
    }
}
