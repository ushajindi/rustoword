//! Поиск совпадений LZ77 по хеш-цепочкам.
//!
//! # Устройство
//!
//! На каждую позицию входа считается хеш трёх байт. `head[hash]` хранит
//! последнюю позицию с таким хешем, `prev[pos & WMASK]` — предыдущую; вместе
//! они образуют односвязную цепочку кандидатов, упорядоченную от свежих к
//! старым. Поиск идёт по цепочке и обрывается, как только кандидат уходит за
//! окно.
//!
//! # Почему длина цепочки ограничена жёстко
//!
//! В XML сплошь и рядом встречаются тысячи одинаковых отступов и повторяющихся
//! закрывающих тегов. Все такие позиции попадают в одну цепочку, и полный её
//! обход превращает поиск в квадратичный по размеру входа: на файле в мегабайт
//! это часы. Предел [`Config::max_chain`] делает время линейным, а потеря в
//! сжатии измеряется долями процента — самые свежие кандидаты в цепочке идут
//! первыми, а они же дают самые короткие расстояния.
//!
//! # Ленивое сопоставление
//!
//! Жадный выбор берёт первое найденное совпадение, даже если со следующей
//! позиции начинается более длинное. Ленивая схема откладывает решение на один
//! байт: если с `pos + 1` совпадение длиннее, байт с `pos` выдаётся литералом.
//! Это стандартный приём zlib и стоит он одного дополнительного поиска.

use crate::deflate::Level;

/// Минимальная длина совпадения (RFC 1951 §3.2.5): пара «длина+расстояние»
/// занимает не меньше десятка бит, и на двух байтах она заведомо проигрывает
/// двум литералам.
pub(crate) const MIN_MATCH: usize = 3;

/// Максимальная длина совпадения.
pub(crate) const MAX_MATCH: usize = 258;

/// Размер окна поиска.
const WSIZE: usize = 32 * 1024;

/// Маска для индексации `prev` по модулю окна.
const WMASK: usize = WSIZE - 1;

/// Разрядность хеш-таблицы. 32768 корзин на окно в 32 КиБ — по корзине на
/// позицию: меньше даёт длинные цепочки из несовпадающих троек.
const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// Признак пустой ячейки цепочки.
const NIL: u32 = u32::MAX;

/// Дальше этого расстояния совпадение из трёх байт невыгодно: код расстояния
/// на таком удалении тянет за собой семь и больше дополнительных бит, и вся
/// пара обходится дороже трёх литералов.
const TOO_FAR: usize = 4096;

/// Сколько байт входа набирает один блок.
///
/// Верхний предел — 65535: ровно столько вмещает поле длины stored-блока, а
/// stored обязан оставаться доступным вариантом для любого блока. Запас в 500
/// байт покрывает совпадение, начавшееся у самой границы (до 258 байт).
const BLOCK_BYTES: usize = 65_000;

/// Сколько токенов набирает один блок.
///
/// Дерево Хаффмана строится на блок целиком, поэтому чем длиннее блок, тем
/// хуже дерево следует за сменой характера данных. 32768 — компромисс: на
/// типичной XML-части это 20–40 КиБ входа.
const BLOCK_TOKENS: usize = 32_768;

/// Один элемент потока LZ77.
///
/// Упакован в `u32`: токенов в блоке бывают десятки тысяч, и шесть байт на
/// каждый значили бы вдвое больше промахов кэша при подсчёте частот.
/// Расстояние ноль у совпадения невозможно — оно и служит признаком литерала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token(u32);

impl Token {
    pub(crate) const fn literal(byte: u8) -> Self {
        Self(byte as u32)
    }

    pub(crate) const fn match_ref(len: u16, dist: u16) -> Self {
        Self(((dist as u32) << 16) | len as u32)
    }

    /// Расстояние совпадения; ноль — литерал.
    pub(crate) const fn dist(self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// Байт литерала либо длина совпадения.
    pub(crate) const fn value(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }
}

/// Настройки поиска, зависящие от уровня.
#[derive(Debug, Clone, Copy)]
struct Config {
    /// Использовать ленивое сопоставление.
    lazy: bool,
    /// С этой длины совпадение считается хорошим и цепочка режется вчетверо.
    good_len: usize,
    /// Пока отложенное совпадение короче — искать замену со следующей позиции.
    max_lazy: usize,
    /// Совпадение такой длины признаётся достаточным, поиск обрывается.
    nice_len: usize,
    /// Предел числа кандидатов в цепочке.
    max_chain: u32,
}

impl Config {
    const fn for_level(level: Level) -> Self {
        match level {
            // Store сюда не доходит — сжатия там нет вовсе; настройки берутся
            // как у Fast, чтобы ветка была тотальной без лишней паники.
            Level::Store | Level::Fast => Self {
                lazy: false,
                good_len: 4,
                max_lazy: 0,
                nice_len: 16,
                max_chain: 8,
            },
            Level::Default => Self {
                lazy: true,
                good_len: 8,
                max_lazy: 16,
                nice_len: 128,
                max_chain: 128,
            },
            // Отличие Best от Default — только глубина перебора. Поднимать
            // заодно `max_lazy` (как это делает zlib на девятке) нельзя: на
            // корпусе это стабильно ухудшало результат на полтора процента.
            // Причина в том, что ленивая замена уже длинного совпадения на
            // чуть более длинное, но вчетверо более далёкое выигрывает пару
            // бит на длине и проигрывает больше на расстоянии — поиск
            // совпадений не видит стоимости кода, которым их потом запишут.
            Level::Best => Self {
                lazy: true,
                good_len: 8,
                max_lazy: 16,
                nice_len: MAX_MATCH,
                max_chain: 512,
            },
        }
    }
}

/// Состояние поиска, переживающее границы блоков.
///
/// Хеш-таблица общая на весь вход: совпадение вправе смотреть за начало
/// текущего блока — окно LZ77 и блочная разбивка независимы.
#[derive(Debug)]
pub(crate) struct Lz77<'a> {
    data: &'a [u8],
    head: Vec<u32>,
    prev: Vec<u32>,
    pos: usize,
    cfg: Config,
}

impl<'a> Lz77<'a> {
    pub(crate) fn new(data: &'a [u8], level: Level) -> Self {
        Self {
            data,
            head: vec![NIL; HASH_SIZE],
            prev: vec![NIL; WSIZE],
            pos: 0,
            cfg: Config::for_level(level),
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Хеш трёх байт с позиции `at`.
    ///
    /// Мультипликативный, а не сдвиговая свёртка zlib: в XML тройки байт
    /// различаются в основном младшими разрядами (пробелы, `<`, `/`), и
    /// умножение размазывает их по всему индексу, тогда как свёртка сдвигами
    /// оставляет их в одной корзине.
    fn hash_at(&self, at: usize) -> Option<usize> {
        let end = at.checked_add(MIN_MATCH)?;
        let s = self.data.get(at..end)?;
        let (Some(&a), Some(&b), Some(&c)) = (s.first(), s.get(1), s.get(2)) else {
            return None;
        };
        let v = (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c);
        Some((v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize)
    }

    /// Ставит позицию в голову её цепочки и возвращает прежнюю голову.
    fn insert(&mut self, at: usize) -> u32 {
        let Some(h) = self.hash_at(at) else {
            return NIL;
        };
        let Some(slot) = self.head.get_mut(h) else {
            return NIL;
        };
        let prev_head = *slot;
        *slot = at as u32;
        if let Some(p) = self.prev.get_mut(at & WMASK) {
            *p = prev_head;
        }
        prev_head
    }

    /// Ищет самое длинное совпадение с позицией `pos`, начиная с кандидата
    /// `head`. Возвращает `(0, 0)`, если ничего лучше `prev_len` не нашлось.
    // Все индексы получены из длин срезов и проверенных сравнений, счётчики
    // ограничены MAX_MATCH и max_chain — переполниться тут нечему.
    #[allow(clippy::arithmetic_side_effects)]
    fn longest_match(&self, pos: usize, head: u32, prev_len: usize) -> (usize, usize) {
        let data = self.data;
        let max_len = data.len().saturating_sub(pos).min(MAX_MATCH);
        if max_len < MIN_MATCH {
            return (0, 0);
        }

        let mut best_len = prev_len.max(MIN_MATCH - 1);
        let mut best_dist = 0usize;
        // Ровно WSIZE не берём намеренно: `prev` индексируется по модулю окна,
        // и запись позиции `pos - WSIZE` к этому моменту уже могла быть занята
        // позицией `pos`. Потеря — одно предельное расстояние из 32768.
        let limit = (pos + 1).saturating_sub(WSIZE);
        let mut chain = if prev_len >= self.cfg.good_len {
            // Длинное совпадение уже есть; выигрыш от дальнейшего перебора
            // измеряется байтами, а стоимость — сотнями сравнений.
            self.cfg.max_chain >> 2
        } else {
            self.cfg.max_chain
        };

        let mut cur = head;
        while chain > 0 && cur != NIL {
            let cand = cur as usize;
            if cand < limit {
                break;
            }
            // Отбраковка одним сравнением: если байт на позиции `best_len` не
            // совпал, кандидат заведомо не длиннее уже найденного, и трогать
            // остальные байты незачем.
            if data.get(cand + best_len) == data.get(pos + best_len) {
                let len = common_len(data, cand, pos, max_len);
                if len > best_len {
                    best_len = len;
                    best_dist = pos - cand;
                    if len >= self.cfg.nice_len || len >= max_len {
                        break;
                    }
                }
            }
            let next = self.prev.get(cand & WMASK).copied().unwrap_or(NIL);
            // Цепочка обязана строго убывать. Если это не так — запись
            // устарела и перезаписана; идти по ней значит зациклиться.
            if next != NIL && next as usize >= cand {
                break;
            }
            cur = next;
            chain -= 1;
        }

        if best_len < MIN_MATCH || best_dist == 0 {
            (0, 0)
        } else {
            (best_len, best_dist)
        }
    }

    /// Отсеивает совпадения, которые в коде выйдут дороже литералов.
    fn worth_it(len: usize, dist: usize) -> bool {
        len >= MIN_MATCH && !(len == MIN_MATCH && dist > TOO_FAR)
    }

    /// Хеширует позиции внутри только что выданного совпадения.
    ///
    /// Пропустить их нельзя: это ровно те тройки, на которые будут ссылаться
    /// последующие поиски, и без них сжатие повторяющегося текста разваливается.
    fn advance_over_match(&mut self, len: usize) {
        let limit = self.data.len().saturating_sub(MIN_MATCH);
        let mut rest = len.saturating_sub(2);
        while rest > 0 {
            self.pos = self.pos.saturating_add(1);
            if self.pos <= limit {
                self.insert(self.pos);
            }
            rest = rest.saturating_sub(1);
        }
        self.pos = self.pos.saturating_add(1);
    }

    /// Набирает токены очередного блока.
    ///
    /// Возвращает границы участка входа, который блок покрывает: энкодеру они
    /// нужны, чтобы оценить и при случае выдать блок как stored.
    pub(crate) fn next_block(&mut self, tokens: &mut Vec<Token>) -> (usize, usize) {
        tokens.clear();
        let start = self.pos;
        if self.cfg.lazy {
            self.fill_lazy(tokens);
        } else {
            self.fill_greedy(tokens);
        }
        (start, self.pos)
    }

    fn fill_greedy(&mut self, tokens: &mut Vec<Token>) {
        let data = self.data;
        let mut covered = 0usize;

        while self.pos < data.len() {
            let head = self.insert(self.pos);
            let (len, dist) = if head == NIL {
                (0, 0)
            } else {
                self.longest_match(self.pos, head, MIN_MATCH - 1)
            };

            if Self::worth_it(len, dist) {
                tokens.push(Token::match_ref(len as u16, dist as u16));
                covered = covered.saturating_add(len);
                // Первая позиция совпадения уже вставлена выше, поэтому
                // дохешировать надо на одну меньше, чем в ленивой ветке.
                self.advance_over_match(len.saturating_add(1));
            } else {
                let Some(&b) = data.get(self.pos) else {
                    break;
                };
                tokens.push(Token::literal(b));
                covered = covered.saturating_add(1);
                self.pos = self.pos.saturating_add(1);
            }

            if covered >= BLOCK_BYTES || tokens.len() >= BLOCK_TOKENS {
                break;
            }
        }
    }

    fn fill_lazy(&mut self, tokens: &mut Vec<Token>) {
        let data = self.data;
        let mut covered = 0usize;

        // Отложенный литерал ленивой схемы. За границу блока он не переносится:
        // иначе конец блока разошёлся бы с концом покрытого участка входа, а
        // stored-вариант блока пишет именно этот участок.
        let mut pending = false;
        let mut match_len = 0usize;
        let mut match_dist = 0usize;

        while self.pos < data.len() {
            let head = self.insert(self.pos);
            let prev_len = match_len;
            let prev_dist = match_dist;
            match_len = 0;
            match_dist = 0;

            // Отложенное совпадение уже достаточно длинное — искать замену
            // со следующей позиции невыгодно.
            if head != NIL && prev_len < self.cfg.max_lazy {
                let (len, dist) = self.longest_match(self.pos, head, prev_len);
                if Self::worth_it(len, dist) {
                    match_len = len;
                    match_dist = dist;
                }
            }

            if prev_len >= MIN_MATCH && match_len <= prev_len {
                // Со следующей позиции лучше не стало — выдаём отложенное
                // совпадение; оно начинается на байт раньше текущей позиции.
                tokens.push(Token::match_ref(prev_len as u16, prev_dist as u16));
                covered = covered.saturating_add(prev_len);
                self.advance_over_match(prev_len);
                pending = false;
                match_len = 0;
                match_dist = 0;
            } else if pending {
                if let Some(&b) = data.get(self.pos.saturating_sub(1)) {
                    tokens.push(Token::literal(b));
                    covered = covered.saturating_add(1);
                }
                self.pos = self.pos.saturating_add(1);
            } else {
                pending = true;
                self.pos = self.pos.saturating_add(1);
            }

            if covered >= BLOCK_BYTES || tokens.len() >= BLOCK_TOKENS {
                break;
            }
        }

        // Хвост: отложенный литерал выдаётся как есть. Совпадение, найденное
        // с той же позиции, здесь уже неприменимо — оно начиналось бы на байт
        // раньше, а тот байт только что стал литералом.
        if pending && let Some(&b) = data.get(self.pos.saturating_sub(1)) {
            tokens.push(Token::literal(b));
        }
    }
}

/// Длина общего префикса двух позиций, но не больше `max`.
// Индексы получены из проверенных срезов, счётчик ограничен `max` ≤ 258.
#[allow(clippy::arithmetic_side_effects)]
fn common_len(data: &[u8], a: usize, b: usize, max: usize) -> usize {
    let mut n = 0usize;
    // Восьмёрками: средняя длина совпадения в разы больше байта, а срез из
    // восьми байт сравнивается одним вызовом memcmp вместо восьми ветвлений.
    while n + 8 <= max {
        let (Some(x), Some(y)) = (data.get(a + n..a + n + 8), data.get(b + n..b + n + 8)) else {
            break;
        };
        if x != y {
            break;
        }
        n += 8;
    }
    while n < max {
        let (Some(&x), Some(&y)) = (data.get(a + n), data.get(b + n)) else {
            break;
        };
        if x != y {
            break;
        }
        n += 1;
    }
    n
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

    /// Разворачивает поток токенов обратно в байты — так проверяется, что
    /// поиск совпадений не соврал ни в длине, ни в расстоянии.
    fn expand(tokens: &[Token]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for t in tokens {
            let d = usize::from(t.dist());
            if d == 0 {
                out.push(t.value() as u8);
            } else {
                let start = out.len() - d;
                for i in 0..usize::from(t.value()) {
                    out.push(out[start + i]);
                }
            }
        }
        out
    }

    fn tokenize_all(data: &[u8], level: Level) -> Vec<Token> {
        let mut lz = Lz77::new(data, level);
        let mut all = Vec::new();
        let mut buf = Vec::new();
        let mut end = 0;
        loop {
            let (s, e) = lz.next_block(&mut buf);
            assert_eq!(s, end, "блоки обязаны идти встык");
            all.extend_from_slice(&buf);
            end = e;
            if lz.is_done() {
                break;
            }
            assert!(e > s, "блок обязан продвигаться");
        }
        assert_eq!(end, data.len(), "блоки обязаны покрыть весь вход");
        all
    }

    #[test]
    fn tokens_reproduce_input_on_every_level() {
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"abcabcabcabc".to_vec(),
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec(),
            b"<w:p><w:r><w:t>hi</w:t></w:r></w:p><w:p><w:r><w:t>hi</w:t></w:r></w:p>".to_vec(),
            (0..5000u32).map(|i| (i % 7) as u8).collect(),
            vec![b'x'; 100_000],
        ];
        for level in [Level::Fast, Level::Default, Level::Best] {
            for data in &cases {
                let tokens = tokenize_all(data, level);
                assert_eq!(
                    expand(&tokens),
                    *data,
                    "уровень {level:?}, {} байт",
                    data.len()
                );
            }
        }
    }

    #[test]
    fn distances_stay_inside_the_window() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        for level in [Level::Fast, Level::Default, Level::Best] {
            for t in tokenize_all(&data, level) {
                assert!(usize::from(t.dist()) < WSIZE, "расстояние вне окна");
                if t.dist() != 0 {
                    let len = usize::from(t.value());
                    assert!((MIN_MATCH..=MAX_MATCH).contains(&len), "длина {len}");
                }
            }
        }
    }

    #[test]
    fn blocks_never_exceed_the_stored_limit() {
        // Длина stored-блока — поле в 16 бит; блок длиннее туда не влезет.
        let data: Vec<u8> = (0..500_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let mut lz = Lz77::new(&data, Level::Default);
        let mut buf = Vec::new();
        loop {
            let (s, e) = lz.next_block(&mut buf);
            assert!(e - s <= 65_535, "блок в {} байт не влезет в stored", e - s);
            if lz.is_done() {
                break;
            }
        }
    }

    #[test]
    fn long_run_is_found_as_one_match() {
        let mut data = b"seed".to_vec();
        data.extend(std::iter::repeat_n(b'z', 1000));
        let tokens = tokenize_all(&data, Level::Best);
        assert!(
            tokens
                .iter()
                .any(|t| t.dist() != 0 && t.value() == MAX_MATCH as u16),
            "прогон из одного байта обязан давать совпадения предельной длины"
        );
    }
}
