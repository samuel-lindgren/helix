use helix_dap as dap;
use helix_lsp::block_on;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodedBytesKind {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedBytes {
    pub inline: String,
    pub pretty: String,
    pub kind: DecodedBytesKind,
}

impl DecodedBytes {
    pub fn label(&self) -> &'static str {
        match self.kind {
            DecodedBytesKind::Text => "text",
            DecodedBytesKind::Json => "json",
        }
    }
}

pub(crate) fn decode_byte_collection(variables: &[dap::Variable]) -> Option<DecodedBytes> {
    let mut indexed = Vec::new();
    let mut string_value = None;

    for variable in variables {
        if let Some(index) = parse_index_name(&variable.name) {
            let ty = variable.ty.as_deref()?;
            if !is_byte_like_type(ty) {
                return None;
            }
            indexed.push((index, parse_byte_value(&variable.value)?));
            continue;
        }

        if let Some(value) = parse_string_view(variable) {
            string_value = Some(value);
            continue;
        }

        if is_ignored_collection_child(&variable.name) {
            continue;
        }

        return None;
    }

    if let Some(value) = string_value {
        if let Some(decoded) = decode_text(&value) {
            return Some(decoded);
        }
    }

    if indexed.is_empty() {
        return None;
    }

    indexed.sort_by_key(|(index, _)| *index);
    let bytes = indexed
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    decode_bytes(&bytes)
}

pub(crate) fn load_byte_collection(
    debugger: &dap::Client,
    variables_reference: usize,
    indexed_variables: Option<usize>,
) -> Option<DecodedBytes> {
    let variables = if let Some(total) = indexed_variables {
        let mut variables = Vec::with_capacity(total.max(64));
        let page_size = 64;
        let mut start = 0usize;
        while start < total {
            let count = (total - start).min(page_size);
            let page = block_on(debugger.variables_with_options(
                variables_reference,
                Some("indexed".to_string()),
                Some(start),
                Some(count),
            ))
            .ok()?;
            let indexed_count = page
                .iter()
                .filter(|variable| parse_index_name(&variable.name).is_some())
                .count();
            if indexed_count == 0 {
                break;
            }
            variables.extend(page);
            start += indexed_count;
        }

        let loaded = variables
            .iter()
            .filter(|variable| parse_index_name(&variable.name).is_some())
            .count();
        if loaded < total {
            return None;
        }

        variables
    } else {
        block_on(debugger.variables(variables_reference)).ok()?
    };

    decode_byte_collection(&variables)
}

fn decode_bytes(bytes: &[u8]) -> Option<DecodedBytes> {
    let text = std::str::from_utf8(bytes).ok()?;
    decode_text(text)
}

fn decode_text(text: &str) -> Option<DecodedBytes> {
    if !is_probably_text(text) {
        return None;
    }

    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(DecodedBytes {
            inline: serde_json::to_string(&value).ok()?,
            pretty: serde_json::to_string_pretty(&value).ok()?,
            kind: DecodedBytesKind::Json,
        });
    }

    Some(DecodedBytes {
        inline: text.to_string(),
        pretty: text.to_string(),
        kind: DecodedBytesKind::Text,
    })
}

fn parse_string_view(variable: &dap::Variable) -> Option<String> {
    if variable.name.trim() != "string()" || variable.ty.as_deref()?.trim() != "string" {
        return None;
    }

    serde_json::from_str::<String>(variable.value.trim()).ok()
}

fn parse_index_name(name: &str) -> Option<usize> {
    let trimmed = name.trim();
    let trimmed = trimmed
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
        .unwrap_or(trimmed);
    trimmed.parse::<usize>().ok()
}

fn is_byte_like_type(ty: &str) -> bool {
    matches!(ty.trim(), "uint8" | "byte")
}

fn is_ignored_collection_child(name: &str) -> bool {
    matches!(name.trim(), "len" | "cap")
}

fn parse_byte_value(value: &str) -> Option<u8> {
    let trimmed = value.trim();
    let token = trimmed
        .split_once('=')
        .map(|(head, _)| head.trim())
        .unwrap_or(trimmed)
        .split_whitespace()
        .next()
        .unwrap_or(trimmed);

    if let Some(hex) = token.strip_prefix("0x") {
        u8::from_str_radix(hex, 16).ok()
    } else {
        token.parse::<u8>().ok()
    }
}

fn is_probably_text(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn byte_variable(index: usize, value: &str) -> dap::Variable {
        dap::Variable {
            name: format!("[{index}]"),
            value: value.to_string(),
            ty: Some("uint8".to_string()),
            presentation_hint: None,
            evaluate_name: None,
            variables_reference: 0,
            named_variables: None,
            indexed_variables: None,
            memory_reference: None,
        }
    }

    fn string_view_variable(value: &str) -> dap::Variable {
        dap::Variable {
            name: "string()".to_string(),
            value: value.to_string(),
            ty: Some("string".to_string()),
            presentation_hint: None,
            evaluate_name: None,
            variables_reference: 0,
            named_variables: None,
            indexed_variables: None,
            memory_reference: None,
        }
    }

    #[test]
    fn decodes_utf8_byte_collections_as_text() {
        let variables = vec![
            byte_variable(0, "104"),
            byte_variable(1, "101"),
            byte_variable(2, "108"),
            byte_variable(3, "108"),
            byte_variable(4, "111"),
        ];

        assert_eq!(
            decode_byte_collection(&variables),
            Some(DecodedBytes {
                inline: "hello".to_string(),
                pretty: "hello".to_string(),
                kind: DecodedBytesKind::Text,
            })
        );
    }

    #[test]
    fn pretty_prints_json_byte_collections() {
        let variables = br#"{"ok":true}"#
            .iter()
            .enumerate()
            .map(|(index, byte)| byte_variable(index, &byte.to_string()))
            .collect::<Vec<_>>();

        let rendered = decode_byte_collection(&variables).unwrap();
        assert_eq!(rendered.kind, DecodedBytesKind::Json);
        assert_eq!(rendered.inline, r#"{"ok":true}"#);
        assert_eq!(rendered.pretty, "{\n  \"ok\": true\n}");
    }

    #[test]
    fn decodes_string_view_json_collections() {
        let variables = vec![
            string_view_variable(r#""{\"ok\":true}""#),
            byte_variable(0, "123"),
            byte_variable(1, "34"),
        ];

        let rendered = decode_byte_collection(&variables).unwrap();
        assert_eq!(rendered.kind, DecodedBytesKind::Json);
        assert_eq!(rendered.inline, r#"{"ok":true}"#);
        assert_eq!(rendered.pretty, "{\n  \"ok\": true\n}");
    }

    #[test]
    fn rejects_non_byte_collections() {
        let variables = vec![dap::Variable {
            name: "[0]".to_string(),
            value: "42".to_string(),
            ty: Some("int".to_string()),
            presentation_hint: None,
            evaluate_name: None,
            variables_reference: 0,
            named_variables: None,
            indexed_variables: None,
            memory_reference: None,
        }];

        assert_eq!(decode_byte_collection(&variables), None);
    }

    #[test]
    fn parses_byte_values_with_hex_suffixes() {
        let variables = vec![
            byte_variable(0, "123 = 0x7b"),
            byte_variable(1, "125 = 0x7d"),
        ];

        let rendered = decode_byte_collection(&variables).unwrap();
        assert_eq!(rendered.kind, DecodedBytesKind::Json);
        assert_eq!(rendered.inline, "{}");
        assert_eq!(rendered.pretty, "{}");
    }
}
