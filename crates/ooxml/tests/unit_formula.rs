//! Разбор формул: тонкие места грамматики Excel, сдвиг ссылок, атаки, фаззер.
//!
//! Простые случаи (`SUM(A1:A2)`) закрыты корпусным тестом на реальных файлах.
//! Здесь собрано то, чего в корпусе нет, но что встретится в первом же чужом
//! файле: имена листов в апострофах, пересечение пробелом, приоритет унарного
//! минуса над степенью, пропущенные аргументы, `#REF!` после удаления строки.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::error::{Error, FormulaError};
use ooxml::formula::{
    Axis, BinOp, CellRef, Expr, ExprKind, MAX_COL, MAX_DEPTH, MAX_ROW, RefBody, Reference, Shift,
    UnaryOp, dependencies, parse_formula, print_formula, shift_refs, translate_refs,
};

// ---------------------------------------------------------------------------
// Вспомогательное
// ---------------------------------------------------------------------------

#[track_caller]
fn ast(src: &str) -> Expr {
    parse_formula(src).unwrap_or_else(|e| panic!("не разобралось {src:?}: {e}"))
}

/// Разбор и печать вернули ровно исходный текст.
#[track_caller]
fn round_trips(src: &str) {
    let back = print_formula(&ast(src));
    assert_eq!(back, src, "круг не сошёлся");
}

#[track_caller]
fn kind_of(src: &str) -> FormulaError {
    match parse_formula(src) {
        Err(Error::Formula { kind, .. }) => kind,
        Err(e) => panic!("{src:?}: ожидалась ошибка формулы, получено {e}"),
        Ok(_) => panic!("{src:?}: ожидалась ошибка, формула разобралась"),
    }
}

/// Единственная ссылка формулы.
#[track_caller]
fn only_ref(src: &str) -> Reference {
    let deps = dependencies(&ast(src));
    assert_eq!(deps.len(), 1, "{src:?}: ожидалась ровно одна ссылка");
    deps.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// Ссылки: абсолютность, листы, внешние книги
// ---------------------------------------------------------------------------

#[test]
fn four_forms_of_absoluteness_are_four_different_references() {
    // Доллар перед буквой и перед цифрой ставится независимо. Один флаг
    // «ссылка абсолютная» потерял бы смешанные формы.
    for (src, col_abs, row_abs) in [
        ("A1", false, false),
        ("$A1", true, false),
        ("A$1", false, true),
        ("$A$1", true, true),
    ] {
        round_trips(src);
        let RefBody::Cell(c) = only_ref(src).body else {
            panic!("{src}: ожидалась ячейка");
        };
        assert_eq!((c.col, c.row), (0, 0), "{src}");
        assert_eq!((c.col_abs, c.row_abs), (col_abs, row_abs), "{src}");
    }
}

#[test]
fn sheet_names_survive_quotes_apostrophes_and_workbooks() {
    for src in [
        "Sheet1!A1",
        "'Лист 1'!$A$1",
        "'Ivan''s Sheet'!A1",
        "[1]Sheet!A1",
        "'[1]Лист 1'!A1",
        "Sheet1:Sheet3!A1",
        "'Лист 1:Лист 3'!A1",
        "стр.1_4!$A$1:$BP$159",
        "Лист1!#REF!",
        "#REF!!A1",
    ] {
        round_trips(src);
    }

    // Удвоенный апостроф внутри имени — экранированный апостроф, а не конец
    // имени: `'Ivan''s Sheet'` — это один лист `Ivan's Sheet`.
    let r = only_ref("'Ivan''s Sheet'!A1");
    assert_eq!(r.sheet.as_ref().unwrap().first, "Ivan's Sheet");
    assert!(r.sheet.as_ref().unwrap().quoted);

    let r = only_ref("[1]Sheet!A1");
    assert_eq!(r.sheet.as_ref().unwrap().book_index(), Some(1));

    let r = only_ref("Sheet1:Sheet3!A1");
    let p = r.sheet.as_ref().unwrap();
    assert_eq!(p.first, "Sheet1");
    assert_eq!(p.last.as_deref(), Some("Sheet3"));
    assert!(p.mentions("Sheet3"));
}

#[test]
fn open_ranges_cover_whole_columns_and_rows() {
    for src in ["A:A", "1:1", "$A:$A", "$1:$7", "A:C", "Sheet1!A:A"] {
        round_trips(src);
    }
    assert!(matches!(only_ref("A:A").body, RefBody::Cols { .. }));
    assert!(matches!(only_ref("1:1").body, RefBody::Rows { .. }));

    // Одинокие `A` и `1` ссылками не являются: первое — имя, второе — число.
    assert!(matches!(ast("A").kind, ExprKind::Name(_)));
    assert!(matches!(ast("1").kind, ExprKind::Num { .. }));
}

#[test]
fn reference_limits_are_the_excel_ones() {
    round_trips("XFD1048576");
    let RefBody::Cell(c) = only_ref("XFD1048576").body else {
        panic!("ожидалась ячейка");
    };
    assert_eq!((c.col, c.row), (MAX_COL, MAX_ROW));

    // За границей листа ссылки нет — только имя. Excel так же: `XFE1` он
    // считает определённым именем, а не ячейкой.
    assert!(matches!(ast("XFE1").kind, ExprKind::Name(_)));
    assert!(matches!(ast("A1048577").kind, ExprKind::Name(_)));
    round_trips("XFE1");
    round_trips("A1048577");
}

#[test]
fn ref_error_is_a_reference_not_an_error_value() {
    // `#REF!` держится как состояние ссылки, а не как значение-ошибка: у него
    // бывает имя листа, и сдвиг обязан уметь его порождать.
    round_trips("#REF!");
    let r = only_ref("#REF!");
    assert!(r.is_invalid());
    assert!(r.sheet.is_none());
    assert!(only_ref("Лист1!#REF!").is_invalid());
}

#[test]
fn all_error_literals_round_trip() {
    for src in [
        "#DIV/0!",
        "#N/A",
        "#VALUE!",
        "#NAME?",
        "#NULL!",
        "#NUM!",
        "#GETTING_DATA",
        "#SPILL!",
        "#CALC!",
    ] {
        round_trips(src);
        assert!(matches!(ast(src).kind, ExprKind::Err(_)), "{src}");
    }
    // `#DIV/0!` содержит `/` и `!`: без распознавания литерала целиком формула
    // распалась бы на деление и мусор.
    round_trips("IF(ISERROR(A1),#N/A,A1/B1)");
}

// ---------------------------------------------------------------------------
// Числа и строки
// ---------------------------------------------------------------------------

#[test]
fn number_spellings_are_preserved_verbatim() {
    // `1`, `1.`, `1.0` и `1E+0` — одно значение и четыре разных текста.
    for (src, value) in [
        ("1E+3", 1000.0),
        ("1.5E-10", 1.5e-10),
        (".5", 0.5),
        ("1.", 1.0),
        ("0", 0.0),
        ("1e3", 1000.0),
        ("00012", 12.0),
        ("1.23456", 1.23456),
    ] {
        round_trips(src);
        let ExprKind::Num { value: v, .. } = ast(src).kind else {
            panic!("{src}: ожидалось число");
        };
        assert!((v - value).abs() < 1e-20, "{src}: {v} != {value}");
    }
}

#[test]
fn exponent_is_not_addition() {
    // `1E+3` — одно число. Разбери лексер это как `1`, имя `E`, плюс и `3` —
    // и получилась бы формула, которая молча считает не то.
    for src in ["1E+3", "1E3", "1E-3", "1e+3"] {
        assert!(matches!(ast(src).kind, ExprKind::Num { .. }), "{src}");
    }

    // Обратная сторона: экспонента забирается ТОЛЬКО вместе с цифрами. В
    // `1E+A1` цифр за знаком нет, поэтому число кончается на `1`, а дальше
    // идут имя `E`, плюс и ссылка. Excel такую запись тоже отвергает —
    // два операнда подряд без оператора, — и мы обязаны отвергать так же,
    // а не молча склеивать её в число.
    assert!(parse_formula("1E+A1").is_err());

    // Зато с оператором между ними всё разбирается и печатается назад.
    round_trips("1*E+A1");
    let ExprKind::Binary { op, .. } = ast("1*E+A1").kind else {
        panic!("ожидался оператор");
    };
    assert_eq!(op, BinOp::Add);
}

#[test]
fn doubled_quotes_inside_strings() {
    let e = ast(r#""он сказал ""да""""#);
    let ExprKind::Str(s) = e.kind else {
        panic!("ожидалась строка");
    };
    assert_eq!(&*s, r#"он сказал "да""#);
    round_trips(r#""он сказал ""да""""#);
    round_trips(r#""""#);
    round_trips(r#""""""#);
    round_trips(r#"IF(A1="","",B1)"#);
}

// ---------------------------------------------------------------------------
// Операторы и приоритеты
// ---------------------------------------------------------------------------

#[test]
fn unary_minus_binds_tighter_than_power() {
    // В Excel `-2^2` равно 4, а не -4: минус прилипает к двойке раньше степени.
    // Почти во всех языках наоборот, поэтому проверяется структура, а не текст.
    let e = ast("-2^2");
    let ExprKind::Binary { op, lhs, .. } = &e.kind else {
        panic!("ожидалась степень наверху, получено {:?}", e.kind);
    };
    assert_eq!(*op, BinOp::Pow);
    assert!(
        matches!(
            &lhs.kind,
            ExprKind::Unary {
                op: UnaryOp::Neg,
                ..
            }
        ),
        "слева от `^` обязан быть унарный минус, получено {:?}",
        lhs.kind
    );
    round_trips("-2^2");
    round_trips("2^-3");
    round_trips("---1");
}

#[test]
fn space_is_the_intersection_operator() {
    // Главная ловушка грамматики: пробел между двумя операндами — оператор.
    let e = ast("SUM(A1:A2 B1:B2)");
    let ExprKind::Func { args, .. } = &e.kind else {
        panic!("ожидался вызов");
    };
    assert_eq!(args.len(), 1, "пересечение — один аргумент, а не два");
    assert!(matches!(
        &args[0].kind,
        ExprKind::Binary {
            op: BinOp::Intersect,
            ..
        }
    ));
    round_trips("SUM(A1:A2 B1:B2)");

    // А запятая в том же месте даёт два аргумента.
    let ExprKind::Func { args, .. } = ast("SUM(A1, B1)").kind else {
        panic!("ожидался вызов");
    };
    assert_eq!(args.len(), 2);

    // `A1 -B1` — вычитание, а не пересечение с отрицанием: знаки `+`/`-`
    // операнд не начинают.
    assert!(matches!(
        ast("A1 -B1").kind,
        ExprKind::Binary { op: BinOp::Sub, .. }
    ));
    round_trips("A1 -B1");
}

#[test]
fn comma_inside_parens_is_union_not_a_separator() {
    // `SUM(A1,B1)` — два аргумента; `SUM((A1,B1))` — один, объединение.
    let ExprKind::Func { args, .. } = ast("SUM((A1,B1))").kind else {
        panic!("ожидался вызов");
    };
    assert_eq!(args.len(), 1);
    let ExprKind::Paren(inner) = &args[0].kind else {
        panic!("ожидались скобки");
    };
    let ExprKind::Union(items) = &inner.kind else {
        panic!("ожидалось объединение, получено {:?}", inner.kind);
    };
    assert_eq!(items.len(), 2);
    round_trips("SUM((A1,B1))");
    round_trips("SUM((A1,B1,C1:C9))");
}

#[test]
fn parentheses_are_kept_because_dropping_them_changes_the_formula() {
    // Без узла скобок печать дала бы `A1+B1*2` — другую формулу.
    round_trips("(A1+B1)*2");
    assert!(matches!(
        ast("(A1+B1)*2").kind,
        ExprKind::Binary { op: BinOp::Mul, .. }
    ));
    round_trips("((((A1))))");
    round_trips("(1)");
    // Лишние скобки не «упрощаются»: их поставил автор файла.
    assert_ne!(print_formula(&ast("(A1)")), "A1");
}

#[test]
fn percent_is_postfix() {
    round_trips("A1%");
    round_trips("50%+1");
    round_trips("A1%%");
    assert!(matches!(ast("A1%").kind, ExprKind::Percent(_)));
    let ExprKind::Binary { op, lhs, .. } = ast("50%+1").kind else {
        panic!("ожидалось сложение");
    };
    assert_eq!(op, BinOp::Add);
    assert!(matches!(lhs.kind, ExprKind::Percent(_)));
}

#[test]
fn comparison_operators_and_concat() {
    for src in [
        "A1=B1",
        "A1<>B1",
        "A1<=B1",
        "A1>=B1",
        "A1<B1",
        "A1>B1",
        "A1&B1&\"x\"",
        "A1&B1=C1",
    ] {
        round_trips(src);
    }
    // `&` связывает сильнее сравнения: `A1&B1=C1` — это `(A1&B1)=C1`.
    let ExprKind::Binary { op, lhs, .. } = ast("A1&B1=C1").kind else {
        panic!("ожидалось сравнение");
    };
    assert_eq!(op, BinOp::Eq);
    assert!(matches!(
        lhs.kind,
        ExprKind::Binary {
            op: BinOp::Concat,
            ..
        }
    ));
}

#[test]
fn array_literals() {
    round_trips("{1,2;3,4}");
    round_trips(r#"{"a";"b"}"#);
    round_trips("{1}");
    round_trips("SUM({1,2;3,4})");
    let ExprKind::Array(rows) = ast("{1,2;3,4}").kind else {
        panic!("ожидался массив");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 2);
}

#[test]
fn function_names_including_the_xlfn_prefix() {
    // `_xlfn.` — метка Excel для функций новее файлового формата. Это часть
    // имени: снимешь её при печати — файл откроется с `#NAME?`.
    for src in [
        "_xlfn.XLOOKUP(A1,B:B,C:C)",
        "_xlfn.IFS(A1>0,1,A1<0,-1)",
        "_xlfn._xlws.FILTER(A:A,B:B)",
        "LOG10(100)",
        "PI()",
        "TODAY()",
        "SUM(A1)",
    ] {
        round_trips(src);
        assert!(
            matches!(ast(src).kind, ExprKind::Func { .. }),
            "{src}: ожидался вызов"
        );
    }
    // `LOG10` — валидный номер столбца плюс валидный номер строки. От ячейки
    // его отличает только следующая скобка.
    assert!(matches!(ast("LOG10").kind, ExprKind::Ref(_)));
    assert!(matches!(ast("LOG10(100)").kind, ExprKind::Func { .. }));
}

#[test]
fn missing_arguments_are_a_node_not_an_absence() {
    // `SUM()` и `SUM(,)` — разные формулы: ноль аргументов против двух пустых.
    let ExprKind::Func { args, .. } = ast("TODAY()").kind else {
        panic!("ожидался вызов");
    };
    assert_eq!(args.len(), 0);
    let ExprKind::Func { args, .. } = ast("IF(A1,,B1)").kind else {
        panic!("ожидался вызов");
    };
    assert_eq!(args.len(), 3);
    assert!(matches!(args[1].kind, ExprKind::Missing));
    round_trips("IF(A1,,B1)");
    round_trips("VLOOKUP(A1,B:C,2,)");
}

#[test]
fn boolean_literals() {
    round_trips("TRUE");
    round_trips("FALSE");
    round_trips("IF(A1,TRUE,FALSE)");
    assert_eq!(ast("TRUE").kind, ExprKind::Bool(true));
}

// ---------------------------------------------------------------------------
// Пробелы
// ---------------------------------------------------------------------------

#[test]
fn formatting_whitespace_survives_the_round_trip() {
    // В корпусе есть формула, записанная в две строки. Печать «канонического»
    // вида изменила бы чужой файл — а вся веха ради того, чтобы не изменять.
    for src in [
        "IF(C4:C999=\"\",\"\",\nXLOOKUP(C4:C999,'Страны'!$A$2:$A$11,'Страны'!$B$2:$B$11, \"Визовый\"))",
        "SUM( A1 , B1 )",
        "A1 + B1",
        "  A1  ",
        "SUM(\n  A1,\n  B1\n)",
        "( A1 + B1 ) * 2",
        "A1 %",
        "{ 1 , 2 ; 3 , 4 }",
        "- A1",
    ] {
        round_trips(src);
    }
}

// ---------------------------------------------------------------------------
// Зависимости
// ---------------------------------------------------------------------------

#[test]
fn dependencies_lists_every_reference_in_order() {
    let deps = dependencies(&ast("SUM(A1:A5)+Лист2!B7*$C$3"));
    assert_eq!(deps.len(), 3);
    assert!(matches!(deps[0].body, RefBody::Area { .. }));
    assert_eq!(deps[1].sheet.as_ref().unwrap().first, "Лист2");
    // Повторы не схлопываются: кто строит граф, тот и решает.
    assert_eq!(dependencies(&ast("A1+A1")).len(), 2);
    // Имя функции ссылкой не является.
    assert_eq!(dependencies(&ast("TODAY()")).len(), 0);
}

// ---------------------------------------------------------------------------
// Сдвиг ссылок при структурной правке
// ---------------------------------------------------------------------------

#[track_caller]
fn shifted(src: &str, sh: &Shift<'_>) -> String {
    let mut e = ast(src);
    shift_refs(&mut e, sh);
    print_formula(&e)
}

#[test]
fn inserting_a_row_moves_absolute_references_too() {
    // ВНИМАНИЕ. Здесь поведение расходится с расхожим «доллар фиксирует
    // ссылку». Доллар фиксирует её при КОПИРОВАНИИ формулы, а при вставке
    // строки едут обе формы — так делает Excel, и иначе структурная правка
    // тихо разрушала бы каждую абсолютную ссылку файла.
    let sh = Shift::insert_rows(0, 1);
    assert_eq!(shifted("A5", &sh), "A6");
    assert_eq!(shifted("$A$5", &sh), "$A$6");
    assert_eq!(shifted("A$5", &sh), "A$6");
    assert_eq!(shifted("SUM(A5:A9)*$B$2", &sh), "SUM(A6:A10)*$B$3");
    // Столбцы вставка строк не трогает.
    assert_eq!(shifted("A:A", &sh), "A:A");
}

#[test]
fn copying_a_formula_moves_only_relative_parts() {
    // А вот ЗДЕСЬ доллар работает так, как все ожидают: это перенос формулы,
    // а не правка листа.
    let mut e = ast("A5+$A$5+A$5+$A5");
    translate_refs(&mut e, 0, 1);
    assert_eq!(print_formula(&e), "A6+$A$5+A$5+$A6");

    let mut e = ast("A5+$A$5");
    translate_refs(&mut e, 2, 0);
    assert_eq!(print_formula(&e), "C5+$A$5");
}

#[test]
fn references_below_the_insertion_point_stay_put() {
    let sh = Shift::insert_rows(10, 1);
    assert_eq!(shifted("A5", &sh), "A5");
    assert_eq!(shifted("A11", &sh), "A12");
}

#[test]
fn inserting_inside_a_range_expands_it() {
    // Вставка НА верхнюю границу двигает диапазон целиком, вставка ВНУТРЬ —
    // расширяет. Разница в один индекс, а результат совсем разный.
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::insert_rows(1, 1)),
        "SUM(A3:A7)"
    );
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::insert_rows(3, 1)),
        "SUM(A2:A7)"
    );
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::insert_rows(5, 1)),
        "SUM(A2:A7)"
    );
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::insert_rows(6, 1)),
        "SUM(A2:A6)"
    );
}

#[test]
fn deleting_the_target_row_produces_ref_error() {
    // Ссылка на удалённую строку становится `#REF!` — как в Excel.
    assert_eq!(shifted("A5", &Shift::delete_rows(4, 1)), "#REF!");
    assert_eq!(shifted("$A$5", &Shift::delete_rows(4, 1)), "#REF!");
    assert_eq!(shifted("A5+B1", &Shift::delete_rows(4, 1)), "#REF!+B1");
    // Ниже удалённой строки — сдвиг вверх.
    assert_eq!(shifted("A9", &Shift::delete_rows(4, 1)), "A8");
    // Выше — без изменений.
    assert_eq!(shifted("A2", &Shift::delete_rows(4, 1)), "A2");
}

#[test]
fn deleting_part_of_a_range_shrinks_it_and_all_of_it_kills_it() {
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::delete_rows(2, 2)),
        "SUM(A2:A4)"
    );
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::delete_rows(0, 2)),
        "SUM(A1:A4)"
    );
    // Разрушается ссылка, а не вся формула: Excel тоже оставляет `=SUM(#REF!)`,
    // а не заменяет весь текст на `#REF!`.
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::delete_rows(1, 5)),
        "SUM(#REF!)"
    );
    assert_eq!(
        shifted("SUM(A2:A6)", &Shift::delete_rows(0, 9)),
        "SUM(#REF!)"
    );
}

#[test]
fn column_edits_work_on_the_other_axis() {
    assert_eq!(shifted("B5", &Shift::insert_cols(0, 1)), "C5");
    assert_eq!(shifted("B5", &Shift::delete_cols(1, 1)), "#REF!");
    assert_eq!(shifted("B:D", &Shift::insert_cols(0, 1)), "C:E");
    assert_eq!(shifted("B:D", &Shift::delete_cols(1, 3)), "#REF!");
    // Строки вставка столбцов не трогает.
    assert_eq!(shifted("1:1", &Shift::insert_cols(0, 1)), "1:1");
}

#[test]
fn edits_respect_which_sheet_they_happened_on() {
    let sh = Shift::insert_rows(0, 1).on_sheet("Лист1");
    // Ссылка без листа указывает на лист самой формулы — он же редактируемый.
    assert_eq!(shifted("A5", &sh), "A6");
    // Явная ссылка на редактируемый лист тоже едет.
    assert_eq!(shifted("Лист1!A5", &sh), "Лист1!A6");
    // А на чужой — нет.
    assert_eq!(shifted("Лист2!A5", &sh), "Лист2!A5");

    // Формула лежит на другом листе: её собственные ссылки не трогаем.
    let sh = Shift::insert_rows(0, 1)
        .on_sheet("Лист1")
        .from_other_sheet();
    assert_eq!(shifted("A5", &sh), "A5");
    assert_eq!(shifted("Лист1!A5", &sh), "Лист1!A6");
}

#[test]
fn shifting_past_the_edge_of_the_sheet_invalidates() {
    assert_eq!(shifted("A1048576", &Shift::insert_rows(0, 1)), "#REF!");
    assert_eq!(shifted("XFD1", &Shift::insert_cols(0, 1)), "#REF!");
    // Уже разрушенная ссылка остаётся разрушенной.
    assert_eq!(shifted("#REF!", &Shift::insert_rows(0, 1)), "#REF!");
}

#[test]
fn shift_reports_how_many_references_changed() {
    let mut e = ast("A5+B9+Лист2!C1");
    assert_eq!(shift_refs(&mut e, &Shift::insert_rows(0, 1)), 2);
    let mut e = ast("A1+A2");
    assert_eq!(shift_refs(&mut e, &Shift::insert_rows(90, 1)), 0);
}

#[test]
fn shifted_formulas_still_round_trip() {
    // Сдвиг обязан оставлять дерево печатаемым и заново разбираемым — иначе
    // записать его обратно в файл нельзя.
    for src in [
        "SUM(A1:A9)",
        "$A$1+Лист2!B2",
        "A:A",
        "1:1",
        "SUM(A1:A2 B1:B2)",
    ] {
        for sh in [
            Shift::insert_rows(0, 3),
            Shift::delete_rows(0, 2),
            Shift::insert_cols(1, 1),
            Shift::delete_cols(0, 1),
        ] {
            let mut e = ast(src);
            shift_refs(&mut e, &sh);
            let text = print_formula(&e);
            let again = parse_formula(&text)
                .unwrap_or_else(|err| panic!("{src} -> {text:?} не разбирается: {err}"));
            assert_eq!(print_formula(&again), text, "{src} -> {text:?}");
        }
    }
}

#[test]
fn axis_is_part_of_the_public_shape() {
    assert_eq!(Shift::insert_rows(0, 1).axis, Axis::Rows);
    assert_eq!(Shift::delete_cols(0, 1).axis, Axis::Cols);
    assert!(!Shift::delete_cols(0, 1).insert);
}

// ---------------------------------------------------------------------------
// Атаки и вырожденный вход
// ---------------------------------------------------------------------------

#[test]
fn a_hundred_thousand_nested_parens_gives_an_error_not_a_stack_overflow() {
    // Рекурсивный спуск без счётчика глубины здесь уронил бы процесс. Ошибка —
    // единственный приемлемый исход: недоверенный вход не должен убивать хост.
    let deep = format!("{}A1{}", "(".repeat(100_000), ")".repeat(100_000));
    assert_eq!(kind_of(&deep), FormulaError::TooDeep);

    // Унарные префиксы рекурсивны так же.
    assert_eq!(kind_of(&"-".repeat(100_000)), FormulaError::TooDeep);
    // И вложенные вызовы.
    let calls = format!("{}A1{}", "SUM(".repeat(50_000), ")".repeat(50_000));
    assert_eq!(kind_of(&calls), FormulaError::TooDeep);

    // Чуть ниже предела всё ещё разбирается — граница не съехала в ноль.
    let ok = MAX_DEPTH as usize / 2;
    let shallow = format!("{}A1{}", "(".repeat(ok), ")".repeat(ok));
    round_trips(&shallow);
}

#[test]
fn deepest_possible_formula_fits_in_a_small_stack() {
    // Счётчик глубины бесполезен, если разрешённая им глубина всё равно не
    // помещается в стек. Первая редакция этого модуля разрешала 256 уровней
    // при 13,4 КиБ кадра на уровень — 3,4 МиБ, и тест выше ронял процесс
    // вместо того, чтобы поймать ошибку.
    //
    // Здесь проверяется цифра, которая действительно важна: **1 МиБ** — стек
    // wasm32 по умолчанию, самая тесная из целевых площадок. Измеренный расход
    // на предельной глубине — около 387 КиБ.
    //
    // Переполнение стека убивает процесс целиком, поймать его нельзя. Значит,
    // провал этого теста выглядит как «fatal runtime error: stack overflow»,
    // а не как обычный `assert`. Так и должно быть: это тот самый отказ,
    // который тест обязан не пропустить.
    let depth = MAX_DEPTH as usize - 1;
    for src in [
        format!("{}A1{}", "SUM(".repeat(depth), ")".repeat(depth)),
        format!("{}A1{}", "(".repeat(depth), ")".repeat(depth)),
        format!("{}A1", "-".repeat(depth)),
    ] {
        let handle = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || {
                let e = parse_formula(&src).expect("предельная глубина обязана разбираться");
                // Печать и разрушение дерева рекурсивны так же, как разбор,
                // и обязаны укладываться в тот же бюджет.
                let text = print_formula(&e);
                drop(e);
                text.len()
            })
            .unwrap();
        assert!(handle.join().unwrap() > 0);
    }
}

#[test]
fn broken_input_yields_errors_with_positions() {
    assert_eq!(kind_of(""), FormulaError::UnexpectedEof);
    assert_eq!(kind_of("SUM(A1"), FormulaError::UnbalancedParen);
    assert_eq!(kind_of("(A1"), FormulaError::UnbalancedParen);
    assert_eq!(kind_of("A1)"), FormulaError::UnbalancedParen);
    assert_eq!(kind_of("\"незакрытая"), FormulaError::UnterminatedString);
    assert_eq!(kind_of("'Лист 1!A1"), FormulaError::UnterminatedString);
    assert_eq!(kind_of("1+"), FormulaError::UnexpectedEof);
    assert_eq!(kind_of("*1"), FormulaError::UnexpectedChar('*'));
    assert!(matches!(
        kind_of("A1 @ B1"),
        FormulaError::UnexpectedChar(_)
    ));

    // Позиция ошибки указывает на место, а не на ноль.
    let Err(Error::Formula { pos, .. }) = parse_formula("A1+A1+\"x") else {
        panic!("ожидалась ошибка");
    };
    assert_eq!(pos, 6);
}

#[test]
fn whitespace_only_and_lone_operators_do_not_panic() {
    for src in ["   ", "\n", ",", ";", ")", "}", "%", "()", "{}", "{", "\""] {
        assert!(parse_formula(src).is_err(), "{src:?} не должно разбираться");
    }
}

// ---------------------------------------------------------------------------
// Фаззер
// ---------------------------------------------------------------------------

/// SplitMix64 — детерминированный генератор на 15 строк.
///
/// Свой, а не из крейта: внешних зависимостей у ядра нет, а тесту нужен ровно
/// повторяемый поток, чтобы упавший прогон можно было воспроизвести по сиду.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

const BASE_SEED: u64 = 0x5DEE_CE66_D3A5_1B17;

fn seed_for(i: u64) -> u64 {
    BASE_SEED ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn iters() -> u64 {
    std::env::var("OOXML_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000)
}

/// Формулы-затравки для мутационного режима.
const SEEDS: &[&str] = &[
    "SUM(A1:A9)",
    "IF(AND(ISNUMBER(C6),ISNUMBER(C5)),C6-C5,\"\")",
    "'Лист 1'!$A$1:$B$2",
    "{1,2;3,4}",
    "-2^2+50%",
    "SUM(A1:A2 B1:B2)",
    "_xlfn.XLOOKUP(A1,B:B,C:C,\"нет\")",
    "[1]Книга!A1&#N/A",
    "SUM((A1,B1))",
    "IF(A1,,B1)",
];

#[test]
fn random_strings_never_panic() {
    // Первый режим: любой мусор обязан дать `Ok` или `Err`, но не панику и не
    // переполнение стека.
    //
    // Чисто случайные строки почти всегда отбраковываются на первом символе и
    // проверяют один и тот же путь. Поэтому половина прогонов — **мутации
    // валидных формул**: замена, вставка и удаление одного символа дают вход,
    // который доходит до глубины парсера и там ломается. Именно такие входы и
    // находят обрывы в середине конструкции, а не на её краю.
    const ALPHABET: &[char] = &[
        'A', 'B', 'Z', 'a', '1', '9', '0', '$', ':', '!', '(', ')', '{', '}', ',', ';', '+', '-',
        '*', '/', '^', '&', '=', '<', '>', '%', '"', '\'', '#', '.', '_', ' ', '\n', '[', ']', 'E',
        'e', 'Ф', '@', '\\', '?',
    ];
    let n = iters();
    let (mut ok, mut mutated_ok) = (0u64, 0u64);

    for i in 0..n {
        let mut rng = Rng(seed_for(i));
        let mutating = rng.chance(50);
        let src = if mutating {
            let base = *rng.pick(SEEDS);
            let mut chars: Vec<char> = base.chars().collect();
            let edits = 1 + rng.below(3);
            for _ in 0..edits {
                if chars.is_empty() {
                    break;
                }
                let at = rng.below(chars.len());
                match rng.below(3) {
                    0 => chars[at] = *rng.pick(ALPHABET),
                    1 => chars.insert(at, *rng.pick(ALPHABET)),
                    _ => {
                        chars.remove(at);
                    }
                }
            }
            chars.into_iter().collect()
        } else {
            let len = rng.below(40);
            (0..len).map(|_| *rng.pick(ALPHABET)).collect::<String>()
        };

        if let Ok(e) = parse_formula(&src) {
            ok += 1;
            if mutating {
                mutated_ok += 1;
            }
            // Всё, что разобралось, обязано печататься и разбираться снова.
            let text = print_formula(&e);
            let again = parse_formula(&text).unwrap_or_else(|err| {
                panic!(
                    "сид {}: {src:?} -> {text:?} не разбирается назад: {err}",
                    seed_for(i)
                )
            });
            assert_eq!(
                print_formula(&again),
                text,
                "сид {}: второй круг разошёлся на {src:?}",
                seed_for(i)
            );
            assert_eq!(again, e, "сид {}: дерево разошлось на {src:?}", seed_for(i));
        }
    }

    println!("случайные строки: {n} прогонов, разобралось {ok} (из них мутаций {mutated_ok})");
    // Сторож полезности: если мутации перестали доходить до успешного разбора,
    // тест превратился бы в проверку одной ветки «первый символ не тот».
    assert!(
        mutated_ok > n / 20,
        "мутации почти не разбираются ({mutated_ok} из {n}) — режим выродился"
    );
}

// --- Генератор случайных деревьев -----------------------------------------

/// Числа задаются парой «текст, значение»: собранное из `f64` написание не
/// обязано совпадать с тем, что напечатает принтер, а тексту верить можно.
const NUMS: &[&str] = &["0", "1", "42", "3.5", ".5", "1.", "1E+3", "1.5E-10", "007"];
const NAMES: &[&str] = &["МоиДанные", "_xlfn.SINGLE", "table_1", "стр.1_4", "Итого"];
const FUNCS: &[&str] = &["SUM", "IF", "_xlfn.XLOOKUP", "PI", "LOG10", "СУММ"];
const STRINGS: &[&str] = &["", "a", "он сказал \"да\"", "x,y;z", "  "];
const WS: &[&str] = &[" ", "  ", "\n", "\t", " \n "];

/// Приоритеты — те же числа, что в парсере.
const OPS: &[(BinOp, u8)] = &[
    (BinOp::Pow, 5),
    (BinOp::Mul, 4),
    (BinOp::Div, 4),
    (BinOp::Add, 3),
    (BinOp::Sub, 3),
    (BinOp::Concat, 2),
    (BinOp::Eq, 1),
    (BinOp::Ne, 1),
    (BinOp::Lt, 1),
    (BinOp::Le, 1),
    (BinOp::Gt, 1),
    (BinOp::Ge, 1),
];

struct Gen<'a> {
    rng: &'a mut Rng,
}

/// Настоящая глубина построенного дерева.
///
/// Считается по готовому дереву, а не по «бюджету», с которым работал
/// генератор. Разница принципиальна: бюджет — это то, что генератору было
/// РАЗРЕШЕНО, а не то, чем он воспользовался. В вехе M5 сторож смотрел именно
/// на разрешение, генератор незаметно сполз к плоским деревьям, и зелёный тест
/// доказывал куда меньше, чем казалось.
fn tree_depth(e: &Expr) -> u32 {
    let sub = match &e.kind {
        ExprKind::Func { args, .. } | ExprKind::Union(args) => {
            args.iter().map(tree_depth).max().unwrap_or(0)
        }
        ExprKind::Unary { operand, .. } => tree_depth(operand),
        ExprKind::Percent(inner) | ExprKind::Paren(inner) => tree_depth(inner),
        ExprKind::Binary { lhs, rhs, .. } => tree_depth(lhs).max(tree_depth(rhs)),
        ExprKind::Array(rows) => rows
            .iter()
            .flat_map(|r| r.iter().map(tree_depth))
            .max()
            .unwrap_or(0),
        _ => 0,
    };
    sub.saturating_add(1)
}

impl Gen<'_> {
    fn ws(&mut self) -> &'static str {
        self.rng.pick(WS)
    }

    fn cell(&mut self) -> CellRef {
        CellRef {
            col: self.rng.below(30) as u32,
            row: self.rng.below(50) as u32,
            col_abs: self.rng.chance(40),
            row_abs: self.rng.chance(40),
        }
    }

    fn reference(&mut self) -> Reference {
        let body = match self.rng.below(5) {
            0 => RefBody::Invalid,
            1 => {
                let (a, b) = (self.cell(), self.cell());
                RefBody::Area { from: a, to: b }
            }
            2 => RefBody::Cols {
                from: ooxml::formula::Line {
                    idx: self.rng.below(20) as u32,
                    abs: self.rng.chance(40),
                },
                to: ooxml::formula::Line {
                    idx: self.rng.below(20) as u32,
                    abs: self.rng.chance(40),
                },
            },
            3 => RefBody::Rows {
                from: ooxml::formula::Line {
                    idx: self.rng.below(20) as u32,
                    abs: self.rng.chance(40),
                },
                to: ooxml::formula::Line {
                    idx: self.rng.below(20) as u32,
                    abs: self.rng.chance(40),
                },
            },
            _ => RefBody::Cell(self.cell()),
        };
        let sheet = if self.rng.chance(30) {
            let names: &[&str] = &["Лист1", "Sheet1", "стр.1_4"];
            let quoted_names: &[&str] = &["Лист 1", "Ivan's Sheet", "a b"];
            Some(Box::new(if self.rng.chance(50) {
                ooxml::formula::SheetPrefix::plain(self.rng.pick(names))
            } else {
                ooxml::formula::SheetPrefix {
                    book: if self.rng.chance(30) {
                        Some("[1]".to_owned())
                    } else {
                        None
                    },
                    first: (*self.rng.pick(quoted_names)).to_owned(),
                    last: None,
                    quoted: true,
                }
            }))
        } else {
            None
        };
        Reference { sheet, body }
    }

    /// Скаляр-константа: то, что допустимо внутри литерала массива.
    fn scalar(&mut self) -> Expr {
        match self.rng.below(4) {
            0 => {
                let raw = *self.rng.pick(NUMS);
                Expr::new(ExprKind::Num {
                    value: raw.parse().unwrap(),
                    raw: raw.into(),
                })
            }
            1 => Expr::str_lit(self.rng.pick(STRINGS)),
            2 => Expr::new(ExprKind::Bool(self.rng.chance(50))),
            _ => Expr::new(ExprKind::Err(*self.rng.pick(&[
                ooxml::formula::ErrKind::Div0,
                ooxml::formula::ErrKind::Na,
                ooxml::formula::ErrKind::Value,
            ]))),
        }
    }

    fn primary(&mut self, depth: u32) -> Expr {
        if depth == 0 {
            return self.scalar();
        }
        match self.rng.below(10) {
            0..=2 => self.scalar(),
            3 | 4 => Expr::reference(self.reference()),
            5 => Expr::new(ExprKind::Name((*self.rng.pick(NAMES)).into())),
            6 | 7 => self.call(depth),
            8 => self.parens(depth),
            _ => self.array(),
        }
    }

    fn call(&mut self, depth: u32) -> Expr {
        let name = *self.rng.pick(FUNCS);
        let n = self.rng.below(4);
        let mut args = Vec::new();
        for _ in 0..n {
            // Пропущенный аргумент допустим только когда их несколько:
            // `SUM(,)` — два пустых, а `SUM()` — ни одного, и это разные вещи.
            // Пропущенный аргумент допустим только когда их несколько.
            let missing = n >= 2 && self.rng.chance(15);
            let mut a = if missing {
                Expr::new(ExprKind::Missing)
            } else {
                self.expr(1, depth.saturating_sub(1), true)
            };
            if self.rng.chance(20) {
                a.set_leading_ws(self.ws());
            }
            // У пропуска нет собственных токенов, поэтому пробел слева и
            // справа от него — один и тот же участок текста. Канонично он
            // весь ведущий; хвостовой сделал бы дерево неразличимым по печати.
            if !missing && self.rng.chance(20) {
                a.set_trailing_ws(self.ws());
            }
            args.push(a);
        }
        Expr::call(name, args)
    }

    fn parens(&mut self, depth: u32) -> Expr {
        let d = depth.saturating_sub(1);
        let inner = if self.rng.chance(25) {
            // Запятая внутри скобок — объединение.
            let n = 2 + self.rng.below(2);
            let mut items = Vec::new();
            for _ in 0..n {
                let mut it = self.expr(1, d, true);
                if self.rng.chance(20) {
                    it.set_leading_ws(self.ws());
                }
                if self.rng.chance(20) {
                    it.set_trailing_ws(self.ws());
                }
                items.push(it);
            }
            Expr::new(ExprKind::Union(items))
        } else {
            let mut e = self.expr(1, d, true);
            if self.rng.chance(25) {
                e.set_leading_ws(self.ws());
            }
            if self.rng.chance(25) {
                e.set_trailing_ws(self.ws());
            }
            e
        };
        Expr::paren(inner)
    }

    fn array(&mut self) -> Expr {
        let rows = 1 + self.rng.below(2);
        let cols = 1 + self.rng.below(3);
        let mut out = Vec::new();
        for _ in 0..rows {
            let mut row = Vec::new();
            for _ in 0..cols {
                let mut e = self.scalar();
                if self.rng.chance(15) {
                    e.set_leading_ws(self.ws());
                }
                if self.rng.chance(15) {
                    e.set_trailing_ws(self.ws());
                }
                row.push(e);
            }
            out.push(row);
        }
        Expr::new(ExprKind::Array(out))
    }

    fn postfix(&mut self, depth: u32) -> Expr {
        let mut e = self.primary(depth);
        while self.rng.chance(12) {
            if self.rng.chance(30) {
                // Пробел перед `%` принадлежит операнду.
                e.set_trailing_ws(self.ws());
            }
            e = Expr::new(ExprKind::Percent(Box::new(e)));
        }
        e
    }

    /// Операнд, каким его видит парсер на входе в уровень: сначала унарные
    /// префиксы, потом ссылочные операторы, потом всё остальное.
    ///
    /// `lead_unary_ok == false` запрещает начинать операнд с `-` или `+`. Это
    /// нужно правому операнду пересечения: пробел становится оператором только
    /// когда следующий токен начинает операнд, а `-` его не начинает. Дерево
    /// `Intersect(A1, -B1)` напечаталось бы как `A1 -B1` и вернулось бы
    /// вычитанием — парсер прав, а такое дерево просто недостижимо.
    fn unary(&mut self, depth: u32, lead_unary_ok: bool) -> Expr {
        if lead_unary_ok && depth > 0 && self.rng.chance(12) {
            let op = if self.rng.chance(80) {
                UnaryOp::Neg
            } else {
                UnaryOp::Plus
            };
            // Операнд унарного префикса разбирается на уровне 6 — он забирает
            // `:` и пересечение, но не `^`.
            let mut operand = self.expr(6, depth.saturating_sub(1), true);
            if self.rng.chance(25) {
                operand.set_leading_ws(self.ws());
            }
            let mut e = Expr::new(ExprKind::Unary {
                op,
                operand: Box::new(operand),
            });
            e.ws_before = String::new().into_boxed_str();
            return e;
        }
        self.postfix(depth)
    }

    /// Зеркало `Parser::expr`: левоассоциативная цепочка с невозрастающим
    /// приоритетом вдоль хребта — именно такие деревья строит разбор, и только
    /// на них равенство `parse(print(ast)) == ast` вообще осмысленно.
    fn expr(&mut self, min_prec: u8, depth: u32, lead_unary_ok: bool) -> Expr {
        let mut lhs = self.unary(depth, lead_unary_ok);
        // Если слева унарный префикс, он уже съел уровни 6..8 — дальше
        // допустимы только операторы не сильнее `^`.
        let mut cur_max: u8 = if matches!(lhs.kind, ExprKind::Unary { .. }) {
            5
        } else {
            8
        };
        while depth > 0 && self.rng.chance(45) {
            let (op, p) = self.choose_op(min_prec, cur_max);
            let Some(op) = op else { break };
            let mut rhs = self.expr(
                p.saturating_add(1),
                depth.saturating_sub(1),
                op != BinOp::Intersect,
            );
            let ws_op = if op == BinOp::Intersect {
                // Пересечение: оператор — сам пробел, и он обязан быть непустым.
                // Ведущий пробел правого операнда слился бы с ним, поэтому там
                // его быть не должно.
                self.ws().to_owned()
            } else {
                let w = if self.rng.chance(25) { self.ws() } else { "" };
                if self.rng.chance(25) {
                    rhs.set_leading_ws(self.ws());
                }
                w.to_owned()
            };
            lhs = Expr::new(ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                ws_op: ws_op.into_boxed_str(),
            });
            cur_max = p;
        }
        lhs
    }

    /// Выбирает оператор с приоритетом в `[min_prec, cur_max]`.
    ///
    /// `:` не порождается с двумя ссылочными операндами намеренно: `A1:B2`
    /// лексер собирает в ОДНУ ссылку-прямоугольник, и дерево `Binary(Range,
    /// Ref, Ref)` после круга вернулось бы другим. Это не дефект парсера, а
    /// свойство грамматики, и генератор обязан его знать.
    fn choose_op(&mut self, min_prec: u8, cur_max: u8) -> (Option<BinOp>, u8) {
        if min_prec <= 7 && cur_max >= 7 && self.rng.chance(15) {
            return (Some(BinOp::Intersect), 7);
        }
        let candidates: Vec<(BinOp, u8)> = OPS
            .iter()
            .copied()
            .filter(|&(_, p)| p >= min_prec && p <= cur_max)
            .collect();
        if candidates.is_empty() {
            return (None, 0);
        }
        let (op, p) = *self.rng.pick(&candidates);
        (Some(op), p)
    }
}

#[test]
fn random_asts_survive_print_and_parse() {
    // Второй режим — главный. Он ловит не падения, а расхождение принтера с
    // парсером: печать, которую собственный разбор понимает иначе. Такое
    // расхождение молча портит чужой файл, и никакой тест на «не паникует»
    // его не увидит.
    let n = iters();
    let mut deepest = 0u32;
    let mut total_nodes = 0u64;

    for i in 0..n {
        let mut rng = Rng(seed_for(i));
        let mut g = Gen { rng: &mut rng };
        let budget = 3 + g.rng.below(6) as u32;
        let mut e = g.expr(1, budget, true);
        if g.rng.chance(20) {
            e.set_leading_ws(g.ws());
        }
        if g.rng.chance(20) {
            e.set_trailing_ws(g.ws());
        }
        deepest = deepest.max(tree_depth(&e));

        let text = print_formula(&e);
        let back = parse_formula(&text).unwrap_or_else(|err| {
            panic!(
                "сид {}: {text:?} не разбирается: {err}\n{e:#?}",
                seed_for(i)
            )
        });
        assert_eq!(
            back,
            e,
            "сид {}: дерево изменилось после круга\nтекст: {text:?}",
            seed_for(i)
        );
        assert_eq!(
            print_formula(&back),
            text,
            "сид {}: печать разошлась",
            seed_for(i)
        );
        let mut nodes = 0u64;
        e.walk(&mut |_| nodes += 1);
        total_nodes += nodes;
    }

    let avg = total_nodes as f64 / n as f64;
    println!(
        "случайные деревья: {n} прогонов, максимальная глубина {deepest}, узлов в среднем {avg:.1}"
    );

    // Сторож вырождения. В вехе M5 генератор незаметно сполз к плоским
    // деревьям, и зелёный тест доказывал куда меньше, чем казалось.
    assert!(
        deepest >= 8,
        "генератор выродился: максимальная глубина дерева всего {deepest}"
    );
    assert!(
        avg >= 4.0,
        "деревья слишком мелкие: в среднем {avg:.1} узлов"
    );
}
