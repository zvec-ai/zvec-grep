use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum JsoncEditError {
    #[error("invalid JSONC configuration")]
    Invalid,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
struct Node {
    start: usize,
    end: usize,
    kind: Kind,
}

#[derive(Clone, Debug)]
enum Kind {
    Object(Vec<Property>),
    Array(Vec<Element>),
    Scalar,
}

#[derive(Clone, Debug)]
struct Property {
    key: String,
    key_start: usize,
    value: Node,
    comma_after: Option<usize>,
}

#[derive(Clone, Debug)]
struct Element {
    value: Node,
    comma_after: Option<usize>,
}

struct Parser<'a> {
    source: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn parse(source: &'a str) -> Result<Node, JsoncEditError> {
        let mut parser = Self {
            source,
            position: 0,
        };
        parser.skip_trivia()?;
        let node = parser.parse_value()?;
        parser.skip_trivia()?;
        if parser.position != source.len() {
            return Err(JsoncEditError::Invalid);
        }
        Ok(node)
    }

    fn parse_value(&mut self) -> Result<Node, JsoncEditError> {
        self.skip_trivia()?;
        let start = self.position;
        match self.byte() {
            Some(b'{') => self.parse_object(start),
            Some(b'[') => self.parse_array(start),
            Some(b'"') => {
                self.parse_string()?;
                Ok(Node {
                    start,
                    end: self.position,
                    kind: Kind::Scalar,
                })
            }
            Some(_) => {
                while let Some(byte) = self.byte() {
                    if byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}') {
                        break;
                    }
                    self.position += 1;
                }
                if self.position == start {
                    return Err(JsoncEditError::Invalid);
                }
                Ok(Node {
                    start,
                    end: self.position,
                    kind: Kind::Scalar,
                })
            }
            None => Err(JsoncEditError::Invalid),
        }
    }

    fn parse_object(&mut self, start: usize) -> Result<Node, JsoncEditError> {
        self.position += 1;
        let mut properties = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.byte() == Some(b'}') {
                self.position += 1;
                return Ok(Node {
                    start,
                    end: self.position,
                    kind: Kind::Object(properties),
                });
            }
            let key_start = self.position;
            if self.byte() != Some(b'"') {
                return Err(JsoncEditError::Invalid);
            }
            let key_end = self.parse_string()?;
            let key: String = serde_json::from_str(&self.source[key_start..key_end])?;
            self.skip_trivia()?;
            if self.byte() != Some(b':') {
                return Err(JsoncEditError::Invalid);
            }
            self.position += 1;
            let value = self.parse_value()?;
            self.skip_trivia()?;
            let comma_after = if self.byte() == Some(b',') {
                let comma = self.position;
                self.position += 1;
                Some(comma)
            } else {
                None
            };
            properties.push(Property {
                key,
                key_start,
                value,
                comma_after,
            });
            self.skip_trivia()?;
            if comma_after.is_some() && self.byte() == Some(b'}') {
                return Err(JsoncEditError::Invalid);
            }
            if comma_after.is_none() && self.byte() != Some(b'}') {
                return Err(JsoncEditError::Invalid);
            }
        }
    }

    fn parse_array(&mut self, start: usize) -> Result<Node, JsoncEditError> {
        self.position += 1;
        let mut elements = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.byte() == Some(b']') {
                self.position += 1;
                return Ok(Node {
                    start,
                    end: self.position,
                    kind: Kind::Array(elements),
                });
            }
            let value = self.parse_value()?;
            self.skip_trivia()?;
            let comma_after = if self.byte() == Some(b',') {
                let comma = self.position;
                self.position += 1;
                Some(comma)
            } else {
                None
            };
            elements.push(Element { value, comma_after });
            self.skip_trivia()?;
            if comma_after.is_some() && self.byte() == Some(b']') {
                return Err(JsoncEditError::Invalid);
            }
            if comma_after.is_none() && self.byte() != Some(b']') {
                return Err(JsoncEditError::Invalid);
            }
        }
    }

    fn parse_string(&mut self) -> Result<usize, JsoncEditError> {
        if self.byte() != Some(b'"') {
            return Err(JsoncEditError::Invalid);
        }
        self.position += 1;
        let mut escaped = false;
        while let Some(byte) = self.byte() {
            self.position += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return Ok(self.position);
            }
        }
        Err(JsoncEditError::Invalid)
    }

    fn skip_trivia(&mut self) -> Result<(), JsoncEditError> {
        loop {
            while self.byte().is_some_and(|byte| byte.is_ascii_whitespace()) {
                self.position += 1;
            }
            if self.byte() == Some(b'/') && self.byte_at(1) == Some(b'/') {
                self.position += 2;
                while self
                    .byte()
                    .is_some_and(|byte| !matches!(byte, b'\n' | b'\r'))
                {
                    self.position += 1;
                }
                continue;
            }
            if self.byte() == Some(b'/') && self.byte_at(1) == Some(b'*') {
                self.position += 2;
                while !(self.byte() == Some(b'*') && self.byte_at(1) == Some(b'/')) {
                    if self.byte().is_none() {
                        return Err(JsoncEditError::Invalid);
                    }
                    self.position += 1;
                }
                self.position += 2;
                continue;
            }
            return Ok(());
        }
    }

    fn byte(&self) -> Option<u8> {
        self.source.as_bytes().get(self.position).copied()
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        self.source.as_bytes().get(self.position + offset).copied()
    }
}

pub(crate) fn set_path(
    source: &str,
    path: &[&str],
    value: &Value,
) -> Result<String, JsoncEditError> {
    if path.is_empty() {
        return format_value(value, "", &indent_unit(source));
    }
    let root = Parser::parse(source)?;
    let mut object = &root;
    for (index, segment) in path.iter().enumerate() {
        let Kind::Object(properties) = &object.kind else {
            return Err(JsoncEditError::Invalid);
        };
        if let Some(property) = properties.iter().find(|property| property.key == *segment) {
            if index + 1 == path.len() {
                let indentation = line_indentation(source, property.key_start);
                let replacement = format_value(value, &indentation, &indent_unit(source))?;
                return Ok(splice(
                    source,
                    property.value.start,
                    property.value.end,
                    &replacement,
                ));
            }
            object = &property.value;
            continue;
        }
        let nested = path[index + 1..].iter().rev().fold(
            value.clone(),
            |current, key| serde_json::json!({(*key): current}),
        );
        return insert_property(source, object, segment, &nested);
    }
    Err(JsoncEditError::Invalid)
}

pub(crate) fn remove_path(source: &str, path: &[&str]) -> Result<String, JsoncEditError> {
    if path.is_empty() {
        return Ok(source.to_owned());
    }
    let root = Parser::parse(source)?;
    let mut object = &root;
    for (index, segment) in path.iter().enumerate() {
        let Kind::Object(properties) = &object.kind else {
            return Ok(source.to_owned());
        };
        let Some(property_index) = properties
            .iter()
            .position(|property| property.key == *segment)
        else {
            return Ok(source.to_owned());
        };
        let property = &properties[property_index];
        if index + 1 != path.len() {
            object = &property.value;
            continue;
        }
        let mut ranges = vec![(property.key_start, property.value.end)];
        if let Some(comma) = property.comma_after {
            ranges.push((comma, comma + 1));
        } else if let Some(comma) = property_index
            .checked_sub(1)
            .and_then(|previous| properties[previous].comma_after)
        {
            ranges.push((comma, comma + 1));
        }
        ranges.sort_by_key(|(start, _)| std::cmp::Reverse(*start));
        let mut next = source.to_owned();
        for (start, end) in ranges {
            next.replace_range(start..end, "");
        }
        return Ok(next);
    }
    Ok(source.to_owned())
}

pub(crate) fn insert_array_strings_at_start(
    source: &str,
    path: &[&str],
    values: &[&str],
) -> Result<String, JsoncEditError> {
    if values.is_empty() {
        return Ok(source.to_owned());
    }
    let root = Parser::parse(source)?;
    let Some(array) = find_path(&root, path) else {
        return Err(JsoncEditError::Invalid);
    };
    let Kind::Array(elements) = &array.kind else {
        return Err(JsoncEditError::Invalid);
    };
    let unit = indent_unit(source);
    let property_indent = line_indentation(source, array.start);
    let item_indent = format!("{property_indent}{unit}");
    let multiline = source[array.start..array.end].contains('\n');
    let serialized = values
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    let insertion = if multiline {
        format!(
            "\n{}{}{}",
            item_indent,
            serialized.join(&format!(",\n{item_indent}")),
            if elements.is_empty() { "" } else { "," }
        )
    } else {
        format!(
            "{}{}",
            serialized.join(", "),
            if elements.is_empty() { "" } else { ", " }
        )
    };
    let offset = array.start + 1;
    Ok(splice(source, offset, offset, &insertion))
}

pub(crate) fn remove_first_array_string(
    source: &str,
    path: &[&str],
    value: &str,
) -> Result<(String, bool), JsoncEditError> {
    let root = Parser::parse(source)?;
    let Some(array) = find_path(&root, path) else {
        return Ok((source.to_owned(), false));
    };
    let Kind::Array(elements) = &array.kind else {
        return Ok((source.to_owned(), false));
    };
    let Some(index) = elements.iter().position(|element| {
        serde_json::from_str::<Value>(&source[element.value.start..element.value.end])
            .ok()
            .and_then(|parsed| parsed.as_str().map(str::to_owned))
            .as_deref()
            == Some(value)
    }) else {
        return Ok((source.to_owned(), false));
    };
    let element = &elements[index];
    let mut ranges = vec![(element.value.start, element.value.end)];
    if let Some(comma) = element.comma_after {
        ranges.push((comma, comma + 1));
    } else if let Some(comma) = index
        .checked_sub(1)
        .and_then(|previous| elements[previous].comma_after)
    {
        ranges.push((comma, comma + 1));
    }
    ranges.sort_by_key(|(start, _)| std::cmp::Reverse(*start));
    let mut next = source.to_owned();
    for (start, end) in ranges {
        next.replace_range(start..end, "");
    }
    Ok((next, true))
}

fn find_path<'a>(root: &'a Node, path: &[&str]) -> Option<&'a Node> {
    let mut current = root;
    for segment in path {
        let Kind::Object(properties) = &current.kind else {
            return None;
        };
        current = &properties
            .iter()
            .find(|property| property.key == *segment)?
            .value;
    }
    Some(current)
}

fn insert_property(
    source: &str,
    object: &Node,
    key: &str,
    value: &Value,
) -> Result<String, JsoncEditError> {
    let Kind::Object(properties) = &object.kind else {
        return Err(JsoncEditError::Invalid);
    };
    let unit = indent_unit(source);
    let parent_indent = line_indentation(source, object.start);
    let child_indent = format!("{parent_indent}{unit}");
    let serialized = format_value(value, &child_indent, &unit)?;
    let property = format!("{}: {serialized}", serde_json::to_string(key)?);
    let close = object.end.saturating_sub(1);
    let begins = if source[..close].ends_with('\n') || source[..close].ends_with('\r') {
        child_indent.clone()
    } else {
        format!("\n{child_indent}")
    };
    let insertion = format!("{begins}{property}\n{parent_indent}");
    let mut next = source.to_owned();
    if let Some(last) = properties.last() {
        next.insert(last.value.end, ',');
        next.insert_str(close + usize::from(last.value.end <= close), &insertion);
    } else {
        next.insert_str(close, &insertion);
    }
    Ok(next)
}

fn splice(source: &str, start: usize, end: usize, replacement: &str) -> String {
    let mut next = String::with_capacity(source.len() + replacement.len());
    next.push_str(&source[..start]);
    next.push_str(replacement);
    next.push_str(&source[end..]);
    next
}

fn line_indentation(source: &str, offset: usize) -> String {
    let line_start = source[..offset]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    source[line_start..offset]
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .collect()
}

fn indent_unit(source: &str) -> String {
    source
        .lines()
        .filter_map(|line| {
            let indentation = line
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .collect::<String>();
            (line[indentation.len()..].starts_with('"') && !indentation.is_empty())
                .then_some(indentation)
        })
        .min_by_key(String::len)
        .unwrap_or_else(|| "  ".to_owned())
}

fn format_value(value: &Value, base_indent: &str, unit: &str) -> Result<String, JsoncEditError> {
    let pretty = serde_json::to_string_pretty(value)?;
    let mut lines = pretty.lines();
    let Some(first) = lines.next() else {
        return Ok(pretty);
    };
    let mut formatted = first.to_owned();
    for line in lines {
        let spaces = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        formatted.push('\n');
        formatted.push_str(base_indent);
        formatted.push_str(&unit.repeat(spaces / 2));
        formatted.push_str(&line[spaces..]);
    }
    Ok(formatted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edits_nested_property_without_losing_comments() {
        let source = "{\n  // theme\n  \"theme\": \"dark\",\n  /* servers */\n  \"mcpServers\": {\"other\": {\"url\": \"x\"}}\n}\n";
        let installed = set_path(
            source,
            &["mcpServers", "zvec_grep"],
            &json!({"command":"zg"}),
        )
        .expect("edit");
        assert!(installed.contains("// theme"));
        assert!(installed.contains("/* servers */"));
        assert!(installed.contains("\"zvec_grep\""));
        let removed = remove_path(&installed, &["mcpServers", "zvec_grep"]).expect("remove");
        assert!(removed.contains("\"other\""));
        assert!(!removed.contains("\"zvec_grep\""));
    }

    #[test]
    fn rejects_trailing_commas() {
        assert!(Parser::parse("{\"a\":1,}").is_err());
        assert!(Parser::parse("[1,]").is_err());
    }
}
