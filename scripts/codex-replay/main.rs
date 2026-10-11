use codex_protocol::models::ResponseItem;
use std::{env, error::Error, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let paths: Vec<_> = env::args().skip(1).collect();
    if paths.is_empty() {
        return Err("no fixtures".into());
    }
    let mut total = 0;
    for path in paths {
        let fixture: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        let items = fixture
            .get("items")
            .and_then(serde_json::Value::as_array)
            .ok_or("missing items array")?;
        if items.is_empty() {
            return Err("empty items array".into());
        }
        for (index, value) in items.iter().enumerate() {
            let item: ResponseItem = serde_json::from_value(value.clone())?;
            if matches!(item, ResponseItem::Other) {
                return Err("unknown item mapped to Other".into());
            }
            let parsed_type = match item {
                ResponseItem::WebSearchCall { .. } => "web_search_call",
                ResponseItem::Message { .. } => "message",
                ResponseItem::FunctionCall { .. } => "function_call",
                ResponseItem::FunctionCallOutput { .. } => "function_call_output",
                _ => return Err("unexpected replay variant".into()),
            };
            if value.get("type").and_then(serde_json::Value::as_str) != Some(parsed_type) {
                return Err("response variant mismatch".into());
            }
            println!("ACCEPTED {path} item={index} type={parsed_type}");
            total += 1;
        }
    }
    println!("ALL_FIXTURES_ACCEPTED items={total}");
    Ok(())
}
