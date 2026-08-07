//! Дерево разбора формулы.
//!
//! # Почему в узлах хранятся пробелы
//!
//! Слой `xml` держится на измеренном выводе: «незначимого форматирования» в
//! OOXML не бывает. Формулы — не исключение. В корпусе есть формула, записанная
//! в две строки:
//!
//! ```text
//! IF(C4:C999="","",
//! XLOOKUP(C4:C999,'Страны'!$A$2:$A$11,'Страны'!$B$2:$B$11, "Визовый"))
//! ```
//!
//! Перевод строки внутри вызова и пробел перед последним аргументом — это то,
//! что человек набрал и что Excel сохранил дословно. Печать «канонического»
//! вида отдала бы другой текст, и цель вехи — точное совпадение с исходником —
//! была бы недостижима принципиально, а не по недосмотру.
//!
//! Отсюда два поля на каждом узле: [`Expr::ws_before`] и [`Expr::ws_after`].
//!
//! # Канонизация: куда пробел приписывается
//!
//! Одну и ту же строку можно разложить по узлам несколькими способами —
//! в `SUM(A1 )` пробел мог бы принадлежать и аргументу, и вызову. Разбор всегда
//! выбирает **самый внутренний узел, чья граница касается пробела**:
//!
//! * `ws_before` получает узел, чей первый токен идёт сразу за пробелом;
//! * `ws_after` — узел, чей последний токен идёт сразу перед ним.
//!
//! Поэтому у [`ExprKind::Binary`] оба поля всегда пусты: его первый токен
//! принадлежит левому операнду, последний — правому. Свойство
//! `parse(print(ast)) == ast` держится только на канонических деревьях;
//! собранное вручную дерево с пробелом «не на том» узле напечатается верно, но
//! после обратного разбора пробел переедет на канонический узел.
//!
//! # Пробел как оператор
//!
//! Пробел между двумя операндами — это оператор пересечения
//! ([`BinOp::Intersect`]), а не форматирование, и хранится он в
//! [`ExprKind::Binary::ws_op`]. Различить их можно только по контексту: в
//! `SUM(A1:A2 B1:B2)` пробел — оператор, в `SUM(A1, B1)` — нет.

use super::refs::Reference;

/// Значение-ошибка Excel.
///
/// `#REF!` в этом списке отсутствует намеренно: у него бывает имя листа
/// (`Лист1!#REF!`) и он участвует в сдвигах, поэтому живёт как
/// [`crate::formula::RefBody::Invalid`] внутри ссылки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrKind {
    Div0,
    Na,
    Value,
    Name,
    Null,
    Num,
    GettingData,
    Spill,
    Calc,
    Field,
    Blocked,
    Connect,
    Unknown,
    Busy,
    External,
}

impl ErrKind {
    /// Текст литерала ровно так, как он пишется в формуле.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Div0 => "#DIV/0!",
            Self::Na => "#N/A",
            Self::Value => "#VALUE!",
            Self::Name => "#NAME?",
            Self::Null => "#NULL!",
            Self::Num => "#NUM!",
            Self::GettingData => "#GETTING_DATA",
            Self::Spill => "#SPILL!",
            Self::Calc => "#CALC!",
            Self::Field => "#FIELD!",
            Self::Blocked => "#BLOCKED!",
            Self::Connect => "#CONNECT!",
            Self::Unknown => "#UNKNOWN!",
            Self::Busy => "#BUSY!",
            Self::External => "#EXTERNAL!",
        }
    }

    /// Все литералы, от длинных к коротким.
    ///
    /// Порядок важен для лексера: `#NUM!` — префикс ничего, но `#N/A` и
    /// гипотетическое `#N/AB` разошлись бы при сравнении в другом порядке.
    /// Дешевле зафиксировать порядок здесь, чем ловить это в лексере.
    pub(crate) const ALL: &'static [Self] = &[
        Self::GettingData,
        Self::Blocked,
        Self::Connect,
        Self::Unknown,
        Self::External,
        Self::Value,
        Self::Div0,
        Self::Spill,
        Self::Field,
        Self::Busy,
        Self::Calc,
        Self::Name,
        Self::Null,
        Self::Num,
        Self::Na,
    ];
}

/// Префиксный оператор.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Унарный минус. Связывает **сильнее** возведения в степень: `-2^2` в
    /// Excel равно 4, а не −4.
    Neg,
    /// Унарный плюс. Значения не меняет, но пишется и обязан пережить круг.
    Plus,
}

/// Инфиксный оператор.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    /// `:` — построение диапазона из двух ссылок.
    Range,
    /// Пробел — пересечение диапазонов. Текст самого оператора лежит в `ws_op`.
    Intersect,
    Pow,
    Mul,
    Div,
    Add,
    Sub,
    /// `&` — склейка строк.
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    /// Текст оператора. Для [`Self::Intersect`] пуст: там оператор — сам
    /// пробел, и он хранится отдельно.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Range => ":",
            Self::Intersect => "",
            Self::Pow => "^",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Concat => "&",
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// Узел дерева: вид плюс окружающие его пробелы.
///
/// # Зачем узлу пробелы
///
/// Чтобы `print(parse(x))` возвращал **исходный текст**, а не эквивалентный.
/// В корпусе есть формула, записанная в две строки:
///
/// ```text
/// IF(C4:C999="","",
/// XLOOKUP(C4:C999,'Страны'!$A$2:$A$11,'Страны'!$B$2:$B$11, "Визовый"))
/// ```
///
/// Перевод строки внутри вызова и пробел перед последним аргументом набрал
/// человек, а Excel сохранил дословно. Печать «канонического» вида отдала бы
/// другой текст — и записать формулу обратно в чужой файл, ничего не изменив,
/// стало бы невозможно принципиально.
///
/// # Канонический вид
///
/// Одну и ту же строку можно разложить по узлам несколькими способами: в
/// `SUM(A1 )` пробел мог бы принадлежать и аргументу, и вызову. Разбор всегда
/// выбирает **самый внутренний узел, чья граница касается пробела**:
///
/// * [`Self::ws_before`] получает узел, чей первый токен идёт сразу за пробелом;
/// * [`Self::ws_after`] — узел, чей последний токен идёт сразу перед ним.
///
/// Отсюда следствие: у [`ExprKind::Binary`] оба поля всегда пусты — его первый
/// токен принадлежит левому операнду, последний правому. Ставить пробелы
/// руками удобнее через [`Self::set_leading_ws`] и [`Self::set_trailing_ws`]:
/// они сами спускаются до нужного узла.
///
/// Равенство `parse(print(ast)) == ast` держится только на канонических
/// деревьях. Собранное вручную дерево с пробелом «не на том» узле напечатается
/// верно, но после обратного разбора пробел переедет на канонический узел.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    /// Пробелы непосредственно перед первым токеном узла.
    pub ws_before: Box<str>,
    /// Пробелы непосредственно после последнего токена узла.
    pub ws_after: Box<str>,
}

/// Вид узла.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ExprKind {
    /// Число вместе с исходным написанием.
    ///
    /// `1`, `1.`, `1.0` и `1E+0` — одно значение `f64` и четыре разных текста.
    /// Восстановить написание из числа нельзя, поэтому оно хранится рядом.
    Num {
        value: f64,
        raw: Box<str>,
    },
    /// Строковый литерал. Значение уже раскодировано: `""` внутри кавычек
    /// превращено в один символ `"`.
    Str(Box<str>),
    Bool(bool),
    Err(ErrKind),
    Ref(Reference),
    /// Определённое имя, `#REF!`-имя или что угодно ещё, что не ссылка.
    Name(Box<str>),
    /// Вызов функции. Имя хранится как в исходнике, вместе с префиксом
    /// `_xlfn.`, которым Excel помечает функции новее файлового формата.
    Func {
        name: Box<str>,
        args: Vec<Expr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    /// Постфиксный `%`: делит на сто.
    Percent(Box<Expr>),
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        /// Пробелы между левым операндом и оператором. Для
        /// [`BinOp::Intersect`] здесь лежит сам оператор и пустым быть не может.
        ws_op: Box<str>,
    },
    /// Объединение диапазонов запятой: `(A1,B1)`.
    ///
    /// Существует только внутри скобок. В списке аргументов та же запятая —
    /// разделитель, и это единственная настоящая двусмысленность грамматики
    /// Excel: `SUM(A1,B1)` — два аргумента, `SUM((A1,B1))` — один.
    Union(Vec<Expr>),
    /// Скобки.
    ///
    /// Хранятся как узел, а не выбрасываются: без них печать `(A1+B1)*2` дала
    /// бы `A1+B1*2`, то есть другую формулу.
    Paren(Box<Expr>),
    /// Литерал массива `{1,2;3,4}`. Внешний вектор — строки, внутренний —
    /// колонки внутри строки.
    Array(Vec<Vec<Expr>>),
    /// Пропущенный аргумент: `IF(A1,,B1)` — три аргумента, средний пуст.
    ///
    /// Печатается пустотой, поэтому пробелы слева и справа от него сливаются в
    /// один неразличимый участок: в `IF(A1, ,B1)` невозможно узнать, чей это
    /// пробел. Канонический вид — весь пробел в `ws_before`, `ws_after` пуст;
    /// разбор строит только такой.
    ///
    /// Excel это допускает и записывает в файл как есть, поэтому пропуск —
    /// полноправный узел, а не отсутствие узла: `SUM(,)` и `SUM()` различны.
    Missing,
}

impl Expr {
    /// Узел без окружающих пробелов.
    #[must_use]
    pub fn new(kind: ExprKind) -> Self {
        Self {
            kind,
            ws_before: String::new().into_boxed_str(),
            ws_after: String::new().into_boxed_str(),
        }
    }

    /// Число с автоматически подобранным написанием.
    #[must_use]
    pub fn num(value: f64) -> Self {
        Self::new(ExprKind::Num {
            value,
            raw: format_num(value).into_boxed_str(),
        })
    }

    #[must_use]
    pub fn str_lit(s: &str) -> Self {
        Self::new(ExprKind::Str(s.into()))
    }

    #[must_use]
    pub fn reference(r: Reference) -> Self {
        Self::new(ExprKind::Ref(r))
    }

    #[must_use]
    pub fn call(name: &str, args: Vec<Self>) -> Self {
        Self::new(ExprKind::Func {
            name: name.into(),
            args,
        })
    }

    #[must_use]
    pub fn binary(op: BinOp, lhs: Self, rhs: Self) -> Self {
        Self::new(ExprKind::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            ws_op: if op == BinOp::Intersect {
                " ".into()
            } else {
                String::new().into_boxed_str()
            },
        })
    }

    #[must_use]
    pub fn paren(inner: Self) -> Self {
        Self::new(ExprKind::Paren(Box::new(inner)))
    }

    /// Приписывает узлу пробелы, которые в исходнике стояли перед ним.
    #[must_use]
    pub fn with_ws_before(mut self, ws: &str) -> Self {
        self.ws_before = ws.into();
        self
    }

    /// Приписывает пробелы **самому внутреннему правому** узлу поддерева.
    ///
    /// Именно так разбор поддерживает канонический вид: пробел перед `,` или
    /// `)` в `SUM(A1 , B1)` принадлежит не вызову, а аргументу, который перед
    /// ним закончился.
    pub fn set_trailing_ws(&mut self, ws: &str) {
        match &mut self.kind {
            ExprKind::Binary { rhs, .. } => rhs.set_trailing_ws(ws),
            ExprKind::Unary { operand, .. } => operand.set_trailing_ws(ws),
            ExprKind::Union(items) => match items.last_mut() {
                Some(last) => last.set_trailing_ws(ws),
                None => self.ws_after = ws.into(),
            },
            // Остальные виды заканчиваются собственным токеном: числом,
            // именем, закрывающей скобкой, знаком процента.
            _ => self.ws_after = ws.into(),
        }
    }

    /// Приписывает пробелы **самому внутреннему левому** узлу поддерева.
    ///
    /// Зеркало [`Self::set_trailing_ws`]. Пробел после `(`, после запятой или
    /// после оператора принадлежит не составному узлу, а тому листу, с которого
    /// этот узел начинается: в `A1 + B1` пробел после `+` — это `ws_before`
    /// у `B1`, а не у сложения.
    pub fn set_leading_ws(&mut self, ws: &str) {
        match &mut self.kind {
            ExprKind::Binary { lhs, .. } => lhs.set_leading_ws(ws),
            ExprKind::Percent(operand) => operand.set_leading_ws(ws),
            ExprKind::Union(items) => match items.first_mut() {
                Some(first) => first.set_leading_ws(ws),
                None => self.ws_before = ws.into(),
            },
            // Остальные виды начинаются собственным токеном: числом, именем,
            // открывающей скобкой, знаком унарного оператора.
            _ => self.ws_before = ws.into(),
        }
    }

    /// Обходит все ссылки дерева.
    pub fn visit_refs<F: FnMut(&Reference)>(&self, f: &mut F) {
        self.walk(&mut |e| {
            if let ExprKind::Ref(r) = &e.kind {
                f(r);
            }
        });
    }

    /// Обходит все ссылки дерева, разрешая их менять.
    pub fn visit_refs_mut<F: FnMut(&mut Reference)>(&mut self, f: &mut F) {
        self.walk_mut(&mut |e| {
            if let ExprKind::Ref(r) = &mut e.kind {
                f(r);
            }
        });
    }

    /// Обход в глубину, родитель раньше детей.
    pub fn walk<F: FnMut(&Self)>(&self, f: &mut F) {
        f(self);
        match &self.kind {
            ExprKind::Func { args, .. } | ExprKind::Union(args) => {
                for a in args {
                    a.walk(f);
                }
            }
            ExprKind::Unary { operand, .. } => operand.walk(f),
            ExprKind::Percent(e) | ExprKind::Paren(e) => e.walk(f),
            ExprKind::Binary { lhs, rhs, .. } => {
                lhs.walk(f);
                rhs.walk(f);
            }
            ExprKind::Array(rows) => {
                for row in rows {
                    for e in row {
                        e.walk(f);
                    }
                }
            }
            ExprKind::Num { .. }
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Err(_)
            | ExprKind::Ref(_)
            | ExprKind::Name(_)
            | ExprKind::Missing => {}
        }
    }

    /// Обход в глубину с правом изменять узлы.
    pub fn walk_mut<F: FnMut(&mut Self)>(&mut self, f: &mut F) {
        f(self);
        match &mut self.kind {
            ExprKind::Func { args, .. } | ExprKind::Union(args) => {
                for a in args {
                    a.walk_mut(f);
                }
            }
            ExprKind::Unary { operand, .. } => operand.walk_mut(f),
            ExprKind::Percent(e) | ExprKind::Paren(e) => e.walk_mut(f),
            ExprKind::Binary { lhs, rhs, .. } => {
                lhs.walk_mut(f);
                rhs.walk_mut(f);
            }
            ExprKind::Array(rows) => {
                for row in rows {
                    for e in row {
                        e.walk_mut(f);
                    }
                }
            }
            ExprKind::Num { .. }
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Err(_)
            | ExprKind::Ref(_)
            | ExprKind::Name(_)
            | ExprKind::Missing => {}
        }
    }
}

/// Написание числа для узлов, собранных не разбором, а кодом.
///
/// `{:?}` у `f64` даёт `1.0` там, где формула хочет `1`. Excel целые числа
/// пишет без дробной части, и обратный разбор такого текста обязан дать то же
/// значение — иначе круг `print` → `parse` разошёлся бы на каждой единице.
fn format_num(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}
