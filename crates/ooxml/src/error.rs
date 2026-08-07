//! Типы ошибок.
//!
//! Ошибка всегда несёт позицию в исходном буфере — без неё отладка байтовой
//! точности превращается в гадание. Позиции хранятся отдельными полями варианта
//! `Error`, а не внутри `*Kind`, чтобы вложенные слои могли переоборачивать
//! ошибку, не теряя контекст.
//!
//! Полный набор вариантов определён здесь сразу, включая слои, которые ещё не
//! написаны: это позволяет вести разработку слоёв параллельно, не конфликтуя
//! на одном файле.

use core::fmt;

/// Результат любой операции крейта.
pub type Result<T> = core::result::Result<T, Error>;

/// Ошибка любого слоя ядра.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Нарушение структуры ZIP-контейнера. `offset` — позиция в файле.
    Zip { kind: ZipError, offset: u64 },
    /// Ошибка распаковки или упаковки deflate-потока.
    Deflate {
        kind: DeflateError,
        in_off: u64,
        out_off: u64,
    },
    /// Ошибка разбора XML. `offset` — позиция внутри части, не внутри пакета.
    Xml { kind: XmlError, offset: u32 },
    /// Нарушение структуры OPC-пакета.
    Opc(OpcError),
    /// Ошибка на уровне модели SpreadsheetML.
    Xlsx(XlsxError),
    /// Ошибка разбора формулы. `pos` — позиция в тексте формулы.
    Formula { kind: FormulaError, pos: u32 },
    /// Превышена одна из квот [`crate::Limits`].
    Limit(LimitError),
    /// Конструкция синтаксически корректна, но ядро её не поддерживает.
    Unsupported(&'static str),
    /// Обёртка, добавляющая имя части к ошибке нижнего слоя.
    ///
    /// Слои `xml`/`deflate` не знают, из какой части пакета пришли байты;
    /// слой OPC навешивает это знание сверху.
    InPart { part: String, source: Box<Error> },
}

/// Что именно сломано в ZIP-контейнере.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ZipError {
    /// Не найдена запись End Of Central Directory.
    NoEndOfCentralDirectory,
    /// Не найден zip64 EOCD locator, хотя поля требуют zip64.
    NoZip64Locator,
    /// Неожиданная сигнатура на позиции, где ожидался заголовок.
    BadSignature { expected: u32, found: u32 },
    /// Центральный каталог выходит за границы буфера.
    DirectoryOutOfBounds,
    /// Данные записи выходят за границы буфера.
    DataOutOfBounds,
    /// Диапазоны двух записей пересекаются — признак сконструированной атаки.
    OverlappingEntries,
    /// Имя в локальном заголовке не совпадает с именем в центральном каталоге.
    NameMismatch,
    /// Заявленное и фактическое число записей расходятся.
    EntryCountMismatch,
    /// Метод сжатия не поддерживается (поддерживаются только 0 и 8).
    UnsupportedMethod(u16),
    /// Запись зашифрована.
    Encrypted,
    /// Многотомный архив.
    MultiDisk,
    /// CRC распакованных данных не совпал с записанным.
    CrcMismatch { expected: u32, actual: u32 },
    /// Размер распакованных данных не совпал с записанным.
    SizeMismatch { expected: u64, actual: u64 },
    /// Некорректное extra-поле.
    BadExtraField,
    /// Заголовок оборван.
    Truncated,
    /// Офсет или размер не помещается в целевой тип при записи.
    OffsetOverflow,
}

/// Что именно сломано в deflate-потоке.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeflateError {
    /// Поток кончился раньше, чем финальный блок.
    UnexpectedEof,
    /// Значение поля BTYPE равно 3 (зарезервировано).
    ReservedBlockType,
    /// В stored-блоке LEN и ~NLEN не дополняют друг друга.
    BadStoredLength,
    /// Дерево Хаффмана переполнено (сумма 2^-len > 1).
    OversubscribedTree,
    /// Дерево Хаффмана неполно и при этом используется более чем одним символом.
    IncompleteTree,
    /// Встречен код длины/расстояния из зарезервированного диапазона.
    InvalidCode,
    /// Расстояние ссылки больше, чем уже распаковано.
    DistanceTooFar,
    /// Некорректная последовательность кодов длин кодовых длин.
    BadCodeLengthRepeat,
}

/// Что именно сломано в XML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum XmlError {
    /// Документ оборван посреди конструкции.
    UnexpectedEof,
    /// Байт не является допустимым в этой позиции.
    UnexpectedByte(u8),
    /// Закрывающий тег не соответствует открывающему.
    MismatchedTag,
    /// Закрыт тег, который не был открыт.
    UnbalancedTag,
    /// Документ содержит более одного корневого элемента.
    MultipleRoots,
    /// Документ не содержит корневого элемента.
    NoRoot,
    /// Значение атрибута не в кавычках.
    UnquotedAttribute,
    /// Атрибут с таким именем уже был в этом элементе.
    DuplicateAttribute,
    /// Ссылка на сущность, отличную от пяти предопределённых.
    UnknownEntity,
    /// Некорректная числовая ссылка на символ.
    BadCharRef,
    /// Числовая ссылка указывает на символ, недопустимый в XML 1.0.
    IllegalChar(u32),
    /// Байты не образуют корректный UTF-8.
    NotUtf8,
    /// Префикс namespace не объявлен.
    UndeclaredPrefix,
    /// Попытка переопределить зарезервированный префикс `xml` или `xmlns`.
    ReservedPrefix,
    /// Некорректное имя (QName с двумя двоеточиями и т. п.).
    BadName,
    /// Встречен `<!DOCTYPE`. Запрещён безусловно — это вектор XXE и billion-laughs.
    DoctypeForbidden,
}

/// Что именно сломано в OPC-пакете.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpcError {
    /// Отсутствует `[Content_Types].xml`.
    NoContentTypes,
    /// Для части не удалось определить content type.
    UnknownContentType(String),
    /// Имя части не соответствует правилам OPC.
    BadPartName(String),
    /// Запрошенная часть отсутствует в пакете.
    PartNotFound(String),
    /// Часть с таким именем уже есть.
    PartExists(String),
    /// Некорректный файл отношений.
    BadRelationships(String),
    /// Ссылка на несуществующий идентификатор отношения.
    RelationshipNotFound(String),
}

/// Что именно сломано в модели SpreadsheetML.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum XlsxError {
    /// Отсутствует `xl/workbook.xml`.
    NoWorkbook,
    /// Лист с таким именем или индексом отсутствует.
    SheetNotFound(String),
    /// Некорректная ссылка на ячейку.
    BadCellRef(String),
    /// Индекс в таблице общих строк выходит за границы.
    SharedStringOutOfRange(u32),
    /// Неизвестный тип ячейки в атрибуте `t`.
    UnknownCellType(String),
    /// Числовое значение ячейки не разбирается.
    BadNumber(String),
    /// Структура `sheetData` нарушена.
    BadSheetData,
}

/// Что именно сломано в формуле.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FormulaError {
    /// Формула оборвана.
    UnexpectedEof,
    /// Неожиданный символ.
    UnexpectedChar(char),
    /// Незакрытая скобка.
    UnbalancedParen,
    /// Незакрытая строковая литера или имя листа в апострофах.
    UnterminatedString,
    /// Некорректная ссылка A1.
    BadReference,
    /// Слишком глубокая вложенность.
    TooDeep,
}

/// Превышение квоты. Отделено от прочих ошибок, потому что означает не
/// «файл битый», а «файл может быть атакой» — вызывающий может захотеть
/// обработать это иначе.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitError {
    InputTooLarge { got: u64, max: u64 },
    TooManyEntries { got: u64, max: u64 },
    PartTooLarge { got: u64, max: u64 },
    TotalUncompressed { got: u64, max: u64 },
    CompressionRatio { got: u64, max: u64 },
    XmlDepth { got: u64, max: u64 },
    TooManyAttrs { got: u64, max: u64 },
    TooManyNodes { got: u64, max: u64 },
    NameTooLong { got: u64, max: u64 },
}

impl Error {
    /// Оборачивает ошибку именем части пакета, в которой она случилась.
    #[must_use]
    pub fn in_part(self, part: &str) -> Self {
        Self::InPart {
            part: part.to_owned(),
            source: Box::new(self),
        }
    }

    /// `true` — если ошибка вызвана превышением квоты, а не порчей данных.
    #[must_use]
    pub fn is_limit(&self) -> bool {
        match self {
            Self::Limit(_) => true,
            Self::InPart { source, .. } => source.is_limit(),
            _ => false,
        }
    }
}

impl From<LimitError> for Error {
    fn from(e: LimitError) -> Self {
        Self::Limit(e)
    }
}

impl From<OpcError> for Error {
    fn from(e: OpcError) -> Self {
        Self::Opc(e)
    }
}

impl From<XlsxError> for Error {
    fn from(e: XlsxError) -> Self {
        Self::Xlsx(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zip { kind, offset } => write!(f, "zip: {kind:?} на офсете {offset}"),
            Self::Deflate {
                kind,
                in_off,
                out_off,
            } => write!(f, "deflate: {kind:?} (вход {in_off}, выход {out_off})"),
            Self::Xml { kind, offset } => write!(f, "xml: {kind:?} на офсете {offset}"),
            Self::Opc(kind) => write!(f, "opc: {kind:?}"),
            Self::Xlsx(kind) => write!(f, "xlsx: {kind:?}"),
            Self::Formula { kind, pos } => write!(f, "формула: {kind:?} на позиции {pos}"),
            Self::Limit(kind) => write!(f, "превышена квота: {kind:?}"),
            Self::Unsupported(what) => write!(f, "не поддерживается: {what}"),
            Self::InPart { part, source } => write!(f, "в части {part}: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InPart { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_part_wraps_and_preserves_limit_flag() {
        let inner = Error::Limit(LimitError::XmlDepth { got: 300, max: 256 });
        assert!(inner.is_limit());
        let wrapped = inner.in_part("/xl/worksheets/sheet1.xml");
        assert!(wrapped.is_limit());
        assert!(wrapped.to_string().contains("sheet1.xml"));
    }

    #[test]
    fn display_mentions_offset() {
        let e = Error::Xml {
            kind: XmlError::DoctypeForbidden,
            offset: 42,
        };
        let s = e.to_string();
        assert!(s.contains("DoctypeForbidden"), "{s}");
        assert!(s.contains("42"), "{s}");
    }
}
