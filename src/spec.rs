//! Loads an SLCR specification (the v1 JSON format) into a property graph.
//!
//! ## Graph mapping
//!
//! | JSON                         | Graph                                                   |
//! |------------------------------|---------------------------------------------------------|
//! | `section` (incl. glossary)   | node `:Section {id, title, body?}`                     |
//! | `requirement`                | node `:Requirement {id, title, body, modality, refinement?, rationale?}` |
//! | `acceptanceCriterion`        | node `:AcceptanceCriterion {id, title, body}`           |
//! | `term`                       | node `:Term {id, title, definition, aliases?}`          |
//! | `children` / `terms` nesting | edge `(parent)-[:CONTAINS {position}]->(child)`         |
//! | glossary                     | the root's last `CONTAINS` child                        |
//! | `acceptanceCriteria`         | edge `(req)-[:HAS_CRITERION {position}]->(ac)`          |
//! | `refines`                    | edge `(req)-[:REFINES]->(refined)`                      |
//! | `dependsOn`                  | edge `(req)-[:DEPENDS_ON]->(target)`                    |
//! | `usesTerm`                   | edge `(node)-[:USES_TERM]->(term)`                      |
//!
//! `position` is the 0-based array index, so `ORDER BY e.position` yields
//! document order. Every node also carries `spec`, the specification name.
//! Derived data (status, satisfaction) is not part of the format and is not
//! loaded; [`apply_overlay`] attaches it as extra node properties.

use crate::store::{GraphStoreMut, NodeId};
use crate::value::Value;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("invalid specification JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("duplicate id {0}")]
    DuplicateId(String),
    #[error("{from} references unknown {kind} {to}")]
    DanglingReference {
        from: String,
        kind: &'static str,
        to: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Document {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    #[allow(dead_code)]
    format_version: String,
    spec: SpecMeta,
    root: TaggedSection,
    glossary: Glossary,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum TaggedSection {
    Section(Section),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecMeta {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Section {
    id: String,
    title: String,
    body: Option<String>,
    #[serde(default)]
    uses_term: Vec<String>,
    #[serde(default)]
    children: Vec<SectionChild>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum SectionChild {
    Section(Section),
    Requirement(Requirement),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Requirement {
    id: String,
    title: String,
    body: String,
    rationale: Option<String>,
    modality: String,
    refinement: Option<String>,
    #[serde(default)]
    refines: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    uses_term: Vec<String>,
    #[serde(default)]
    acceptance_criteria: Vec<Criterion>,
    #[serde(default)]
    children: Vec<TaggedRequirement>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum TaggedRequirement {
    Requirement(Requirement),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Criterion {
    AcceptanceCriterion {
        id: String,
        title: String,
        body: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Glossary {
    #[allow(dead_code)]
    kind: String,
    id: String,
    title: String,
    body: Option<String>,
    #[serde(default)]
    terms: Vec<Term>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Term {
    #[allow(dead_code)]
    kind: String,
    id: String,
    title: String,
    #[serde(default)]
    aliases: Vec<String>,
    definition: String,
}

/// Node ids by SLCR id, returned so callers can attach overlays.
pub type SpecIndex = HashMap<String, NodeId>;

struct Loader<'a> {
    store: &'a mut dyn GraphStoreMut,
    spec: String,
    ids: SpecIndex,
    /// (source SLCR id, edge label, target SLCR id, allowed target prefixes)
    pending: Vec<(String, &'static str, String, &'static [&'static str])>,
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

impl Loader<'_> {
    fn node(
        &mut self,
        label: &str,
        id: &str,
        mut props: BTreeMap<String, Value>,
    ) -> Result<NodeId, SpecError> {
        if self.ids.contains_key(id) {
            return Err(SpecError::DuplicateId(id.to_string()));
        }
        props.insert("id".into(), s(id));
        props.insert("spec".into(), s(&self.spec));
        let n = self.store.add_node(vec![label.to_string()], props);
        self.ids.insert(id.to_string(), n);
        Ok(n)
    }

    fn contains(&mut self, parent: NodeId, child: NodeId, label: &str, position: usize) {
        let props = BTreeMap::from([("position".to_string(), Value::Int(position as i64))]);
        self.store
            .add_edge(parent, child, vec![label.to_string()], props);
    }

    fn refs(
        &mut self,
        from: &str,
        label: &'static str,
        targets: &[String],
        kinds: &'static [&'static str],
    ) {
        for t in targets {
            self.pending
                .push((from.to_string(), label, t.clone(), kinds));
        }
    }

    fn section(&mut self, sec: &Section) -> Result<NodeId, SpecError> {
        let mut props = BTreeMap::from([("title".to_string(), s(&sec.title))]);
        if let Some(b) = &sec.body {
            props.insert("body".into(), s(b));
        }
        let n = self.node("Section", &sec.id, props)?;
        self.refs(&sec.id, "USES_TERM", &sec.uses_term, &["TERM-"]);
        for (i, child) in sec.children.iter().enumerate() {
            let c = match child {
                SectionChild::Section(x) => self.section(x)?,
                SectionChild::Requirement(x) => self.requirement(x)?,
            };
            self.contains(n, c, "CONTAINS", i);
        }
        Ok(n)
    }

    fn requirement(&mut self, r: &Requirement) -> Result<NodeId, SpecError> {
        let mut props = BTreeMap::from([
            ("title".to_string(), s(&r.title)),
            ("body".to_string(), s(&r.body)),
            ("modality".to_string(), s(&r.modality)),
        ]);
        if let Some(x) = &r.rationale {
            props.insert("rationale".into(), s(x));
        }
        if let Some(x) = &r.refinement {
            props.insert("refinement".into(), s(x));
        }
        let n = self.node("Requirement", &r.id, props)?;
        self.refs(&r.id, "REFINES", &r.refines, &["REQ-"]);
        self.refs(&r.id, "DEPENDS_ON", &r.depends_on, &["REQ-", "SEC-"]);
        self.refs(&r.id, "USES_TERM", &r.uses_term, &["TERM-"]);
        for (i, Criterion::AcceptanceCriterion { id, title, body }) in
            r.acceptance_criteria.iter().enumerate()
        {
            let props = BTreeMap::from([
                ("title".to_string(), s(title)),
                ("body".to_string(), s(body)),
            ]);
            let c = self.node("AcceptanceCriterion", id, props)?;
            self.contains(n, c, "HAS_CRITERION", i);
        }
        for (i, TaggedRequirement::Requirement(child)) in r.children.iter().enumerate() {
            let c = self.requirement(child)?;
            self.contains(n, c, "CONTAINS", i);
        }
        Ok(n)
    }
}

/// Load a v1 specification document. Structural validation is limited to
/// what the mapping needs (known fields, unique ids, resolvable references);
/// full schema validation and `x-checkInvariants` remain `check`'s job.
pub fn load_spec(json: &str, store: &mut dyn GraphStoreMut) -> Result<SpecIndex, SpecError> {
    let doc: Document = serde_json::from_str(json)?;
    let mut l = Loader {
        store,
        spec: doc.spec.name.clone(),
        ids: HashMap::new(),
        pending: Vec::new(),
    };
    let TaggedSection::Section(root_section) = &doc.root;
    let root = l.section(root_section)?;

    let g = &doc.glossary;
    let mut props = BTreeMap::from([("title".to_string(), s(&g.title))]);
    if let Some(b) = &g.body {
        props.insert("body".into(), s(b));
    }
    let gloss = l.node("Section", &g.id, props)?;
    let pos = root_section.children.len();
    l.contains(root, gloss, "CONTAINS", pos);
    for (i, t) in g.terms.iter().enumerate() {
        let mut props = BTreeMap::from([
            ("title".to_string(), s(&t.title)),
            ("definition".to_string(), s(&t.definition)),
        ]);
        if !t.aliases.is_empty() {
            props.insert(
                "aliases".into(),
                Value::List(t.aliases.iter().map(|a| s(a)).collect()),
            );
        }
        let n = l.node("Term", &t.id, props)?;
        l.contains(gloss, n, "CONTAINS", i);
    }

    for (from, label, to, kinds) in std::mem::take(&mut l.pending) {
        let target = l
            .ids
            .get(&to)
            .copied()
            .filter(|_| kinds.iter().any(|k| to.starts_with(k)))
            .ok_or_else(|| SpecError::DanglingReference {
                from: from.clone(),
                kind: label,
                to: to.clone(),
            })?;
        let source = l.ids[&from];
        l.store
            .add_edge(source, target, vec![label.to_string()], BTreeMap::new());
    }
    Ok(l.ids)
}

/// Attach derived, non-normative properties (e.g. `status` from validator
/// results) to nodes by SLCR id. Input shape: `{"REQ-001": {"status": "satisfied"}}`.
/// Values may be JSON strings, numbers, booleans, null or arrays thereof.
pub fn apply_overlay(
    json: &str,
    index: &SpecIndex,
    store: &mut dyn GraphStoreMut,
) -> Result<(), SpecError> {
    let doc: BTreeMap<String, BTreeMap<String, serde_json::Value>> = serde_json::from_str(json)?;
    for (id, props) in doc {
        let node = *index.get(&id).ok_or_else(|| SpecError::DanglingReference {
            from: "overlay".into(),
            kind: "node",
            to: id.clone(),
        })?;
        for (k, v) in props {
            store.set_node_property(node, &k, from_json(&v));
        }
    }
    Ok(())
}

fn from_json(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN))),
        serde_json::Value::String(x) => Value::String(x.clone()),
        serde_json::Value::Array(xs) => Value::List(xs.iter().map(from_json).collect()),
        serde_json::Value::Object(m) => {
            Value::Map(m.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
    }
}
