//! Abstract syntax for the SLCR GQL query profile.
//!
//! The AST covers exactly the profile described in `PROFILE.md`. Anything the
//! parser accepts is representable here; anything outside the profile is
//! rejected before an AST is produced.

use std::collections::BTreeMap;

pub type Span = std::ops::Range<usize>;

/// A linear composite query: a sequence of clauses ending in `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub clauses: Vec<Clause>,
    pub ret: Return,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// `MATCH <pattern>, ... [WHERE <expr>]`
    Match(Match),
    /// GQL `FILTER [WHERE] <expr>`: keep rows where the condition is TRUE.
    Filter(Expr),
    /// GQL `FOR <var> IN <expr>` (Cypher alias: `UNWIND <expr> AS <var>`).
    For { var: String, list: Expr },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub patterns: Vec<PathPattern>,
    pub where_: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    pub start: NodePattern,
    pub steps: Vec<(EdgePattern, NodePattern)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodePattern {
    pub var: Option<String>,
    pub labels: Option<LabelExpr>,
    pub props: Vec<(String, Expr)>,
    pub where_: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `-[]->`
    Right,
    /// `<-[]-`
    Left,
    /// `-[]-` (and Cypher `<-[]->`): either direction.
    Any,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgePattern {
    pub var: Option<String>,
    pub labels: Option<LabelExpr>,
    pub props: Vec<(String, Expr)>,
    pub where_: Option<Expr>,
    pub direction: Direction,
    /// `{m,n}` (GQL) or `*m..n` (Cypher alias). `None` means exactly one hop.
    pub quantifier: Option<Quantifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantifier {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LabelExpr {
    Label(String),
    /// `%` — any (non-empty) label set.
    Wildcard,
    And(Box<LabelExpr>, Box<LabelExpr>),
    Or(Box<LabelExpr>, Box<LabelExpr>),
    Not(Box<LabelExpr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Return {
    pub distinct: bool,
    pub items: ReturnItems,
    pub order_by: Vec<SortItem>,
    pub offset: Option<Expr>,
    pub limit: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReturnItems {
    /// `RETURN *`
    Star,
    Items(Vec<ReturnItem>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
    /// Source text of the expression; the column name when there is no alias.
    pub text: String,
}

impl ReturnItem {
    pub fn column_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.text)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    pub expr: Expr,
    pub text: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    Xor,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    In,
    StartsWith,
    EndsWith,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
    Pos,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Param(String),
    Var(String),
    List(Vec<Expr>),
    Map(BTreeMap<String, Expr>),
    Property(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `e IS [NOT] NULL`
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `e IS [NOT] LABELED <label expr>` (Cypher alias: `e:Label`)
    IsLabeled {
        expr: Box<Expr>,
        labels: LabelExpr,
        negated: bool,
    },
    /// Ordinary or aggregate function call. `name` is the canonical
    /// (lower-case, GQL) name; Cypher aliases are resolved by the parser.
    Call {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
    },
    /// `count(*)`
    CountStar,
    /// Produced by the planner: reference to a computed aggregate.
    AggRef(usize),
}

impl Expr {
    pub fn contains_aggregate(&self) -> bool {
        match self {
            Expr::CountStar | Expr::AggRef(_) => true,
            Expr::Call { name, args, .. } => {
                crate::functions::is_aggregate(name) || args.iter().any(Expr::contains_aggregate)
            }
            Expr::List(xs) => xs.iter().any(Expr::contains_aggregate),
            Expr::Map(m) => m.values().any(Expr::contains_aggregate),
            Expr::Property(e, _) | Expr::Unary(_, e) => e.contains_aggregate(),
            Expr::IsNull { expr, .. } | Expr::IsLabeled { expr, .. } => expr.contains_aggregate(),
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                a.contains_aggregate() || b.contains_aggregate()
            }
            Expr::Literal(_) | Expr::Param(_) | Expr::Var(_) => false,
        }
    }

    /// Visit every variable name referenced by this expression.
    pub fn visit_vars<'a>(&'a self, f: &mut impl FnMut(&'a str)) {
        match self {
            Expr::Var(v) => f(v),
            Expr::List(xs) => xs.iter().for_each(|x| x.visit_vars(f)),
            Expr::Map(m) => m.values().for_each(|x| x.visit_vars(f)),
            Expr::Call { args, .. } => args.iter().for_each(|x| x.visit_vars(f)),
            Expr::Property(e, _) | Expr::Unary(_, e) => e.visit_vars(f),
            Expr::IsNull { expr, .. } | Expr::IsLabeled { expr, .. } => expr.visit_vars(f),
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                a.visit_vars(f);
                b.visit_vars(f);
            }
            Expr::Literal(_) | Expr::Param(_) | Expr::CountStar | Expr::AggRef(_) => {}
        }
    }
}

/// Non-normative data-loading statement used by tests and fixtures:
/// one or more `CREATE`/`INSERT` clauses of graph patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixture {
    pub inserts: Vec<Vec<PathPattern>>,
}
