//! Minimal Gherkin reader: features, backgrounds, scenarios, outlines,
//! docstrings and tables. Enough for the openCypher and opengql TCKs.

#[derive(Debug, Clone)]
pub struct Step {
    pub text: String,
    pub doc: Option<String>,
    pub table: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub steps: Vec<Step>,
}

fn split_row(line: &str) -> Vec<String> {
    let inner = line.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some('|') => {
                    cur.push('|');
                    chars.next();
                }
                Some('n') => {
                    cur.push('\n');
                    chars.next();
                }
                Some('\\') => {
                    cur.push('\\');
                    chars.next();
                }
                _ => cur.push('\\'),
            },
            '|' => cells.push(std::mem::take(&mut cur).trim().to_string()),
            c => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

const KEYWORDS: &[&str] = &["Given ", "When ", "Then ", "And ", "But "];

pub fn parse(src: &str) -> Vec<Scenario> {
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut background: Vec<Step> = Vec::new();
    let mut out = Vec::new();

    enum Block {
        None,
        Background,
        Scenario { name: String, outline: bool },
    }
    let mut block = Block::None;
    let mut steps: Vec<Step> = Vec::new();
    let mut examples: Vec<Vec<String>> = Vec::new();
    let mut in_examples = false;

    let flush = |block: &Block,
                 steps: &mut Vec<Step>,
                 examples: &mut Vec<Vec<String>>,
                 background: &mut Vec<Step>,
                 out: &mut Vec<Scenario>| {
        match block {
            Block::None => {}
            Block::Background => *background = std::mem::take(steps),
            Block::Scenario { name, outline } => {
                let mut all = background.clone();
                all.extend(std::mem::take(steps));
                if *outline && examples.len() > 1 {
                    let header = examples[0].clone();
                    for (i, row) in examples[1..].iter().enumerate() {
                        let sub = |s: &str| {
                            let mut s = s.to_string();
                            for (h, v) in header.iter().zip(row) {
                                s = s.replace(&format!("<{h}>"), v);
                            }
                            s
                        };
                        let steps = all
                            .iter()
                            .map(|st| Step {
                                text: sub(&st.text),
                                doc: st.doc.as_deref().map(sub),
                                table: st
                                    .table
                                    .iter()
                                    .map(|r| r.iter().map(|c| sub(c)).collect())
                                    .collect(),
                            })
                            .collect();
                        out.push(Scenario {
                            name: format!("{name} #{}", i + 1),
                            steps,
                        });
                    }
                } else {
                    out.push(Scenario {
                        name: name.clone(),
                        steps: all,
                    });
                }
                examples.clear();
            }
        }
    };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue;
        }
        if line.starts_with("Feature:") {
            continue;
        }
        if line.starts_with("Background:") {
            flush(&block, &mut steps, &mut examples, &mut background, &mut out);
            block = Block::Background;
            in_examples = false;
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("Scenario Outline:")
            .or_else(|| line.strip_prefix("Scenario Template:"))
        {
            flush(&block, &mut steps, &mut examples, &mut background, &mut out);
            block = Block::Scenario {
                name: rest.trim().to_string(),
                outline: true,
            };
            in_examples = false;
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("Scenario:")
            .or_else(|| line.strip_prefix("Example:"))
        {
            flush(&block, &mut steps, &mut examples, &mut background, &mut out);
            block = Block::Scenario {
                name: rest.trim().to_string(),
                outline: false,
            };
            in_examples = false;
            continue;
        }
        if line.starts_with("Examples:") {
            in_examples = true;
            continue;
        }
        if line.starts_with('|') {
            if in_examples {
                examples.push(split_row(line));
            } else if let Some(st) = steps.last_mut() {
                st.table.push(split_row(line));
            }
            continue;
        }
        if line.starts_with("\"\"\"") {
            let indent = lines[i - 1].len() - lines[i - 1].trim_start().len();
            let mut doc = Vec::new();
            while i < lines.len() && !lines[i].trim().starts_with("\"\"\"") {
                let l = lines[i];
                let cut = l.len() - l.trim_start().len();
                doc.push(&l[cut.min(indent)..]);
                i += 1;
            }
            i += 1;
            if let Some(st) = steps.last_mut() {
                st.doc = Some(doc.join("\n"));
            }
            continue;
        }
        if let Some(k) = KEYWORDS.iter().find(|k| line.starts_with(**k)) {
            steps.push(Step {
                text: line[k.len()..].trim().to_string(),
                doc: None,
                table: Vec::new(),
            });
        }
    }
    flush(&block, &mut steps, &mut examples, &mut background, &mut out);
    out
}
