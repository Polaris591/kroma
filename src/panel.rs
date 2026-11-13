//! Structured content a plugin may put in its own panel. The host renders it;
//! a plugin never draws cells, owns input or writes terminal escapes.
//!
//! A plugin may return plain text (unchanged behavior) or JSON: either an array
//! of blocks or `{"blocks":[...]}`. Unknown block types are ignored rather than
//! failing the panel, and every field is bounded so a huge payload cannot stall
//! a redraw.
use serde_json::{json, Value};

const MAX_BLOCKS: usize = 200;
const MAX_ROWS: usize = 200;
const MAX_COLUMNS: usize = 12;
const MAX_TEXT: usize = 2000;

#[derive(Debug, Clone, PartialEq)]
pub enum PanelBlock {
    Heading(String),
    Text(String),
    List(Vec<String>),
    KeyValue(Vec<(String, String)>),
    Progress {
        label: String,
        value: f64,
        max: f64,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Divider,
}

fn clip(value: &str) -> String {
    value.chars().take(MAX_TEXT).collect()
}

/// Plain text stays one text block, so every existing plugin keeps working.
pub fn parse(content: &str) -> Vec<PanelBlock> {
    let trimmed = content.trim();
    let parsed = if trimmed.starts_with('[') || trimmed.starts_with('{') {
        serde_json::from_str::<Value>(trimmed).ok()
    } else {
        None
    };
    let items = match &parsed {
        Some(Value::Array(items)) => Some(items.clone()),
        Some(value) => value.get("blocks").and_then(Value::as_array).cloned(),
        None => None,
    };
    match items {
        Some(items) => blocks_of(&items),
        None => vec![PanelBlock::Text(content.trim_end().to_owned())],
    }
}

fn blocks_of(items: &[Value]) -> Vec<PanelBlock> {
    items.iter().take(MAX_BLOCKS).filter_map(block).collect()
}

fn block(value: &Value) -> Option<PanelBlock> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(clip)
            .unwrap_or_default()
    };
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .take(MAX_COLUMNS * MAX_ROWS)
                    .filter_map(Value::as_str)
                    .map(clip)
                    .collect()
            })
            .unwrap_or_default()
    };
    match value.get("type").and_then(Value::as_str)? {
        "heading" => Some(PanelBlock::Heading(text("text"))),
        "text" => Some(PanelBlock::Text(text("text"))),
        "list" => Some(PanelBlock::List(strings("items"))),
        "keyvalue" => Some(PanelBlock::KeyValue(
            value
                .get("rows")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .take(MAX_ROWS)
                        .map(|row| {
                            let get = |key: &str| {
                                row.get(key)
                                    .and_then(Value::as_str)
                                    .map(clip)
                                    .unwrap_or_default()
                            };
                            (get("key"), get("value"))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )),
        "progress" => Some(PanelBlock::Progress {
            label: text("label"),
            value: value.get("value").and_then(Value::as_f64).unwrap_or(0.0),
            max: value.get("max").and_then(Value::as_f64).unwrap_or(1.0),
        }),
        "table" => Some(PanelBlock::Table {
            headers: strings("headers"),
            rows: table_rows(value),
        }),
        "divider" => Some(PanelBlock::Divider),
        _ => None,
    }
}

fn table_rows(value: &Value) -> Vec<Vec<String>> {
    value
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .take(MAX_ROWS)
                .map(|row| {
                    row.as_array()
                        .map(|cells| {
                            cells
                                .iter()
                                .take(MAX_COLUMNS)
                                .map(|cell| match cell {
                                    Value::String(text) => clip(text),
                                    other => clip(&other.to_string()),
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The same content for a frontend that has real widgets. The terminal uses
/// [`render_text`], so one schema serves both without a second protocol.
pub fn to_json(blocks: &[PanelBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|block| match block {
            PanelBlock::Heading(text) => json!({"type":"heading","text":text}),
            PanelBlock::Text(text) => json!({"type":"text","text":text}),
            PanelBlock::List(items) => json!({"type":"list","items":items}),
            PanelBlock::KeyValue(rows) => json!({
                "type":"keyvalue",
                "rows": rows.iter().map(|(key, value)| json!({"key":key,"value":value})).collect::<Vec<_>>()
            }),
            PanelBlock::Progress { label, value, max } => {
                json!({"type":"progress","label":label,"value":value,"max":max})
            }
            PanelBlock::Table { headers, rows } => {
                json!({"type":"table","headers":headers,"rows":rows})
            }
            PanelBlock::Divider => json!({"type":"divider"}),
        })
        .collect()
}

fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width_of(text) <= width {
        return text.to_owned();
    }
    let limit = width.saturating_sub(1);
    let mut used = 0;
    let mut truncated = String::new();
    for character in text.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + char_width > limit {
            break;
        }
        used += char_width;
        truncated.push(character);
    }
    truncated.push('…');
    truncated
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        for word in line.split_whitespace() {
            let projected = width_of(&row) + usize::from(!row.is_empty()) + width_of(word);
            if projected > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
        }
        rows.push(row);
    }
    rows
}

fn bar(value: f64, max: f64, cells: usize) -> String {
    let max = if max > 0.0 { max } else { 1.0 };
    let ratio = (value / max).clamp(0.0, 1.0);
    let filled = (ratio * cells as f64).round() as usize;
    format!(
        "[{}{}]",
        "█".repeat(filled.min(cells)),
        "░".repeat(cells.saturating_sub(filled))
    )
}

/// Terminal rendering: the same blocks as aligned plain text.
pub fn render_text(blocks: &[PanelBlock], width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    for block in blocks {
        match block {
            PanelBlock::Heading(text) => {
                if !text.trim().is_empty() {
                    lines.extend(wrap_words(&format!("■ {text}"), width));
                    lines.push(String::new());
                }
            }
            PanelBlock::Text(text) => lines.extend(wrap_words(text, width)),
            PanelBlock::List(items) => {
                for item in items {
                    for (index, row) in wrap_words(&format!("• {item}"), width)
                        .into_iter()
                        .enumerate()
                    {
                        if index == 0 {
                            lines.push(row);
                        } else {
                            lines.push(format!("  {row}"));
                        }
                    }
                }
            }
            PanelBlock::KeyValue(rows) => {
                let key_width = rows
                    .iter()
                    .map(|(key, _)| width_of(key))
                    .max()
                    .unwrap_or(0)
                    .min(width / 2);
                for (key, value) in rows {
                    lines.push(format!(
                        "{}  {value}",
                        pad(&truncate(key, key_width), key_width)
                    ));
                }
            }
            PanelBlock::Progress { label, value, max } => {
                let percent = if *max > 0.0 {
                    (value / max * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                };
                lines.push(
                    format!("{} {percent:.0}%  {label}", bar(*value, *max, 16))
                        .trim_end()
                        .to_owned(),
                );
            }
            PanelBlock::Table { headers, rows } => {
                lines.extend(table_lines(headers, rows, width));
            }
            PanelBlock::Divider => lines.push("─".repeat(width)),
        }
    }
    lines
}

fn pad(text: &str, width: usize) -> String {
    let mut padded = text.to_owned();
    for _ in width_of(text)..width {
        padded.push(' ');
    }
    padded
}

fn table_lines(headers: &[String], rows: &[Vec<String>], width: usize) -> Vec<String> {
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .min(MAX_COLUMNS);
    if columns == 0 {
        return Vec::new();
    }
    let mut widths = vec![3usize; columns];
    for (index, header) in headers.iter().take(columns).enumerate() {
        widths[index] = widths[index].max(width_of(header));
    }
    for row in rows {
        for (index, cell) in row.iter().take(columns).enumerate() {
            widths[index] = widths[index].max(width_of(cell));
        }
    }
    let separators = columns.saturating_sub(1) * 3;
    while widths.iter().sum::<usize>() + separators > width {
        let widest = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, value)| **value)
            .map(|(index, _)| index)
            .unwrap_or(0);
        if widths[widest] <= 5 {
            break;
        }
        widths[widest] -= 1;
    }
    let mut lines = Vec::new();
    let join = |cells: &[String]| {
        (0..columns)
            .map(|index| {
                let cell = cells.get(index).map(String::as_str).unwrap_or("");
                pad(&truncate(cell, widths[index]), widths[index])
            })
            .collect::<Vec<_>>()
            .join(" │ ")
    };
    if !headers.is_empty() {
        lines.push(join(headers));
        lines.push(
            widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("─┼─"),
        );
    }
    for row in rows {
        lines.push(join(row));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_stays_one_block() {
        assert_eq!(
            parse("hello\nworld"),
            vec![PanelBlock::Text("hello\nworld".into())]
        );
    }

    #[test]
    fn json_blocks_are_parsed_and_unknown_types_are_ignored() {
        let blocks = parse(
            r#"{"blocks":[
                {"type":"heading","text":"状态"},
                {"type":"list","items":["a","b"]},
                {"type":"progress","label":"测试","value":3,"max":10},
                {"type":"unknown","text":"ignored"},
                {"type":"divider"}
            ]}"#,
        );
        assert_eq!(
            blocks,
            vec![
                PanelBlock::Heading("状态".into()),
                PanelBlock::List(vec!["a".into(), "b".into()]),
                PanelBlock::Progress {
                    label: "测试".into(),
                    value: 3.0,
                    max: 10.0
                },
                PanelBlock::Divider,
            ]
        );
    }

    #[test]
    fn terminal_rendering_keeps_tables_and_progress_readable() {
        let text = render_text(
            &[
                PanelBlock::Progress {
                    label: "测试".into(),
                    value: 3.0,
                    max: 10.0,
                },
                PanelBlock::Table {
                    headers: vec!["名称".into(), "结果".into()],
                    rows: vec![vec!["unit".into(), "ok".into()]],
                },
            ],
            40,
        )
        .join("\n");
        assert!(text.contains("30%"), "{text}");
        assert!(text.contains('█') && text.contains('░'), "{text}");
        assert!(text.contains("名称"), "{text}");
        assert!(text.contains("unit"), "{text}");
    }

    #[test]
    fn round_trip_to_json_matches_the_wire_shape() {
        let blocks = parse(r#"[{"type":"keyvalue","rows":[{"key":"a","value":"1"}]}]"#);
        assert_eq!(
            to_json(&blocks),
            vec![json!({"type":"keyvalue","rows":[{"key":"a","value":"1"}]})]
        );
    }
}
