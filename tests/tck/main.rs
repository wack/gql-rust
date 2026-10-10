//! Runs the vendored openCypher and opengql TCKs against the SLCR profile.
//!
//! Every scenario gets one outcome:
//!   pass      expected result or expected error produced
//!   wrong     a result was produced that differs from the expected one, or a
//!             result was produced where an error was expected (silent wrong
//!             answer: always a CI failure)
//!   errored   the query is in the profile grammar but raised an error where
//!             a result was expected (loud divergence)
//!   rejected  the query or its setup is outside the profile (not attempted)
//!
//! `cargo test --test tck` fails on any `wrong` and on any scenario that was
//! `pass` in `tck/baseline.tsv` but no longer is. Set TCK_UPDATE_BASELINE=1 to
//! rewrite the baseline; set TCK_VERBOSE=1 to list non-passing scenarios;
//! TCK_FILTER=<substring> limits the run. TCK_STORE=turso runs every scenario
//! against an in-memory `TursoStore` (needs `--features turso`) instead of the
//! reference `PetgraphStore`, against the same baseline.

mod gherkin;
mod values;

use slcr_gql::{
    Error, GraphStore, GraphStoreMut, Params, PetgraphStore, QueryResult, load_fixture, query,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use values::{P, TV, from_value, multiset_eq, to_value};

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Pass,
    Wrong(String),
    Errored(String),
    Rejected(String),
}

impl Outcome {
    fn tag(&self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Wrong(_) => "wrong",
            Outcome::Errored(_) => "errored",
            Outcome::Rejected(_) => "rejected",
        }
    }
    fn detail(&self) -> &str {
        match self {
            Outcome::Pass => "",
            Outcome::Wrong(s) | Outcome::Errored(s) | Outcome::Rejected(s) => s,
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tck")
}

fn features(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            features(&p, out);
        } else if p.extension().is_some_and(|e| e == "feature") {
            out.push(p);
        }
    }
}

struct Run {
    store: Box<dyn GraphStoreMut>,
    params: Params,
    result: Option<Result<QueryResult, Error>>,
}

fn new_store() -> Box<dyn GraphStoreMut> {
    match std::env::var("TCK_STORE").as_deref() {
        Err(_) | Ok("petgraph") => Box::new(PetgraphStore::new()),
        #[cfg(feature = "turso")]
        Ok("turso") => Box::new(slcr_gql::TursoStore::in_memory().expect("open turso store")),
        Ok(other) => panic!("unknown TCK_STORE `{other}` (is its feature enabled?)"),
    }
}

fn named_graph(name: &str) -> Option<String> {
    let p = root()
        .join("opencypher/graphs")
        .join(name)
        .join(format!("{name}.cypher"));
    std::fs::read_to_string(p).ok()
}

fn compare(
    r: &QueryResult,
    g: &dyn GraphStore,
    table: &[Vec<String>],
    ordered: bool,
    unordered_lists: bool,
) -> Result<(), String> {
    let header = &table[0];
    let mut cols = Vec::new();
    for h in header {
        match r.columns.iter().position(|c| c == h) {
            Some(i) => cols.push(i),
            None => return Err(format!("missing column `{h}`; got {:?}", r.columns)),
        }
    }
    if cols.len() != r.columns.len() {
        return Err(format!("columns {:?}, expected {:?}", r.columns, header));
    }
    let mut expected = Vec::new();
    for row in &table[1..] {
        let vals = row
            .iter()
            .map(|c| P::parse(c))
            .collect::<Result<Vec<_>, _>>()?;
        expected.push(TV::List(vals));
    }
    let actual: Vec<TV> = r
        .rows
        .iter()
        .map(|row| TV::List(cols.iter().map(|&i| from_value(&row[i], g)).collect()))
        .collect();
    let ok = actual.len() == expected.len()
        && if ordered {
            actual
                .iter()
                .zip(&expected)
                .all(|(a, e)| a.eq(e, unordered_lists))
        } else {
            multiset_eq(&actual, &expected, unordered_lists)
        };
    if ok {
        Ok(())
    } else {
        let show = |rows: &Vec<Vec<slcr_gql::Value>>| {
            rows.iter()
                .map(|r| {
                    r.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        Err(format!(
            "got [{}], expected {:?}",
            show(&r.rows),
            &table[1..]
        ))
    }
}

fn run_scenario(s: &gherkin::Scenario) -> Outcome {
    let mut run = Run {
        store: new_store(),
        params: Params::new(),
        result: None,
    };
    for step in &s.steps {
        let t = step.text.trim_end_matches(':');
        let doc = step.doc.as_deref().unwrap_or("");
        match t {
            "an empty graph"
            | "any graph"
            | "the omitted graph"
            | "the empty-person graph"
            | "an empty catalog" => {
                run.store = new_store();
            }
            t if t.starts_with("the ") && t.ends_with(" graph") => {
                let name = &t[4..t.len() - 6];
                match named_graph(name) {
                    Some(src) => {
                        if let Err(e) = load_fixture(run.store.as_mut(), &src) {
                            return Outcome::Rejected(format!("named graph {name}: {e}"));
                        }
                    }
                    None => return Outcome::Rejected(format!("unknown graph {name}")),
                }
            }
            "having executed" => {
                if let Err(e) = load_fixture(run.store.as_mut(), doc) {
                    return Outcome::Rejected(format!("fixture: {e}"));
                }
            }
            "parameters are" => {
                for row in &step.table {
                    let v = match P::parse(&row[1]).ok().as_ref().and_then(to_value) {
                        Some(v) => v,
                        None => return Outcome::Rejected(format!("parameter {}", row[1])),
                    };
                    run.params.insert(row[0].clone(), v);
                }
            }
            "executing query" => {
                run.result = Some(query(run.store.as_ref(), doc, &run.params));
                if let Some(Err(Error::Unsupported { feature, .. })) = &run.result {
                    return Outcome::Rejected(format!("unsupported: {feature}"));
                }
            }
            t if t.starts_with("the result should be") || t == "result should be empty" => {
                let r = match run.result.take() {
                    Some(Ok(r)) => r,
                    Some(Err(e @ Error::Syntax { .. })) => {
                        return Outcome::Rejected(format!("syntax: {e}"));
                    }
                    Some(Err(e)) => return Outcome::Errored(e.to_string()),
                    None => return Outcome::Rejected("no query".into()),
                };
                if t.ends_with("empty") {
                    if !r.rows.is_empty() {
                        return Outcome::Wrong(format!("expected no rows, got {}", r.rows.len()));
                    }
                } else {
                    let ordered = t.contains("in order");
                    let unordered_lists = t.contains("ignoring element order for lists");
                    if step.table.is_empty() {
                        return Outcome::Rejected("result table missing".into());
                    }
                    if let Err(msg) = compare(
                        &r,
                        run.store.as_ref(),
                        &step.table,
                        ordered,
                        unordered_lists,
                    ) {
                        if msg.contains("TCK value")
                            || msg.starts_with("expected `")
                            || msg.contains("unrecognised")
                        {
                            return Outcome::Rejected(format!("expected value notation: {msg}"));
                        }
                        return Outcome::Wrong(msg);
                    }
                }
            }
            t if t.contains("should be raised") => match run.result.take() {
                Some(Ok(r)) => {
                    return Outcome::Wrong(format!(
                        "expected error ({t}), got {} row(s)",
                        r.rows.len()
                    ));
                }
                Some(Err(_)) => {}
                None => return Outcome::Rejected("no query".into()),
            },
            "no side effects" => {}
            t if t.starts_with("the side effects should be") => {}
            other => return Outcome::Rejected(format!("step: {other}")),
        }
    }
    Outcome::Pass
}

fn main() {
    let filter = std::env::var("TCK_FILTER").ok();
    let verbose = std::env::var("TCK_VERBOSE").is_ok();
    let mut files = Vec::new();
    features(&root().join("opencypher/features"), &mut files);
    features(&root().join("opengql/features"), &mut files);

    let mut results: BTreeMap<String, Outcome> = BTreeMap::new();
    for f in &files {
        let rel = f.strip_prefix(root()).unwrap().display().to_string();
        let src = std::fs::read_to_string(f).unwrap();
        for s in gherkin::parse(&src) {
            let id = format!("{rel} :: {}", s.name);
            if filter
                .as_ref()
                .is_some_and(|flt| !id.contains(flt.as_str()))
            {
                continue;
            }
            let outcome = std::panic::catch_unwind(|| run_scenario(&s))
                .unwrap_or_else(|_| Outcome::Wrong("panic".into()));
            results.insert(id, outcome);
        }
    }

    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (id, o) in &results {
        let suite = if id.starts_with("opencypher") {
            "opencypher"
        } else {
            "opengql"
        };
        *counts.entry((suite, o.tag())).or_default() += 1;
        if verbose && *o != Outcome::Pass {
            println!("{:8} {id}\n         {}", o.tag(), o.detail());
        }
    }
    let store = std::env::var("TCK_STORE").unwrap_or_else(|_| "petgraph".into());
    println!("\nSLCR GQL profile — TCK summary ({store} store)");
    for suite in ["opencypher", "opengql"] {
        let get = |t| counts.get(&(suite, t)).copied().unwrap_or(0);
        println!(
            "  {suite:10}  pass {:5}  wrong {:3}  errored {:4}  rejected {:5}",
            get("pass"),
            get("wrong"),
            get("errored"),
            get("rejected")
        );
    }

    let baseline_path = root().join("baseline.tsv");
    let mut failed = false;
    for (id, o) in &results {
        if let Outcome::Wrong(msg) = o {
            println!("WRONG {id}\n      {msg}");
            failed = true;
        }
    }
    if std::env::var("TCK_UPDATE_BASELINE").is_ok() {
        let mut out = String::from(
            "# scenario\toutcome (generated by TCK_UPDATE_BASELINE=1 cargo test --test tck)\n",
        );
        for (id, o) in &results {
            writeln!(out, "{id}\t{}", o.tag()).unwrap();
        }
        std::fs::write(&baseline_path, out).unwrap();
        println!("baseline written to {}", baseline_path.display());
    } else if let Ok(base) = std::fs::read_to_string(&baseline_path) {
        let mut improved = 0;
        for line in base.lines().filter(|l| !l.starts_with('#')) {
            let Some((id, tag)) = line.split_once('\t') else {
                continue;
            };
            let Some(now) = results.get(id) else { continue };
            if tag == "pass" && *now != Outcome::Pass {
                println!(
                    "REGRESSION {id}: was pass, now {} ({})",
                    now.tag(),
                    now.detail()
                );
                failed = true;
            } else if tag != "pass" && *now == Outcome::Pass {
                improved += 1;
            }
        }
        if improved > 0 {
            println!("{improved} scenario(s) newly passing; rerun with TCK_UPDATE_BASELINE=1");
        }
    }
    if failed {
        std::process::exit(1);
    }
}
