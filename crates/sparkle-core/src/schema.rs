//! The concise schema DSL: `name, age int, bio: a short bio`.

use serde_json::{Map, Value, json};

use crate::{Error, Result};

/// Converts the DSL to JSON schema. Fields split on newlines when any are present, else commas.
pub fn parse_dsl(input: &str) -> Result<Value> {
    let separator = if input.trim().contains('\n') { '\n' } else { ',' };
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in input.split(separator).map(str::trim).filter(|field| !field.is_empty()) {
        let (declaration, description) = match field.split_once(':') {
            Some((declaration, description)) => (declaration.trim(), Some(description.trim())),
            None => (field, None),
        };
        let mut words = declaration.split_whitespace();
        let name = words
            .next()
            .ok_or_else(|| Error::Config(format!("schema field has no name: '{field}'")))?;
        let kind = match words.next().unwrap_or("str") {
            "str" => "string",
            "int" => "integer",
            "float" => "number",
            "bool" => "boolean",
            other => {
                return Err(Error::Config(format!(
                    "unknown schema type '{other}', use str, int, float or bool"
                )));
            }
        };
        let mut property = json!({ "type": kind });
        if let Some(description) = description {
            property["description"] = json!(description);
        }
        properties.insert(name.to_string(), property);
        required.push(json!(name));
    }
    Ok(json!({ "type": "object", "properties": properties, "required": required }))
}

/// Accepts inline JSON schema, a path to a JSON file, or the DSL.
pub fn resolve(input: &str) -> Result<Value> {
    if input.trim_start().starts_with('{') {
        return Ok(serde_json::from_str(input)?);
    }
    let path = std::path::Path::new(input);
    if path.is_file() {
        return Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?);
    }
    parse_dsl(input)
}

/// Wraps a schema so the model returns many objects under an `items` key.
pub fn multi(schema: Value) -> Value {
    json!({
        "type": "object",
        "properties": { "items": { "type": "array", "items": schema } },
        "required": ["items"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python_llm_output() {
        assert_eq!(
            parse_dsl("name, age int, bio: their bio").unwrap(),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "age": { "type": "integer" },
                    "bio": { "type": "string", "description": "their bio" }
                },
                "required": ["name", "age", "bio"]
            })
        );
    }

    #[test]
    fn newlines_allow_commas_in_descriptions() {
        let schema = parse_dsl("name: first, last\nok bool").unwrap();
        assert_eq!(schema["properties"]["name"]["description"], "first, last");
        assert_eq!(schema["properties"]["ok"]["type"], "boolean");
    }

    #[test]
    fn rejects_unknown_types() {
        assert!(parse_dsl("age integer").is_err());
    }
}
