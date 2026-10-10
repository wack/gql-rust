//! Runtime values and their comparison semantics.

use crate::store::{EdgeId, NodeId};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Node(NodeId),
    Edge(EdgeId),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "BOOLEAN",
            Value::Int(_) => "INTEGER",
            Value::Float(_) => "FLOAT",
            Value::String(_) => "STRING",
            Value::List(_) => "LIST",
            Value::Map(_) => "MAP",
            Value::Node(_) => "NODE",
            Value::Edge(_) => "EDGE",
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => {
                if x.fract() == 0.0 && x.is_finite() {
                    write!(f, "{x:.1}")
                } else {
                    write!(f, "{x}")
                }
            }
            Value::String(s) => write!(f, "'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            Value::List(xs) => {
                write!(f, "[")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "]")
            }
            Value::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            Value::Node(n) => write!(f, "node#{}", n.0),
            Value::Edge(e) => write!(f, "edge#{}", e.0),
        }
    }
}

/// Three-valued equality (`=`). `None` is UNKNOWN.
pub fn equals(a: &Value, b: &Value) -> Option<bool> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => None,
        (Int(x), Int(y)) => Some(x == y),
        (Int(_) | Float(_), Int(_) | Float(_)) => {
            let (x, y) = (a.as_f64()?, b.as_f64()?);
            Some(x == y)
        }
        (Bool(x), Bool(y)) => Some(x == y),
        (String(x), String(y)) => Some(x == y),
        (Node(x), Node(y)) => Some(x == y),
        (Edge(x), Edge(y)) => Some(x == y),
        (List(xs), List(ys)) => {
            if xs.len() != ys.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (x, y) in xs.iter().zip(ys) {
                match equals(x, y) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        (Map(xs), Map(ys)) => {
            if xs.len() != ys.len() || xs.keys().ne(ys.keys()) {
                return Some(false);
            }
            let mut unknown = false;
            for (x, y) in xs.values().zip(ys.values()) {
                match equals(x, y) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        _ => Some(false),
    }
}

/// Three-valued ordering comparison (`<` etc). `None` means UNKNOWN: either
/// an operand is null or the values are not comparable.
pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Int(_) | Float(_), Int(_) | Float(_)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (String(x), String(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (List(xs), List(ys)) => {
            for (x, y) in xs.iter().zip(ys) {
                match compare(x, y)? {
                    Ordering::Equal => {}
                    o => return Some(o),
                }
            }
            Some(xs.len().cmp(&ys.len()))
        }
        _ => None,
    }
}

/// Equivalence used for grouping, DISTINCT and `IN`-free deduplication:
/// like `=`, but null is equivalent to null and NaN to NaN.
pub fn equivalent(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Float(x), Float(y)) if x.is_nan() && y.is_nan() => true,
        (List(xs), List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| equivalent(x, y))
        }
        (Map(xs), Map(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys)
                    .all(|((k1, v1), (k2, v2))| k1 == k2 && equivalent(v1, v2))
        }
        _ => equals(a, b) == Some(true),
    }
}

/// Total order used by `ORDER BY`. Ascending: values of different types are
/// ordered MAP < NODE < EDGE < LIST < STRING < BOOLEAN < NUMBER, with
/// NaN after all numbers and null last.
pub fn sort_order(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Map(_) => 0,
            Value::Node(_) => 1,
            Value::Edge(_) => 2,
            Value::List(_) => 3,
            Value::String(_) => 4,
            Value::Bool(_) => 5,
            Value::Int(_) | Value::Float(_) => 6,
            Value::Null => 7,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(_) | Value::Float(_), _) => {
            let (x, y) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            match (x.is_nan(), y.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => x.partial_cmp(&y).unwrap(),
            }
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Node(x), Value::Node(y)) => x.cmp(y),
        (Value::Edge(x), Value::Edge(y)) => x.cmp(y),
        (Value::List(xs), Value::List(ys)) => {
            for (x, y) in xs.iter().zip(ys) {
                match sort_order(x, y) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            xs.len().cmp(&ys.len())
        }
        (Value::Map(xs), Value::Map(ys)) => {
            for ((k1, v1), (k2, v2)) in xs.iter().zip(ys) {
                match k1.cmp(k2).then_with(|| sort_order(v1, v2)) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            xs.len().cmp(&ys.len())
        }
        _ => Ordering::Equal,
    }
}
