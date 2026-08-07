//! Блочный энкодер DEFLATE (RFC 1951).
//!
//! # Как выбирается тип блока
//!
//! Для каждого блока считается стоимость всех трёх представлений — stored,
//! fixed и dynamic — и берётся минимальная. Без этого маленькие входы
//! раздувались бы: одно только динамическое дерево стоит несколько десятков
//! байт, и на строке в двадцать байт stored выигрывает у него втрое.
//! Оценка точная, а не приблизительная: частоты символов уже посчитаны, а
//! заголовок динамического блока строится до записи, так что его длина
//! известна до бита.
//!
//! # Таблицы длин и расстояний
//!
//! Продублированы из `inflate`, а не вынесены в общий модуль: там они читаются
//! «символ → база», здесь нужно обратное отображение «длина → символ», и
//! общего у двух наборов только числа. Значения сверяются тестом
//! `tables_match_the_decoder`.

use crate::deflate::Level;
use crate::deflate::bitwriter::BitWriter;
use crate::deflate::lz77::{Lz77, MAX_MATCH, Token};

/// Символов в алфавите литералов и длин (0..=285).
const NUM_LIT: usize = 286;
/// Символов в алфавите расстояний.
const NUM_DIST: usize = 30;
/// Символов в алфавите длин кодовых длин.
const NUM_CLEN: usize = 19;
/// Символ конца блока.
const END_OF_BLOCK: usize = 256;
/// Предел длины кода в DEFLATE.
const MAX_CODE_BITS: usize = 15;
/// Предел длины кода в алфавите длин кодовых длин (поле HCLEN — 3 бита).
const MAX_CLEN_BITS: usize = 7;
/// Максимальная длина stored-блока: поле LEN шестнадцатибитное.
const STORED_MAX: usize = 65_535;

/// Базовая длина совпадения для кодов 257..=285.
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

/// Порядок передачи длин кодов алфавита длин кодов (§3.2.7).
const CLEN_ORDER: [u8; NUM_CLEN] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Номер кода длины для каждой длины совпадения 3..=258.
///
/// Индекс в [`LENGTH_BASE`], а не сам символ: так таблица укладывается в байт.
const LEN_INDEX: [u8; MAX_MATCH + 1] = build_len_index();

// Границы задаются самими таблицами и проверяются условиями циклов; функция
// вычисляется компилятором, так что выход за границу был бы ошибкой сборки.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
const fn build_len_index() -> [u8; MAX_MATCH + 1] {
    let mut t = [0u8; MAX_MATCH + 1];
    let mut i = 0;
    while i < 29 {
        let base = LENGTH_BASE[i] as usize;
        // Символ покрывает диапазон в 2^extra длин; последний (258) — ровно
        // одну, и он затирает хвост предыдущего диапазона, идя после него.
        let span = 1usize << LENGTH_EXTRA[i];
        let mut k = 0;
        while k < span && base + k <= MAX_MATCH {
            t[base + k] = i as u8;
            k += 1;
        }
        i += 1;
    }
    t
}

/// Длины кодов фиксированного дерева литералов/длин (§3.2.6).
const FIXED_LIT_LENS: [u8; 288] = build_fixed_lit_lens();

#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
const fn build_fixed_lit_lens() -> [u8; 288] {
    let mut t = [0u8; 288];
    let mut i = 0;
    while i < 288 {
        t[i] = if i < 144 {
            8
        } else if i < 256 {
            9
        } else if i < 280 {
            7
        } else {
            8
        };
        i += 1;
    }
    t
}

/// Фиксированное дерево расстояний: все 32 кода по пять бит.
///
/// Коды 30 и 31 в потоке недопустимы, но существуют в дереве — без них
/// каноническая нумерация оставшихся кодов оказалась бы другой.
const FIXED_DIST_LENS: [u8; 32] = [5; 32];

/// Номер кода расстояния.
///
/// Считается арифметикой, а не таблицей: таблица на 32768 входов заняла бы
/// больше кэша, чем экономит, а формула — это разрядность `dist - 1` плюс
/// один бит, отделяющий половину диапазона.
fn dist_index(dist: usize) -> usize {
    if dist <= 4 {
        return dist.saturating_sub(1);
    }
    let v = (dist.saturating_sub(1)) as u32;
    let bits = 32u32.saturating_sub(v.leading_zeros());
    let half = (v >> bits.saturating_sub(2)) & 1;
    (bits.saturating_sub(1))
        .saturating_mul(2)
        .saturating_add(half) as usize
}

/// Частоты символов блока.
#[derive(Debug)]
struct Freqs {
    lit: [u32; NUM_LIT],
    dist: [u32; NUM_DIST],
    /// Дополнительные биты длин и расстояний. Деревом они не кодируются и
    /// одинаковы для fixed и dynamic, но в стоимость блока входят.
    extra: u64,
}

impl Freqs {
    fn count(tokens: &[Token]) -> Self {
        let mut f = Self {
            lit: [0; NUM_LIT],
            dist: [0; NUM_DIST],
            extra: 0,
        };
        // Конец блока присутствует в любом сжатом блоке ровно один раз.
        if let Some(c) = f.lit.get_mut(END_OF_BLOCK) {
            *c = 1;
        }
        for t in tokens {
            let d = usize::from(t.dist());
            let v = usize::from(t.value());
            if d == 0 {
                if let Some(c) = f.lit.get_mut(v) {
                    *c = c.saturating_add(1);
                }
                continue;
            }
            let li = LEN_INDEX.get(v).copied().unwrap_or(0) as usize;
            if let Some(c) = f.lit.get_mut(li.saturating_add(257)) {
                *c = c.saturating_add(1);
            }
            let di = dist_index(d);
            if let Some(c) = f.dist.get_mut(di) {
                *c = c.saturating_add(1);
            }
            let le = LENGTH_EXTRA.get(li).copied().unwrap_or(0);
            let de = DIST_EXTRA.get(di).copied().unwrap_or(0);
            f.extra = f
                .extra
                .saturating_add(u64::from(le))
                .saturating_add(u64::from(de));
        }
        f
    }
}

/// Длины и коды одного алфавита.
#[derive(Debug)]
struct Alphabet<const N: usize> {
    lens: [u8; N],
    codes: [u16; N],
}

impl<const N: usize> Alphabet<N> {
    fn from_lens(lens: [u8; N]) -> Self {
        let mut codes = [0u16; N];
        build_codes(&lens, &mut codes);
        Self { lens, codes }
    }

    /// Стоимость символа в битах; ноль означает «символа нет в дереве».
    fn width(&self, sym: usize) -> u64 {
        u64::from(self.lens.get(sym).copied().unwrap_or(0))
    }

    fn put(&self, bw: &mut BitWriter, sym: usize) {
        let (Some(&len), Some(&code)) = (self.lens.get(sym), self.codes.get(sym)) else {
            return;
        };
        bw.write_bits(u32::from(code), u32::from(len));
    }
}

/// Присваивает канонические коды по длинам.
///
/// Порядок — тот же, что у декодера (RFC 1951 §3.2.2): сначала по возрастанию
/// длины, внутри длины — по номеру символа. Код сразу разворачивается, потому
/// что в поток он ложится старшим битом вперёд, а writer пишет младшими.
fn build_codes(lens: &[u8], out: &mut [u16]) {
    let mut count = [0u32; MAX_CODE_BITS + 1];
    for &l in lens {
        if let Some(c) = count.get_mut(usize::from(l)) {
            *c = c.saturating_add(1);
        }
    }
    if let Some(c) = count.get_mut(0) {
        *c = 0;
    }

    let mut next = [0u32; MAX_CODE_BITS + 2];
    let mut code = 0u32;
    for len in 1..=MAX_CODE_BITS {
        let prev = count.get(len.saturating_sub(1)).copied().unwrap_or(0);
        code = code.saturating_add(prev).wrapping_shl(1);
        if let Some(slot) = next.get_mut(len) {
            *slot = code;
        }
    }

    for (sym, &l) in lens.iter().enumerate() {
        if l == 0 {
            continue;
        }
        let len = u32::from(l);
        let Some(slot) = next.get_mut(usize::from(l)) else {
            continue;
        };
        let code = *slot;
        *slot = slot.saturating_add(1);
        if let Some(o) = out.get_mut(sym) {
            *o = reverse_bits(code, len);
        }
    }
}

/// Разворачивает младшие `len` бит кода.
fn reverse_bits(code: u32, len: u32) -> u16 {
    if len == 0 || len > 16 {
        return 0;
    }
    (code.reverse_bits() >> (32u32.saturating_sub(len))) as u16
}

/// Сколько уровней глубины различает построитель дерева.
///
/// Оптимальное дерево Хаффмана глубже `d` требует суммарной частоты не меньше
/// `fib(d + 2)`; при блоке в 65 000 токенов это даёт глубину меньше 25. Сорока
/// восьми хватает с запасом, а срыв за границу всё равно обрезается.
const DEPTH_SLOTS: usize = 48;

/// Ссылка «родителя нет».
const NO_PARENT: u32 = u32::MAX;

/// Считает длины кодов Хаффмана по частотам, не длиннее `max_bits`.
///
/// Символы с нулевой частотой получают длину ноль — в поток они не попадут.
fn code_lengths(freq: &[u32], max_bits: usize, out: &mut [u8]) {
    for slot in out.iter_mut() {
        *slot = 0;
    }

    // Используемые символы по возрастанию частоты; при равных частотах — по
    // номеру символа, иначе результат зависел бы от порядка обхода.
    let mut used: Vec<(u32, u16)> = freq
        .iter()
        .enumerate()
        .filter(|&(_, &f)| f > 0)
        .filter_map(|(s, &f)| u16::try_from(s).ok().map(|s| (f, s)))
        .collect();
    used.sort_unstable();

    match used.len() {
        0 => return,
        1 => {
            // Дерево из одного кода. Вызывающий такого не допускает, но
            // обрабатывать вход обязаны все ветки.
            if let Some(&(_, sym)) = used.first()
                && let Some(slot) = out.get_mut(usize::from(sym))
            {
                *slot = 1;
            }
            return;
        }
        _ => {}
    }

    let mut count = optimal_depths(&used);
    limit_depth(&mut count, max_bits, used.len());

    // Раздача длин: самому частому символу — самый короткий код. Это не хуже
    // любого другого распределения того же набора длин (неравенство о
    // перестановках), а обход с конца отсортированного списка делает выбор
    // однозначным.
    let mut it = used.iter().rev();
    for len in 1..=max_bits {
        let n = count.get(len).copied().unwrap_or(0);
        for _ in 0..n {
            let Some(&(_, sym)) = it.next() else {
                break;
            };
            if let Some(slot) = out.get_mut(usize::from(sym)) {
                *slot = len as u8;
            }
        }
    }
}

/// Считает, сколько кодов каждой длины даёт оптимальное дерево Хаффмана.
///
/// Само дерево не нужно: длину конкретному символу назначает вызывающий, и
/// для стоимости важен только набор длин.
///
/// Слияние двух очередей вместо кучи: листья уже отсортированы, а внутренние
/// узлы рождаются в порядке неубывания веса — значит обе очереди читаются с
/// головы, и куча ничего не добавила бы.
fn optimal_depths(sorted: &[(u32, u16)]) -> [u32; DEPTH_SLOTS] {
    let mut count = [0u32; DEPTH_SLOTS];
    let n = sorted.len();
    if n < 2 {
        if let Some(c) = count.get_mut(usize::from(n == 1)) {
            *c = n as u32;
        }
        return count;
    }

    let mut parent = vec![NO_PARENT; n.saturating_mul(2).saturating_sub(1)];
    let mut internal: Vec<u64> = Vec::with_capacity(n.saturating_sub(1));
    let mut leaf = 0usize;
    let mut inode = 0usize;

    for k in 0..n.saturating_sub(1) {
        let (a, wa) = pick_min(sorted, &internal, &mut leaf, &mut inode, n);
        let (b, wb) = pick_min(sorted, &internal, &mut leaf, &mut inode, n);
        let node = n.saturating_add(k);
        for child in [a, b] {
            if let Some(p) = parent.get_mut(child) {
                *p = node as u32;
            }
        }
        internal.push(wa.saturating_add(wb));
    }

    for i in 0..n {
        let mut depth = 0usize;
        let mut cur = i;
        while let Some(&p) = parent.get(cur) {
            if p == NO_PARENT {
                break;
            }
            depth = depth.saturating_add(1);
            cur = p as usize;
            if depth >= DEPTH_SLOTS.saturating_sub(1) {
                break;
            }
        }
        if let Some(c) = count.get_mut(depth) {
            *c = c.saturating_add(1);
        }
    }
    count
}

/// Достаёт узел с наименьшим весом из очереди листьев или внутренних узлов.
///
/// При равенстве берётся лист: так дерево получается мельче, а значит реже
/// приходится ограничивать глубину.
fn pick_min(
    sorted: &[(u32, u16)],
    internal: &[u64],
    leaf: &mut usize,
    inode: &mut usize,
    n: usize,
) -> (usize, u64) {
    let lw = sorted.get(*leaf).map(|&(f, _)| u64::from(f));
    let iw = internal.get(*inode).copied();
    match (lw, iw) {
        (Some(l), Some(i)) if l <= i => {
            let id = *leaf;
            *leaf = leaf.saturating_add(1);
            (id, l)
        }
        (Some(l), None) => {
            let id = *leaf;
            *leaf = leaf.saturating_add(1);
            (id, l)
        }
        (_, Some(i)) => {
            let id = n.saturating_add(*inode);
            *inode = inode.saturating_add(1);
            (id, i)
        }
        // Узлов всегда ровно столько, сколько запрашивает цикл слияния;
        // ветка нужна только чтобы обойтись без паники.
        (None, None) => (0, 0),
    }
}

/// Ограничивает глубину дерева, сохраняя равенство Крафта.
///
/// # Почему эвристика, а не package-merge
///
/// Package-merge даёт оптимальный код с ограниченной глубиной. Мерилом здесь
/// служит то, как часто и на каком алфавите предел вообще достигается. На
/// корпусе (606 записей, 14,6 МБ) ограничение сработало 49 раз и **все 49 —
/// на алфавите длин кодовых длин**, где предел равен семи битам при девятнадцати
/// символах. Ни разу — на пятнадцатибитных деревьях литералов и расстояний:
/// глубина больше пятнадцати требует частот с фибоначчиевым разбросом
/// (1, 1, 2, 3, 5, ... 46368 на блок), а в XML таких не бывает.
///
/// То есть оптимизировать здесь нечего: срабатывает предел на алфавите из
/// девятнадцати символов, где разница между оптимальным и почти оптимальным
/// кодом — единицы бит на блок. Package-merge принёс бы полторы сотни строк
/// ради этих единиц.
///
/// Взят приём zlib: слишком длинные коды сначала подтягиваются к пределу
/// (дерево становится перенасыщенным), затем избыток раздаётся вниз шагами
/// «лист с уровня b уходит на b+1, и туда же переезжает один код с предельного
/// уровня». Каждый такой шаг меняет сумму Крафта ровно на -2^-max, что
/// делает завершение очевидным. Результат всё равно проверяется на точное
/// равенство: если оно не достигнуто, берётся заведомо корректное плоское
/// дерево — оно хуже по сжатию, но валидно всегда. На корпусе и на миллиарде
/// байт фаззера этот откат не срабатывал ни разу.
fn limit_depth(count: &mut [u32; DEPTH_SLOTS], max_bits: usize, n_used: usize) {
    if max_bits == 0 || max_bits >= DEPTH_SLOTS {
        return;
    }

    let mut overflow = 0u32;
    for len in max_bits.saturating_add(1)..DEPTH_SLOTS {
        let c = count.get(len).copied().unwrap_or(0);
        overflow = overflow.saturating_add(c);
        if let Some(slot) = count.get_mut(len) {
            *slot = 0;
        }
    }
    if overflow == 0 {
        return;
    }
    if let Some(slot) = count.get_mut(max_bits) {
        *slot = slot.saturating_add(overflow);
    }

    let total: u64 = 1u64 << max_bits;
    let mut kraft = kraft_sum(count, max_bits);

    // Каждый шаг уменьшает сумму на единицу, а сумма — целое, поэтому цикл
    // конечен; счётчик страхует лишь от невозможного состояния счётчиков.
    let mut guard = kraft.saturating_add(1);
    while kraft > total && guard > 0 {
        guard = guard.saturating_sub(1);
        let Some(b) = (1..max_bits)
            .rev()
            .find(|&l| count.get(l).copied().unwrap_or(0) > 0)
        else {
            break;
        };
        if count.get(max_bits).copied().unwrap_or(0) == 0 {
            break;
        }
        if let Some(slot) = count.get_mut(b) {
            *slot = slot.saturating_sub(1);
        }
        if let Some(slot) = count.get_mut(b.saturating_add(1)) {
            *slot = slot.saturating_add(2);
        }
        if let Some(slot) = count.get_mut(max_bits) {
            *slot = slot.saturating_sub(1);
        }
        kraft = kraft.saturating_sub(1);
    }

    if kraft != total {
        flat_depths(count, max_bits, n_used);
    }
}

/// Сумма Крафта в единицах 2^-max_bits.
fn kraft_sum(count: &[u32; DEPTH_SLOTS], max_bits: usize) -> u64 {
    let mut sum = 0u64;
    for len in 1..=max_bits {
        let c = u64::from(count.get(len).copied().unwrap_or(0));
        sum = sum.saturating_add(c.wrapping_shl((max_bits.saturating_sub(len)) as u32));
    }
    sum
}

/// Плоское полное дерево на `n` листьев — страховочный вариант.
///
/// `2^k - n` листьев на глубине `k-1` и `2n - 2^k` на глубине `k`, где
/// `k = ceil(log2 n)`. Сумма Крафта равна единице по построению, а глубина не
/// превышает девяти для любого алфавита DEFLATE.
fn flat_depths(count: &mut [u32; DEPTH_SLOTS], max_bits: usize, n: usize) {
    for slot in count.iter_mut() {
        *slot = 0;
    }
    if n < 2 {
        if let Some(slot) = count.get_mut(1) {
            *slot = n as u32;
        }
        return;
    }
    let n64 = n as u64;
    let mut k = 1usize;
    while (1u64 << k) < n64 && k < max_bits {
        k = k.saturating_add(1);
    }
    let pow = 1u64 << k;
    let short = pow.saturating_sub(n64);
    let long = n64.saturating_mul(2).saturating_sub(pow);
    if let Some(slot) = count.get_mut(k.saturating_sub(1)) {
        *slot = short as u32;
    }
    if let Some(slot) = count.get_mut(k) {
        *slot = long as u32;
    }
}

/// Гарантирует, что в алфавите не меньше двух используемых символов.
///
/// Дерево из единственного кода формально допустимо, и наш распаковщик его
/// принимает, но часть распаковщиков в природе на нём спотыкается. Два кода по
/// одному биту стоят несколько лишних бит в заголовке и снимают вопрос.
fn force_two_symbols(freq: &mut [u32]) {
    let mut used = freq.iter().filter(|&&f| f > 0).count();
    if used >= 2 {
        return;
    }
    // Добираем ровно до двух, начиная с младших номеров: лишний символ в
    // дереве расстояний поднял бы HDIST и удлинил заголовок ни за что.
    for slot in freq.iter_mut() {
        if used >= 2 {
            break;
        }
        if *slot == 0 {
            *slot = 1;
            used = used.saturating_add(1);
        }
    }
}

/// Динамические деревья блока.
#[derive(Debug)]
struct Trees {
    lit: Alphabet<NUM_LIT>,
    dist: Alphabet<NUM_DIST>,
}

impl Trees {
    fn build(freqs: &Freqs) -> Self {
        let mut lit_freq = freqs.lit;
        let mut dist_freq = freqs.dist;
        force_two_symbols(&mut lit_freq);
        force_two_symbols(&mut dist_freq);

        let mut lit_lens = [0u8; NUM_LIT];
        let mut dist_lens = [0u8; NUM_DIST];
        code_lengths(&lit_freq, MAX_CODE_BITS, &mut lit_lens);
        code_lengths(&dist_freq, MAX_CODE_BITS, &mut dist_lens);

        Self {
            lit: Alphabet::from_lens(lit_lens),
            dist: Alphabet::from_lens(dist_lens),
        }
    }
}

/// Заголовок динамического блока.
#[derive(Debug)]
struct Header {
    hlit: usize,
    hdist: usize,
    hclen: usize,
    /// Поток алфавита длин кодов: символ, значение дополнительных бит и их число.
    ops: Vec<(u8, u8, u8)>,
    clen: Alphabet<NUM_CLEN>,
    /// Полная длина заголовка в битах, без трёх бит типа блока.
    bits: u64,
}

impl Header {
    fn build(trees: &Trees) -> Self {
        let hlit = used_upto(&trees.lit.lens, 257);
        let hdist = used_upto(&trees.dist.lens, 1);

        let mut seq: Vec<u8> = Vec::with_capacity(hlit.saturating_add(hdist));
        seq.extend(trees.lit.lens.iter().take(hlit));
        seq.extend(trees.dist.lens.iter().take(hdist));

        let (ops, mut clen_freq) = rle_lengths(&seq);
        force_two_symbols(&mut clen_freq);
        let mut clen_lens = [0u8; NUM_CLEN];
        code_lengths(&clen_freq, MAX_CLEN_BITS, &mut clen_lens);

        // Хвост нулевых длин в переставленном порядке не передаётся — ради
        // этого перестановка HCLEN и существует.
        let mut hclen = NUM_CLEN;
        while hclen > 4 {
            let idx = CLEN_ORDER
                .get(hclen.saturating_sub(1))
                .copied()
                .unwrap_or(0);
            if clen_lens.get(usize::from(idx)).copied().unwrap_or(0) != 0 {
                break;
            }
            hclen = hclen.saturating_sub(1);
        }

        let clen = Alphabet::from_lens(clen_lens);
        let mut bits = 14u64.saturating_add((hclen as u64).saturating_mul(3));
        for &(sym, _, width) in &ops {
            bits = bits
                .saturating_add(clen.width(usize::from(sym)))
                .saturating_add(u64::from(width));
        }

        Self {
            hlit,
            hdist,
            hclen,
            ops,
            clen,
            bits,
        }
    }

    fn emit(&self, bw: &mut BitWriter) {
        bw.write_bits(self.hlit.saturating_sub(257) as u32, 5);
        bw.write_bits(self.hdist.saturating_sub(1) as u32, 5);
        bw.write_bits(self.hclen.saturating_sub(4) as u32, 4);
        for &idx in CLEN_ORDER.iter().take(self.hclen) {
            let l = self.clen.lens.get(usize::from(idx)).copied().unwrap_or(0);
            bw.write_bits(u32::from(l), 3);
        }
        for &(sym, value, width) in &self.ops {
            self.clen.put(bw, usize::from(sym));
            bw.write_bits(u32::from(value), u32::from(width));
        }
    }
}

/// Номер последнего используемого символа плюс один, но не меньше `min`.
fn used_upto(lens: &[u8], min: usize) -> usize {
    let last = lens
        .iter()
        .rposition(|&l| l != 0)
        .map_or(0, |i| i.saturating_add(1));
    last.max(min)
}

/// Сворачивает последовательность длин кодов в поток символов 0..=18 (§3.2.7).
fn rle_lengths(seq: &[u8]) -> (Vec<(u8, u8, u8)>, [u32; NUM_CLEN]) {
    let mut ops: Vec<(u8, u8, u8)> = Vec::new();
    let mut freq = [0u32; NUM_CLEN];
    let bump = |freq: &mut [u32; NUM_CLEN], sym: u8| {
        if let Some(c) = freq.get_mut(usize::from(sym)) {
            *c = c.saturating_add(1);
        }
    };

    let mut i = 0usize;
    while i < seq.len() {
        let Some(&v) = seq.get(i) else {
            break;
        };
        let mut run = 1usize;
        while seq.get(i.saturating_add(run)) == Some(&v) {
            run = run.saturating_add(1);
        }
        i = i.saturating_add(run);

        if v == 0 {
            let mut left = run;
            while left >= 11 {
                // Остаток в один-два нуля пришлось бы выдавать по отдельности;
                // дешевле укоротить повтор так, чтобы хвост забрал символ 17.
                let take = if left > 138 && left.saturating_sub(138) < 3 {
                    left.saturating_sub(3)
                } else {
                    left.min(138)
                };
                ops.push((18, take.saturating_sub(11) as u8, 7));
                bump(&mut freq, 18);
                left = left.saturating_sub(take);
            }
            if left >= 3 {
                ops.push((17, left.saturating_sub(3) as u8, 3));
                bump(&mut freq, 17);
                left = 0;
            }
            for _ in 0..left {
                ops.push((0, 0, 0));
                bump(&mut freq, 0);
            }
        } else {
            ops.push((v, 0, 0));
            bump(&mut freq, v);
            let mut left = run.saturating_sub(1);
            while left >= 3 {
                let take = left.min(6);
                ops.push((16, take.saturating_sub(3) as u8, 2));
                bump(&mut freq, 16);
                left = left.saturating_sub(take);
            }
            for _ in 0..left {
                ops.push((v, 0, 0));
                bump(&mut freq, v);
            }
        }
    }
    (ops, freq)
}

/// Стоимость потока символов блока в битах при заданных деревьях.
fn stream_bits(freqs: &Freqs, lit_lens: &[u8], dist_lens: &[u8]) -> u64 {
    let mut bits = freqs.extra;
    for (sym, &f) in freqs.lit.iter().enumerate() {
        if f == 0 {
            continue;
        }
        let w = u64::from(lit_lens.get(sym).copied().unwrap_or(0));
        bits = bits.saturating_add(u64::from(f).saturating_mul(w));
    }
    for (sym, &f) in freqs.dist.iter().enumerate() {
        if f == 0 {
            continue;
        }
        let w = u64::from(dist_lens.get(sym).copied().unwrap_or(0));
        bits = bits.saturating_add(u64::from(f).saturating_mul(w));
    }
    bits
}

/// Пишет токены блока и символ конца блока.
fn emit_tokens(
    bw: &mut BitWriter,
    tokens: &[Token],
    lit: &Alphabet<NUM_LIT>,
    dist: &Alphabet<NUM_DIST>,
) {
    for t in tokens {
        let d = usize::from(t.dist());
        let v = usize::from(t.value());
        if d == 0 {
            lit.put(bw, v);
            continue;
        }
        let li = LEN_INDEX.get(v).copied().unwrap_or(0) as usize;
        lit.put(bw, li.saturating_add(257));
        let base = LENGTH_BASE.get(li).copied().unwrap_or(3);
        let width = LENGTH_EXTRA.get(li).copied().unwrap_or(0);
        bw.write_bits(v.saturating_sub(usize::from(base)) as u32, u32::from(width));

        let di = dist_index(d);
        dist.put(bw, di);
        let dbase = DIST_BASE.get(di).copied().unwrap_or(1);
        let dwidth = DIST_EXTRA.get(di).copied().unwrap_or(0);
        bw.write_bits(
            d.saturating_sub(usize::from(dbase)) as u32,
            u32::from(dwidth),
        );
    }
    lit.put(bw, END_OF_BLOCK);
}

/// То же для фиксированного дерева расстояний — оно на 32 символа.
fn emit_tokens_fixed(
    bw: &mut BitWriter,
    tokens: &[Token],
    lit: &Alphabet<288>,
    dist: &Alphabet<32>,
) {
    for t in tokens {
        let d = usize::from(t.dist());
        let v = usize::from(t.value());
        if d == 0 {
            lit.put(bw, v);
            continue;
        }
        let li = LEN_INDEX.get(v).copied().unwrap_or(0) as usize;
        lit.put(bw, li.saturating_add(257));
        let base = LENGTH_BASE.get(li).copied().unwrap_or(3);
        let width = LENGTH_EXTRA.get(li).copied().unwrap_or(0);
        bw.write_bits(v.saturating_sub(usize::from(base)) as u32, u32::from(width));

        let di = dist_index(d);
        dist.put(bw, di);
        let dbase = DIST_BASE.get(di).copied().unwrap_or(1);
        let dwidth = DIST_EXTRA.get(di).copied().unwrap_or(0);
        bw.write_bits(
            d.saturating_sub(usize::from(dbase)) as u32,
            u32::from(dwidth),
        );
    }
    lit.put(bw, END_OF_BLOCK);
}

/// Пишет один stored-блок. Участок обязан быть не длиннее [`STORED_MAX`].
fn emit_stored(bw: &mut BitWriter, data: &[u8], start: usize, end: usize, last: bool) {
    bw.write_bits(u32::from(last), 1);
    bw.write_bits(0, 2);
    bw.align_to_byte();
    let len = end.saturating_sub(start).min(STORED_MAX);
    bw.write_bits(len as u32, 16);
    // NLEN — инвертированная LEN; на ней распаковщик ловит рассинхронизацию.
    bw.write_bits(!(len as u32) & 0xFFFF, 16);
    if let Some(s) = data.get(start..start.saturating_add(len)) {
        bw.write_bytes(s);
    }
}

/// Пишет участок входа цепочкой stored-блоков, закрывая последний финальным.
fn emit_stored_run(bw: &mut BitWriter, data: &[u8], start: usize, end: usize) {
    let mut p = start;
    loop {
        let chunk_end = p.saturating_add(STORED_MAX).min(end);
        let last = chunk_end >= end;
        emit_stored(bw, data, p, chunk_end, last);
        if last {
            break;
        }
        p = chunk_end;
    }
}

/// Выбирает представление блока и пишет его.
fn emit_block(
    bw: &mut BitWriter,
    data: &[u8],
    start: usize,
    end: usize,
    tokens: &[Token],
    last: bool,
) {
    let freqs = Freqs::count(tokens);

    let trees = Trees::build(&freqs);
    let header = Header::build(&trees);
    let dynamic_bits = 3u64.saturating_add(header.bits).saturating_add(stream_bits(
        &freqs,
        &trees.lit.lens,
        &trees.dist.lens,
    ));

    let fixed_bits = 3u64.saturating_add(stream_bits(&freqs, &FIXED_LIT_LENS, &FIXED_DIST_LENS));

    // Stored выравнивается на байт, поэтому его цена зависит от того, на каком
    // бите поток застали. Три бита заголовка, добивка, LEN, NLEN и сами данные.
    let span = end.saturating_sub(start);
    let stored_bits = if span <= STORED_MAX {
        let after_header = bw.bit_len().saturating_add(3);
        let pad = after_header.wrapping_neg() % 8;
        Some(
            3u64.saturating_add(pad)
                .saturating_add(32)
                .saturating_add((span as u64).saturating_mul(8)),
        )
    } else {
        None
    };

    if let Some(sb) = stored_bits
        && sb <= dynamic_bits
        && sb <= fixed_bits
    {
        emit_stored(bw, data, start, end, last);
        return;
    }

    bw.write_bits(u32::from(last), 1);
    if fixed_bits <= dynamic_bits {
        bw.write_bits(1, 2);
        let lit = Alphabet::from_lens(FIXED_LIT_LENS);
        let dist = Alphabet::from_lens(FIXED_DIST_LENS);
        emit_tokens_fixed(bw, tokens, &lit, &dist);
    } else {
        bw.write_bits(2, 2);
        header.emit(bw);
        emit_tokens(bw, tokens, &trees.lit, &trees.dist);
    }
}

/// Сжимает вход в поток DEFLATE.
pub(crate) fn compress(input: &[u8], level: Level) -> Vec<u8> {
    // Позиции в хеш-цепочках хранятся как `u32`. Вход, не помещающийся в этот
    // диапазон, на 32-битных целях невозможен, а на 64-битных означал бы тихую
    // порчу данных — такой уходит в stored, где позиций нет вовсе.
    if level == Level::Store || u32::try_from(input.len()).is_err() {
        let mut bw = BitWriter::with_capacity(input.len().saturating_add(64));
        emit_stored_run(&mut bw, input, 0, input.len());
        return bw.finish();
    }

    let mut bw = BitWriter::with_capacity(input.len().wrapping_div(2).saturating_add(64));
    let mut lz = Lz77::new(input, level);
    let mut tokens: Vec<Token> = Vec::new();

    loop {
        let (start, end) = lz.next_block(&mut tokens);
        let last = lz.is_done();
        if end <= start && !last {
            // Блок не продвинулся. По построению невозможно, но зациклиться
            // хуже, чем недожать: остаток уходит stored-блоками.
            emit_stored_run(&mut bw, input, start, input.len());
            return bw.finish();
        }
        emit_block(&mut bw, input, start, end, &tokens, last);
        if last {
            break;
        }
    }
    bw.finish()
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
    use crate::deflate::lz77::MIN_MATCH;

    #[test]
    fn tables_match_the_decoder() {
        // Каждая длина 3..=258 обязана попадать в свой диапазон.
        for (len, &code) in LEN_INDEX.iter().enumerate().skip(MIN_MATCH) {
            let i = code as usize;
            let base = LENGTH_BASE[i] as usize;
            let span = 1usize << LENGTH_EXTRA[i];
            assert!(
                (base..base + span).contains(&len) || len == 258,
                "длина {len} попала в код {i}"
            );
            assert!(len >= base, "длина {len} меньше базы кода {i}");
        }
        assert_eq!(LEN_INDEX[258], 28);
        assert_eq!(LEN_INDEX[3], 0);

        // То же для расстояний.
        for &d in &[
            1usize, 2, 3, 4, 5, 6, 7, 8, 9, 100, 4096, 24577, 32767, 32768,
        ] {
            let i = dist_index(d);
            let base = DIST_BASE[i] as usize;
            let span = 1usize << DIST_EXTRA[i];
            assert!(
                (base..base + span).contains(&d),
                "расстояние {d} попало в код {i} с базой {base}"
            );
        }
    }

    #[test]
    fn every_distance_lands_in_its_range() {
        for d in 1usize..=32_768 {
            let i = dist_index(d);
            assert!(i < NUM_DIST, "расстояние {d} дало код {i}");
            let base = DIST_BASE[i] as usize;
            let span = 1usize << DIST_EXTRA[i];
            assert!((base..base + span).contains(&d), "расстояние {d}");
        }
    }

    /// Сумма Крафта набора длин в единицах 2^-15.
    fn kraft(lens: &[u8]) -> u64 {
        lens.iter()
            .filter(|&&l| l > 0)
            .map(|&l| 1u64 << (15 - l))
            .sum()
    }

    #[test]
    fn code_lengths_are_complete_and_bounded() {
        let cases: Vec<Vec<u32>> = vec![
            vec![1, 1],
            vec![5, 1, 1, 1],
            vec![1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987],
            (0..286u32).map(|i| i + 1).collect(),
            (0..286u32)
                .map(|i| if i % 3 == 0 { 1000 } else { 1 })
                .collect(),
        ];
        for freq in &cases {
            let mut lens = vec![0u8; freq.len()];
            code_lengths(freq, MAX_CODE_BITS, &mut lens);
            assert!(lens.iter().all(|&l| l <= 15), "код длиннее 15 бит");
            assert_eq!(kraft(&lens), 1 << 15, "дерево не полное: {freq:?}");
            for (i, &f) in freq.iter().enumerate() {
                assert_eq!(f > 0, lens[i] > 0, "символ {i}: частота и длина разошлись");
            }
        }
    }

    #[test]
    fn depth_limit_survives_fibonacci_frequencies() {
        // Фибоначчиевы частоты — единственный способ загнать дерево глубже
        // пятнадцати бит. Здесь ветка ограничения действительно исполняется.
        let mut freq = vec![1u32, 1];
        while freq.len() < 40 {
            let n = freq.len();
            freq.push(freq[n - 1] + freq[n - 2]);
        }
        let mut lens = vec![0u8; freq.len()];
        code_lengths(&freq, MAX_CODE_BITS, &mut lens);
        assert!(lens.iter().all(|&l| l <= 15), "{lens:?}");
        assert_eq!(kraft(&lens), 1 << 15);
    }

    #[test]
    fn clen_tree_stays_within_seven_bits() {
        let mut freq = vec![1u32, 1];
        while freq.len() < NUM_CLEN {
            let n = freq.len();
            freq.push(freq[n - 1] + freq[n - 2]);
        }
        let mut lens = vec![0u8; NUM_CLEN];
        code_lengths(&freq, MAX_CLEN_BITS, &mut lens);
        assert!(lens.iter().all(|&l| l <= 7), "{lens:?}");
        assert_eq!(kraft(&lens), 1 << 15);
    }

    #[test]
    fn flat_tree_is_a_valid_fallback() {
        for n in 2usize..=286 {
            let mut count = [0u32; DEPTH_SLOTS];
            flat_depths(&mut count, MAX_CODE_BITS, n);
            let total: u32 = count.iter().sum();
            assert_eq!(total as usize, n, "потеряны листья при n={n}");
            let kraft = kraft_sum(&count, MAX_CODE_BITS);
            assert_eq!(kraft, 1 << MAX_CODE_BITS, "n={n}");
        }
    }

    #[test]
    fn canonical_codes_are_prefix_free() {
        let lens: [u8; 8] = [3, 3, 3, 3, 3, 2, 4, 4];
        let mut codes = [0u16; 8];
        build_codes(&lens, &mut codes);
        // Развёрнутый код: проверяем, что ни один не является префиксом другого,
        // сравнивая младшие биты.
        for i in 0..8 {
            for j in 0..8 {
                if i == j {
                    continue;
                }
                let (li, lj) = (lens[i] as u32, lens[j] as u32);
                let short = li.min(lj);
                let mask = (1u16 << short) - 1;
                assert_ne!(
                    codes[i] & mask,
                    codes[j] & mask,
                    "коды {i} и {j} имеют общий префикс"
                );
            }
        }
    }

    #[test]
    fn rle_reproduces_the_length_sequence() {
        let cases: Vec<Vec<u8>> = vec![
            vec![0; 300],
            vec![4; 300],
            {
                let mut v = vec![0u8; 140];
                v.extend([3, 3, 3, 3, 3, 3, 3, 0, 0, 5]);
                v
            },
            (0..280u16).map(|i| (i % 16) as u8).collect(),
        ];
        for seq in &cases {
            let (ops, _) = rle_lengths(seq);
            let mut out: Vec<u8> = Vec::new();
            for &(sym, value, _) in &ops {
                match sym {
                    16 => {
                        let last = *out.last().unwrap();
                        for _ in 0..(value as usize + 3) {
                            out.push(last);
                        }
                    }
                    17 => out.extend(std::iter::repeat_n(0u8, value as usize + 3)),
                    18 => out.extend(std::iter::repeat_n(0u8, value as usize + 11)),
                    v => out.push(v),
                }
            }
            assert_eq!(out, *seq, "RLE исказил последовательность длин");
        }
    }
}
