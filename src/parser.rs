//! Parser for the SLCR GQL profile.
//!
//! Two stages: [`crate::lexer`] produces tokens; the combinators here build
//! a recursive-descent parser over that token stream. Each grammar rule is a
//! function, mirroring the profile grammar in `PROFILE.md`.

use crate::ast::*;
use crate::error::{Error, unsupported_keyword};
use crate::lexer::{Span, Token, lexer};
use chumsky::input::{Input as _, ValueInput};
use chumsky::prelude::*;
use std::collections::BTreeMap;

type E<'t> = extra::Err<Rich<'t, Token, Span>>;

/// Words that may not be used as unquoted variable names.
pub const RESERVED: &[&str] = &[
    "MATCH",
    "WHERE",
    "RETURN",
    "DISTINCT",
    "AS",
    "ORDER",
    "BY",
    "ASC",
    "ASCENDING",
    "DESC",
    "DESCENDING",
    "SKIP",
    "OFFSET",
    "LIMIT",
    "AND",
    "OR",
    "XOR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "FOR",
    "UNWIND",
    "STARTS",
    "ENDS",
    "CONTAINS",
    "LABELED",
    "CREATE",
    "INSERT",
    "OPTIONAL",
    "WITH",
    "MERGE",
    "DELETE",
    "DETACH",
    "SET",
    "REMOVE",
    "CALL",
    "UNION",
    "YIELD",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "EXISTS",
    "FILTER",
    "LET",
    "NEXT",
    "FOREACH",
    "USE",
];

fn is_reserved(s: &str) -> bool {
    RESERVED.iter().any(|k| k.eq_ignore_ascii_case(s))
}

fn kw<'t, I>(k: &'static str) -> impl Parser<'t, I, (), E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! { Token::Ident(s) if s.eq_ignore_ascii_case(k) => () }.labelled(k)
}

fn op<'t, I>(o: &'static str) -> impl Parser<'t, I, (), E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Op(o)).ignored()
}

/// A variable name: an unreserved identifier or any quoted identifier.
fn variable<'t, I>() -> impl Parser<'t, I, String, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! {
        Token::Ident(s) if !is_reserved(&s) => s,
        Token::QuotedIdent(s) => s,
    }
    .labelled("variable")
}

/// A label, property key or function name: any identifier.
fn name<'t, I>() -> impl Parser<'t, I, String, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! {
        Token::Ident(s) => s,
        Token::QuotedIdent(s) => s,
    }
    .labelled("name")
}

fn label_expr<'t, I>() -> impl Parser<'t, I, LabelExpr, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(|lexpr| {
        let primary = choice((
            name().map(LabelExpr::Label),
            op("%").to(LabelExpr::Wildcard),
            lexpr.delimited_by(op("("), op(")")),
        ));
        let negation = op("!")
            .repeated()
            .foldr(primary, |_, e| LabelExpr::Not(Box::new(e)));
        let conj = negation
            .clone()
            .foldl(op("&").ignore_then(negation).repeated(), |a, b| {
                LabelExpr::And(Box::new(a), Box::new(b))
            });
        // Cypher writes alternation as `:A|B` or `:A|:B`.
        conj.clone().foldl(
            op("|").then(op(":").or_not()).ignore_then(conj).repeated(),
            |a, b| LabelExpr::Or(Box::new(a), Box::new(b)),
        )
    })
}

/// `:A`, `:A:B` (Cypher conjunction), `IS A&B` (GQL).
fn label_spec<'t, I>() -> impl Parser<'t, I, LabelExpr, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let head = op(":").or(kw("IS")).ignore_then(label_expr());
    head.foldl(op(":").ignore_then(label_expr()).repeated(), |a, b| {
        LabelExpr::And(Box::new(a), Box::new(b))
    })
}

fn int_lit<'t, I>() -> impl Parser<'t, I, u32, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! { Token::Int(n) => n }.try_map(|n, span| {
        u32::try_from(n).map_err(|_| Rich::custom(span, "quantifier bound too large"))
    })
}

/// GQL quantifiers following an edge pattern: `{m,n}`, `{m,}`, `{,n}`, `{n}`, `*`, `+`.
fn gql_quantifier<'t, I>() -> impl Parser<'t, I, Quantifier, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let braces = int_lit()
        .or_not()
        .then(op(",").ignore_then(int_lit().or_not()).or_not())
        .delimited_by(op("{"), op("}"))
        .try_map(|(lo, rest), span| match (lo, rest) {
            (Some(n), None) => Ok(Quantifier {
                min: n,
                max: Some(n),
            }),
            (lo, Some(hi)) => Ok(Quantifier {
                min: lo.unwrap_or(0),
                max: hi,
            }),
            (None, None) => Err(Rich::custom(span, "empty quantifier")),
        });
    choice((
        braces,
        op("*").to(Quantifier { min: 0, max: None }),
        op("+").to(Quantifier { min: 1, max: None }),
    ))
}

/// Cypher variable-length syntax inside an edge body: `*`, `*n`, `*m..n`, `*m..`, `*..n`.
fn cypher_star<'t, I>() -> impl Parser<'t, I, Quantifier, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    op("*")
        .ignore_then(
            int_lit()
                .or_not()
                .then(op("..").ignore_then(int_lit().or_not()).or_not())
                .or_not(),
        )
        .map(|spec| match spec {
            None | Some((None, None)) => Quantifier { min: 1, max: None },
            Some((Some(n), None)) => Quantifier {
                min: n,
                max: Some(n),
            },
            Some((lo, Some(hi))) => Quantifier {
                min: lo.unwrap_or(1),
                max: hi,
            },
        })
}

pub(crate) fn expr<'t, I>(src: &'t str) -> impl Parser<'t, I, Expr, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(move |expr| {
        let literal = select! {
            Token::Float(x) => Expr::Literal(Literal::Float(x)),
            Token::Str(s) => Expr::Literal(Literal::String(s)),
            Token::Param(p) => Expr::Param(p),
        }
        .or(select! { Token::Int(n) => n }.try_map(|n, span| {
            i64::try_from(n)
                .map(|n| Expr::Literal(Literal::Int(n)))
                .map_err(|_| Rich::custom(span, "integer literal overflows INT64"))
        }))
        .or(kw("TRUE").to(Expr::Literal(Literal::Bool(true))))
        .or(kw("FALSE").to(Expr::Literal(Literal::Bool(false))))
        .or(kw("NULL").to(Expr::Literal(Literal::Null)));

        let list = expr
            .clone()
            .separated_by(op(","))
            .collect::<Vec<_>>()
            .delimited_by(op("["), op("]"))
            .map(Expr::List);

        let map = name()
            .then_ignore(op(":"))
            .then(expr.clone())
            .separated_by(op(","))
            .collect::<Vec<_>>()
            .delimited_by(op("{"), op("}"))
            .try_map(|entries, span| {
                let mut m = BTreeMap::new();
                for (k, v) in entries {
                    if m.insert(k.clone(), v).is_some() {
                        return Err(Rich::custom(span, format!("duplicate map key `{k}`")));
                    }
                }
                Ok(Expr::Map(m))
            });

        let count_star = select! { Token::Ident(s) if s.eq_ignore_ascii_case("count") => () }
            .then(op("(").then(op("*")).then(op(")")))
            .to(Expr::CountStar);

        let call = name()
            .then(
                kw("DISTINCT")
                    .or_not()
                    .then(expr.clone().separated_by(op(",")).collect::<Vec<_>>())
                    .delimited_by(op("("), op(")")),
            )
            .try_map(|(name, (distinct, args)), span| {
                let canonical = crate::functions::canonical_name(&name)
                    .ok_or_else(|| Rich::custom(span, format!("unsupported function `{name}`")))?;
                Ok(Expr::Call {
                    name: canonical.to_string(),
                    distinct: distinct.is_some(),
                    args,
                })
            });

        let atom = choice((
            literal,
            count_star,
            call,
            variable().map(Expr::Var),
            list,
            map,
            expr.clone().delimited_by(op("("), op(")")),
        ))
        .boxed();

        enum Postfix {
            Prop(String),
            Index(Expr),
            Labels(LabelExpr),
        }
        let postfix = atom
            .foldl(
                choice((
                    op(".").ignore_then(name()).map(Postfix::Prop),
                    expr.clone()
                        .delimited_by(op("["), op("]"))
                        .map(Postfix::Index),
                    label_spec_colon().map(Postfix::Labels),
                ))
                .repeated(),
                |e, p| match p {
                    Postfix::Prop(k) => Expr::Property(Box::new(e), k),
                    Postfix::Index(i) => Expr::Index(Box::new(e), Box::new(i)),
                    Postfix::Labels(l) => Expr::IsLabeled {
                        expr: Box::new(e),
                        labels: l,
                        negated: false,
                    },
                },
            )
            .boxed();

        // INT64::MIN cannot be written as a positive literal; accept it negated.
        let int64_min = op("-")
            .ignore_then(select! { Token::Int(n) if n == 1u128 << 63 => () })
            .to(Expr::Literal(Literal::Int(i64::MIN)));
        let unary = int64_min
            .or(choice((op("-").to(UnOp::Neg), op("+").to(UnOp::Pos)))
                .repeated()
                .foldr(postfix, fold_unary))
            .boxed();

        let power = unary
            .clone()
            .foldl(op("^").ignore_then(unary).repeated(), |a, b| {
                bin(BinOp::Pow, a, b)
            })
            .boxed();

        let mul_op = choice((
            op("*").to(BinOp::Mul),
            op("/").to(BinOp::Div),
            op("%").to(BinOp::Mod),
        ));
        let mul = power
            .clone()
            .foldl(mul_op.then(power).repeated(), |a, (o, b)| bin(o, a, b))
            .boxed();

        let add_op = choice((
            op("+").to(BinOp::Add),
            op("-").to(BinOp::Sub),
            op("||").to(BinOp::Concat),
        ));
        let add = mul
            .clone()
            .foldl(add_op.then(mul).repeated(), |a, (o, b)| bin(o, a, b))
            .boxed();

        enum Pred {
            Null(bool),
            Labeled(bool, LabelExpr),
            Bin(BinOp, Expr),
        }
        let pred = choice((
            kw("IS")
                .ignore_then(kw("NOT").or_not())
                .then_ignore(kw("NULL"))
                .map(|n| Pred::Null(n.is_some())),
            kw("IS")
                .ignore_then(kw("NOT").or_not())
                .then_ignore(kw("LABELED"))
                .then(label_expr())
                .map(|(n, l)| Pred::Labeled(n.is_some(), l)),
            kw("IN")
                .ignore_then(add.clone())
                .map(|e| Pred::Bin(BinOp::In, e)),
            kw("STARTS")
                .then(kw("WITH"))
                .ignore_then(add.clone())
                .map(|e| Pred::Bin(BinOp::StartsWith, e)),
            kw("ENDS")
                .then(kw("WITH"))
                .ignore_then(add.clone())
                .map(|e| Pred::Bin(BinOp::EndsWith, e)),
            kw("CONTAINS")
                .ignore_then(add.clone())
                .map(|e| Pred::Bin(BinOp::Contains, e)),
        ));
        let predicated = add
            .foldl(pred.repeated(), |e, p| match p {
                Pred::Null(negated) => Expr::IsNull {
                    expr: Box::new(e),
                    negated,
                },
                Pred::Labeled(negated, labels) => Expr::IsLabeled {
                    expr: Box::new(e),
                    labels,
                    negated,
                },
                Pred::Bin(o, rhs) => bin(o, e, rhs),
            })
            .boxed();

        let cmp_op = choice((
            op("=").to(BinOp::Eq),
            op("<>").to(BinOp::Ne),
            op("!=").to(BinOp::Ne),
            op("<=").to(BinOp::Le),
            op(">=").to(BinOp::Ge),
            op("<").to(BinOp::Lt),
            op(">").to(BinOp::Gt),
        ));
        // Chained comparisons: `a < b < c` means `a < b AND b < c`.
        let comparison = predicated
            .clone()
            .then(cmp_op.then(predicated).repeated().collect::<Vec<_>>())
            .map(|(first, rest)| {
                let mut lhs = first;
                let mut acc: Option<Expr> = None;
                for (o, rhs) in rest {
                    let c = bin(o, lhs, rhs.clone());
                    acc = Some(match acc {
                        None => c,
                        Some(prev) => bin(BinOp::And, prev, c),
                    });
                    lhs = rhs;
                }
                acc.unwrap_or(lhs)
            })
            .boxed();

        let not = kw("NOT")
            .repeated()
            .foldr(comparison, |_, e| Expr::Unary(UnOp::Not, Box::new(e)))
            .boxed();
        let and = not
            .clone()
            .foldl(kw("AND").ignore_then(not).repeated(), |a, b| {
                bin(BinOp::And, a, b)
            })
            .boxed();
        let xor = and
            .clone()
            .foldl(kw("XOR").ignore_then(and).repeated(), |a, b| {
                bin(BinOp::Xor, a, b)
            })
            .boxed();
        let or = xor
            .clone()
            .foldl(kw("OR").ignore_then(xor).repeated(), |a, b| {
                bin(BinOp::Or, a, b)
            });
        let _ = src;
        or.labelled("expression")
    })
}

/// `:Label` used as a postfix predicate in expressions (Cypher `n:A:B`).
fn label_spec_colon<'t, I>() -> impl Parser<'t, I, LabelExpr, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    op(":")
        .ignore_then(label_expr())
        .foldl(op(":").ignore_then(label_expr()).repeated(), |a, b| {
            LabelExpr::And(Box::new(a), Box::new(b))
        })
}

fn bin(o: BinOp, a: Expr, b: Expr) -> Expr {
    Expr::Binary(o, Box::new(a), Box::new(b))
}

fn fold_unary(o: UnOp, e: Expr) -> Expr {
    // Fold `-<literal>` so that INT64::MIN is expressible and literals stay literals.
    match (o, e) {
        (UnOp::Neg, Expr::Literal(Literal::Int(n))) => Expr::Literal(Literal::Int(-n)),
        (UnOp::Neg, Expr::Literal(Literal::Float(x))) => Expr::Literal(Literal::Float(-x)),
        (o, e) => Expr::Unary(o, Box::new(e)),
    }
}

fn props<'t, I>(src: &'t str) -> impl Parser<'t, I, Vec<(String, Expr)>, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    name()
        .then_ignore(op(":"))
        .then(expr(src))
        .separated_by(op(","))
        .collect::<Vec<_>>()
        .delimited_by(op("{"), op("}"))
}

fn node_pattern<'t, I>(src: &'t str) -> impl Parser<'t, I, NodePattern, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    variable()
        .or_not()
        .then(label_spec().or_not())
        .then(props(src).or_not())
        .then(kw("WHERE").ignore_then(expr(src)).or_not())
        .delimited_by(op("("), op(")"))
        .map(|(((var, labels), props), where_)| NodePattern {
            var,
            labels,
            props: props.unwrap_or_default(),
            where_,
        })
}

fn edge_pattern<'t, I>(src: &'t str) -> impl Parser<'t, I, EdgePattern, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let body = variable()
        .or_not()
        .then(label_spec().or_not())
        .then(cypher_star().or_not())
        .then(props(src).or_not())
        .then(kw("WHERE").ignore_then(expr(src)).or_not())
        .delimited_by(op("["), op("]"));

    // Full forms: `-[..]->`, `<-[..]-`, `-[..]-`, `<-[..]->`.
    // Abbreviated: `->`, `<-`, `-`, and Cypher's `-->`, `<--`, `--`, `<-->`.
    let full = op("<")
        .or_not()
        .then_ignore(op("-"))
        .then(body)
        .then_ignore(op("-"))
        .then(op(">").or_not())
        .map(|((l, b), r)| (l.is_some(), Some(b), r.is_some()));
    let abbrev = op("<")
        .or_not()
        .then_ignore(op("-"))
        .then_ignore(op("-").or_not())
        .then(op(">").or_not())
        .map(|(l, r)| (l.is_some(), None, r.is_some()));

    choice((full, abbrev))
        .then(gql_quantifier().or_not())
        .try_map(|((left, body, right), gql_q), span| {
            let direction = match (left, right) {
                (false, true) => Direction::Right,
                (true, false) => Direction::Left,
                _ => Direction::Any,
            };
            let ((((var, labels), star), props), where_) =
                body.unwrap_or(((((None, None), None), None), None));
            let quantifier = match (star, gql_q) {
                (Some(_), Some(_)) => {
                    return Err(Rich::custom(span, "edge has both `*` and a GQL quantifier"));
                }
                (a, b) => a.or(b),
            };
            if let Some(Quantifier {
                min,
                max: Some(max),
            }) = quantifier
                && min > max
            {
                return Err(Rich::custom(
                    span,
                    "quantifier lower bound exceeds upper bound",
                ));
            }
            Ok(EdgePattern {
                var,
                labels,
                props: props.unwrap_or_default(),
                where_,
                direction,
                quantifier,
            })
        })
}

pub(crate) fn path_pattern<'t, I>(src: &'t str) -> impl Parser<'t, I, PathPattern, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    node_pattern(src)
        .then(
            edge_pattern(src)
                .then(node_pattern(src))
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(|(start, steps)| PathPattern { start, steps })
}

fn query_parser<'t, I>(src: &'t str) -> impl Parser<'t, I, Query, E<'t>>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let match_clause = kw("MATCH")
        .ignore_then(
            path_pattern(src)
                .separated_by(op(","))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then(kw("WHERE").ignore_then(expr(src)).or_not())
        .map(|(patterns, where_)| Clause::Match(Match { patterns, where_ }));

    let for_clause = kw("FOR")
        .ignore_then(variable())
        .then_ignore(kw("IN"))
        .then(expr(src))
        .map(|(var, list)| Clause::For { var, list });

    let unwind_clause = kw("UNWIND")
        .ignore_then(expr(src))
        .then_ignore(kw("AS"))
        .then(variable())
        .map(|(list, var)| Clause::For { var, list });

    let filter_clause = kw("FILTER")
        .ignore_then(kw("WHERE").or_not())
        .ignore_then(expr(src))
        .map(Clause::Filter);

    let clause = choice((match_clause, for_clause, unwind_clause, filter_clause));

    let text_of = move |span: Span| src[span.start..span.end].to_string();

    let item = expr(src)
        .map_with(move |e, x| (e, text_of(x.span())))
        .then(kw("AS").ignore_then(variable()).or_not())
        .map(|((expr, text), alias)| ReturnItem { expr, alias, text });

    let items = op("*").to(ReturnItems::Star).or(item
        .separated_by(op(","))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(ReturnItems::Items));

    let sort_item = expr(src)
        .map_with(move |e, x| (e, text_of(x.span())))
        .then(
            choice((
                kw("ASC").to(false),
                kw("ASCENDING").to(false),
                kw("DESC").to(true),
                kw("DESCENDING").to(true),
            ))
            .or_not(),
        )
        .map(|((expr, text), d)| SortItem {
            expr,
            text,
            descending: d.unwrap_or(false),
        });

    let ret = kw("RETURN")
        .ignore_then(kw("DISTINCT").or_not())
        .then(items)
        .then(
            kw("ORDER")
                .then(kw("BY"))
                .ignore_then(
                    sort_item
                        .separated_by(op(","))
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .or_not(),
        )
        .then(kw("SKIP").or(kw("OFFSET")).ignore_then(expr(src)).or_not())
        .then(kw("LIMIT").ignore_then(expr(src)).or_not())
        .map(|((((distinct, items), order_by), offset), limit)| Return {
            distinct: distinct.is_some(),
            items,
            order_by: order_by.unwrap_or_default(),
            offset,
            limit,
        });

    clause
        .repeated()
        .collect::<Vec<_>>()
        .then(ret)
        .then_ignore(op(";").or_not())
        .then_ignore(end())
        .map(|(clauses, ret)| Query { clauses, ret })
}

fn fixture_parser<'t, I>(src: &'t str) -> impl Parser<'t, I, Fixture, E<'t>>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    kw("CREATE")
        .or(kw("INSERT"))
        .ignore_then(
            path_pattern(src)
                .separated_by(op(","))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .then_ignore(op(";").or_not())
        .then_ignore(end())
        .map(|inserts| Fixture { inserts })
}

fn lex(src: &str) -> Result<Vec<(Token, Span)>, Error> {
    lexer().parse(src).into_result().map_err(|errs| {
        let e = &errs[0];
        Error::Syntax {
            message: e.to_string(),
            span: e.span().into_range(),
        }
    })
}

fn classify(src: &str, tokens: &[(Token, Span)], errs: Vec<Rich<'_, Token, Span>>) -> Error {
    if let Some((word, span)) = tokens.iter().find_map(|(t, s)| match t {
        Token::Ident(w) => unsupported_keyword(w).map(|k| (k, s)),
        _ => None,
    }) {
        return Error::Unsupported {
            feature: word.to_string(),
            span: span.into_range(),
        };
    }
    // Unknown function names and path variables are profile exclusions, not typos.
    for w in tokens.windows(3) {
        match (&w[0].0, &w[1].0, &w[2].0) {
            (Token::Ident(f), Token::Op("("), _)
                if !is_reserved(f) && crate::functions::canonical_name(f).is_none() =>
            {
                return Error::Unsupported {
                    feature: format!("function {f}()"),
                    span: w[0].1.into_range(),
                };
            }
            (Token::Ident(_), Token::Op("="), Token::Op("(")) => {
                return Error::Unsupported {
                    feature: "path variable".into(),
                    span: w[0].1.into_range(),
                };
            }
            _ => {}
        }
    }
    let e = &errs[0];
    let _ = src;
    Error::Syntax {
        message: e.to_string(),
        span: e.span().into_range(),
    }
}

/// Parse a query in the SLCR GQL profile.
pub fn parse_query(src: &str) -> Result<Query, Error> {
    let tokens = lex(src)?;
    let input = tokens
        .as_slice()
        .map((src.len()..src.len()).into(), |(t, s)| (t, s));
    query_parser(src)
        .parse(input)
        .into_result()
        .map_err(|errs| classify(src, &tokens, errs))
}

/// Parse a non-normative data-loading script (`CREATE`/`INSERT` patterns).
pub fn parse_fixture(src: &str) -> Result<Fixture, Error> {
    let tokens = lex(src)?;
    let input = tokens
        .as_slice()
        .map((src.len()..src.len()).into(), |(t, s)| (t, s));
    fixture_parser(src)
        .parse(input)
        .into_result()
        .map_err(|errs| {
            let e = &errs[0];
            Error::Syntax {
                message: e.to_string(),
                span: e.span().into_range(),
            }
        })
}
