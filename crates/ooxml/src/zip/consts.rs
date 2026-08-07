//! Константы формата ZIP (APPNOTE.TXT 6.3.10).
//!
//! Вынесены в отдельный файл не ради красоты: опечатка в сигнатуре проявляется
//! как «файл битый на офсете 0», и искать её потом дорого. Одно определение —
//! одно место, где может быть ошибка.

/// `PK\x03\x04` — локальный заголовок файла.
pub(crate) const SIG_LOCAL: u32 = 0x0403_4b50;
/// `PK\x01\x02` — запись центрального каталога.
pub(crate) const SIG_CENTRAL: u32 = 0x0201_4b50;
/// `PK\x05\x06` — End Of Central Directory.
pub(crate) const SIG_EOCD: u32 = 0x0605_4b50;
/// `PK\x06\x06` — zip64 End Of Central Directory record.
pub(crate) const SIG_ZIP64_EOCD: u32 = 0x0606_4b50;
/// `PK\x06\x07` — zip64 End Of Central Directory locator.
pub(crate) const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50;
/// `PK\x07\x08` — data descriptor.
///
/// Единственная сигнатура формата, которая **необязательна**: APPNOTE 4.3.9.3
/// разрешает дескриптор без неё, и такие архивы встречаются. Поэтому форму
/// дескриптора приходится определять по содержимому, а не по наличию сигнатуры.
pub(crate) const SIG_DATA_DESCRIPTOR: u32 = 0x0807_4b50;

/// Сигнатура EOCD в виде байт — для поиска сканированием с конца.
pub(crate) const EOCD_SIG_BYTES: [u8; 4] = [b'P', b'K', 5, 6];

/// Фиксированная часть локального заголовка (без имени и extra).
pub(crate) const LOCAL_HEADER_LEN: usize = 30;
/// Фиксированная часть записи центрального каталога.
pub(crate) const CENTRAL_HEADER_LEN: usize = 46;
/// Фиксированная часть EOCD (без комментария архива).
pub(crate) const EOCD_LEN: usize = 22;
/// zip64 EOCD locator — всегда ровно 20 байт, поля расширения не предусмотрены.
pub(crate) const ZIP64_LOCATOR_LEN: usize = 20;
/// Содержательная часть zip64 EOCD record без «extensible data sector».
///
/// Поле `size of zip64 end of central directory record` считается **от байта
/// после самого себя**, поэтому полный размер записи — `12 + size`, а
/// `size >= ZIP64_EOCD_FIXED`.
pub(crate) const ZIP64_EOCD_FIXED: usize = 44;
/// Минимальный полный размер zip64 EOCD record: 4 (сигнатура) + 8 (size) + 44.
pub(crate) const ZIP64_EOCD_LEN: usize = 56;

/// Максимальная длина комментария архива — поле длины 16-битное.
pub(crate) const MAX_COMMENT: usize = 0xFFFF;

/// bit 0 — данные зашифрованы.
pub(crate) const FLAG_ENCRYPTED: u16 = 0x0001;
/// bit 3 — crc и размеры вынесены в data descriptor после данных.
pub(crate) const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
/// bit 6 — strong encryption (APPNOTE 7.x). Тоже шифрование, тоже отказ.
pub(crate) const FLAG_STRONG_ENCRYPTION: u16 = 0x0040;
/// bit 11 — имя и комментарий в UTF-8, иначе CP437.
pub(crate) const FLAG_UTF8: u16 = 0x0800;

pub(crate) const METHOD_STORE: u16 = 0;
pub(crate) const METHOD_DEFLATE: u16 = 8;

/// Идентификатор «zip64 extended information extra field».
///
/// Единственный extra-заголовок, который ядро интерпретирует. Всё остальное —
/// включая 520-байтовый `0xA220` «Open Packaging Growth Hint» от MS Office —
/// непрозрачные байты, которые копируются как есть.
pub(crate) const EXTRA_ZIP64: u16 = 0x0001;

/// Значение-маркер «настоящее значение лежит в zip64 extra».
pub(crate) const U32_MARKER: u32 = 0xFFFF_FFFF;
/// То же для 16-битных полей (номер диска, число записей).
pub(crate) const U16_MARKER: u16 = 0xFFFF;
