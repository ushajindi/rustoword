//! Канонические коды Хаффмана и двухуровневая таблица просмотра.
//!
//! Дерево задаётся только длинами кодов (RFC 1951 §3.2.2) — сами коды
//! восстанавливаются однозначно. Отсюда же берутся проверки корректности:
//! сумма 2^-len должна равняться единице.
//!
//! # Почему таблица, а не спуск по дереву
//!
//! Побитовый спуск даёт тот же ответ, но тратит одно обращение к памяти на
//! каждый бит кода — при средней длине 8–9 бит это на порядок медленнее.
//! Корневая таблица на [`ROOT_BITS`] бит разбирает подавляющее большинство
//! кодов реального потока за одно чтение; редкие длинные коды уходят в
//! подтаблицы, и только для них нужно второе обращение. Один уровень на все 15
//! бит потребовал бы 32768 записей на каждый блок — перестроение таблицы стало
//! бы дороже самой распаковки коротких блоков.

use crate::deflate::bitreader::BitReader;
use crate::error::DeflateError;

/// Сколько бит разбирает корневая таблица.
const ROOT_BITS: u32 = 9;

/// Максимальная длина кода в DEFLATE.
const MAX_BITS: u32 = 15;

/// Признак ссылки на подтаблицу в записи таблицы.
const LINK: u32 = 1 << 31;

/// Запись-лист: длина кода в битах 16..=20, символ в битах 0..=15.
/// Нулевая запись означает «кода нет» — так помечены дырки неполного дерева.
const fn leaf_entry(sym: u16, len: u32) -> u32 {
    (len << 16) | (sym as u32)
}

/// Запись-ссылка: `LINK`, разрядность подтаблицы в 24..=28, офсет в 0..=23.
const fn link_entry(off: usize, bits: u32) -> u32 {
    LINK | (bits << 24) | ((off as u32) & 0x00FF_FFFF)
}

/// Разворачивает младшие `len` бит.
///
/// Код Хаффмана записан в поток старшим битом вперёд, а ридер отдаёт биты
/// младшим вперёд — развернуть код один раз при построении дешевле, чем
/// разворачивать прочитанное на каждом символе.
fn reverse_bits(code: u32, len: u32) -> u32 {
    code.reverse_bits().wrapping_shr(32u32.wrapping_sub(len))
}

/// Декодер одного алфавита Хаффмана.
#[derive(Debug, Clone)]
pub(crate) struct HuffTable {
    entries: Vec<u32>,
    root: u32,
}

impl HuffTable {
    /// Строит таблицу по длинам кодов; индекс в срезе — номер символа.
    pub(crate) fn build(lengths: &[u8]) -> Result<Self, DeflateError> {
        let mut count = [0u32; 16];
        for &l in lengths {
            // Длины приходят из 4-битных полей, так что промах невозможен;
            // `get_mut` тут вместо индексации, а не вместо проверки.
            if let Some(c) = count.get_mut(usize::from(l)) {
                *c = c.saturating_add(1);
            }
        }
        if let Some(c) = count.get_mut(0) {
            *c = 0;
        }

        let mut max = 0u32;
        let mut total = 0u32;
        for len in 1..=MAX_BITS {
            let c = count.get(len as usize).copied().unwrap_or(0);
            if c > 0 {
                max = len;
            }
            total = total.saturating_add(c);
        }

        // Неравенство Крафта: `left` — сколько кодов данной длины ещё свободно.
        let mut left: i64 = 1;
        for len in 1..=MAX_BITS {
            let c = count.get(len as usize).copied().unwrap_or(0);
            left = left.saturating_mul(2).saturating_sub(i64::from(c));
            if left < 0 {
                return Err(DeflateError::OversubscribedTree);
            }
        }
        // Неполное дерево из единственного кода — законная и часто
        // встречающаяся форма дерева расстояний: запись, где все совпадения
        // идут на одно и то же расстояние. Отвергать её нельзя.
        if left > 0 && total > 1 {
            return Err(DeflateError::IncompleteTree);
        }

        let root = ROOT_BITS.min(max).max(1);
        let root_size = 1usize << root;
        let mut entries = vec![0u32; root_size];

        // Первый код каждой длины по правилу канонической нумерации.
        let mut next_code = [0u32; 17];
        let mut code = 0u32;
        for len in 1..=MAX_BITS as usize {
            let prev = count.get(len.wrapping_sub(1)).copied().unwrap_or(0);
            code = code.saturating_add(prev).wrapping_shl(1);
            if let Some(slot) = next_code.get_mut(len) {
                *slot = code;
            }
        }

        // Коды длиннее корня — их немного, поэтому проще собрать в вектор и
        // пройти дважды, чем повторять всю нумерацию.
        let mut long: Vec<(u16, u32, u32)> = Vec::new();
        for (sym, &l) in lengths.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let len = u32::from(l);
            let Some(nc) = next_code.get_mut(len as usize) else {
                continue;
            };
            let code = *nc;
            *nc = nc.saturating_add(1);
            let rev = reverse_bits(code, len);
            let Ok(sym) = u16::try_from(sym) else {
                continue;
            };

            if len <= root {
                // Код короче корня — он покрывает целое семейство индексов,
                // различающихся старшими битами, которые в него не входят.
                let leaf = leaf_entry(sym, len);
                let step = 1usize << len;
                let mut i = rev as usize;
                while i < root_size {
                    if let Some(e) = entries.get_mut(i) {
                        *e = leaf;
                    }
                    i = i.saturating_add(step);
                }
            } else {
                long.push((sym, len, rev));
            }
        }

        if !long.is_empty() {
            Self::fill_subtables(&mut entries, &long, root, root_size);
        }

        Ok(Self { entries, root })
    }

    fn fill_subtables(
        entries: &mut Vec<u32>,
        long: &[(u16, u32, u32)],
        root: u32,
        root_size: usize,
    ) {
        // Разрядность подтаблицы задаёт самый длинный код с тем же корневым
        // префиксом — короче нельзя, длиннее значит впустую занятая память.
        let mut sub_bits = vec![0u32; root_size];
        for &(_, len, rev) in long {
            let prefix = (rev as usize) & root_size.wrapping_sub(1);
            let need = len.saturating_sub(root);
            if let Some(b) = sub_bits.get_mut(prefix) {
                *b = (*b).max(need);
            }
        }

        let mut sub_off = vec![0usize; root_size];
        for prefix in 0..root_size {
            let bits = sub_bits.get(prefix).copied().unwrap_or(0);
            if bits == 0 {
                continue;
            }
            let off = entries.len();
            entries.resize(off.saturating_add(1usize << bits), 0);
            if let Some(s) = sub_off.get_mut(prefix) {
                *s = off;
            }
            if let Some(e) = entries.get_mut(prefix) {
                *e = link_entry(off, bits);
            }
        }

        for &(sym, len, rev) in long {
            let prefix = (rev as usize) & root_size.wrapping_sub(1);
            let bits = sub_bits.get(prefix).copied().unwrap_or(0);
            let off = sub_off.get(prefix).copied().unwrap_or(0);
            let leaf = leaf_entry(sym, len);
            let step = 1usize << len.saturating_sub(root);
            let size = 1usize << bits;
            let mut i = (rev as usize).wrapping_shr(root);
            while i < size {
                if let Some(e) = entries.get_mut(off.saturating_add(i)) {
                    *e = leaf;
                }
                i = i.saturating_add(step);
            }
        }
    }

    /// Декодирует один символ, потребляя из потока ровно длину его кода.
    pub(crate) fn decode(&self, br: &mut BitReader<'_>) -> Result<u16, DeflateError> {
        let look = br.peek(self.root) as usize;
        let mut e = self.entries.get(look).copied().unwrap_or(0);

        if e & LINK != 0 {
            let bits = (e >> 24) & 0x1F;
            let off = (e & 0x00FF_FFFF) as usize;
            // Индекс в подтаблице — биты, лежащие за корневым префиксом.
            let idx = br
                .peek(self.root.saturating_add(bits))
                .wrapping_shr(self.root) as usize;
            e = self
                .entries
                .get(off.saturating_add(idx))
                .copied()
                .unwrap_or(0);
        }

        let len = (e >> 16) & 0x1F;
        if e & LINK != 0 || len == 0 {
            return Err(DeflateError::InvalidCode);
        }
        br.consume(len).ok_or(DeflateError::UnexpectedEof)?;
        Ok((e & 0xFFFF) as u16)
    }
}

#[cfg(test)]
mod tests {
    // В тестах паника — это способ сообщить о провале, а не дефект.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    /// Кодирует последовательность канонических кодов в поток LSB-first.
    fn encode(codes: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u64;
        let mut n = 0u32;
        for &(code, len) in codes {
            // Код пишется старшим битом вперёд.
            for i in (0..len).rev() {
                acc |= u64::from((code >> i) & 1) << n;
                n += 1;
            }
            while n >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                n -= 8;
            }
        }
        if n > 0 {
            out.push(acc as u8);
        }
        out
    }

    #[test]
    fn rfc_example_tree() {
        // RFC 1951 §3.2.2: длины 3,3,3,3,3,2,4,4 дают коды
        // A=010 B=011 C=100 D=101 E=110 F=00 G=1110 H=1111.
        let t = HuffTable::build(&[3, 3, 3, 3, 3, 2, 4, 4]).unwrap();
        let stream = encode(&[(0b010, 3), (0b00, 2), (0b1111, 4), (0b1110, 4)]);
        let mut br = BitReader::new(&stream);
        assert_eq!(t.decode(&mut br).unwrap(), 0);
        assert_eq!(t.decode(&mut br).unwrap(), 5);
        assert_eq!(t.decode(&mut br).unwrap(), 7);
        assert_eq!(t.decode(&mut br).unwrap(), 6);
    }

    #[test]
    fn oversubscribed_is_rejected() {
        // Три кода длины 1 в двоичное дерево не помещаются.
        assert_eq!(
            HuffTable::build(&[1, 1, 1]).err(),
            Some(DeflateError::OversubscribedTree)
        );
    }

    #[test]
    fn incomplete_with_many_symbols_is_rejected() {
        assert_eq!(
            HuffTable::build(&[1, 2, 3]).err(),
            Some(DeflateError::IncompleteTree)
        );
    }

    #[test]
    fn single_code_of_length_one_is_accepted() {
        // Дерево расстояний из одного символа — законная форма, встречается
        // в реальных потоках.
        let t = HuffTable::build(&[1]).unwrap();
        let mut br = BitReader::new(&[0x00]);
        assert_eq!(t.decode(&mut br).unwrap(), 0);
        // Вторая половина кодового пространства не занята — там ошибка.
        let mut br = BitReader::new(&[0xFF]);
        assert_eq!(t.decode(&mut br), Err(DeflateError::InvalidCode));
    }

    #[test]
    fn empty_tree_decodes_nothing() {
        let t = HuffTable::build(&[0, 0, 0]).unwrap();
        let mut br = BitReader::new(&[0xAA, 0xBB]);
        assert_eq!(t.decode(&mut br), Err(DeflateError::InvalidCode));
    }

    #[test]
    fn long_codes_go_through_subtable() {
        // 15 символов с длинами 1..=14 и ещё один той же длины 14 —
        // полное дерево, где половина кодов длиннее корня в 9 бит.
        let mut lens = Vec::new();
        for len in 1..=14u8 {
            lens.push(len);
        }
        lens.push(14);
        let t = HuffTable::build(&lens).unwrap();

        // Канонические коды: символ i длины i+1 имеет код 2^(i+1)-2.
        let mut codes = Vec::new();
        for (sym, &len) in lens.iter().enumerate() {
            let len = u32::from(len);
            let code = if sym < 14 {
                (1u32 << len).wrapping_sub(2)
            } else {
                (1u32 << len).wrapping_sub(1)
            };
            codes.push((code, len));
        }
        let stream = encode(&codes);
        let mut br = BitReader::new(&stream);
        for sym in 0..lens.len() {
            assert_eq!(t.decode(&mut br).unwrap(), sym as u16, "символ {sym}");
        }
    }

    #[test]
    fn truncated_stream_reports_eof_not_panic() {
        let t = HuffTable::build(&[3, 3, 3, 3, 3, 2, 4, 4]).unwrap();
        let mut br = BitReader::new(&[]);
        assert_eq!(t.decode(&mut br), Err(DeflateError::UnexpectedEof));
    }
}
