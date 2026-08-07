//! Полезная нагрузка элемента и атрибута.
//!
//! # Почему всё хранится «избыточно»
//!
//! Может показаться, что `<w:t w:customStyle = "1" >` описывается именем,
//! списком пар «имя-значение» и признаком пустоты. Замеры корпуса говорят
//! обратное: в 43 реальных файлах живут 39 атрибутов с пробелами вокруг `=`,
//! 31 тег с двойным пробелом после имени, 181 тег с пробелом перед `/>`,
//! 24 — с пробелом перед `>`, 7 тегов с переводом строки внутри. Ни один из
//! этих байтов не восстановим из «смысла» тега, а значит, каждый обязан иметь
//! свой спан. Отсюда `pre_ws`, `ws_before_eq`, `ws_after_eq`, `pre_close_ws`,
//! `close_trailing_ws` — они выглядят как излишество ровно до первого diff'а.
//!
//! # Два буфера
//!
//! Спан адресует либо исходник, либо **арену нового содержимого** — общий
//! `Vec<u8>` документа, куда дописываются заэкранированные значения из правок.
//! Какой из двух — говорит флаг на владельце спана, а не тег внутри `Span`:
//! тег удвоил бы размер спана, а спанов в дереве миллионы.

use core::fmt;

use crate::bytes::Span;

/// Флаги элемента.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct ElemFlags(u8);

impl ElemFlags {
    /// `qname` и `close_qname` лежат в арене, а не в исходнике.
    ///
    /// Один бит на оба имени: закрывающий тег созданного нами элемента всегда
    /// пишется тем же спаном, что и открывающий — переименования элемента в
    /// API нет, а значит, разойтись они не могут.
    pub const QNAME_IN_ARENA: u8 = 1 << 0;

    #[must_use]
    pub const fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    pub const fn set(&mut self, bit: u8) {
        self.0 |= bit;
    }
}

impl fmt::Debug for ElemFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.has(Self::QNAME_IN_ARENA) {
            f.write_str("QNAME_IN_ARENA")
        } else {
            f.write_str("-")
        }
    }
}

/// Данные элемента.
///
/// Лежат в отдельной таблице документа, а узел ссылается на них индексом:
/// иначе `Node` не влез бы в бюджет 40 байт (см. [`crate::dom::Node`]).
///
/// Атрибуты хранятся не вектором на элемент, а диапазоном в общем векторе
/// документа. На листе с 300 тыс. элементов это разница между одной аллокацией
/// и тремястами тысячами.
#[derive(Clone, Copy, Debug)]
pub struct ElementData {
    /// Имя как написано, вместе с префиксом: `w:p`, а не пара (URI, `p`).
    pub(crate) qname: Span,
    /// Локальная часть — подспан `qname`.
    pub(crate) local: Span,
    /// Пробелы непосредственно перед `>` или `/>`.
    pub(crate) pre_close_ws: Span,
    /// Сырое имя в `</...>`. Пустой спан — тега нет (стоит `EMPTY_TAG`).
    ///
    /// Хранится отдельно от `qname`, хотя XML требует их совпадения: спан
    /// закрывающего тега нужен, чтобы вычислить границу внутренности элемента,
    /// а по ней проверяется инвариант покрытия.
    pub(crate) close_qname: Span,
    /// Пробелы между именем и `>` в закрывающем теге: `</w:p  >`.
    pub(crate) close_trailing_ws: Span,
    /// Начало диапазона атрибутов в общем векторе.
    pub(crate) attrs_from: u32,
    /// Длина диапазона атрибутов.
    pub(crate) attrs_len: u32,
    /// Интернированный URI namespace; [`ElementData::NO_NS`] — вне namespace.
    ///
    /// На байты не влияет вовсе: в файл пишется `qname`. Нужен только
    /// типизированному API, чтобы опознавать элементы по URI, а не по
    /// написанию префикса, которое документ волен выбрать любым.
    pub(crate) ns_uri: u32,
    pub(crate) flags: ElemFlags,
}

impl ElementData {
    /// Элемент не принадлежит ни одному namespace.
    pub const NO_NS: u32 = u32::MAX;

    /// Позиция первого байта за открывающим тегом.
    ///
    /// Выводится, а не хранится: `pre_close_ws` кончается вплотную к `>` или
    /// `/>`, и длина закрывающей последовательности известна из `EMPTY_TAG`.
    #[must_use]
    pub(crate) const fn open_end(&self, empty: bool) -> u32 {
        let tail = if empty { 2 } else { 1 };
        self.pre_close_ws.end().saturating_add(tail)
    }

    /// Позиция `<` закрывающего тега.
    ///
    /// `</` — два байта перед именем; для самозакрывающегося тега смысла не
    /// имеет и не вызывается.
    #[must_use]
    pub(crate) const fn close_start(&self) -> u32 {
        self.close_qname.start().saturating_sub(2)
    }
}

/// Флаги атрибута.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct AttrFlags(u8);

impl AttrFlags {
    /// `name` лежит в арене.
    pub const NAME_IN_ARENA: u8 = 1 << 0;
    /// `value` лежит в арене — значит, значение записали мы, а не документ.
    pub const VALUE_IN_ARENA: u8 = 1 << 1;
    /// Все три пробельных спана лежат в арене (атрибут целиком создан нами).
    pub const WS_IN_ARENA: u8 = 1 << 2;

    #[must_use]
    pub const fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    pub const fn set(&mut self, bit: u8) {
        self.0 |= bit;
    }
}

impl fmt::Debug for AttrFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:03b}", self.0)
    }
}

/// Атрибут в том виде, в каком он лежит в файле.
///
/// Порядок атрибутов — порядок появления, без сортировки: в OOXML корень
/// объявляет `xmlns:x14ac`, `xmlns:xr`, `xmlns:mc`, а рядом лежит
/// `mc:Ignorable="x14ac xr"`, который ссылается на префиксы **по именам**.
///
/// `value` — **сырое**, всё ещё экранированное значение. Декодированного
/// значения в дереве нет вообще: путь от спана к переэкранированию отсутствует
/// в коде, поэтому `&#34;` (44 вхождения в корпусе) не может превратиться в
/// `&quot;` (83 вхождения) сам собой.
#[derive(Clone, Copy, Debug)]
pub struct Attr {
    /// Пробелы перед именем. Минимум один байт — иначе это не атрибут.
    pub(crate) pre_ws: Span,
    /// Имя вместе с префиксом.
    pub(crate) name: Span,
    /// Пробелы между именем и `=`.
    pub(crate) ws_before_eq: Span,
    /// Пробелы между `=` и открывающей кавычкой.
    pub(crate) ws_after_eq: Span,
    /// Значение без кавычек, сырое.
    pub(crate) value: Span,
    /// Символ кавычки: `"` или `'`. В корпусе все 648 183 — двойные, но
    /// правило всё равно «как было»: одинарная кавычка законна.
    pub(crate) quote: u8,
    pub(crate) flags: AttrFlags,
}

impl Attr {
    #[must_use]
    pub const fn quote(&self) -> u8 {
        self.quote
    }

    #[must_use]
    pub const fn name_span(&self) -> Span {
        self.name
    }

    #[must_use]
    pub const fn value_span(&self) -> Span {
        self.value
    }

    /// Значение записано нами, а не взято из исходника.
    #[must_use]
    pub const fn is_edited(&self) -> bool {
        self.flags.has(AttrFlags::VALUE_IN_ARENA)
    }
}

/// Локальная часть имени: всё после единственного двоеточия.
#[must_use]
pub(crate) fn local_of(name: &[u8]) -> &[u8] {
    match name.iter().position(|&b| b == b':') {
        Some(i) => name.get(i.saturating_add(1)..).unwrap_or(name),
        None => name,
    }
}

/// Префикс имени без двоеточия; пустой срез — префикса нет.
#[must_use]
pub(crate) fn prefix_of(name: &[u8]) -> &[u8] {
    match name.iter().position(|&b| b == b':') {
        Some(i) => name.get(..i).unwrap_or(&[]),
        None => &[],
    }
}
