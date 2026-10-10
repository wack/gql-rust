//! Non-normative data loading (`CREATE`/`INSERT` patterns), used by the TCK
//! harness and examples. Not part of the query profile.

use crate::ast::{Expr, LabelExpr, NodePattern, PathPattern};
use crate::error::Error;
use crate::exec::{Params, execute};
use crate::store::{GraphStoreMut, NodeId};
use crate::value::Value;
use std::collections::{BTreeMap, HashMap};

fn labels(l: &Option<LabelExpr>, edge: bool) -> Result<Vec<String>, Error> {
    fn walk(l: &LabelExpr, out: &mut Vec<String>) -> Result<(), Error> {
        match l {
            LabelExpr::Label(s) => {
                out.push(s.clone());
                Ok(())
            }
            LabelExpr::And(a, b) => {
                walk(a, out)?;
                walk(b, out)
            }
            _ => Err(Error::Semantic(
                "insert labels must be a conjunction of names".into(),
            )),
        }
    }
    let mut out = Vec::new();
    if let Some(l) = l {
        walk(l, &mut out)?;
    }
    if edge && out.len() != 1 {
        return Err(Error::Semantic(
            "an inserted edge needs exactly one label".into(),
        ));
    }
    Ok(out)
}

/// Evaluate a constant expression (no variables) via the query engine.
fn constant(e: &Expr) -> Result<Value, Error> {
    use crate::ast::*;
    let q = Query {
        clauses: vec![],
        ret: Return {
            distinct: false,
            items: ReturnItems::Items(vec![ReturnItem {
                expr: e.clone(),
                alias: Some("v".into()),
                text: String::new(),
            }]),
            order_by: vec![],
            offset: None,
            limit: None,
        },
    };
    let empty = crate::store::PetgraphStore::new();
    Ok(execute(&empty, &q, &Params::new())?
        .rows
        .remove(0)
        .remove(0))
}

fn props(p: &[(String, Expr)]) -> Result<BTreeMap<String, Value>, Error> {
    p.iter()
        .map(|(k, e)| Ok((k.clone(), constant(e)?)))
        .collect()
}

fn node(
    store: &mut dyn GraphStoreMut,
    vars: &mut HashMap<String, NodeId>,
    n: &NodePattern,
) -> Result<NodeId, Error> {
    if let Some(v) = &n.var
        && let Some(id) = vars.get(v)
    {
        if n.labels.is_some() || !n.props.is_empty() {
            return Err(Error::Semantic(format!("variable `{v}` already created")));
        }
        return Ok(*id);
    }
    let id = store.add_node(labels(&n.labels, false)?, props(&n.props)?);
    if let Some(v) = &n.var {
        vars.insert(v.clone(), id);
    }
    Ok(id)
}

fn insert(
    store: &mut dyn GraphStoreMut,
    vars: &mut HashMap<String, NodeId>,
    p: &PathPattern,
) -> Result<(), Error> {
    let mut cur = node(store, vars, &p.start)?;
    for (e, n) in &p.steps {
        if e.quantifier.is_some() || e.where_.is_some() {
            return Err(Error::Semantic("cannot insert a quantified edge".into()));
        }
        let next = node(store, vars, n)?;
        let (s, t) = match e.direction {
            crate::ast::Direction::Right => (cur, next),
            crate::ast::Direction::Left => (next, cur),
            crate::ast::Direction::Any => {
                return Err(Error::Semantic("inserted edges must be directed".into()));
            }
        };
        store.add_edge(s, t, labels(&e.labels, true)?, props(&e.props)?);
        cur = next;
    }
    Ok(())
}

/// Parse and apply a fixture script. Variables are shared across its clauses.
pub fn load_fixture(store: &mut dyn GraphStoreMut, text: &str) -> Result<(), Error> {
    let f = crate::parser::parse_fixture(text)?;
    let mut vars = HashMap::new();
    for clause in &f.inserts {
        for p in clause {
            insert(store, &mut vars, p)?;
        }
    }
    Ok(())
}
