//! Open Packaging Conventions — слой между ZIP-контейнером и XML-частями.
//!
//! # Что добавляет этот слой
//!
//! Слой `zip` знает записи, слой `dom` знает деревья. Ни один из них не знает,
//! **какая запись чем является**: что `[Content_Types].xml` — не часть, что
//! `word/` — каталог, что `_rels/.rels` описывает граф ссылок, что
//! `printerSettings1.bin` разбирать как XML нельзя. Это знание живёт здесь.
//!
//! # Главное решение слоя
//!
//! Здесь принимается решение «эту часть переписываем, эту копируем» — и оно
//! принимается в пользу копирования всегда, когда это возможно. Часть, которую
//! прочитали, но не изменили, копируется побайтово. Часть, которую разобрали в
//! дерево и не изменили, копируется побайтово. Часть, которую изменили, а потом
//! вернули к исходному виду, копируется побайтово: перед перепаковкой новые
//! байты сравниваются со старыми.
//!
//! Причина одна и измеренная: наш deflate не воспроизводит поток Office (ноль
//! совпадений из 450, `docs/zip-fidelity.md` §1). Значит, любая перепаковка —
//! это потеря байтовой идентичности, и платить ею можно только за настоящее
//! изменение.
//!
//! # Пример
//!
//! ```no_run
//! use ooxml::Limits;
//! use ooxml::opc::{Package, PartName};
//!
//! # fn demo(data: &[u8]) -> ooxml::Result<()> {
//! let mut pkg = Package::open(data, Limits::strict())?;
//! let sheet = PartName::new("/xl/worksheets/sheet1.xml")?;
//! assert!(pkg.has(&sheet));
//!
//! // Часть не тронута — сохранение вернёт исходный файл байт в байт.
//! assert_eq!(pkg.save()?, data);
//! # Ok(())
//! # }
//! ```

mod content_types;
mod package;
mod partname;
mod rels;

pub use content_types::{CONTENT_TYPES_NAME, CONTENT_TYPES_NS, ContentTypes};
pub use package::Package;
pub use partname::PartName;
pub use rels::{EMPTY_RELS, RELATIONSHIPS_NS, Rel, Rels, TargetMode};
