//! Semantic checking and evaluation.
//!
//! Matching semantics: DIFFERENT EDGES (an edge is bound at most once per
//! `MATCH` statement), WALK path mode, homomorphic nodes. This is GQL's
//! default match mode and coincides with Cypher's relationship isomorphism,
//! and it guarantees termination of unbounded quantifiers on finite graphs.

use crate::BoxFuture;
use crate::ast::*;
use crate::error::Error;
use crate::functions::{self, call_scalar, is_aggregate};
use crate::store::{EdgeId, GraphStore, Incidence, NodeId};
use crate::value::{Value, compare, equals, equivalent, sort_order};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

pub type Params = BTreeMap<String, Value>;
type Row = HashMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// Semantic analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Node,
    Edge,
    EdgeGroup,
    Value,
}

struct Checker<'a> {
    params: &'a Params,
    scope: BTreeMap<String, Kind>,
}

impl Checker<'_> {
    fn bind(&mut self, var: &str, kind: Kind) -> Result<(), Error> {
        match self.scope.get(var) {
            Some(k) if *k != kind => Err(Error::Semantic(format!(
                "variable `{var}` is already bound as {k:?}, cannot rebind as {kind:?}"
            ))),
            Some(_) => Ok(()),
            None => {
                self.scope.insert(var.to_string(), kind);
                Ok(())
            }
        }
    }

    fn expr(&self, e: &Expr, allow_agg: bool, extra: &BTreeMap<String, Kind>) -> Result<(), Error> {
        let mut err = None;
        e.visit_vars(&mut |v| {
            if err.is_none() && !self.scope.contains_key(v) && !extra.contains_key(v) {
                err = Some(Error::Semantic(format!("undefined variable `{v}`")));
            }
        });
        if let Some(err) = err {
            return Err(err);
        }
        self.types(e, extra)?;
        self.walk(e, allow_agg, false)
    }

    fn kind_of(&self, e: &Expr, extra: &BTreeMap<String, Kind>) -> Option<Kind> {
        match e {
            Expr::Var(v) => extra.get(v).or_else(|| self.scope.get(v)).copied(),
            _ => None,
        }
    }

    /// Static type checks that are decidable from variable kinds alone.
    fn types(&self, e: &Expr, extra: &BTreeMap<String, Kind>) -> Result<(), Error> {
        let elem = |x: &Expr| {
            matches!(
                self.kind_of(x, extra),
                Some(Kind::Node | Kind::Edge | Kind::EdgeGroup)
            )
        };
        let bad = |what: &str| Err(Error::Semantic(format!("invalid argument type for {what}")));
        match e {
            Expr::Unary(UnOp::Not, x) if elem(x) => return bad("NOT"),
            Expr::Binary(BinOp::And | BinOp::Or | BinOp::Xor, a, b) if elem(a) || elem(b) => {
                return bad("a boolean operator");
            }
            Expr::Call { name, args, .. } => {
                let k = args.first().and_then(|a| self.kind_of(a, extra));
                match (name.as_str(), k) {
                    ("type", Some(Kind::Node | Kind::EdgeGroup)) => return bad("type()"),
                    ("labels", Some(Kind::Edge | Kind::EdgeGroup)) => return bad("labels()"),
                    _ => {}
                }
            }
            _ => {}
        }
        let mut res = Ok(());
        let mut visit = |x: &Expr| {
            if res.is_ok() {
                res = self.types(x, extra);
            }
        };
        match e {
            Expr::List(xs) => xs.iter().for_each(&mut visit),
            Expr::Map(m) => m.values().for_each(&mut visit),
            Expr::Call { args, .. } => args.iter().for_each(&mut visit),
            Expr::Property(x, _) | Expr::Unary(_, x) => visit(x),
            Expr::IsNull { expr, .. } | Expr::IsLabeled { expr, .. } => visit(expr),
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                visit(a);
                visit(b);
            }
            _ => {}
        }
        res
    }

    /// A WHERE condition must not be a bare graph element.
    fn condition(&self, e: &Expr, extra: &BTreeMap<String, Kind>) -> Result<(), Error> {
        if matches!(
            self.kind_of(e, extra),
            Some(Kind::Node | Kind::Edge | Kind::EdgeGroup)
        ) {
            return Err(Error::Semantic(
                "WHERE condition must be BOOLEAN, not a graph element".into(),
            ));
        }
        Ok(())
    }

    fn walk(&self, e: &Expr, allow_agg: bool, in_agg: bool) -> Result<(), Error> {
        match e {
            Expr::Param(p) if !self.params.contains_key(p) => {
                Err(Error::Semantic(format!("missing parameter `${p}`")))
            }
            Expr::CountStar if !allow_agg || in_agg => Err(Error::Semantic(
                "aggregate function used in an invalid position".into(),
            )),
            Expr::Call {
                name,
                args,
                distinct,
            } => {
                let agg = is_aggregate(name);
                if agg && (!allow_agg || in_agg) {
                    return Err(Error::Semantic(format!(
                        "aggregate function {name}() used in an invalid position"
                    )));
                }
                if *distinct && !agg {
                    return Err(Error::Semantic(format!(
                        "DISTINCT is not valid for {name}()"
                    )));
                }
                match functions::expected_arity(name) {
                    Some(n) if args.len() != n => {
                        return Err(Error::Semantic(format!(
                            "{name}() takes {n} argument(s), got {}",
                            args.len()
                        )));
                    }
                    None if args.is_empty() => {
                        return Err(Error::Semantic(format!("{name}() needs arguments")));
                    }
                    _ => {}
                }
                args.iter()
                    .try_for_each(|a| self.walk(a, allow_agg, in_agg || agg))
            }
            Expr::List(xs) => xs.iter().try_for_each(|x| self.walk(x, allow_agg, in_agg)),
            Expr::Map(m) => m.values().try_for_each(|x| self.walk(x, allow_agg, in_agg)),
            Expr::Property(x, _) | Expr::Unary(_, x) => self.walk(x, allow_agg, in_agg),
            Expr::IsNull { expr, .. } | Expr::IsLabeled { expr, .. } => {
                self.walk(expr, allow_agg, in_agg)
            }
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                self.walk(a, allow_agg, in_agg)?;
                self.walk(b, allow_agg, in_agg)
            }
            _ => Ok(()),
        }
    }

    fn node(&mut self, n: &NodePattern) -> Result<(), Error> {
        if let Some(v) = &n.var {
            self.bind(v, Kind::Node)?;
        }
        Ok(())
    }

    fn edge(&mut self, e: &EdgePattern) -> Result<(), Error> {
        if let Some(v) = &e.var {
            let kind = if e.quantifier.is_some() {
                Kind::EdgeGroup
            } else {
                Kind::Edge
            };
            self.bind(v, kind)?;
        }
        Ok(())
    }

    fn pattern_exprs(&self, pats: &[PathPattern]) -> Result<(), Error> {
        let none = BTreeMap::new();
        for p in pats {
            let nodes = std::iter::once(&p.start).chain(p.steps.iter().map(|(_, n)| n));
            for n in nodes {
                for (_, e) in &n.props {
                    self.expr(e, false, &none)?;
                }
                if let Some(w) = &n.where_ {
                    self.expr(w, false, &none)?;
                }
            }
            for (e, _) in &p.steps {
                // Inside a quantified edge the variable is a singleton.
                let mut local = BTreeMap::new();
                if let (Some(v), Some(_)) = (&e.var, e.quantifier) {
                    local.insert(v.clone(), Kind::Edge);
                }
                for (_, x) in &e.props {
                    self.expr(x, false, &none)?;
                }
                if let Some(w) = &e.where_ {
                    self.expr(w, false, &local)?;
                }
            }
        }
        Ok(())
    }
}

fn check(q: &Query, params: &Params) -> Result<(), Error> {
    let mut c = Checker {
        params,
        scope: BTreeMap::new(),
    };
    let none = BTreeMap::new();
    for clause in &q.clauses {
        match clause {
            Clause::Match(m) => {
                let mut edge_vars = HashSet::new();
                for p in &m.patterns {
                    for (e, _) in &p.steps {
                        if let Some(v) = &e.var
                            && !edge_vars.insert(v.as_str())
                        {
                            return Err(Error::Semantic(format!(
                                "edge variable `{v}` used twice in one MATCH (edges are matched at most once)"
                            )));
                        }
                    }
                }
                for p in &m.patterns {
                    c.node(&p.start)?;
                    for (e, n) in &p.steps {
                        c.edge(e)?;
                        c.node(n)?;
                    }
                }
                c.pattern_exprs(&m.patterns)?;
                if let Some(w) = &m.where_ {
                    c.expr(w, false, &none)?;
                    c.condition(w, &none)?;
                }
            }
            Clause::Filter(cond) => {
                c.expr(cond, false, &none)?;
                c.condition(cond, &none)?;
            }
            Clause::For { var, list } => {
                c.expr(list, false, &none)?;
                if c.scope.contains_key(var) {
                    return Err(Error::Semantic(format!("variable `{var}` already defined")));
                }
                c.bind(var, Kind::Value)?;
            }
        }
    }
    let r = &q.ret;
    let items = match &r.items {
        ReturnItems::Star => {
            if c.scope.is_empty() {
                return Err(Error::Semantic(
                    "RETURN * with no variables in scope".into(),
                ));
            }
            Vec::new()
        }
        ReturnItems::Items(items) => items.clone(),
    };
    let mut names = HashSet::new();
    for it in &items {
        c.expr(&it.expr, true, &none)?;
        if !names.insert(it.column_name().to_string()) {
            return Err(Error::Semantic(format!(
                "duplicate column name `{}`",
                it.column_name()
            )));
        }
    }
    let aggregating = items.iter().any(|i| i.expr.contains_aggregate());
    if aggregating {
        let keys: Vec<&Expr> = items
            .iter()
            .filter(|i| !i.expr.contains_aggregate())
            .map(|i| &i.expr)
            .collect();
        for it in items.iter().filter(|i| i.expr.contains_aggregate()) {
            grouping_ok(&it.expr, &keys)?;
        }
    }
    // ORDER BY may see projected aliases; when the projection is neither
    // DISTINCT nor aggregating it may also see the incoming variables.
    let mut aliases = BTreeMap::new();
    for it in &items {
        aliases.insert(it.column_name().to_string(), Kind::Value);
    }
    for s in &r.order_by {
        if items.iter().any(|i| i.expr == s.expr) {
            continue;
        }
        if aggregating {
            let keys: Vec<&Expr> = items
                .iter()
                .filter(|i| !i.expr.contains_aggregate())
                .map(|i| &i.expr)
                .collect();
            order_ok(&s.expr, &keys, &aliases)?;
            c.expr(&s.expr, true, &aliases)?;
        } else if r.distinct {
            let mut err = None;
            s.expr.visit_vars(&mut |v| {
                if err.is_none() && !aliases.contains_key(v) {
                    err = Some(Error::Semantic(format!(
                        "ORDER BY after DISTINCT may only use projected columns (`{v}`)"
                    )));
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
            c.expr(&s.expr, false, &aliases)?;
        } else {
            c.expr(&s.expr, false, &aliases)?;
        }
    }
    for e in r.offset.iter().chain(r.limit.iter()) {
        let mut has_var = false;
        e.visit_vars(&mut |_| has_var = true);
        if has_var {
            return Err(Error::Semantic(
                "OFFSET/LIMIT must not reference variables".into(),
            ));
        }
        c.walk(e, false, false)?;
    }
    Ok(())
}

/// ORDER BY over an aggregating projection: outside aggregates, only
/// projected aliases and (simple) grouping keys may be used.
fn order_ok(e: &Expr, keys: &[&Expr], aliases: &BTreeMap<String, Kind>) -> Result<(), Error> {
    match e {
        Expr::Var(v) if aliases.contains_key(v) => Ok(()),
        Expr::Var(_) | Expr::Property(..) if keys.contains(&e) => Ok(()),
        Expr::CountStar => Ok(()),
        Expr::Call { name, .. } if is_aggregate(name) => Ok(()),
        Expr::Var(v) => Err(Error::Semantic(format!(
            "ORDER BY after aggregation may only use projected columns (`{v}`)"
        ))),
        Expr::Call { args, .. } => args.iter().try_for_each(|a| order_ok(a, keys, aliases)),
        Expr::List(xs) => xs.iter().try_for_each(|a| order_ok(a, keys, aliases)),
        Expr::Map(m) => m.values().try_for_each(|a| order_ok(a, keys, aliases)),
        Expr::Property(x, _) | Expr::Unary(_, x) => order_ok(x, keys, aliases),
        Expr::IsNull { expr, .. } | Expr::IsLabeled { expr, .. } => order_ok(expr, keys, aliases),
        Expr::Index(a, b) | Expr::Binary(_, a, b) => {
            order_ok(a, keys, aliases)?;
            order_ok(b, keys, aliases)
        }
        Expr::Literal(_) | Expr::Param(_) | Expr::AggRef(_) => Ok(()),
    }
}

/// Every variable used outside an aggregate in an aggregating item must be
/// covered by a grouping key.
fn grouping_ok(e: &Expr, keys: &[&Expr]) -> Result<(), Error> {
    let simple = matches!(e, Expr::Var(_))
        || matches!(e, Expr::Property(x, _) if matches!(**x, Expr::Var(_)));
    if simple && keys.contains(&e) {
        return Ok(());
    }
    match e {
        Expr::CountStar => Ok(()),
        Expr::Call { name, .. } if is_aggregate(name) => Ok(()),
        Expr::Var(v) => Err(Error::Semantic(format!(
            "ambiguous aggregation: `{v}` is not a grouping key"
        ))),
        Expr::Call { args, .. } => args.iter().try_for_each(|a| grouping_ok(a, keys)),
        Expr::List(xs) => xs.iter().try_for_each(|a| grouping_ok(a, keys)),
        Expr::Map(m) => m.values().try_for_each(|a| grouping_ok(a, keys)),
        Expr::Property(x, _) | Expr::Unary(_, x) => grouping_ok(x, keys),
        Expr::IsNull { expr, .. } | Expr::IsLabeled { expr, .. } => grouping_ok(expr, keys),
        Expr::Index(a, b) | Expr::Binary(_, a, b) => {
            grouping_ok(a, keys)?;
            grouping_ok(b, keys)
        }
        Expr::Literal(_) | Expr::Param(_) | Expr::AggRef(_) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    store: &'a dyn GraphStore,
    params: &'a Params,
}

/// A graph element whose properties a pattern constrains.
#[derive(Clone, Copy)]
enum Element {
    Node(NodeId),
    Edge(EdgeId),
}

fn data(msg: impl Into<String>) -> Error {
    Error::Data(msg.into())
}

fn truth(v: &Value, what: &str) -> Result<Option<bool>, Error> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(data(format!(
            "{what} expects BOOLEAN, got {}",
            other.type_name()
        ))),
    }
}

fn from_truth(t: Option<bool>) -> Value {
    t.map(Value::Bool).unwrap_or(Value::Null)
}

fn labels_match(l: &LabelExpr, labels: &[String]) -> bool {
    match l {
        LabelExpr::Label(s) => labels.iter().any(|x| x == s),
        LabelExpr::Wildcard => !labels.is_empty(),
        LabelExpr::And(a, b) => labels_match(a, labels) && labels_match(b, labels),
        LabelExpr::Or(a, b) => labels_match(a, labels) || labels_match(b, labels),
        LabelExpr::Not(a) => !labels_match(a, labels),
    }
}

/// A label that every match must carry, usable for an index lookup.
fn required_label(l: &LabelExpr) -> Option<&str> {
    match l {
        LabelExpr::Label(s) => Some(s),
        LabelExpr::And(a, b) => required_label(a).or_else(|| required_label(b)),
        _ => None,
    }
}

impl Ctx<'_> {
    async fn node_labeled(&self, n: NodeId, l: &LabelExpr) -> bool {
        labels_match(l, &self.store.node_labels(n).await)
    }

    async fn edge_labeled(&self, e: EdgeId, l: &LabelExpr) -> bool {
        labels_match(l, &self.store.edge_labels(e).await)
    }

    async fn property(&self, el: Element, key: &str) -> Value {
        match el {
            Element::Node(n) => self.store.node_property(n, key).await,
            Element::Edge(e) => self.store.edge_property(e, key).await,
        }
        .unwrap_or(Value::Null)
    }

    async fn eval_all(&self, es: &[Expr], row: &Row, aggs: &[Value]) -> Result<Vec<Value>, Error> {
        let mut out = Vec::with_capacity(es.len());
        for e in es {
            out.push(self.eval(e, row, aggs).await?);
        }
        Ok(out)
    }

    fn eval<'s>(
        &'s self,
        e: &'s Expr,
        row: &'s Row,
        aggs: &'s [Value],
    ) -> BoxFuture<'s, Result<Value, Error>> {
        Box::pin(async move {
            Ok(match e {
                Expr::Literal(l) => match l {
                    Literal::Null => Value::Null,
                    Literal::Bool(b) => Value::Bool(*b),
                    Literal::Int(i) => Value::Int(*i),
                    Literal::Float(x) => Value::Float(*x),
                    Literal::String(s) => Value::String(s.clone()),
                },
                Expr::Param(p) => self
                    .params
                    .get(p)
                    .cloned()
                    .ok_or_else(|| Error::Semantic(format!("missing parameter `${p}`")))?,
                Expr::Var(v) => row
                    .get(v)
                    .cloned()
                    .ok_or_else(|| Error::Semantic(format!("undefined variable `{v}`")))?,
                Expr::AggRef(i) => aggs[*i].clone(),
                Expr::CountStar => {
                    return Err(Error::Semantic("count(*) outside aggregation".into()));
                }
                Expr::List(xs) => Value::List(self.eval_all(xs, row, aggs).await?),
                Expr::Map(m) => {
                    let mut out = BTreeMap::new();
                    for (k, x) in m {
                        out.insert(k.clone(), self.eval(x, row, aggs).await?);
                    }
                    Value::Map(out)
                }
                Expr::Property(x, key) => match self.eval(x, row, aggs).await? {
                    Value::Null => Value::Null,
                    Value::Node(n) => self.property(Element::Node(n), key).await,
                    Value::Edge(r) => self.property(Element::Edge(r), key).await,
                    Value::Map(m) => m.get(key).cloned().unwrap_or(Value::Null),
                    other => {
                        return Err(data(format!(
                            "cannot access property of {}",
                            other.type_name()
                        )));
                    }
                },
                Expr::Index(x, i) => {
                    let (x, i) = (
                        self.eval(x, row, aggs).await?,
                        self.eval(i, row, aggs).await?,
                    );
                    match (x, i) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::List(xs), Value::Int(i)) => {
                            let len = xs.len() as i64;
                            let idx = if i < 0 { len + i } else { i };
                            if (0..len).contains(&idx) {
                                xs[idx as usize].clone()
                            } else {
                                Value::Null
                            }
                        }
                        (Value::Map(m), Value::String(k)) => {
                            m.get(&k).cloned().unwrap_or(Value::Null)
                        }
                        (Value::Node(n), Value::String(k)) => {
                            self.property(Element::Node(n), &k).await
                        }
                        (Value::Edge(r), Value::String(k)) => {
                            self.property(Element::Edge(r), &k).await
                        }
                        (x, i) => {
                            return Err(data(format!(
                                "cannot index {} with {}",
                                x.type_name(),
                                i.type_name()
                            )));
                        }
                    }
                }
                Expr::Unary(op, x) => {
                    let v = self.eval(x, row, aggs).await?;
                    match (op, v) {
                        (_, Value::Null) => Value::Null,
                        (UnOp::Not, v) => from_truth(truth(&v, "NOT")?.map(|b| !b)),
                        (UnOp::Neg, Value::Int(i)) => {
                            Value::Int(i.checked_neg().ok_or_else(|| data("integer overflow"))?)
                        }
                        (UnOp::Neg, Value::Float(f)) => Value::Float(-f),
                        (UnOp::Pos, v @ (Value::Int(_) | Value::Float(_))) => v,
                        (_, v) => {
                            return Err(data(format!("unary operator on {}", v.type_name())));
                        }
                    }
                }
                Expr::Binary(op, a, b) => {
                    let x = self.eval(a, row, aggs).await?;
                    if matches!(op, BinOp::And | BinOp::Or | BinOp::Xor) {
                        // Report a bad left operand before evaluating the right.
                        truth(&x, "boolean operator")?;
                    }
                    let y = self.eval(b, row, aggs).await?;
                    binary(*op, x, y)?
                }
                Expr::IsNull { expr, negated } => {
                    Value::Bool(self.eval(expr, row, aggs).await?.is_null() != *negated)
                }
                Expr::IsLabeled {
                    expr,
                    labels,
                    negated,
                } => match self.eval(expr, row, aggs).await? {
                    Value::Null => Value::Null,
                    Value::Node(n) => Value::Bool(self.node_labeled(n, labels).await != *negated),
                    Value::Edge(r) => Value::Bool(self.edge_labeled(r, labels).await != *negated),
                    other => {
                        return Err(data(format!("label test on {}", other.type_name())));
                    }
                },
                Expr::Call { name, args, .. } => {
                    if is_aggregate(name) {
                        return Err(Error::Semantic(format!("{name}() outside aggregation")));
                    }
                    let args = self.eval_all(args, row, aggs).await?;
                    call_scalar(name, args, self.store).await?
                }
            })
        })
    }
}

/// Apply a binary operator to evaluated operands. The boolean connectives
/// do not short-circuit, so they check both operand types.
fn binary(op: BinOp, x: Value, y: Value) -> Result<Value, Error> {
    use BinOp::*;
    Ok(match op {
        And | Or | Xor => {
            let x = truth(&x, "boolean operator")?;
            let y = truth(&y, "boolean operator")?;
            from_truth(match op {
                And => match (x, y) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                },
                Or => match (x, y) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                },
                _ => match (x, y) {
                    (Some(p), Some(q)) => Some(p != q),
                    _ => None,
                },
            })
        }
        Eq => from_truth(equals(&x, &y)),
        Ne => from_truth(equals(&x, &y).map(|b| !b)),
        Lt | Le | Gt | Ge => {
            let nan = |v: &Value| matches!(v, Value::Float(f) if f.is_nan());
            let num = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_));
            if x.is_null() || y.is_null() {
                Value::Null
            } else if (nan(&x) || nan(&y)) && num(&x) && num(&y) {
                Value::Bool(false)
            } else {
                from_truth(compare(&x, &y).map(|o| match op {
                    Lt => o == Ordering::Less,
                    Le => o != Ordering::Greater,
                    Gt => o == Ordering::Greater,
                    _ => o != Ordering::Less,
                }))
            }
        }
        In => match y {
            Value::Null => Value::Null,
            Value::List(ys) => {
                let mut unknown = false;
                for v in &ys {
                    match equals(&x, v) {
                        Some(true) => return Ok(Value::Bool(true)),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                if unknown {
                    Value::Null
                } else {
                    Value::Bool(false)
                }
            }
            other => {
                return Err(data(format!(
                    "IN expects a LIST, got {}",
                    other.type_name()
                )));
            }
        },
        StartsWith | EndsWith | Contains => match (&x, &y) {
            (Value::String(s), Value::String(t)) => Value::Bool(match op {
                StartsWith => s.starts_with(t.as_str()),
                EndsWith => s.ends_with(t.as_str()),
                _ => s.contains(t.as_str()),
            }),
            _ => Value::Null,
        },
        Concat => match (x, y) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::String(s), Value::String(t)) => Value::String(s + &t),
            (Value::List(mut s), Value::List(t)) => {
                s.extend(t);
                Value::List(s)
            }
            (x, y) => {
                return Err(data(format!(
                    "cannot concatenate {} and {}",
                    x.type_name(),
                    y.type_name()
                )));
            }
        },
        Add | Sub | Mul | Div | Mod | Pow => arith(op, x, y)?,
    })
}

pub(crate) fn arith(op: BinOp, x: Value, y: Value) -> Result<Value, Error> {
    use BinOp::*;
    match (x, y) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) if op != Pow => {
            let r = match op {
                Add => a.checked_add(b),
                Sub => a.checked_sub(b),
                Mul => a.checked_mul(b),
                Div => {
                    if b == 0 {
                        return Err(data("division by zero"));
                    }
                    a.checked_div(b)
                }
                _ => {
                    if b == 0 {
                        return Err(data("division by zero"));
                    }
                    a.checked_rem(b)
                }
            };
            r.map(Value::Int).ok_or_else(|| data("integer overflow"))
        }
        (a @ (Value::Int(_) | Value::Float(_)), b @ (Value::Int(_) | Value::Float(_))) => {
            let f = |v: Value| match v {
                Value::Int(i) => i as f64,
                Value::Float(x) => x,
                _ => unreachable!(),
            };
            let (a, b) = (f(a), f(b));
            Ok(Value::Float(match op {
                Add => a + b,
                Sub => a - b,
                Mul => a * b,
                Div => a / b,
                Mod => a % b,
                _ => a.powf(b),
            }))
        }
        (a, b) => Err(data(format!(
            "arithmetic on {} and {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Pattern matching
// ---------------------------------------------------------------------------

struct Matcher<'a, 'c> {
    ctx: &'c Ctx<'a>,
    patterns: &'c [PathPattern],
    used: HashSet<EdgeId>,
    out: Vec<Row>,
}

impl Matcher<'_, '_> {
    async fn props_ok(
        &self,
        props: &[(String, Expr)],
        row: &Row,
        el: Element,
    ) -> Result<bool, Error> {
        for (k, e) in props {
            let want = self.ctx.eval(e, row, &[]).await?;
            let have = self.ctx.property(el, k).await;
            if equals(&have, &want) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Try to bind `np` to `node`; returns the extended row on success.
    async fn bind_node(
        &self,
        np: &NodePattern,
        node: NodeId,
        row: &Row,
    ) -> Result<Option<Row>, Error> {
        if let Some(l) = &np.labels
            && !self.ctx.node_labeled(node, l).await
        {
            return Ok(None);
        }
        let mut row = row.clone();
        if let Some(v) = &np.var {
            match row.get(v) {
                Some(Value::Node(n)) if *n == node => {}
                Some(_) => return Ok(None),
                None => {
                    row.insert(v.clone(), Value::Node(node));
                }
            }
        }
        if !self.props_ok(&np.props, &row, Element::Node(node)).await? {
            return Ok(None);
        }
        if let Some(w) = &np.where_
            && truth(&self.ctx.eval(w, &row, &[]).await?, "WHERE")? != Some(true)
        {
            return Ok(None);
        }
        Ok(Some(row))
    }

    async fn edge_ok(&self, ep: &EdgePattern, edge: EdgeId, row: &Row) -> Result<bool, Error> {
        if let Some(l) = &ep.labels
            && !self.ctx.edge_labeled(edge, l).await
        {
            return Ok(false);
        }
        if !self.props_ok(&ep.props, row, Element::Edge(edge)).await? {
            return Ok(false);
        }
        if let Some(w) = &ep.where_ {
            let mut row = row.clone();
            if let Some(v) = &ep.var {
                row.insert(v.clone(), Value::Edge(edge));
            }
            if truth(&self.ctx.eval(w, &row, &[]).await?, "WHERE")? != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Incident edges in pattern direction, as (edge, far endpoint).
    async fn steps_from(&self, node: NodeId, dir: Direction) -> Vec<(EdgeId, NodeId)> {
        let s = self.ctx.store;
        let out = || async { s.edges(node, Incidence::Outgoing).await };
        let inc = || async { s.edges(node, Incidence::Incoming).await };
        match dir {
            Direction::Right => out().await.iter().map(|e| (e.id, e.target)).collect(),
            Direction::Left => inc().await.iter().map(|e| (e.id, e.source)).collect(),
            // A self-loop is one edge, not two traversals.
            Direction::Any => {
                let mut v: Vec<_> = out().await.iter().map(|e| (e.id, e.target)).collect();
                v.extend(
                    inc()
                        .await
                        .iter()
                        .filter(|e| e.source != e.target)
                        .map(|e| (e.id, e.source)),
                );
                v
            }
        }
    }

    async fn run(&mut self, row: Row) -> Result<(), Error> {
        self.pattern(0, row).await
    }

    fn pattern(&mut self, pi: usize, row: Row) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let Some(p) = self.patterns.get(pi) else {
                self.out.push(row);
                return Ok(());
            };
            let candidates: Vec<NodeId> = match p.start.var.as_ref().and_then(|v| row.get(v)) {
                Some(Value::Node(n)) => vec![*n],
                Some(_) => vec![],
                None => match p.start.labels.as_ref().and_then(required_label) {
                    Some(l) => self.ctx.store.nodes_with_label(l).await,
                    None => self.ctx.store.nodes().await,
                },
            };
            for n in candidates {
                if let Some(r) = self.bind_node(&p.start, n, &row).await? {
                    self.step(pi, 0, n, r).await?;
                }
            }
            Ok(())
        })
    }

    fn step(
        &mut self,
        pi: usize,
        si: usize,
        cur: NodeId,
        row: Row,
    ) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let p = &self.patterns[pi];
            let Some((ep, np)) = p.steps.get(si) else {
                return self.pattern(pi + 1, row).await;
            };
            match ep.quantifier {
                None => {
                    for (edge, far) in self.steps_from(cur, ep.direction).await {
                        if self.used.contains(&edge) || !self.edge_ok(ep, edge, &row).await? {
                            continue;
                        }
                        let mut r = row.clone();
                        if let Some(v) = &ep.var {
                            match r.get(v) {
                                Some(Value::Edge(e)) if *e == edge => {}
                                Some(_) => continue,
                                None => {
                                    r.insert(v.clone(), Value::Edge(edge));
                                }
                            }
                        }
                        if let Some(r) = self.bind_node(np, far, &r).await? {
                            self.used.insert(edge);
                            self.step(pi, si + 1, far, r).await?;
                            self.used.remove(&edge);
                        }
                    }
                    Ok(())
                }
                Some(q) => {
                    let mut trail = Vec::new();
                    self.quantified(pi, si, q, cur, &row, &mut trail).await
                }
            }
        })
    }

    fn quantified<'s>(
        &'s mut self,
        pi: usize,
        si: usize,
        q: Quantifier,
        cur: NodeId,
        row: &'s Row,
        trail: &'s mut Vec<EdgeId>,
    ) -> BoxFuture<'s, Result<(), Error>> {
        Box::pin(async move {
            let (ep, np) = &self.patterns[pi].steps[si];
            let depth = trail.len() as u32;
            if depth >= q.min {
                let mut r = row.clone();
                let ok = match &ep.var {
                    None => true,
                    Some(v) => {
                        let list = Value::List(trail.iter().map(|e| Value::Edge(*e)).collect());
                        match r.get(v) {
                            Some(bound) => equivalent(bound, &list),
                            None => {
                                r.insert(v.clone(), list);
                                true
                            }
                        }
                    }
                };
                if ok && let Some(r) = self.bind_node(np, cur, &r).await? {
                    self.step(pi, si + 1, cur, r).await?;
                }
            }
            if q.max.is_some_and(|m| depth >= m) {
                return Ok(());
            }
            for (edge, far) in self.steps_from(cur, ep.direction).await {
                if self.used.contains(&edge) {
                    continue;
                }
                // Per-iteration constraints see the edge variable as a singleton.
                let mut local = row.clone();
                if let Some(v) = &ep.var {
                    local.remove(v);
                }
                if !self.edge_ok(ep, edge, &local).await? {
                    continue;
                }
                self.used.insert(edge);
                trail.push(edge);
                self.quantified(pi, si, q, far, row, trail).await?;
                trail.pop();
                self.used.remove(&edge);
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

/// Replace aggregate calls with `AggRef`s, recording them in `aggs`.
fn extract_aggs(e: &Expr, aggs: &mut Vec<Expr>) -> Expr {
    let agg =
        matches!(e, Expr::CountStar) || matches!(e, Expr::Call { name, .. } if is_aggregate(name));
    if agg {
        let i = aggs.iter().position(|a| a == e).unwrap_or_else(|| {
            aggs.push(e.clone());
            aggs.len() - 1
        });
        return Expr::AggRef(i);
    }
    let mut r = |x: &Expr| Box::new(extract_aggs(x, aggs));
    match e {
        Expr::List(xs) => Expr::List(xs.iter().map(|x| extract_aggs(x, aggs)).collect()),
        Expr::Map(m) => Expr::Map(
            m.iter()
                .map(|(k, x)| (k.clone(), extract_aggs(x, aggs)))
                .collect(),
        ),
        Expr::Call {
            name,
            distinct,
            args,
        } => Expr::Call {
            name: name.clone(),
            distinct: *distinct,
            args: args.iter().map(|x| extract_aggs(x, aggs)).collect(),
        },
        Expr::Property(x, k) => Expr::Property(r(x), k.clone()),
        Expr::Unary(o, x) => Expr::Unary(*o, r(x)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: r(expr),
            negated: *negated,
        },
        Expr::IsLabeled {
            expr,
            labels,
            negated,
        } => Expr::IsLabeled {
            expr: r(expr),
            labels: labels.clone(),
            negated: *negated,
        },
        Expr::Index(a, b) => Expr::Index(r(a), r(b)),
        Expr::Binary(o, a, b) => Expr::Binary(*o, r(a), r(b)),
        other => other.clone(),
    }
}

async fn compute_agg(ctx: &Ctx<'_>, agg: &Expr, rows: &[&Row]) -> Result<Value, Error> {
    let (name, distinct, arg) = match agg {
        Expr::CountStar => return Ok(Value::Int(rows.len() as i64)),
        Expr::Call {
            name,
            distinct,
            args,
        } => (name.as_str(), *distinct, &args[0]),
        _ => unreachable!(),
    };
    let mut vals: Vec<Value> = Vec::new();
    for r in rows {
        let v = ctx.eval(arg, r, &[]).await?;
        if v.is_null() || (distinct && vals.iter().any(|x| equivalent(x, &v))) {
            continue;
        }
        vals.push(v);
    }
    Ok(match name {
        "count" => Value::Int(vals.len() as i64),
        "collect_list" => Value::List(vals),
        "min" => vals.into_iter().min_by(sort_order).unwrap_or(Value::Null),
        "max" => vals.into_iter().max_by(sort_order).unwrap_or(Value::Null),
        "sum" | "avg" => {
            let n = vals.len();
            let mut acc = Value::Int(0);
            for v in vals {
                if !matches!(v, Value::Int(_) | Value::Float(_)) {
                    return Err(data(format!("{name}() over {}", v.type_name())));
                }
                acc = arith(BinOp::Add, acc, v)?;
            }
            if name == "sum" {
                acc
            } else if n == 0 {
                Value::Null
            } else {
                arith(BinOp::Div, acc_float(acc), Value::Float(n as f64))?
            }
        }
        _ => unreachable!(),
    })
}

fn acc_float(v: Value) -> Value {
    match v {
        Value::Int(i) => Value::Float(i as f64),
        v => v,
    }
}

async fn non_negative_int(
    ctx: &Ctx<'_>,
    e: &Option<Expr>,
    what: &str,
) -> Result<Option<usize>, Error> {
    let Some(e) = e else { return Ok(None) };
    match ctx.eval(e, &Row::new(), &[]).await? {
        Value::Int(i) if i >= 0 => Ok(Some(i as usize)),
        other => Err(data(format!(
            "{what} expects a non-negative INTEGER, got {other}"
        ))),
    }
}

async fn project(
    ctx: &Ctx<'_>,
    ret: &Return,
    rows: Vec<Row>,
    in_scope: &[String],
) -> Result<QueryResult, Error> {
    let items: Vec<ReturnItem> = match &ret.items {
        ReturnItems::Star => {
            let mut vars = in_scope.to_vec();
            vars.sort();
            vars.into_iter()
                .map(|v| ReturnItem {
                    expr: Expr::Var(v.clone()),
                    alias: None,
                    text: v,
                })
                .collect()
        }
        ReturnItems::Items(items) => items.clone(),
    };
    let columns: Vec<String> = items.iter().map(|i| i.column_name().to_string()).collect();
    let aggregating = items.iter().any(|i| i.expr.contains_aggregate());

    // Each output record carries its projected values and a scope for ORDER BY.
    let mut out: Vec<(Vec<Value>, Row, Vec<Value>)> = Vec::new();

    if aggregating {
        let mut agg_exprs = Vec::new();
        let rewritten: Vec<Expr> = items
            .iter()
            .map(|i| extract_aggs(&i.expr, &mut agg_exprs))
            .collect();
        let order_rewritten: Vec<Expr> = ret
            .order_by
            .iter()
            .map(|s| extract_aggs(&s.expr, &mut agg_exprs))
            .collect();
        let key_idx: Vec<usize> = (0..items.len())
            .filter(|&i| !items[i].expr.contains_aggregate())
            .collect();

        let mut groups: Vec<(Vec<Value>, Vec<&Row>)> = Vec::new();
        for r in &rows {
            let mut key = Vec::with_capacity(key_idx.len());
            for &i in &key_idx {
                key.push(ctx.eval(&items[i].expr, r, &[]).await?);
            }
            match groups
                .iter_mut()
                .find(|(k, _)| k.iter().zip(&key).all(|(a, b)| equivalent(a, b)))
            {
                Some((_, members)) => members.push(r),
                None => groups.push((key, vec![r])),
            }
        }
        if groups.is_empty() && key_idx.is_empty() {
            groups.push((Vec::new(), Vec::new()));
        }
        for (key, members) in groups {
            let mut aggs = Vec::with_capacity(agg_exprs.len());
            for a in &agg_exprs {
                aggs.push(compute_agg(ctx, a, &members).await?);
            }
            // Scope: grouping keys by their own expression + aliases.
            let mut scope = members.first().map(|r| (*r).clone()).unwrap_or_default();
            let mut vals = Vec::with_capacity(items.len());
            let mut k = key.into_iter();
            for (i, expr) in rewritten.iter().enumerate() {
                let v = if key_idx.contains(&i) {
                    k.next().unwrap()
                } else {
                    ctx.eval(expr, &scope, &aggs).await?
                };
                vals.push(v);
            }
            for (it, v) in items.iter().zip(&vals) {
                scope.insert(it.column_name().to_string(), v.clone());
            }
            out.push((vals, scope, aggs));
        }
        let mut sorted = out;
        sort_records(ctx, ret, &items, &order_rewritten, &mut sorted).await?;
        return finish(ctx, ret, columns, sorted).await;
    }

    for r in rows {
        let mut vals = Vec::with_capacity(items.len());
        for it in &items {
            vals.push(ctx.eval(&it.expr, &r, &[]).await?);
        }
        let mut scope = r;
        for (it, v) in items.iter().zip(&vals) {
            scope.insert(it.column_name().to_string(), v.clone());
        }
        out.push((vals, scope, Vec::new()));
    }
    if ret.distinct {
        let mut seen: Vec<(Vec<Value>, Row, Vec<Value>)> = Vec::new();
        for rec in out {
            if !seen
                .iter()
                .any(|s| s.0.iter().zip(&rec.0).all(|(a, b)| equivalent(a, b)))
            {
                seen.push(rec);
            }
        }
        out = seen;
    }
    let order: Vec<Expr> = ret.order_by.iter().map(|s| s.expr.clone()).collect();
    sort_records(ctx, ret, &items, &order, &mut out).await?;
    finish(ctx, ret, columns, out).await
}

async fn sort_records(
    ctx: &Ctx<'_>,
    ret: &Return,
    items: &[ReturnItem],
    order: &[Expr],
    recs: &mut Vec<(Vec<Value>, Row, Vec<Value>)>,
) -> Result<(), Error> {
    if ret.order_by.is_empty() {
        return Ok(());
    }
    let mut keyed = Vec::with_capacity(recs.len());
    for rec in recs.drain(..) {
        let mut keys = Vec::with_capacity(order.len());
        for (s, e) in ret.order_by.iter().zip(order) {
            // An ORDER BY expression identical to a projected one sorts by that column.
            let v = match items.iter().position(|i| i.expr == s.expr) {
                Some(i) => rec.0[i].clone(),
                None => ctx.eval(e, &rec.1, &rec.2).await?,
            };
            keys.push(v);
        }
        keyed.push((keys, rec));
    }
    keyed.sort_by(|(a, _), (b, _)| {
        for ((x, y), s) in a.iter().zip(b).zip(&ret.order_by) {
            let o = sort_order(x, y);
            let o = if s.descending { o.reverse() } else { o };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    recs.extend(keyed.into_iter().map(|(_, r)| r));
    Ok(())
}

async fn finish(
    ctx: &Ctx<'_>,
    ret: &Return,
    columns: Vec<String>,
    recs: Vec<(Vec<Value>, Row, Vec<Value>)>,
) -> Result<QueryResult, Error> {
    let mut rows: Vec<Vec<Value>> = recs.into_iter().map(|r| r.0).collect();
    if ret.distinct && rows.len() > 1 {
        // Aggregating DISTINCT: dedupe after grouping.
        let mut seen: Vec<Vec<Value>> = Vec::new();
        for r in rows {
            if !seen
                .iter()
                .any(|s| s.iter().zip(&r).all(|(a, b)| equivalent(a, b)))
            {
                seen.push(r);
            }
        }
        rows = seen;
    }
    let offset = non_negative_int(ctx, &ret.offset, "OFFSET")
        .await?
        .unwrap_or(0);
    let limit = non_negative_int(ctx, &ret.limit, "LIMIT").await?;
    let rows = rows
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    Ok(QueryResult { columns, rows })
}

/// Check and evaluate a parsed query.
pub async fn execute(
    store: &dyn GraphStore,
    q: &Query,
    params: &Params,
) -> Result<QueryResult, Error> {
    check(q, params)?;
    let ctx = Ctx { store, params };
    let mut rows = vec![Row::new()];
    let mut in_scope: Vec<String> = Vec::new();
    for clause in &q.clauses {
        match clause {
            Clause::Match(m) => {
                let mut next = Vec::new();
                for row in rows {
                    let mut matcher = Matcher {
                        ctx: &ctx,
                        patterns: &m.patterns,
                        used: HashSet::new(),
                        out: Vec::new(),
                    };
                    matcher.run(row).await?;
                    for r in matcher.out {
                        let keep = match &m.where_ {
                            None => true,
                            Some(w) => truth(&ctx.eval(w, &r, &[]).await?, "WHERE")? == Some(true),
                        };
                        if keep {
                            next.push(r);
                        }
                    }
                }
                rows = next;
                for p in &m.patterns {
                    let vars = std::iter::once(&p.start.var)
                        .chain(p.steps.iter().flat_map(|(e, n)| [&e.var, &n.var]));
                    for v in vars.flatten() {
                        if !in_scope.contains(v) {
                            in_scope.push(v.clone());
                        }
                    }
                }
            }
            Clause::Filter(cond) => {
                let mut next = Vec::new();
                for r in rows {
                    if truth(&ctx.eval(cond, &r, &[]).await?, "FILTER")? == Some(true) {
                        next.push(r);
                    }
                }
                rows = next;
            }
            Clause::For { var, list } => {
                let mut next = Vec::new();
                for row in rows {
                    let items = match ctx.eval(list, &row, &[]).await? {
                        Value::Null => Vec::new(),
                        Value::List(xs) => xs,
                        other => vec![other],
                    };
                    for v in items {
                        let mut r = row.clone();
                        r.insert(var.clone(), v);
                        next.push(r);
                    }
                }
                rows = next;
                in_scope.push(var.clone());
            }
        }
    }
    project(&ctx, &q.ret, rows, &in_scope).await
}
