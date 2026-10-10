//! Built-in functions. Names are canonicalized to their GQL spelling; Cypher
//! spellings are accepted as documented aliases.

use crate::error::Error;
use crate::store::GraphStore;
use crate::value::Value;

/// (accepted spelling, canonical name)
const NAMES: &[(&str, &str)] = &[
    // Aggregates
    ("count", "count"),
    ("sum", "sum"),
    ("avg", "avg"),
    ("min", "min"),
    ("max", "max"),
    ("collect_list", "collect_list"),
    ("collect", "collect_list"),
    // Strings
    ("upper", "upper"),
    ("toupper", "upper"),
    ("lower", "lower"),
    ("tolower", "lower"),
    ("trim", "trim"),
    ("char_length", "char_length"),
    ("character_length", "char_length"),
    // Lists
    ("cardinality", "cardinality"),
    ("size", "size"),
    // Numbers
    ("abs", "abs"),
    ("floor", "floor"),
    ("ceil", "ceil"),
    ("ceiling", "ceil"),
    ("sqrt", "sqrt"),
    ("mod", "mod"),
    ("power", "power"),
    // General
    ("coalesce", "coalesce"),
    // Cypher compatibility extensions (not GQL)
    ("labels", "labels"),
    ("type", "type"),
];

pub fn canonical_name(name: &str) -> Option<&'static str> {
    NAMES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map(|(_, c)| *c)
}

pub fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "avg" | "min" | "max" | "collect_list"
    )
}

fn arity(name: &str, args: &[Value], n: usize) -> Result<(), Error> {
    if args.len() != n {
        return Err(Error::Semantic(format!(
            "{name}() takes {n} argument(s), got {}",
            args.len()
        )));
    }
    Ok(())
}

pub fn expected_arity(name: &str) -> Option<usize> {
    match name {
        "coalesce" => None,
        "mod" | "power" => Some(2),
        _ => Some(1),
    }
}

fn type_err(name: &str, v: &Value) -> Error {
    Error::Data(format!("{name}() does not accept {}", v.type_name()))
}

pub fn call_scalar(name: &str, args: Vec<Value>, store: &dyn GraphStore) -> Result<Value, Error> {
    if name == "coalesce" {
        return Ok(args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null));
    }
    if matches!(name, "mod" | "power") {
        arity(name, &args, 2)?;
        let mut it = args.into_iter();
        let (a, b) = (it.next().unwrap(), it.next().unwrap());
        let op = if name == "mod" {
            crate::ast::BinOp::Mod
        } else {
            crate::ast::BinOp::Pow
        };
        return crate::exec::arith(op, a, b);
    }
    arity(name, &args, 1)?;
    let v = args.into_iter().next().unwrap();
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match (name, &v) {
        ("upper", Value::String(s)) => Value::String(s.to_uppercase()),
        ("lower", Value::String(s)) => Value::String(s.to_lowercase()),
        ("trim", Value::String(s)) => Value::String(s.trim().to_string()),
        ("char_length" | "size", Value::String(s)) => Value::Int(s.chars().count() as i64),
        ("cardinality" | "size", Value::List(xs)) => Value::Int(xs.len() as i64),
        ("cardinality", Value::Map(m)) => Value::Int(m.len() as i64),
        ("abs", Value::Int(i)) => Value::Int(
            i.checked_abs()
                .ok_or_else(|| Error::Data("integer overflow in abs()".into()))?,
        ),
        ("abs", Value::Float(x)) => Value::Float(x.abs()),
        ("floor", Value::Int(i)) => Value::Float(*i as f64),
        ("floor", Value::Float(x)) => Value::Float(x.floor()),
        ("ceil", Value::Int(i)) => Value::Float(*i as f64),
        ("ceil", Value::Float(x)) => Value::Float(x.ceil()),
        ("sqrt", Value::Int(i)) => Value::Float((*i as f64).sqrt()),
        ("sqrt", Value::Float(x)) => Value::Float(x.sqrt()),
        ("labels", Value::Node(n)) => Value::List(
            store
                .node_labels(*n)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
        ("type", Value::Edge(e)) => store
            .edge_labels(*e)
            .into_iter()
            .next()
            .map(Value::String)
            .unwrap_or(Value::Null),
        _ => return Err(type_err(name, &v)),
    })
}
