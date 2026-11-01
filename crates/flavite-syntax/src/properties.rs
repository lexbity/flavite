//! Conservative scalar patches. Unsupported YAML remains source-editable.
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ScalarValue {
    Text(String),
    Bool(bool),
    Number(String),
    Null,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarProperty {
    pub key: String,
    pub value: ScalarValue,
    pub range: Range<usize>,
    pub quote: Option<char>,
}
/// Only flat, unambiguous scalar YAML is editable. The full canonical parser
/// still supplies inspection for unsupported lists/maps/anchors.
pub fn inspect(text: &str) -> Result<Vec<ScalarProperty>, String> {
    if !text.starts_with("---\n") {
        return Err("No supported frontmatter; edit source".into());
    }
    let mut offset = 4;
    let mut properties = Vec::new();
    for line in text[4..].split_inclusive('\n') {
        let raw = line.trim_end_matches('\n');
        if matches!(raw, "---" | "...") {
            return Ok(properties);
        }
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            offset += line.len();
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            return Err("Nested YAML requires source editing".into());
        }
        let (key, rest) = raw
            .split_once(':')
            .ok_or("Malformed frontmatter; edit source")?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Complex YAML keys require source editing".into());
        }
        if properties.iter().any(|p: &ScalarProperty| p.key == key) {
            return Err("Duplicate key; edit source".into());
        }
        let leading = rest.len() - rest.trim_start().len();
        let value_start = offset + key.len() + 1 + leading;
        let rest = rest.trim_start();
        let (value, len, quote) = scalar(rest)?;
        let trailing = rest[len..].trim_start();
        if !trailing.is_empty() && !trailing.starts_with('#') {
            return Err("Unsupported YAML value; edit source".into());
        }
        properties.push(ScalarProperty {
            key: key.into(),
            value,
            range: value_start..value_start + len,
            quote,
        });
        offset += line.len();
    }
    Err("Unclosed frontmatter; edit source".into())
}
fn scalar(raw: &str) -> Result<(ScalarValue, usize, Option<char>), String> {
    if let Some(stripped) = raw.strip_prefix('\'') {
        let mut value = String::new();
        let mut chars = stripped.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            if c == '\'' {
                if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    chars.next();
                    value.push('\'');
                } else {
                    return Ok((ScalarValue::Text(value), at + 2, Some('\'')));
                }
            } else {
                value.push(c);
            }
        }
        return Err("Unclosed YAML quote; edit source".into());
    }
    if let Some(stripped) = raw.strip_prefix('"') {
        let mut escaped = false;
        for (at, c) in stripped.char_indices() {
            if c == '"' && !escaped {
                let len = at + 2;
                let value: String = serde_json::from_str(&raw[..len])
                    .map_err(|_| "Unsupported YAML escape; edit source")?;
                return Ok((ScalarValue::Text(value), len, Some('"')));
            }
            if c == '\\' {
                escaped = !escaped;
            } else {
                escaped = false;
            }
        }
        return Err("Unclosed YAML quote; edit source".into());
    }
    let end = raw
        .char_indices()
        .find(|(at, c)| *c == '#' && (*at == 0 || raw[..*at].ends_with(char::is_whitespace)))
        .map_or(raw.len(), |(at, _)| at);
    let value = raw[..end].trim_end();
    if value.contains([
        ':', '[', ']', '{', '}', '&', '*', '!', '|', '>', '\'', '"', '`',
    ]) || value.starts_with(['-', '?', '%', '@'])
    {
        return Err("Complex YAML requires source editing".into());
    }
    let typed = match value {
        "true" | "True" | "TRUE" => ScalarValue::Bool(true),
        "false" | "False" | "FALSE" => ScalarValue::Bool(false),
        "null" | "Null" | "NULL" | "~" | "" => ScalarValue::Null,
        s if s.parse::<f64>().is_ok_and(f64::is_finite) => ScalarValue::Number(s.into()),
        s => ScalarValue::Text(s.into()),
    };
    Ok((typed, value.len(), None))
}
/// Shared supported-scalar reader for the canonical frontmatter extractor.
pub(crate) fn simple_scalar(raw: &str) -> Option<ScalarValue> {
    let (value, length, _) = scalar(raw).ok()?;
    let rest = raw[length..].trim_start();
    (rest.is_empty() || rest.starts_with('#')).then_some(value)
}

pub fn patch(text: &str, key: &str, value: &ScalarValue) -> Result<(Range<usize>, String), String> {
    if key.len() > 8192 {
        return Err("Property key exceeds limit".into());
    }
    let props = inspect(text)?;
    let p = props
        .iter()
        .find(|p| p.key == key)
        .ok_or("Missing property; edit source")?;
    let mut rendered = match value {
        ScalarValue::Text(s) => {
            if s.len() > 8192 || s.contains(['\0', '\n', '\r']) {
                return Err("Invalid property text".into());
            }
            if p.quote == Some('\'') {
                format!("'{}'", s.replace('\'', "''"))
            } else {
                serde_json::to_string(s).map_err(|_| "Invalid property text")?
            }
        }
        ScalarValue::Bool(b) => b.to_string(),
        ScalarValue::Number(n)
            if n.len() <= 128
                && n.parse::<f64>().is_ok_and(f64::is_finite)
                && n.bytes()
                    .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) =>
        {
            n.clone()
        }
        ScalarValue::Null => "null".into(),
        ScalarValue::Number(_) => return Err("Invalid finite number".into()),
    };
    if matches!(key, "title" | "aliases" | "tags" | "tag" | "id")
        && !matches!(value, ScalarValue::Text(_))
    {
        return Err("This property requires text; edit lists in source".into());
    }
    if p.range.is_empty() && text[p.range.end..].starts_with('#') {
        rendered.push(' ');
    }
    Ok((p.range.clone(), rendered))
}
