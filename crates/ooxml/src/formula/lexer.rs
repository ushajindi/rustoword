//! Токенайзер формул.
//!
//! # Что здесь неочевидно
//!
//! * **Пробел — значимый токен.** Он либо оператор пересечения, либо
//!   форматирование, которое обязано пережить круг разбор → печать. Различить
//!   их лексер не может (это контекст), поэтому он просто цепляет пробелы к
//!   следующему токену в поле [`Token::ws`], а решает [`super::parser`].
//! * **`1E+3` — одно число, а не сложение.** Экспонента забирается вместе со
//!   знаком, но только если за ней действительно есть цифры: в `1E+A1` буква
//!   `E` начинает имя.
//! * **Ссылка распознаётся целиком, до разбора на операторы.** Иначе `A:A` и
//!   `1:1` были бы неотличимы от «имя, двоеточие, имя» и «число, двоеточие,
//!   число», а имя листа `'Лист 1'` развалилось бы на кавычки и пробел.
//! * **`LOG10(` — это функция, а не ячейка LOG10.** Буквы `LOG` — допустимый
//!   номер столбца, цифры `10` — допустимый номер строки, так что ссылка
//!   разбирается успешно и оказывается неверной. Спасает только следующий
//!   символ: открывающая скобка означает вызов.
//! * **Ошибки — литералы.** `#DIV/0!` содержит `/` и `!`, и если бы лексер
//!   дошёл до них обычным путём, формула распалась бы на деление и мусор.

use super::ast::{BinOp, ErrKind};
use super::refs::{self, Reference, is_name_char, is_name_start};
use crate::error::{Error, FormulaError, Result};

/// Разновидность токена.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokKind {
    Num {
        value: f64,
        raw: Box<str>,
    },
    Str(Box<str>),
    Bool(bool),
    Err(ErrKind),
    Ref(Reference),
    /// Имя. `call` — сразу за именем стоит `(`, значит это вызов функции.
    Name {
        text: Box<str>,
        call: bool,
    },
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Semi,
    Percent,
    /// Инфиксный оператор. `Add`/`Sub` в префиксной позиции читаются как
    /// унарные — это решает парсер, лексер их не различает.
    Op(BinOp),
    Eof,
}

impl TokKind {
    /// Может ли токен начинать операнд.
    ///
    /// Нужно ровно для одного решения: пробел между двумя операндами — это
    /// оператор пересечения. Знаки `+`/`-` сюда не входят: `A1 -B1` в Excel
    /// вычитание, а не пересечение с отрицанием.
    pub(crate) const fn starts_operand(&self) -> bool {
        matches!(
            self,
            Self::Num { .. }
                | Self::Str(_)
                | Self::Bool(_)
                | Self::Err(_)
                | Self::Ref(_)
                | Self::Name { .. }
                | Self::LParen
                | Self::LBrace
        )
    }
}

/// Токен вместе с предшествующими ему пробелами.
#[derive(Debug, Clone)]
pub(crate) struct Token<'a> {
    pub kind: TokKind,
    /// Пробелы перед токеном. Пусты в подавляющем большинстве случаев.
    pub ws: &'a str,
    /// Позиция первого байта самого токена — для сообщений об ошибке.
    pub pos: usize,
}

fn at(s: &str, i: usize) -> Option<char> {
    s.get(i..).and_then(|t| t.chars().next())
}

fn err(kind: FormulaError, pos: usize) -> Error {
    Error::Formula {
        kind,
        pos: u32::try_from(pos).unwrap_or(u32::MAX),
    }
}

/// Может ли с этого символа начинаться ссылка или имя.
fn ref_or_name_start(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '$' || c == '\'' || c == '[' || c == '#'
}

/// Разбивает формулу на токены. Последним всегда идёт [`TokKind::Eof`].
pub(crate) fn tokenize(src: &str) -> Result<Vec<Token<'_>>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        let ws_from = i;
        while at(src, i).is_some_and(char::is_whitespace) {
            i = i.saturating_add(1);
        }
        let ws = src.get(ws_from..i).unwrap_or("");
        let pos = i;

        let Some(c) = at(src, i) else {
            out.push(Token {
                kind: TokKind::Eof,
                ws,
                pos,
            });
            return Ok(out);
        };

        let (kind, next) = if c == '"' {
            scan_string(src, i)?
        } else if ref_or_name_start(c) {
            scan_ref_name_or_number(src, i)?
        } else if c == '.' && at(src, i.saturating_add(1)).is_some_and(|d| d.is_ascii_digit()) {
            scan_number(src, i)?
        } else {
            scan_punct(src, i).ok_or_else(|| err(FormulaError::UnexpectedChar(c), i))?
        };
        i = next;
        out.push(Token { kind, ws, pos });
    }
}

/// Строковый литерал. Кавычка внутри удваивается: `"он сказал ""да"""`.
fn scan_string(src: &str, i: usize) -> Result<(TokKind, usize)> {
    let mut p = i.saturating_add(1);
    let mut out = String::new();
    loop {
        let Some(c) = at(src, p) else {
            return Err(err(FormulaError::UnterminatedString, i));
        };
        p = p.saturating_add(c.len_utf8());
        if c == '"' {
            if at(src, p) == Some('"') {
                out.push('"');
                p = p.saturating_add(1);
            } else {
                return Ok((TokKind::Str(out.into_boxed_str()), p));
            }
        } else {
            out.push(c);
        }
    }
}

/// Единая точка входа для всего, что начинается с буквы, цифры, `$`, `'`,
/// `[` или `#`. Порядок проб здесь и есть разрешение неоднозначностей.
fn scan_ref_name_or_number(src: &str, i: usize) -> Result<(TokKind, usize)> {
    // 1. Ссылка целиком, вместе с именем листа и обеими половинами диапазона.
    if let Some((r, end)) = refs::scan(src, i) {
        // `LOG10(`: разбор ссылки удался, но следом скобка — значит, это имя
        // функции. Ссылка с именем листа так спутаться не может.
        let call_ahead = at(src, end) == Some('(') && r.sheet.is_none();
        if !call_ahead {
            return Ok((TokKind::Ref(r), end));
        }
    }

    let c = at(src, i).unwrap_or('\0');

    // 2. Литерал-ошибка. `#REF!` сюда не доходит: его забрала ссылка.
    if c == '#' {
        for e in ErrKind::ALL {
            let t = e.text();
            if src.get(i..).is_some_and(|s| s.starts_with(t)) {
                return Ok((TokKind::Err(*e), i.saturating_add(t.len())));
            }
        }
        return Err(err(FormulaError::BadReference, i));
    }

    // 3. Незакрытый апостроф имени листа — иначе он утёк бы в «неожиданный
    //    символ», а причина у него совсем другая.
    if c == '\'' {
        return Err(err(FormulaError::UnterminatedString, i));
    }
    if c == '[' || c == '$' {
        return Err(err(FormulaError::BadReference, i));
    }

    // 4. Число.
    if c.is_ascii_digit() {
        return scan_number(src, i);
    }

    // 5. Имя: определённое имя, `TRUE`, `_xlfn.XLOOKUP`, имя функции.
    let mut p = i;
    while at(src, p).is_some_and(is_name_char) {
        p = p.saturating_add(at(src, p).map_or(1, char::len_utf8));
    }
    if p == i {
        return Err(err(FormulaError::UnexpectedChar(c), i));
    }
    let text = src.get(i..p).unwrap_or("");
    // Excel хранит булевы литералы прописными; регистр входа не сохраняется.
    if text.eq_ignore_ascii_case("TRUE") {
        return Ok((TokKind::Bool(true), p));
    }
    if text.eq_ignore_ascii_case("FALSE") {
        return Ok((TokKind::Bool(false), p));
    }
    Ok((
        TokKind::Name {
            text: text.into(),
            call: at(src, p) == Some('('),
        },
        p,
    ))
}

/// Число. Исходное написание сохраняется: `1`, `1.`, `.5` и `1E+3` — четыре
/// разных текста, и восстановить их из `f64` невозможно.
fn scan_number(src: &str, i: usize) -> Result<(TokKind, usize)> {
    let mut p = i;
    while at(src, p).is_some_and(|c| c.is_ascii_digit()) {
        p = p.saturating_add(1);
    }
    if at(src, p) == Some('.') {
        p = p.saturating_add(1);
        while at(src, p).is_some_and(|c| c.is_ascii_digit()) {
            p = p.saturating_add(1);
        }
    }
    // Экспонента забирается только вместе с цифрами: в `1E+A1` знак `+` —
    // это сложение, а `E` начинает имя.
    if matches!(at(src, p), Some('e' | 'E')) {
        let mut q = p.saturating_add(1);
        if matches!(at(src, q), Some('+' | '-')) {
            q = q.saturating_add(1);
        }
        if at(src, q).is_some_and(|c| c.is_ascii_digit()) {
            while at(src, q).is_some_and(|c| c.is_ascii_digit()) {
                q = q.saturating_add(1);
            }
            p = q;
        }
    }
    let raw = src.get(i..p).unwrap_or("");
    let value = raw.parse::<f64>().map_err(|_| {
        // Форма `1.` для Rust допустима, так что сюда попадает только то,
        // чего сканер не мог собрать. Оставляем читаемую причину.
        err(FormulaError::UnexpectedChar('.'), i)
    })?;
    Ok((
        TokKind::Num {
            value,
            raw: raw.into(),
        },
        p,
    ))
}

/// Знаки препинания и операторы. Двухсимвольные сравнения проверяются раньше
/// односимвольных, иначе `<=` распалось бы на `<` и `=`.
fn scan_punct(src: &str, i: usize) -> Option<(TokKind, usize)> {
    let rest = src.get(i..)?;
    for (t, k) in [
        ("<=", TokKind::Op(BinOp::Le)),
        (">=", TokKind::Op(BinOp::Ge)),
        ("<>", TokKind::Op(BinOp::Ne)),
    ] {
        if rest.starts_with(t) {
            return Some((k, i.saturating_add(2)));
        }
    }
    let c = at(src, i)?;
    let kind = match c {
        '(' => TokKind::LParen,
        ')' => TokKind::RParen,
        '{' => TokKind::LBrace,
        '}' => TokKind::RBrace,
        ',' => TokKind::Comma,
        ';' => TokKind::Semi,
        '%' => TokKind::Percent,
        ':' => TokKind::Op(BinOp::Range),
        '^' => TokKind::Op(BinOp::Pow),
        '*' => TokKind::Op(BinOp::Mul),
        '/' => TokKind::Op(BinOp::Div),
        '+' => TokKind::Op(BinOp::Add),
        '-' => TokKind::Op(BinOp::Sub),
        '&' => TokKind::Op(BinOp::Concat),
        '=' => TokKind::Op(BinOp::Eq),
        '<' => TokKind::Op(BinOp::Lt),
        '>' => TokKind::Op(BinOp::Gt),
        _ => return None,
    };
    Some((kind, i.saturating_add(1)))
}
