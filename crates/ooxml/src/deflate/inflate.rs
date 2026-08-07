//! Декодер DEFLATE (RFC 1951).

use crate::deflate::bitreader::BitReader;
use crate::deflate::huffman::HuffTable;
use crate::error::{DeflateError, Error, Result};
use crate::limits::Limits;

/// Базовая длина совпадения для кодов 257..=285 (RFC 1951, таблица §3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Базовое расстояние для кодов 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Порядок, в котором пишутся длины кодов алфавита длин кодов (§3.2.7).
/// Перестановка ставит редко используемые длины в конец, чтобы хвост нулей
/// можно было не передавать.
const CLEN_ORDER: [u8; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Символ конца блока.
const END_OF_BLOCK: u16 = 256;

/// Максимум символов литералов/длин, реально определённых RFC.
const MAX_LIT_SYMBOLS: usize = 286;
/// Максимум символов расстояний.
const MAX_DIST_SYMBOLS: usize = 30;

/// Длины кодов фиксированного дерева литералов/длин (§3.2.6).
fn fixed_lit_lengths() -> [u8; 288] {
    let mut lens = [0u8; 288];
    for (sym, slot) in lens.iter_mut().enumerate() {
        *slot = match sym {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    lens
}

/// Длины кодов фиксированного дерева расстояний: все 32 кода по 5 бит.
/// Коды 30 и 31 существуют в дереве, но недопустимы в потоке — это ловится
/// при поиске базы расстояния.
fn fixed_dist_lengths() -> [u8; 32] {
    [5u8; 32]
}

/// Состояние одного вызова распаковки.
struct Inflater<'a, 'o, 'l> {
    br: BitReader<'a>,
    out: &'o mut Vec<u8>,
    limits: &'l Limits,
    /// Длина `out` на входе — расстояния и лимиты считаются от неё, а не от
    /// начала буфера: вызывающий вправе дописывать в непустой вектор.
    out_start: usize,
    /// Порог выхода, на котором пересчитываются квоты.
    next_check: u64,
}

impl Inflater<'_, '_, '_> {
    fn produced(&self) -> u64 {
        self.out.len().saturating_sub(self.out_start) as u64
    }

    /// Навешивает на ошибку позиции во входе и выходе.
    fn wrap(&self, kind: DeflateError) -> Error {
        Error::Deflate {
            kind,
            in_off: self.br.bit_pos() / 8,
            out_off: self.produced(),
        }
    }

    fn take_bits(&mut self, n: u32) -> Result<u32> {
        match self.br.bits(n) {
            Some(v) => Ok(v),
            None => Err(self.wrap(DeflateError::UnexpectedEof)),
        }
    }

    /// Пересчитывает квоты по фактически произведённому выходу.
    ///
    /// Заявленный в заголовке ZIP размер здесь бесполезен: в бомбе он
    /// правдоподобен, а поток за ним — гигабайты. Поэтому проверка идёт по
    /// тому, что действительно записано, каждые [`Limits::RATIO_CHECK_STRIDE`]
    /// байт.
    #[inline]
    fn check_limits(&mut self) -> Result<()> {
        let produced = self.produced();
        if produced < self.next_check {
            return Ok(());
        }
        self.next_check = produced.saturating_add(Limits::RATIO_CHECK_STRIDE);
        self.limits.check_part_size(produced)?;
        self.limits
            .check_ratio(produced, self.br.bytes_consumed() as u64)
    }

    fn block_stored(&mut self) -> Result<()> {
        self.br.align_to_byte();
        let len = self.take_bits(16)? as u16;
        let nlen = self.take_bits(16)? as u16;
        // Дублирование LEN в инвертированном виде — единственная защита
        // stored-блока от рассинхронизации потока.
        if len != !nlen {
            return Err(self.wrap(DeflateError::BadStoredLength));
        }
        let Some(data) = self.br.take_bytes(usize::from(len)) else {
            return Err(self.wrap(DeflateError::UnexpectedEof));
        };
        self.out.extend_from_slice(data);
        self.check_limits()
    }

    fn copy_match(&mut self, length: usize, distance: usize) -> Result<()> {
        if distance == 0 || (distance as u64) > self.produced() {
            return Err(self.wrap(DeflateError::DistanceTooFar));
        }
        let start = self.out.len().saturating_sub(distance);
        if distance >= length {
            // Источник целиком лежит до текущего конца — можно блоком.
            self.out
                .extend_from_within(start..start.saturating_add(length));
        } else {
            // distance < length: копия читает байты, которые сама же и
            // допишет (развёртка RLE). Только побайтово и только вперёд —
            // блочное копирование прочитало бы ещё не существующие данные.
            for i in 0..length {
                let Some(b) = self.out.get(start.saturating_add(i)).copied() else {
                    break;
                };
                self.out.push(b);
            }
        }
        Ok(())
    }

    fn block_huffman(&mut self, lit: &HuffTable, dist: &HuffTable) -> Result<()> {
        loop {
            let sym = match lit.decode(&mut self.br) {
                Ok(s) => s,
                Err(k) => return Err(self.wrap(k)),
            };

            if sym < END_OF_BLOCK {
                self.out.push(sym as u8);
            } else if sym == END_OF_BLOCK {
                return Ok(());
            } else {
                let li = usize::from(sym).wrapping_sub(257);
                let (Some(&base), Some(&extra)) = (LENGTH_BASE.get(li), LENGTH_EXTRA.get(li))
                else {
                    return Err(self.wrap(DeflateError::InvalidCode));
                };
                let length =
                    usize::from(base).saturating_add(self.take_bits(u32::from(extra))? as usize);

                let dsym = match dist.decode(&mut self.br) {
                    Ok(s) => s,
                    Err(k) => return Err(self.wrap(k)),
                };
                let di = usize::from(dsym);
                let (Some(&dbase), Some(&dextra)) = (DIST_BASE.get(di), DIST_EXTRA.get(di)) else {
                    return Err(self.wrap(DeflateError::InvalidCode));
                };
                let distance =
                    usize::from(dbase).saturating_add(self.take_bits(u32::from(dextra))? as usize);

                self.copy_match(length, distance)?;
            }

            self.check_limits()?;
        }
    }

    /// Читает динамический заголовок и строит оба дерева (§3.2.7).
    fn dynamic_tables(&mut self) -> Result<(HuffTable, HuffTable)> {
        let hlit = (self.take_bits(5)? as usize).saturating_add(257);
        let hdist = (self.take_bits(5)? as usize).saturating_add(1);
        let hclen = (self.take_bits(4)? as usize).saturating_add(4);
        // Пять бит вмещают 288 и 32, но символов сверх 286 и 30 не существует —
        // такой заголовок описывает несуществующий алфавит.
        if hlit > MAX_LIT_SYMBOLS || hdist > MAX_DIST_SYMBOLS {
            return Err(self.wrap(DeflateError::InvalidCode));
        }

        let mut clen = [0u8; 19];
        for i in 0..hclen {
            let v = self.take_bits(3)? as u8;
            if let Some(&pos) = CLEN_ORDER.get(i)
                && let Some(slot) = clen.get_mut(usize::from(pos))
            {
                *slot = v;
            }
        }
        let clen_table = match HuffTable::build(&clen) {
            Ok(t) => t,
            Err(k) => return Err(self.wrap(k)),
        };

        let total = hlit.saturating_add(hdist);
        let mut lens = [0u8; MAX_LIT_SYMBOLS + MAX_DIST_SYMBOLS];
        let mut i = 0usize;
        while i < total {
            let sym = match clen_table.decode(&mut self.br) {
                Ok(s) => s,
                Err(k) => return Err(self.wrap(k)),
            };
            match sym {
                0..=15 => {
                    if let Some(slot) = lens.get_mut(i) {
                        *slot = sym as u8;
                    }
                    i = i.saturating_add(1);
                }
                // 16 повторяет предыдущую длину: если предыдущей нет,
                // повторять нечего — поток сломан.
                16 => {
                    let Some(prev) = i.checked_sub(1).and_then(|p| lens.get(p).copied()) else {
                        return Err(self.wrap(DeflateError::BadCodeLengthRepeat));
                    };
                    let rep = (self.take_bits(2)? as usize).saturating_add(3);
                    i = self.fill_lengths(&mut lens, i, rep, total, prev)?;
                }
                17 => {
                    let rep = (self.take_bits(3)? as usize).saturating_add(3);
                    i = self.fill_lengths(&mut lens, i, rep, total, 0)?;
                }
                18 => {
                    let rep = (self.take_bits(7)? as usize).saturating_add(11);
                    i = self.fill_lengths(&mut lens, i, rep, total, 0)?;
                }
                _ => return Err(self.wrap(DeflateError::InvalidCode)),
            }
        }

        let (Some(lit_lens), Some(dist_lens)) = (lens.get(..hlit), lens.get(hlit..total)) else {
            return Err(self.wrap(DeflateError::BadCodeLengthRepeat));
        };
        let lit = match HuffTable::build(lit_lens) {
            Ok(t) => t,
            Err(k) => return Err(self.wrap(k)),
        };
        let dist = match HuffTable::build(dist_lens) {
            Ok(t) => t,
            Err(k) => return Err(self.wrap(k)),
        };
        Ok((lit, dist))
    }

    /// Записывает `rep` копий `value` начиная с `at`; повтор за границу
    /// массива длин — признак битого потока, а не повод обрезать.
    fn fill_lengths(
        &self,
        lens: &mut [u8],
        at: usize,
        rep: usize,
        total: usize,
        value: u8,
    ) -> Result<usize> {
        let end = at.saturating_add(rep);
        if end > total {
            return Err(self.wrap(DeflateError::BadCodeLengthRepeat));
        }
        let Some(slice) = lens.get_mut(at..end) else {
            return Err(self.wrap(DeflateError::BadCodeLengthRepeat));
        };
        slice.fill(value);
        Ok(end)
    }

    fn run(&mut self) -> Result<usize> {
        loop {
            let bfinal = self.take_bits(1)?;
            let btype = self.take_bits(2)?;
            match btype {
                0 => self.block_stored()?,
                1 => {
                    let lit = match HuffTable::build(&fixed_lit_lengths()) {
                        Ok(t) => t,
                        Err(k) => return Err(self.wrap(k)),
                    };
                    let dist = match HuffTable::build(&fixed_dist_lengths()) {
                        Ok(t) => t,
                        Err(k) => return Err(self.wrap(k)),
                    };
                    self.block_huffman(&lit, &dist)?;
                }
                2 => {
                    let (lit, dist) = self.dynamic_tables()?;
                    self.block_huffman(&lit, &dist)?;
                }
                _ => return Err(self.wrap(DeflateError::ReservedBlockType)),
            }
            if bfinal == 1 {
                break;
            }
        }
        // Размер части досчитывается точно: пороговая проверка могла
        // пропустить последний, неполный отрезок выхода. Коэффициент сжатия
        // здесь намеренно не пересчитывается — на объёме меньше одного шага
        // проверки высокое отношение ничему не угрожает, а вот отвергнуть
        // маленькую, но плотно сжатую часть было бы ложной тревогой.
        self.limits.check_part_size(self.produced())?;
        Ok(self.br.bytes_consumed())
    }
}

/// Распаковывает поток, дописывая результат в `out`.
pub(crate) fn inflate_into(input: &[u8], out: &mut Vec<u8>, limits: &Limits) -> Result<usize> {
    limits.check_input(input.len() as u64)?;
    let out_start = out.len();
    let mut state = Inflater {
        br: BitReader::new(input),
        out,
        limits,
        out_start,
        next_check: Limits::RATIO_CHECK_STRIDE,
    };
    state.run()
}
