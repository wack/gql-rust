//! TCK result-value notation: parsing expected cells and converting actual
//! engine values into the same shape for comparison.

use pollster::block_on;
use slcr_gql::{GraphStore, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub enum TV {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<TV>),
    Map(BTreeMap<String, TV>),
    Node {
        labels: BTreeSet<String>,
        props: BTreeMap<String, TV>,
    },
    Rel {
        label: String,
        props: BTreeMap<String, TV>,
    },
    Path(#[allow(dead_code)] String),
}

impl TV {
    pub fn eq(&self, other: &TV, unordered_lists: bool) -> bool {
        use TV::*;
        match (self, other) {
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => {
                (a.is_nan() && b.is_nan())
                    || a == b
                    || (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0)
            }
            (Str(a), Str(b)) => a == b,
            (List(a), List(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                if unordered_lists {
                    multiset_eq(a, b, true)
                } else {
                    a.iter().zip(b).all(|(x, y)| x.eq(y, false))
                }
            }
            (Map(a), Map(b)) => map_eq(a, b, unordered_lists),
            (
                Node {
                    labels: l1,
                    props: p1,
                },
                Node {
                    labels: l2,
                    props: p2,
                },
            ) => l1 == l2 && map_eq(p1, p2, false),
            (
                Rel {
                    label: l1,
                    props: p1,
                },
                Rel {
                    label: l2,
                    props: p2,
                },
            ) => l1 == l2 && map_eq(p1, p2, false),
            _ => false,
        }
    }
}

fn map_eq(a: &BTreeMap<String, TV>, b: &BTreeMap<String, TV>, u: bool) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((k1, v1), (k2, v2))| k1 == k2 && v1.eq(v2, u))
}

pub fn multiset_eq(a: &[TV], b: &[TV], unordered_lists: bool) -> bool {
    let mut used = vec![false; b.len()];
    'outer: for x in a {
        for (j, y) in b.iter().enumerate() {
            if !used[j] && x.eq(y, unordered_lists) {
                used[j] = true;
                continue 'outer;
            }
        }
        return false;
    }
    true
}

pub fn from_value(v: &Value, g: &dyn GraphStore) -> TV {
    match v {
        Value::Null => TV::Null,
        Value::Bool(b) => TV::Bool(*b),
        Value::Int(i) => TV::Int(*i),
        Value::Float(x) => TV::Float(*x),
        Value::String(s) => TV::Str(s.clone()),
        Value::List(xs) => TV::List(xs.iter().map(|x| from_value(x, g)).collect()),
        Value::Map(m) => TV::Map(
            m.iter()
                .map(|(k, x)| (k.clone(), from_value(x, g)))
                .collect(),
        ),
        Value::Node(n) => TV::Node {
            labels: block_on(g.node_labels(*n)).unwrap().into_iter().collect(),
            props: block_on(g.node_properties(*n))
                .unwrap()
                .iter()
                .map(|(k, x)| (k.clone(), from_value(x, g)))
                .collect(),
        },
        Value::Edge(e) => TV::Rel {
            label: block_on(g.edge_labels(*e))
                .unwrap()
                .into_iter()
                .next()
                .unwrap_or_default(),
            props: block_on(g.edge_properties(*e))
                .unwrap()
                .iter()
                .map(|(k, x)| (k.clone(), from_value(x, g)))
                .collect(),
        },
    }
}

/// Convert a parsed TCK value to an engine value (for parameters).
pub fn to_value(t: &TV) -> Option<Value> {
    Some(match t {
        TV::Null => Value::Null,
        TV::Bool(b) => Value::Bool(*b),
        TV::Int(i) => Value::Int(*i),
        TV::Float(x) => Value::Float(*x),
        TV::Str(s) => Value::String(s.clone()),
        TV::List(xs) => Value::List(xs.iter().map(to_value).collect::<Option<_>>()?),
        TV::Map(m) => Value::Map(
            m.iter()
                .map(|(k, v)| Some((k.clone(), to_value(v)?)))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

pub struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    pub fn parse(src: &'a str) -> Result<TV, String> {
        let mut p = P {
            s: src.as_bytes(),
            i: 0,
        };
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(format!("trailing input in TCK value `{src}`"));
        }
        Ok(v)
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected `{}` at {}", c as char, self.i))
        }
    }

    fn rest(&self) -> &str {
        std::str::from_utf8(&self.s[self.i..]).unwrap()
    }

    fn word(&mut self, w: &str) -> bool {
        self.ws();
        if self.rest().starts_with(w) {
            self.i += w.len();
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        self.ws();
        if self.eat(b'`') {
            let start = self.i;
            while self.s[self.i] != b'`' {
                self.i += 1;
            }
            let s = self.rest_slice(start);
            self.i += 1;
            return Ok(s);
        }
        let start = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_')
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(format!("expected identifier at {}", self.i));
        }
        Ok(self.rest_slice(start))
    }

    fn rest_slice(&self, start: usize) -> String {
        String::from_utf8(self.s[start..self.i].to_vec()).unwrap()
    }

    fn props(&mut self) -> Result<BTreeMap<String, TV>, String> {
        let mut m = BTreeMap::new();
        if !self.eat(b'{') {
            return Ok(m);
        }
        if self.eat(b'}') {
            return Ok(m);
        }
        loop {
            let k = self.ident()?;
            self.expect(b':')?;
            let v = self.value()?;
            m.insert(k, v);
            if self.eat(b'}') {
                return Ok(m);
            }
            self.expect(b',')?;
        }
    }

    fn value(&mut self) -> Result<TV, String> {
        match self.peek().ok_or("unexpected end of TCK value")? {
            b'\'' => {
                self.i += 1;
                let mut s = Vec::new();
                loop {
                    let c = *self.s.get(self.i).ok_or("unterminated string")?;
                    self.i += 1;
                    match c {
                        b'\\' => {
                            let n = self.s[self.i];
                            self.i += 1;
                            s.push(match n {
                                b'n' => b'\n',
                                b't' => b'\t',
                                other => other,
                            });
                        }
                        b'\'' => break,
                        c => s.push(c),
                    }
                }
                Ok(TV::Str(String::from_utf8(s).map_err(|e| e.to_string())?))
            }
            b'[' => {
                self.i += 1;
                if self.peek() == Some(b':') {
                    self.i += 1;
                    let label = self.ident()?;
                    let props = self.props()?;
                    self.expect(b']')?;
                    return Ok(TV::Rel { label, props });
                }
                let mut xs = Vec::new();
                if self.eat(b']') {
                    return Ok(TV::List(xs));
                }
                loop {
                    xs.push(self.value()?);
                    if self.eat(b']') {
                        return Ok(TV::List(xs));
                    }
                    self.expect(b',')?;
                }
            }
            b'{' => Ok(TV::Map(self.props()?)),
            b'(' => {
                self.i += 1;
                let mut labels = BTreeSet::new();
                while self.eat(b':') {
                    labels.insert(self.ident()?);
                }
                let props = self.props()?;
                self.expect(b')')?;
                Ok(TV::Node { labels, props })
            }
            b'<' => {
                let start = self.i;
                let mut depth = 0;
                while self.i < self.s.len() {
                    match self.s[self.i] {
                        b'<' if self.i == start => depth += 1,
                        b'>' if depth == 1 && self.s.get(self.i - 1) != Some(&b'-') => {
                            self.i += 1;
                            break;
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
                Ok(TV::Path(self.rest_slice(start)))
            }
            _ => {
                if self.word("null") {
                    return Ok(TV::Null);
                }
                if self.word("true") {
                    return Ok(TV::Bool(true));
                }
                if self.word("false") {
                    return Ok(TV::Bool(false));
                }
                if self.word("NaN") {
                    return Ok(TV::Float(f64::NAN));
                }
                if self.word("-Inf") || self.word("-Infinity") {
                    return Ok(TV::Float(f64::NEG_INFINITY));
                }
                if self.word("Inf") || self.word("Infinity") {
                    return Ok(TV::Float(f64::INFINITY));
                }
                let start = self.i;
                let mut float = false;
                while self.i < self.s.len() {
                    let c = self.s[self.i];
                    if c.is_ascii_digit() || c == b'-' || c == b'+' {
                    } else if c == b'.' || c == b'e' || c == b'E' {
                        float = true;
                    } else {
                        break;
                    }
                    self.i += 1;
                }
                let t = self.rest_slice(start);
                if t.is_empty() {
                    return Err(format!("unrecognised TCK value at `{}`", self.rest()));
                }
                if float {
                    t.parse().map(TV::Float).map_err(|e| format!("{t}: {e}"))
                } else {
                    t.parse().map(TV::Int).map_err(|e| format!("{t}: {e}"))
                }
            }
        }
    }
}
