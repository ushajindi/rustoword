//! Разбор формулы методом восхождения по приоритетам.
//!
//! # Приоритеты Excel, от самого сильного к самому слабому
//!
//! | уровень | операторы | замечание |
//! |---|---|---|
//! | 8 | `:` | построение диапазона |
//! | 7 | ` ` | пересечение — **оператором служит сам пробел** |
//! | 6 | `-x`, `+x` | унарные префиксы |
//! | 5 | `^` | |
//! | 4 | `*` `/` | |
//! | 3 | `+` `-` | |
//! | 2 | `&` | |
//! | 1 | `=` `<>` `<` `<=` `>` `>=` | |
//!
//! Две строки этой таблицы расходятся с интуицией, и обе стоят разобранных
//! часов:
//!
//! 1. **Унарный минус сильнее степени.** `-2^2` в Excel равно `4`, потому что
//!    читается как `(-2)^2`. В математике и почти во всех языках наоборот.
//! 2. **Ссылочные операторы сильнее унарного минуса.** `-A1:B2` — это
//!    `-(A1:B2)`. Поэтому операнд унарного префикса разбирается на уровне 6, а
//!    не на самом сильном: он обязан захватить `:` и пробел, но не `^`.
//!
//! # Запятая
//!
//! Запятой в этой таблице нет намеренно. В Excel она одновременно оператор
//! объединения (`(A1,B1)` — одна ссылка из двух областей) и разделитель
//! аргументов (`SUM(A1,B1)` — два аргумента). Различить их приоритетом нельзя:
//! разница чисто позиционная. Поэтому запятая обрабатывается на уровне списка —
//! в скобках она строит [`ExprKind::Union`], в вызове разделяет аргументы, — а
//! в восхождение по приоритетам не входит вовсе.
//!
//! # Пробел
//!
//! Пробел между двумя операндами — оператор пересечения; в любом другом месте —
//! форматирование, которое надо сохранить. Решение принимается по одному
//! признаку: закончился операнд, дальше пробел, и следующий токен снова
//! начинает операнд ([`TokKind::starts_operand`]). Знаки `+`/`-` операнд не
//! начинают, иначе `A1 -B1` стало бы пересечением вместо вычитания.
//!
//! # Глубина и размер кадра
//!
//! Рекурсивный спуск на недоверенном входе упирается не в логику, а в стек:
//! `((((…))))` на сто тысяч уровней уронил бы процесс раньше, чем разбор успел
//! сообщить об ошибке. Счётчик глубины поднимается на каждом входе в
//! [`Parser::expr`] — то есть на каждой скобке, каждом аргументе и каждом
//! унарном префиксе.
//!
//! Одного счётчика мало: предел обязан быть согласован с **размером кадра**.
//! Первая редакция ставила предел 256 и измерялась в 13,4 КиБ стека на уровень
//! вложенности — это 3,4 МиБ, больше, чем есть у потока `libtest` (2 МиБ) и у
//! wasm32 (1 МиБ по умолчанию). «Защита» сама вызывала то самое переполнение,
//! от которого защищала: тест на сто тысяч скобок ронял процесс.
//!
//! Поэтому сделано и то, и другое:
//!
//! * цепочка вызовов на уровень укорочена с семи кадров до четырёх —
//!   `expr` → `unary` → `primary` → (`arg_list`), — постфиксный `%`,
//!   восхождение и разбор аргумента слиты в вызывающие;
//! * `Reference` ужат за `Box` (см. [`super::refs`]): `ExprKind` похудел со 104
//!   байт до 48, `Expr` — со 136 до 80, а с ними и временные в кадрах;
//! * предел снижен до [`MAX_DEPTH`].
//!
//! После этого измерено 6,3 КиБ на уровень, то есть **387 КиБ на худшем входе**
//! (отладочная сборка; в release заметно меньше). Запас до 1 МиБ — 2,6 раза.
//! Число не оставлено на веру: его держит тест
//! `deepest_possible_formula_fits_in_a_small_stack`.

use super::ast::{BinOp, Expr, ExprKind, UnaryOp};
use super::lexer::{TokKind, Token, tokenize};
use crate::error::{Error, FormulaError, Result};

/// Предел вложенности выражения.
///
/// Excel сам не пускает глубже 64 уровней вложенных функций — это его
/// документированное ограничение, и файла, который его превышает, не бывает.
/// Здесь ровно та же цифра, и она же держит худший случай стека в пределах
/// сотен килобайт: см. раздел про размер кадра в заголовке модуля.
pub const MAX_DEPTH: u32 = 64;

/// Приоритет унарных префиксов. Ссылочные операторы выше, арифметика ниже.
const UNARY_PREC: u8 = 6;

const fn prec(op: BinOp) -> u8 {
    match op {
        BinOp::Range => 8,
        BinOp::Intersect => 7,
        BinOp::Pow => 5,
        BinOp::Mul | BinOp::Div => 4,
        BinOp::Add | BinOp::Sub => 3,
        BinOp::Concat => 2,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 1,
    }
}

/// Символ токена — только для текста ошибки.
fn char_of(k: &TokKind) -> char {
    match k {
        TokKind::Comma => ',',
        TokKind::Semi => ';',
        TokKind::RBrace => '}',
        TokKind::RParen => ')',
        TokKind::LBrace => '{',
        TokKind::Percent => '%',
        TokKind::Op(o) => o.text().chars().next().unwrap_or('?'),
        _ => '?',
    }
}

struct Parser<'a> {
    toks: Vec<Token<'a>>,
    i: usize,
    depth: u32,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &TokKind {
        self.toks.get(self.i).map_or(&TokKind::Eof, |t| &t.kind)
    }

    fn pos(&self) -> usize {
        self.toks.get(self.i).map_or(0, |t| t.pos)
    }

    fn err(&self, kind: FormulaError) -> Error {
        self.err_at(kind, self.pos())
    }

    fn err_at(&self, kind: FormulaError, pos: usize) -> Error {
        Error::Formula {
            kind,
            pos: u32::try_from(pos).unwrap_or(u32::MAX),
        }
    }

    /// Забирает пробелы перед текущим токеном, оставляя на их месте пустоту.
    ///
    /// «Забирает» — не для красоты: так каждый пробельный участок исходника
    /// расходуется ровно один раз, и печать не может ни потерять его, ни
    /// продублировать.
    fn take_ws(&mut self) -> &'a str {
        self.toks
            .get_mut(self.i)
            .map_or("", |t| core::mem::take(&mut t.ws))
    }

    /// Забирает текущий токен **перемещением**, а не копией.
    ///
    /// Копия здесь стоила бы дорого не по времени, а по стеку: клон `TokKind`
    /// живёт во временной переменной вызывающего, и в отладочной сборке такие
    /// временные складываются в кадр, который умножается на глубину рекурсии.
    fn bump(&mut self) -> TokKind {
        let k = self.toks.get_mut(self.i).map_or(TokKind::Eof, |t| {
            core::mem::replace(&mut t.kind, TokKind::Eof)
        });
        if self.i < self.toks.len() {
            self.i = self.i.saturating_add(1);
        }
        k
    }

    fn eat(&mut self, k: &TokKind) -> bool {
        if self.peek() == k {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Приписывает уже разобранному выражению пробелы, которые стоят перед
    /// закрывающим токеном или разделителем.
    fn absorb_trailing(&mut self, e: &mut Expr) {
        let ws = self.take_ws();
        if !ws.is_empty() {
            e.set_trailing_ws(ws);
        }
    }

    /// Восхождение по приоритетам.
    ///
    /// Счётчик глубины не восстанавливается на путях `?`: ошибка обрывает весь
    /// разбор, и значение счётчика после неё никого не интересует. Отдельная
    /// функция-обёртка ради симметрии стоила бы кадра на каждом уровне.
    fn expr(&mut self, min_prec: u8) -> Result<Expr> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            return Err(self.err(FormulaError::TooDeep));
        }
        let mut lhs = self.unary()?;
        loop {
            let (op, ws_op) = match self.peek() {
                TokKind::Op(o) => {
                    let o = *o;
                    if prec(o) < min_prec {
                        break;
                    }
                    (o, self.take_ws())
                }
                // Пересечение: оператор — сам пробел, отдельного токена нет.
                k if k.starts_operand() => {
                    if prec(BinOp::Intersect) < min_prec {
                        break;
                    }
                    let ws = self.take_ws();
                    if ws.is_empty() {
                        // Два операнда подряд без пробела — не пересечение, а
                        // синтаксическая ошибка вроде `1"a"`. Пусто и осталось
                        // пусто, возвращать нечего.
                        break;
                    }
                    (BinOp::Intersect, ws)
                }
                _ => break,
            };
            if op != BinOp::Intersect {
                self.bump();
            }
            // Все инфиксные операторы Excel левоассоциативны, включая `^`:
            // `2^3^2` равно 64, а не 512.
            let rhs = self.expr(prec(op).saturating_add(1))?;
            lhs = Expr::new(ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                ws_op: ws_op.into(),
            });
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(lhs)
    }

    /// Унарные префиксы, а следом постфиксный `%`.
    ///
    /// Слиты в одну функцию ради кадра: разделение стоило бы лишнего уровня
    /// стека на каждой вложенности, а логики не добавляет.
    fn unary(&mut self) -> Result<Expr> {
        let op = match self.peek() {
            TokKind::Op(BinOp::Sub) => Some(UnaryOp::Neg),
            TokKind::Op(BinOp::Add) => Some(UnaryOp::Plus),
            _ => None,
        };
        if let Some(op) = op {
            let ws_before = self.take_ws();
            self.bump();
            // Уровень 6: операнд забирает `:` и пересечение, но останавливается
            // перед `^` — см. таблицу в заголовке модуля.
            let operand = self.expr(UNARY_PREC)?;
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op,
                    operand: Box::new(operand),
                },
                ws_before: ws_before.into(),
                ws_after: String::new().into_boxed_str(),
            });
        }

        let mut e = self.primary()?;
        while matches!(self.peek(), TokKind::Percent) {
            // Пробел перед `%` принадлежит операнду: `A1 %` — это `A1` с
            // хвостовым пробелом и процент сразу за ним.
            self.absorb_trailing(&mut e);
            self.bump();
            e = Expr::new(ExprKind::Percent(Box::new(e)));
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr> {
        let ws_before = self.take_ws();
        let pos = self.pos();
        let kind = match self.bump() {
            TokKind::Num { value, raw } => ExprKind::Num { value, raw },
            TokKind::Str(s) => ExprKind::Str(s),
            TokKind::Bool(b) => ExprKind::Bool(b),
            TokKind::Err(e) => ExprKind::Err(e),
            TokKind::Ref(r) => ExprKind::Ref(r),
            TokKind::Name { text, call } => {
                if call {
                    self.bump(); // `(`
                    let args = self.arg_list()?;
                    if !self.eat(&TokKind::RParen) {
                        return Err(self.err(FormulaError::UnbalancedParen));
                    }
                    ExprKind::Func { name: text, args }
                } else {
                    ExprKind::Name(text)
                }
            }
            TokKind::LParen => ExprKind::Paren(Box::new(self.paren_body()?)),
            TokKind::LBrace => ExprKind::Array(self.array_rows()?),
            TokKind::Eof => return Err(self.err_at(FormulaError::UnexpectedEof, pos)),
            TokKind::RParen => return Err(self.err_at(FormulaError::UnbalancedParen, pos)),
            other => return Err(self.err_at(FormulaError::UnexpectedChar(char_of(&other)), pos)),
        };
        Ok(Expr {
            kind,
            ws_before: ws_before.into(),
            ws_after: String::new().into_boxed_str(),
        })
    }

    /// Внутренность круглых скобок, уже после `(` и вместе с `)`.
    ///
    /// Здесь запятая — оператор объединения, а не разделитель аргументов.
    fn paren_body(&mut self) -> Result<Expr> {
        let mut items = Vec::new();
        loop {
            let e = self.expr(1)?;
            items.push(e);
            if let Some(last) = items.last_mut() {
                self.absorb_trailing(last);
            }
            if !self.eat(&TokKind::Comma) {
                break;
            }
        }
        if !self.eat(&TokKind::RParen) {
            return Err(self.err(FormulaError::UnbalancedParen));
        }
        if items.len() == 1 {
            return items
                .pop()
                .ok_or_else(|| self.err(FormulaError::UnexpectedEof));
        }
        Ok(Expr::new(ExprKind::Union(items)))
    }

    /// Список аргументов вызова, уже после `(` и до `)`.
    ///
    /// Excel допускает пропущенные аргументы: `IF(A1,,B1)` — три аргумента,
    /// средний пуст. Пустой список (`TODAY()`) — не то же самое, что один
    /// пропущенный, поэтому проверка на `)` идёт первой.
    fn arg_list(&mut self) -> Result<Vec<Expr>> {
        if matches!(self.peek(), TokKind::RParen) {
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        loop {
            if matches!(self.peek(), TokKind::Comma | TokKind::RParen) {
                // Пропуск печатается пустотой, поэтому пробелы вокруг него
                // неразличимы. Канонично класть их все в `ws_before`.
                let ws = self.take_ws();
                args.push(Expr::new(ExprKind::Missing).with_ws_before(ws));
            } else {
                let e = self.expr(1)?;
                args.push(e);
                if let Some(last) = args.last_mut() {
                    self.absorb_trailing(last);
                }
            }
            if !self.eat(&TokKind::Comma) {
                return Ok(args);
            }
        }
    }

    /// Строки литерала массива, уже после `{` и вместе с закрывающей `}`.
    fn array_rows(&mut self) -> Result<Vec<Vec<Expr>>> {
        let mut rows = Vec::new();
        let mut row = Vec::new();
        loop {
            let e = self.expr(1)?;
            row.push(e);
            if let Some(last) = row.last_mut() {
                self.absorb_trailing(last);
            }
            match self.peek() {
                TokKind::Comma => {
                    self.bump();
                }
                TokKind::Semi => {
                    self.bump();
                    rows.push(core::mem::take(&mut row));
                }
                TokKind::RBrace => {
                    self.bump();
                    rows.push(row);
                    return Ok(rows);
                }
                _ => return Err(self.err(FormulaError::UnbalancedParen)),
            }
        }
    }
}

/// Разбирает формулу в дерево.
///
/// Текст подаётся **без ведущего `=`**: в файле формулы хранятся именно так,
/// внутри элемента `<f>`.
pub(super) fn parse(src: &str) -> Result<Expr> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        depth: 0,
    };
    let mut e = p.expr(1)?;
    p.absorb_trailing(&mut e);
    if !matches!(p.peek(), TokKind::Eof) {
        let kind = match p.peek() {
            TokKind::RParen => FormulaError::UnbalancedParen,
            other => FormulaError::UnexpectedChar(char_of(other)),
        };
        return Err(p.err(kind));
    }
    Ok(e)
}
