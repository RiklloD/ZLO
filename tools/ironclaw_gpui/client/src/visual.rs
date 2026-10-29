//! Native visuals in a reply. An agent draws a chart, a table or stat tiles
//! by writing a fenced block tagged `visual` whose body is a JSON spec; the
//! app draws it with GPUI instead of showing the code. GPUI-free so the
//! split and the checks are unit-tested. Agents learn the format from
//! `crates/contracts/ironclaw_host_api/prompts/visuals.md`: keep both in step.

use serde::Deserialize;
use serde_json::Value;

const MAX_POINTS: usize = 500;
const MAX_SERIES: usize = 6;
const MAX_ROWS: usize = 200;
const MAX_COLUMNS: usize = 12;
const MAX_STATS: usize = 8;

/// A reply cut into what Markdown draws and what the app draws.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Text(String),
    Visual(Visual),
    /// The spec doesn't hold: why, and the block as written.
    Invalid {
        error: String,
        source: String,
    },
    /// A visual block still streaming in.
    Pending,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Visual {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// Put on every value: "%", "ms", or a currency sign ("$", "€", "CHF").
    pub unit: Option<String>,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Bar {
        x: Vec<String>,
        series: Series,
    },
    Line {
        x: Vec<String>,
        series: Vec<Series>,
    },
    Area {
        x: Vec<String>,
        series: Vec<Series>,
    },
    Pie {
        x: Vec<String>,
        series: Series,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Stats(Vec<Stat>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub label: String,
    pub value: String,
    pub delta: Option<String>,
    /// Whether the delta is good news; read from its sign when unset.
    pub good: Option<bool>,
}

impl Stat {
    pub fn delta_good(&self) -> Option<bool> {
        let delta = self.delta.as_deref()?.trim();
        self.good.or(match delta.chars().next()? {
            '+' | '↑' => Some(true),
            '-' | '−' | '↓' => Some(false),
            _ => None,
        })
    }
}

/// Cut `text` at its `visual` blocks. A block not closed yet is `Pending`
/// while `streaming`, and parsed as it stands once the reply is final.
pub fn split(text: &str, streaming: bool) -> Vec<Segment> {
    if !text.contains("```visual") {
        return vec![Segment::Text(text.to_string())];
    }
    let mut out = Vec::new();
    let mut prose = String::new();
    // The backtick run of an open fence; `visual` says whose it is.
    let mut fence: Option<(usize, bool)> = None;
    let mut spec = String::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        match fence {
            None if ticks >= 3 => {
                let visual = trimmed[ticks..].trim().eq_ignore_ascii_case("visual");
                fence = Some((ticks, visual));
                if visual {
                    flush(&mut prose, &mut out);
                } else {
                    prose.push_str(line);
                }
            }
            None => prose.push_str(line),
            Some((open, visual)) => {
                let closes = ticks >= open && ticks == trimmed.len();
                if closes {
                    fence = None;
                }
                match (visual, closes) {
                    (true, true) => out.push(parse_segment(&std::mem::take(&mut spec))),
                    (true, false) => spec.push_str(line),
                    (false, _) => prose.push_str(line),
                }
            }
        }
    }
    flush(&mut prose, &mut out);
    if let Some((_, true)) = fence {
        out.push(match streaming {
            true => Segment::Pending,
            false => parse_segment(&spec),
        });
    }
    out
}

fn flush(prose: &mut String, out: &mut Vec<Segment>) {
    if !prose.trim().is_empty() {
        out.push(Segment::Text(prose.clone()));
    }
    prose.clear();
}

fn parse_segment(source: &str) -> Segment {
    match parse(source) {
        Ok(visual) => Segment::Visual(visual),
        Err(error) => Segment::Invalid {
            error,
            source: source.trim_end().to_string(),
        },
    }
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    subtitle: Option<String>,
    unit: Option<String>,
    #[serde(default)]
    x: Vec<Value>,
    #[serde(default)]
    series: Vec<RawSeries>,
    #[serde(default)]
    columns: Vec<Value>,
    #[serde(default)]
    rows: Vec<Vec<Value>>,
    #[serde(default)]
    items: Vec<RawStat>,
}

#[derive(Deserialize)]
struct RawSeries {
    name: Option<String>,
    values: Vec<f64>,
}

#[derive(Deserialize)]
struct RawStat {
    label: String,
    value: Value,
    delta: Option<Value>,
    good: Option<bool>,
}

pub fn parse(source: &str) -> Result<Visual, String> {
    let raw: Raw = serde_json::from_str(source).map_err(|e| format!("not valid JSON: {e}"))?;
    let x: Vec<String> = raw.x.iter().map(label).collect();
    let body = match raw.kind.as_str() {
        "bar" | "pie" => {
            let mut series = checked_series(&x, raw.series)?;
            if series.len() != 1 {
                return Err(format!(
                    "a {} chart takes one series; use \"line\" to compare several",
                    raw.kind
                ));
            }
            let series = series.remove(0);
            if raw.kind == "bar" {
                Body::Bar { x, series }
            } else {
                if series.values.iter().any(|v| *v < 0.) {
                    return Err("pie values can't be negative".into());
                }
                Body::Pie { x, series }
            }
        }
        "line" => Body::Line {
            series: checked_series(&x, raw.series)?,
            x,
        },
        "area" => Body::Area {
            series: checked_series(&x, raw.series)?,
            x,
        },
        "table" => {
            let columns: Vec<String> = raw.columns.iter().map(label).collect();
            if columns.is_empty() || columns.len() > MAX_COLUMNS {
                return Err(format!("a table needs 1 to {MAX_COLUMNS} columns"));
            }
            if raw.rows.len() > MAX_ROWS {
                return Err(format!("a table holds at most {MAX_ROWS} rows"));
            }
            if let Some(i) = raw.rows.iter().position(|r| r.len() != columns.len()) {
                return Err(format!(
                    "row {} has {} cells, there are {} columns",
                    i + 1,
                    raw.rows[i].len(),
                    columns.len()
                ));
            }
            Body::Table {
                columns,
                rows: raw
                    .rows
                    .iter()
                    .map(|r| r.iter().map(label).collect())
                    .collect(),
            }
        }
        "stats" => {
            if raw.items.is_empty() || raw.items.len() > MAX_STATS {
                return Err(format!("stats takes 1 to {MAX_STATS} items"));
            }
            Body::Stats(
                raw.items
                    .into_iter()
                    .map(|s| Stat {
                        label: s.label,
                        value: label(&s.value),
                        delta: s.delta.as_ref().map(label).filter(|d| !d.is_empty()),
                        good: s.good,
                    })
                    .collect(),
            )
        }
        other => {
            return Err(format!(
                "unknown type \"{other}\"; use bar, line, area, pie, table or stats"
            ));
        }
    };
    Ok(Visual {
        title: raw.title.filter(|t| !t.trim().is_empty()),
        subtitle: raw.subtitle.filter(|t| !t.trim().is_empty()),
        unit: raw.unit.filter(|u| !u.trim().is_empty()),
        body,
    })
}

fn checked_series(x: &[String], raw: Vec<RawSeries>) -> Result<Vec<Series>, String> {
    if x.is_empty() || x.len() > MAX_POINTS {
        return Err(format!("\"x\" needs 1 to {MAX_POINTS} labels"));
    }
    if raw.is_empty() || raw.len() > MAX_SERIES {
        return Err(format!("a chart takes 1 to {MAX_SERIES} series"));
    }
    raw.into_iter()
        .enumerate()
        .map(|(i, s)| {
            if s.values.len() != x.len() {
                return Err(format!(
                    "series {} has {} values, \"x\" has {}",
                    i + 1,
                    s.values.len(),
                    x.len()
                ));
            }
            Ok(Series {
                name: s.name.unwrap_or_else(|| format!("Series {}", i + 1)),
                values: s.values,
            })
        })
        .collect()
}

fn label(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A value as a tooltip shows it: up to two decimals, thousands grouped.
pub fn exact(v: f64, unit: Option<&str>) -> String {
    let rounded = (v * 100.).round() / 100.;
    let mut digits = format!("{:.2}", rounded.abs());
    while digits.ends_with('0') {
        digits.pop();
    }
    if digits.ends_with('.') {
        digits.pop();
    }
    let (int, frac) = digits.split_once('.').unwrap_or((&digits, ""));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if !frac.is_empty() {
        grouped = format!("{grouped}.{frac}");
    }
    let sign = if rounded < 0. { "-" } else { "" };
    with_unit(&format!("{sign}{grouped}"), unit)
}

/// A value as an axis tick shows it: 1.2k, 3.4M.
pub fn compact(v: f64, unit: Option<&str>) -> String {
    let a = v.abs();
    let text = match a {
        a if a >= 1e9 => format!("{:.1}B", v / 1e9),
        a if a >= 1e6 => format!("{:.1}M", v / 1e6),
        a if a >= 1e4 => format!("{:.1}k", v / 1e3),
        _ => return exact(v, unit),
    };
    with_unit(&text.replace(".0", ""), unit)
}

fn with_unit(number: &str, unit: Option<&str>) -> String {
    match unit.map(str::trim) {
        None | Some("") => number.to_string(),
        Some(u @ ("$" | "€" | "£" | "¥")) => match number.strip_prefix('-') {
            Some(n) => format!("-{u}{n}"),
            None => format!("{u}{number}"),
        },
        Some(u @ "%") => format!("{number}{u}"),
        Some(u) => format!("{number} {u}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_prose_visuals_and_other_fences() {
        let text = "Sales grew.\n\n```visual\n{\"type\":\"bar\",\"x\":[\"Q1\",2024],\"series\":[{\"values\":[1,2]}]}\n```\n\n```rust\n```visual\n```\nDone.";
        let parts = split(text, false);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], Segment::Text("Sales grew.\n\n".into()));
        let Segment::Visual(v) = &parts[1] else {
            panic!("{parts:?}")
        };
        assert_eq!(
            v.body,
            Body::Bar {
                x: vec!["Q1".into(), "2024".into()],
                series: Series {
                    name: "Series 1".into(),
                    values: vec![1., 2.]
                }
            }
        );
        // A visual fence inside another code block stays code.
        assert!(
            matches!(&parts[2], Segment::Text(t) if t.contains("```visual") && t.ends_with("Done."))
        );
    }

    #[test]
    fn open_block_waits_while_streaming_then_reports_errors() {
        let text = "Here:\n```visual\n{\"type\":\"line\",\"x\":[\"a\",\"b\"],\"series\":[{\"values\":[1]}]}";
        assert_eq!(split(text, true)[1], Segment::Pending);
        let Segment::Invalid { error, .. } = &split(text, false)[1] else {
            panic!()
        };
        assert_eq!(error, "series 1 has 1 values, \"x\" has 2");
    }

    #[test]
    fn formats_values_with_units() {
        assert_eq!(exact(1234567.891, Some("$")), "$1,234,567.89");
        assert_eq!(exact(-0.5, Some("%")), "-0.5%");
        assert_eq!(compact(12_000., Some("ms")), "12k ms");
        assert_eq!(compact(2_500_000., Some("€")), "€2.5M");
    }
}
