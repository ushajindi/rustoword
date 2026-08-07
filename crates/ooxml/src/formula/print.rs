//! Печать дерева обратно в текст формулы.
//!
//! Цель — не «читаемый вид», а **тот же текст, что был на входе**. Всё, что
//! разбор сохранил (написание чисел, пробелы, скобки, апострофы вокруг имени
//! листа, префикс `_xlfn.`), печатается дословно. Отсюда правило: печать ничего
//! не решает сама. Ни одного «поставим пробел для красоты», ни одного «уберём
//! лишние скобки» — каждое такое улучшение сделало бы круг разбор → печать
//! невоспроизводимым, а вместе с ним и правку чужого файла.
//!
//! Единственные места, где написание всё же нормализуется, — регистр `TRUE`,
//! `FALSE` и букв столбца: Excel и сам приводит их к верхнему регистру, и в
//! файлах корпуса других форм нет.

use super::ast::{BinOp, Expr, ExprKind, UnaryOp};

/// Печатает дерево в текст формулы (без ведущего `=`).
#[must_use]
pub(super) fn print(e: &Expr) -> String {
    let mut out = String::new();
    write(e, &mut out);
    out
}

fn write(e: &Expr, out: &mut String) {
    out.push_str(&e.ws_before);
    match &e.kind {
        ExprKind::Num { raw, .. } => out.push_str(raw),
        ExprKind::Str(s) => write_string(s, out),
        ExprKind::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
        ExprKind::Err(k) => out.push_str(k.text()),
        ExprKind::Ref(r) => r.write(out),
        ExprKind::Name(n) => out.push_str(n),
        ExprKind::Missing => {}
        ExprKind::Func { name, args } => {
            out.push_str(name);
            out.push('(');
            join(args, ',', out);
            out.push(')');
        }
        ExprKind::Unary { op, operand } => {
            out.push(match op {
                UnaryOp::Neg => '-',
                UnaryOp::Plus => '+',
            });
            write(operand, out);
        }
        ExprKind::Percent(inner) => {
            write(inner, out);
            out.push('%');
        }
        ExprKind::Binary {
            op,
            lhs,
            rhs,
            ws_op,
        } => {
            write(lhs, out);
            out.push_str(ws_op);
            // У пересечения текст оператора пуст: оператором служит ws_op.
            if *op != BinOp::Intersect {
                out.push_str(op.text());
            }
            write(rhs, out);
        }
        ExprKind::Union(items) => join(items, ',', out),
        ExprKind::Paren(inner) => {
            out.push('(');
            write(inner, out);
            out.push(')');
        }
        ExprKind::Array(rows) => {
            out.push('{');
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                join(row, ',', out);
            }
            out.push('}');
        }
    }
    out.push_str(&e.ws_after);
}

fn join(items: &[Expr], sep: char, out: &mut String) {
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        write(it, out);
    }
}

/// Строковый литерал: кавычка внутри удваивается.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}
