use std::collections::BTreeMap;

use serde_json::Value;

use crate::filters::{cardigann_regex, url_encode};

pub type Variables = BTreeMap<String, Value>;

pub fn render(input: &str, variables: &Variables) -> Result<String, String> {
    render_with_current(input, variables, None, false)
}

pub fn render_search_path(input: &str, variables: &Variables) -> Result<String, String> {
    render_with_current(input, variables, None, true)
}

fn render_with_current(
    input: &str,
    variables: &Variables,
    current: Option<&Value>,
    encode_expansions: bool,
) -> Result<String, String> {
    let mut output = String::new();
    let mut cursor = 0;
    while let Some(relative_start) = input[cursor..].find("{{") {
        let start = cursor + relative_start;
        output.push_str(&input[cursor..start]);
        let tag_end = input[start + 2..]
            .find("}}")
            .map(|offset| start + 2 + offset)
            .ok_or_else(|| "unterminated Cardigann template tag".to_string())?;
        let tag = input[start + 2..tag_end]
            .trim()
            .trim_start_matches('-')
            .trim_end_matches('-')
            .trim();
        let body_start = tag_end + 2;

        if let Some(condition) = tag.strip_prefix("if ") {
            let block = find_control_block(input, body_start)?;
            let selected = if evaluate(condition, variables, current)?.truthy() {
                &input[body_start..block.else_start.unwrap_or(block.end_start)]
            } else if block.else_start.is_some() {
                &input[block.else_end.expect("else end")..block.end_start]
            } else {
                ""
            };
            output.push_str(&render_with_current(
                selected,
                variables,
                current,
                encode_expansions,
            )?);
            cursor = block.end_end;
            continue;
        }

        if let Some(expression) = tag.strip_prefix("range ") {
            let block = find_control_block(input, body_start)?;
            let body_end = block.else_start.unwrap_or(block.end_start);
            let body = &input[body_start..body_end];
            let (bindings, collection) = expression
                .split_once(":=")
                .map(|(bindings, collection)| (Some(bindings.trim()), collection.trim()))
                .unwrap_or((None, expression));
            let value = evaluate(collection, variables, current)?;
            let mut rendered_any = false;
            if let Value::Array(values) = value.value {
                for (index, item) in values.iter().enumerate() {
                    rendered_any = true;
                    let mut scoped_variables = variables.clone();
                    if let Some(bindings) = bindings {
                        let mut bindings = bindings.split(',').map(str::trim);
                        if let Some(index_name) = bindings.next().filter(|name| !name.is_empty()) {
                            scoped_variables.insert(index_name.to_string(), Value::from(index));
                        }
                        if let Some(value_name) = bindings.next().filter(|name| !name.is_empty()) {
                            scoped_variables.insert(value_name.to_string(), item.clone());
                        }
                    }
                    output.push_str(&render_with_current(
                        body,
                        &scoped_variables,
                        Some(item),
                        encode_expansions,
                    )?);
                }
            }
            if !rendered_any && block.else_start.is_some() {
                output.push_str(&render_with_current(
                    &input[block.else_end.expect("else end")..block.end_start],
                    variables,
                    current,
                    encode_expansions,
                )?);
            }
            cursor = block.end_end;
            continue;
        }

        if tag == "else" || tag == "end" {
            return Err(format!("unexpected Cardigann template tag `{tag}`"));
        }

        let expansion = evaluate(tag, variables, current)?.as_string();
        if encode_expansions {
            output.push_str(&url_encode(&expansion).replace('+', "%20"));
        } else {
            output.push_str(&expansion);
        }
        cursor = body_start;
    }
    output.push_str(&input[cursor..]);
    Ok(output)
}

struct ControlBlock {
    else_start: Option<usize>,
    else_end: Option<usize>,
    end_start: usize,
    end_end: usize,
}

fn find_control_block(input: &str, mut cursor: usize) -> Result<ControlBlock, String> {
    let mut depth = 0usize;
    let mut else_start = None;
    let mut else_end = None;
    while let Some(relative_start) = input[cursor..].find("{{") {
        let start = cursor + relative_start;
        let tag_end = input[start + 2..]
            .find("}}")
            .map(|offset| start + 2 + offset)
            .ok_or_else(|| "unterminated Cardigann template control tag".to_string())?;
        let tag = input[start + 2..tag_end]
            .trim()
            .trim_start_matches('-')
            .trim_end_matches('-')
            .trim();
        if tag.starts_with("if ") || tag.starts_with("range ") {
            depth += 1;
        } else if tag == "end" {
            if depth == 0 {
                return Ok(ControlBlock {
                    else_start,
                    else_end,
                    end_start: start,
                    end_end: tag_end + 2,
                });
            }
            depth -= 1;
        } else if tag == "else" && depth == 0 {
            else_start = Some(start);
            else_end = Some(tag_end + 2);
        }
        cursor = tag_end + 2;
    }
    Err("Cardigann template control block is missing `end`".to_string())
}

#[derive(Debug, Clone)]
struct Evaluated {
    value: Value,
}

impl Evaluated {
    fn as_string(&self) -> String {
        match &self.value {
            Value::Null => String::new(),
            Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
            Value::Number(value) => value.to_string(),
            Value::String(value) => value.clone(),
            Value::Array(values) => values
                .iter()
                .map(value_as_string)
                .collect::<Vec<_>>()
                .join(""),
            Value::Object(_) => self.value.to_string(),
        }
    }

    fn truthy(&self) -> bool {
        match &self.value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
            Value::String(value) => !value.is_empty(),
            Value::Array(values) => !values.is_empty(),
            Value::Object(values) => !values.is_empty(),
        }
    }
}

fn value_as_string(value: &Value) -> String {
    Evaluated {
        value: value.clone(),
    }
    .as_string()
}

fn evaluate(
    expression: &str,
    variables: &Variables,
    current: Option<&Value>,
) -> Result<Evaluated, String> {
    let tokens = tokenize(expression)?;
    if tokens.is_empty() {
        return Ok(Evaluated { value: Value::Null });
    }
    let mut parser = ExpressionParser {
        tokens: &tokens,
        cursor: 0,
        variables,
        current,
    };
    let value = parser.parse_expression()?;
    if parser.cursor != tokens.len() {
        return Err(format!(
            "unexpected token `{}` in Cardigann template expression `{expression}`",
            tokens[parser.cursor]
        ));
    }
    Ok(Evaluated { value })
}

struct ExpressionParser<'a> {
    tokens: &'a [String],
    cursor: usize,
    variables: &'a Variables,
    current: Option<&'a Value>,
}

impl ExpressionParser<'_> {
    fn parse_expression(&mut self) -> Result<Value, String> {
        let token = self
            .tokens
            .get(self.cursor)
            .ok_or_else(|| "missing Cardigann template expression".to_string())?
            .clone();
        self.cursor += 1;
        if token == "(" {
            let value = self.parse_expression()?;
            self.expect(")")?;
            return Ok(value);
        }
        match token.as_str() {
            "eq" | "ne" => {
                let left = self.parse_expression()?;
                let right = self.parse_expression()?;
                let equal = value_as_string(&left) == value_as_string(&right);
                Ok(Value::Bool(if token == "eq" { equal } else { !equal }))
            }
            "and" | "or" => {
                let mut values = Vec::new();
                while self.cursor < self.tokens.len() && self.tokens[self.cursor] != ")" {
                    values.push(self.parse_expression()?);
                }
                if token == "and" {
                    let mut last = Value::Bool(true);
                    for value in values {
                        if !(Evaluated {
                            value: value.clone(),
                        })
                        .truthy()
                        {
                            return Ok(value);
                        }
                        last = value;
                    }
                    Ok(last)
                } else {
                    for value in values {
                        if (Evaluated {
                            value: value.clone(),
                        })
                        .truthy()
                        {
                            return Ok(value);
                        }
                    }
                    Ok(Value::Null)
                }
            }
            "join" => {
                let values = self.parse_expression()?;
                let separator = value_as_string(&self.parse_expression()?);
                let joined = match values {
                    Value::Array(values) => values
                        .iter()
                        .map(value_as_string)
                        .collect::<Vec<_>>()
                        .join(&separator),
                    value => value_as_string(&value),
                };
                Ok(Value::String(joined))
            }
            "re_replace" => {
                let input = value_as_string(&self.parse_expression()?);
                let pattern = value_as_string(&self.parse_expression()?);
                let replacement = value_as_string(&self.parse_expression()?);
                let regex = cardigann_regex(&pattern)
                    .map_err(|error| format!("invalid re_replace pattern `{pattern}`: {error}"))?;
                Ok(Value::String(
                    regex.replace_all(&input, replacement.as_str()).into_owned(),
                ))
            }
            ")" => Err("unexpected `)` in Cardigann template expression".to_string()),
            _ => Ok(self.resolve(&token)),
        }
    }

    fn resolve(&self, token: &str) -> Value {
        if token == "." {
            return self.current.cloned().unwrap_or(Value::Null);
        }
        if let Some(value) = self.variables.get(token) {
            return value.clone();
        }
        if token.starts_with('.') {
            return self.variables.get(token).cloned().unwrap_or(Value::Null);
        }
        if token.eq_ignore_ascii_case("true") {
            return Value::Bool(true);
        }
        if token.eq_ignore_ascii_case("false") {
            return Value::Bool(false);
        }
        Value::String(token.to_string())
    }

    fn expect(&mut self, expected: &str) -> Result<(), String> {
        match self.tokens.get(self.cursor).map(String::as_str) {
            Some(token) if token == expected => {
                self.cursor += 1;
                Ok(())
            }
            _ => Err(format!(
                "expected `{expected}` in Cardigann template expression"
            )),
        }
    }
}

fn tokenize(expression: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut chars = expression.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        if character == '(' || character == ')' {
            tokens.push(character.to_string());
            continue;
        }
        if character == '"' || character == '\'' {
            let quote = character;
            let mut token = String::new();
            let mut escaped = false;
            let mut closed = false;
            for next in chars.by_ref() {
                if escaped {
                    match next {
                        'n' => token.push('\n'),
                        'r' => token.push('\r'),
                        't' => token.push('\t'),
                        '\\' | '"' | '\'' => token.push(next),
                        other => {
                            token.push('\\');
                            token.push(other);
                        }
                    }
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == quote {
                    closed = true;
                    break;
                } else {
                    token.push(next);
                }
            }
            if !closed {
                return Err("unterminated quoted Cardigann template value".to_string());
            }
            tokens.push(token);
            continue;
        }
        let mut token = String::from(character);
        while let Some(next) = chars.peek() {
            if next.is_whitespace() || *next == '(' || *next == ')' {
                break;
            }
            token.push(chars.next().expect("peeked character"));
        }
        tokens.push(token);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables() -> Variables {
        BTreeMap::from([
            (
                ".Keywords".to_string(),
                Value::String("arch linux".to_string()),
            ),
            (
                ".Categories".to_string(),
                Value::Array(vec![Value::String("1".into()), Value::String("2".into())]),
            ),
            (".Config.freeleech".to_string(), Value::Bool(true)),
            (".False".to_string(), Value::Bool(false)),
        ])
    }

    #[test]
    fn renders_cardigann_if_range_join_and_regex_functions() {
        let vars = variables();
        assert_eq!(
            render(
                "{{ if .Categories }}{{ range .Categories }}{{.}};{{end}}{{ else }}0{{ end }}",
                &vars
            )
            .unwrap(),
            "1;2;"
        );
        assert_eq!(
            render("{{ join .Categories \",\" }}", &vars).unwrap(),
            "1,2"
        );
        assert_eq!(
            render("{{ re_replace .Keywords \"[\\s]+\" \"%\" }}", &vars).unwrap(),
            "arch%linux"
        );
    }

    #[test]
    fn renders_nested_boolean_conditions() {
        let vars = variables();
        assert_eq!(
            render(
                "{{ if and (.Config.freeleech) (eq .False .False) }}yes{{ else }}no{{ end }}",
                &vars
            )
            .unwrap(),
            "yes"
        );
    }

    #[test]
    fn treats_nonempty_template_strings_as_true_but_false_null_as_false() {
        let vars = BTreeMap::from([
            (".Empty".to_string(), Value::String(String::new())),
            (
                ".FalseString".to_string(),
                Value::String("false".to_string()),
            ),
            (".ZeroString".to_string(), Value::String("0".to_string())),
            (".Value".to_string(), Value::String("value".to_string())),
            (".False".to_string(), Value::Null),
        ]);
        for (expression, expected) in [
            (".Empty", "no"),
            (".FalseString", "yes"),
            (".ZeroString", "yes"),
            (".Value", "yes"),
            (".False", "no"),
        ] {
            assert_eq!(
                render(
                    &format!("{{{{ if {expression} }}}}yes{{{{ else }}}}no{{{{ end }}}}"),
                    &vars
                )
                .unwrap(),
                expected,
                "{expression}"
            );
        }
    }

    #[test]
    fn search_path_rendering_url_encodes_expansions_not_literal_path_text() {
        let vars = BTreeMap::from([
            (
                ".Keywords".to_string(),
                Value::String("space slash/value".to_string()),
            ),
            (
                ".Categories".to_string(),
                Value::Array(vec![
                    Value::String("one two".into()),
                    Value::String("x/y".into()),
                ]),
            ),
        ]);
        assert_eq!(
            render_search_path(
                "search/{{ .Keywords }}/{{ join .Categories \",\" }}/{{ re_replace .Keywords \" \" \"-\" }}",
                &vars,
            )
            .unwrap(),
            "search/space%20slash%2Fvalue/one%20two%2Cx%2Fy/space-slash%2Fvalue"
        );
        assert_eq!(
            render("search/{{ .Keywords }}", &vars).unwrap(),
            "search/space slash/value"
        );
    }

    #[test]
    fn renders_range_index_and_value_bindings() {
        let vars = variables();
        assert_eq!(
            render(
                "{{ range $index, $element := .Categories }}{{$index}}={{$element}};{{end}}",
                &vars,
            )
            .unwrap(),
            "0=1;1=2;"
        );
    }
}
