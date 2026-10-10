//! slcr-query: run a profile query against an SLCR specification file.
//!
//! Usage: slcr-query [--tsv] [--overlay FILE] [--param NAME=JSON]... SPEC.json QUERY

use slcr_gql::{Params, PetgraphStore, Value, apply_overlay, load_spec, query};
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage: slcr-query [--tsv] [--overlay FILE] [--param NAME=JSON]... SPEC.json QUERY");
    ExitCode::from(2)
}

/// Render a value for display: strings unquoted, everything else as in the engine.
fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn main() -> ExitCode {
    pollster::block_on(run())
}

async fn run() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut tsv, mut overlay, mut params, mut positional) =
        (false, None, Params::new(), Vec::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tsv" => tsv = true,
            "--overlay" => overlay = args.next(),
            "--param" => {
                let Some((k, v)) = args.next().and_then(|p| {
                    p.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                }) else {
                    return usage();
                };
                let value = match serde_json::from_str::<serde_json::Value>(&v) {
                    Ok(serde_json::Value::String(s)) => Value::String(s),
                    Ok(serde_json::Value::Number(n)) if n.is_i64() => {
                        Value::Int(n.as_i64().unwrap())
                    }
                    Ok(serde_json::Value::Number(n)) => Value::Float(n.as_f64().unwrap()),
                    Ok(serde_json::Value::Bool(b)) => Value::Bool(b),
                    _ => Value::String(v),
                };
                params.insert(k, value);
            }
            "-h" | "--help" => return usage(),
            _ => positional.push(a),
        }
    }
    let [spec_path, q] = positional.as_slice() else {
        return usage();
    };

    let mut g = PetgraphStore::new();
    let loaded = match std::fs::read_to_string(spec_path) {
        Ok(src) => load_spec(&src, &mut g).await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    let index = match loaded {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{spec_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(path) = overlay {
        let res = match std::fs::read_to_string(&path) {
            Ok(src) => apply_overlay(&src, &index, &mut g)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = res {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    }

    let r = match query(&g, q, &params).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();
    if tsv {
        println!("{}", r.columns.join("\t"));
        for row in rows {
            println!("{}", row.join("\t"));
        }
        return ExitCode::SUCCESS;
    }
    let widths: Vec<usize> = (0..r.columns.len())
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([r.columns[i].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    println!("{}", line(&r.columns));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in &rows {
        println!("{}", line(row));
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    ExitCode::SUCCESS
}
