use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use encoding_rs::Encoding;
use percent_encoding::percent_decode_str;
use regex::Regex;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
use url::Url;

use crate::definition::{FilterBlock, scalar_to_string};
use crate::template::{Variables, render};

#[cfg(test)]
pub fn apply_filters(
    input: &str,
    filters: &[FilterBlock],
    variables: &Variables,
) -> Result<String, String> {
    apply_filters_with_encoding(input, filters, variables, encoding_rs::UTF_8)
}

pub fn apply_filters_with_encoding(
    input: &str,
    filters: &[FilterBlock],
    variables: &Variables,
    encoding: &'static Encoding,
) -> Result<String, String> {
    let mut value = input.to_string();
    for filter in filters {
        let args = rendered_args(filter, variables)?;
        value = match filter.name.as_str() {
            "append" => value + args.first().map(String::as_str).unwrap_or_default(),
            "prepend" => format!("{}{}", args.first().cloned().unwrap_or_default(), value),
            "trim" => trim(&value, &args),
            "tolower" => value.to_lowercase(),
            "toupper" => value.to_uppercase(),
            "replace" => value.replace(
                args.first().map(String::as_str).unwrap_or_default(),
                args.get(1).map(String::as_str).unwrap_or_default(),
            ),
            "re_replace" => regex_replace(&value, &args)?,
            "regexp" => regexp_extract(&value, &args)?,
            "split" => split(&value, &args),
            "querystring" => {
                query_string(&value, args.first().map(String::as_str).unwrap_or_default())
            }
            "urldecode" => url_decode(&value, encoding),
            "urlencode" => url_encode_with_encoding(&value, encoding),
            "htmldecode" => html_escape::decode_html_entities(&value).into_owned(),
            "htmlencode" => html_encode(&value),
            "dateparse" | "timeparse" => {
                parse_date(&value, args.first().map(String::as_str)).unwrap_or(value)
            }
            "timeago" | "reltime" => parse_relative_time(&value)?,
            "fuzzytime" => parse_fuzzy_time(&value)?,
            "diacritics" => replace_diacritics(&value, args.first().map(String::as_str))?,
            "validfilename" => valid_filename(&value),
            "jsonjoinarray" => json_join_array(&value, &args)?,
            "validate" => validate(&value, &args)?,
            // Cardigann uses these filters only for diagnostics; neither transforms data.
            "hexdump" | "strdump" => value,
            other => return Err(format!("unsupported Cardigann filter `{other}`")),
        };
    }
    Ok(value)
}

fn rendered_args(filter: &FilterBlock, variables: &Variables) -> Result<Vec<String>, String> {
    let values = match filter.args.as_ref() {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(serde_yaml::Value::Sequence(values)) => values
            .iter()
            .filter_map(scalar_to_string)
            .collect::<Vec<_>>(),
        Some(value) => scalar_to_string(value).into_iter().collect(),
    };
    values
        .into_iter()
        .map(|value| render(&value, variables))
        .collect()
}

fn regex_replace(value: &str, args: &[String]) -> Result<String, String> {
    let pattern = args.first().map(String::as_str).unwrap_or_default();
    let replacement = args.get(1).map(String::as_str).unwrap_or_default();
    cardigann_regex(pattern)
        .map_err(|error| format!("invalid re_replace pattern `{pattern}`: {error}"))
        .map(|regex| regex.replace_all(value, replacement).into_owned())
}

fn regexp_extract(value: &str, args: &[String]) -> Result<String, String> {
    let pattern = args.first().map(String::as_str).unwrap_or_default();
    let regex = cardigann_regex(pattern)
        .map_err(|error| format!("invalid regexp pattern `{pattern}`: {error}"))?;
    let captures = regex
        .captures(value)
        .ok_or_else(|| format!("regexp `{pattern}` did not match `{value}`"))?;
    let group = args
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| usize::from(captures.len() > 1));
    captures
        .get(group)
        .map(|capture| capture.as_str().to_string())
        .ok_or_else(|| format!("regexp `{pattern}` has no capture group {group}"))
}

pub(crate) fn cardigann_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let pattern = pattern
        .replace(r"\p{IsCJKUnifiedIdeographs}", r"\p{Han}")
        .replace(r"\P{IsCJKUnifiedIdeographs}", r"\P{Han}");
    Regex::new(&pattern)
}

fn split(value: &str, args: &[String]) -> String {
    let separator = args
        .first()
        .and_then(|separator| separator.chars().next())
        .unwrap_or(' ');
    let index = args
        .get(1)
        .and_then(|value| value.parse::<isize>().ok())
        .unwrap_or(0);
    let parts = value.split(separator).collect::<Vec<_>>();
    let index = if index < 0 {
        parts.len() as isize + index
    } else {
        index
    };
    parts
        .get(index.max(0) as usize)
        .copied()
        .unwrap_or_default()
        .to_string()
}

fn trim(value: &str, args: &[String]) -> String {
    match args.first().and_then(|cutset| cutset.chars().next()) {
        Some(cutset) => value.trim_matches(cutset).to_string(),
        None => value.trim().to_string(),
    }
}

fn url_decode(value: &str, encoding: &'static Encoding) -> String {
    let bytes = percent_decode_str(&value.replace('+', " ")).collect::<Vec<_>>();
    let (decoded, _, _) = encoding.decode(&bytes);
    decoded.into_owned()
}

pub(crate) fn url_encode_with_encoding(value: &str, encoding: &'static Encoding) -> String {
    let (bytes, _, _) = encoding.encode(value);
    let mut encoded = String::with_capacity(value.len());
    for &byte in bytes.as_ref() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'*'
            | b'('
            | b')' => encoded.push(char::from(byte)),
            b' ' => encoded.push('+'),
            byte => {
                use std::fmt::Write;
                write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    encoded
}

pub(crate) fn url_encode(value: &str) -> String {
    url_encode_with_encoding(value, encoding_rs::UTF_8)
}

fn html_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => encoded.push_str("&amp;"),
            '<' => encoded.push_str("&lt;"),
            '>' => encoded.push_str("&gt;"),
            '"' => encoded.push_str("&quot;"),
            '\'' => encoded.push_str("&#39;"),
            character => encoded.push(character),
        }
    }
    encoded
}

fn query_string(value: &str, key: &str) -> String {
    let parsed =
        Url::parse(value).or_else(|_| Url::parse(&format!("https://cardigann.invalid/?{value}")));
    parsed
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.into_owned())
        })
        .unwrap_or_default()
}

fn parse_date(value: &str, pattern: Option<&str>) -> Result<String, String> {
    parse_date_at(value, pattern, now_utc())
}

fn parse_date_at(value: &str, pattern: Option<&str>, now: DateTime<Utc>) -> Result<String, String> {
    if value.eq_ignore_ascii_case("now") {
        return Ok(now.to_rfc3339());
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc).to_rfc3339());
    }
    if let Ok(timestamp) = value.trim().parse::<i64>()
        && let Some(parsed) = Utc.timestamp_opt(timestamp, 0).single()
    {
        return Ok(parsed.to_rfc3339());
    }
    let Some(pattern) = pattern else {
        return Ok(value.to_string());
    };
    let chrono_pattern = dotnet_to_chrono_pattern(pattern);
    if let Ok(parsed) = DateTime::parse_from_str(value.trim(), &chrono_pattern) {
        return Ok(parsed.with_timezone(&Utc).to_rfc3339());
    }
    if let Ok(parsed) = NaiveDateTime::parse_from_str(value.trim(), &chrono_pattern) {
        return Ok(Utc.from_utc_datetime(&parsed).to_rfc3339());
    }
    if let Ok(parsed) = NaiveDate::parse_from_str(value.trim(), &chrono_pattern)
        && let Some(parsed) = parsed.and_hms_opt(0, 0, 0)
    {
        return Ok(Utc.from_utc_datetime(&parsed).to_rfc3339());
    }
    if let Some(parsed) = parse_partial_cardigann_date(value.trim(), &chrono_pattern, now) {
        return Ok(parsed.to_rfc3339());
    }
    Err(format!("could not parse date `{value}` using `{pattern}`"))
}

fn parse_partial_cardigann_date(
    value: &str,
    chrono_pattern: &str,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match chrono_pattern {
        "%m.%d" => NaiveDate::parse_from_str(&format!("{}.{value}", now.format("%Y")), "%Y.%m.%d")
            .ok()?
            .and_hms_opt(0, 0, 0)
            .map(|date| Utc.from_utc_datetime(&date)),
        "%m/%d %H:%M" | "%m/%d %H:%M:%S" => NaiveDateTime::parse_from_str(
            &format!("{}/{}", now.format("%Y"), value),
            &format!("%Y/{chrono_pattern}"),
        )
        .ok()
        .map(|date| Utc.from_utc_datetime(&date)),
        "%H:%M" | "%H:%M:%S" => NaiveDateTime::parse_from_str(
            &format!("{} {value}", now.format("%Y-%m-%d")),
            &format!("%Y-%m-%d {chrono_pattern}"),
        )
        .ok()
        .map(|date| Utc.from_utc_datetime(&date)),
        "%H:%M %:z" | "%H:%M:%S %:z" => DateTime::parse_from_str(
            &format!("{} {value}", now.format("%Y-%m-%d")),
            &format!("%Y-%m-%d {chrono_pattern}"),
        )
        .ok()
        .map(|date| date.with_timezone(&Utc)),
        _ => None,
    }
}

fn parse_fuzzy_time(value: &str) -> Result<String, String> {
    parse_relative_time(value)
        .or_else(|_| parse_unknown_datetime(value).map(|date| date.to_rfc3339()))
}

fn parse_unknown_datetime(value: &str) -> Result<DateTime<Utc>, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("now") {
        return Ok(now_utc());
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(parsed) = DateTime::parse_from_rfc2822(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(timestamp) = value.parse::<i64>()
        && let Some(parsed) = Utc.timestamp_opt(timestamp, 0).single()
    {
        return Ok(parsed);
    }
    for pattern in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%d/%m/%Y %H:%M",
        "%m/%d/%Y %H:%M",
        "%b %e, %Y %H:%M",
        "%B %e, %Y %H:%M",
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(value, pattern) {
            return Ok(Utc.from_utc_datetime(&parsed));
        }
    }
    for pattern in ["%Y-%m-%d", "%d/%m/%Y", "%m/%d/%Y", "%b %e, %Y", "%B %e, %Y"] {
        if let Ok(parsed) = NaiveDate::parse_from_str(value, pattern)
            && let Some(parsed) = parsed.and_hms_opt(0, 0, 0)
        {
            return Ok(Utc.from_utc_datetime(&parsed));
        }
    }
    Err(format!("could not parse fuzzy time `{value}`"))
}

pub(crate) fn normalize_unknown_date(value: &str) -> String {
    parse_unknown_datetime(value)
        .map(|date| date.to_rfc3339())
        .unwrap_or_else(|_| value.trim().to_string())
}

fn dotnet_to_chrono_pattern(pattern: &str) -> String {
    let replacements = [
        ("dddd", "%A"),
        ("ddd", "%a"),
        ("yyyy", "%Y"),
        ("yyy", "%Y"),
        ("yy", "%y"),
        ("MMMM", "%B"),
        ("MMM", "%b"),
        ("MM", "%m"),
        ("M", "%m"),
        ("dd", "%d"),
        ("d", "%d"),
        ("HH", "%H"),
        ("H", "%H"),
        ("hh", "%I"),
        ("h", "%I"),
        ("mm", "%M"),
        ("m", "%M"),
        ("ss", "%S"),
        ("s", "%S"),
        ("fff", "%.3f"),
        ("zzz", "%:z"),
        ("zz", "%z"),
        ("z", "%z"),
        ("tt", "%p"),
    ];
    let mut output = String::with_capacity(pattern.len());
    let mut cursor = 0;
    while cursor < pattern.len() {
        let rest = &pattern[cursor..];
        if let Some((token, replacement)) = replacements
            .iter()
            .find(|(token, _)| rest.starts_with(*token))
        {
            output.push_str(replacement);
            cursor += token.len();
        } else {
            let character = rest.chars().next().expect("cursor is in bounds");
            output.push(character);
            cursor += character.len_utf8();
        }
    }
    output
}

fn parse_relative_time(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_lowercase();
    if normalized == "now" || normalized == "just now" {
        return Ok(now_utc().to_rfc3339());
    }
    let regex = Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(second|minute|hour|day|week|month|year)s?")
        .expect("static relative time regex");
    let mut seconds = 0f64;
    for captures in regex.captures_iter(&normalized) {
        let amount = captures[1].parse::<f64>().unwrap_or_default();
        let factor = match &captures[2].to_ascii_lowercase()[..] {
            "second" => 1.0,
            "minute" => 60.0,
            "hour" => 3_600.0,
            "day" => 86_400.0,
            "week" => 604_800.0,
            "month" => 2_629_746.0,
            "year" => 31_556_952.0,
            _ => 0.0,
        };
        seconds += amount * factor;
    }
    if seconds == 0.0 {
        return Err(format!("could not parse relative time `{value}`"));
    }
    Ok((now_utc() - Duration::seconds(seconds.round() as i64)).to_rfc3339())
}

fn now_utc() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

fn valid_filename(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            other => other,
        })
        .collect()
}

fn replace_diacritics(value: &str, operation: Option<&str>) -> Result<String, String> {
    if operation != Some("replace") {
        return Err("diacritics filter requires args: replace".to_string());
    }
    Ok(value
        .nfd()
        .filter(|character| !is_combining_mark(*character))
        .collect())
}

fn json_join_array(value: &str, args: &[String]) -> Result<String, String> {
    let path = args
        .first()
        .ok_or_else(|| "jsonjoinarray requires a JSONPath argument".to_string())?;
    let separator = args
        .get(1)
        .ok_or_else(|| "jsonjoinarray requires a separator argument".to_string())?;
    let document: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| format!("jsonjoinarray input is not JSON: {error}"))?;
    let values = select_json_path(&document, path)?;
    let values = if values.len() == 1 {
        match values[0] {
            serde_json::Value::Array(values) => values.iter().collect(),
            _ => values,
        }
    } else {
        values
    };
    Ok(values
        .into_iter()
        .map(json_value_as_string)
        .collect::<Vec<_>>()
        .join(separator))
}

fn select_json_path<'a>(
    document: &'a serde_json::Value,
    path: &str,
) -> Result<Vec<&'a serde_json::Value>, String> {
    let mut remaining = path
        .strip_prefix('$')
        .ok_or_else(|| format!("unsupported jsonjoinarray JSONPath `{path}`: expected `$`"))?;
    let mut values = vec![document];
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix('.') {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            let key = &rest[..end];
            if key.is_empty() {
                return Err(format!("unsupported jsonjoinarray JSONPath `{path}`"));
            }
            values = values
                .into_iter()
                .filter_map(|value| value.get(key))
                .collect();
            remaining = &rest[end..];
        } else if let Some(rest) = remaining.strip_prefix("[*]") {
            values = values
                .into_iter()
                .flat_map(|value| value.as_array().into_iter().flatten())
                .collect();
            remaining = rest;
        } else if let Some(rest) = remaining.strip_prefix('[') {
            let end = rest.find(']').ok_or_else(|| {
                format!("unsupported jsonjoinarray JSONPath `{path}`: missing `]`")
            })?;
            let index = rest[..end].parse::<usize>().map_err(|_| {
                format!("unsupported jsonjoinarray JSONPath `{path}`: expected an array index")
            })?;
            values = values
                .into_iter()
                .filter_map(|value| value.get(index))
                .collect();
            remaining = &rest[end + 1..];
        } else {
            return Err(format!("unsupported jsonjoinarray JSONPath `{path}`"));
        }
    }
    Ok(values)
}

fn json_value_as_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn validate(value: &str, args: &[String]) -> Result<String, String> {
    const DELIMITERS: &[char] = &[',', ' ', '/', ')', '(', '.', ';', '[', ']', '"', '|', ':'];
    let allowed_source = args
        .first()
        .map(|value| value.to_lowercase())
        .unwrap_or_default();
    let allowed = allowed_source
        .split(DELIMITERS)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let candidates_source = value.to_lowercase();
    let candidates = candidates_source
        .split(DELIMITERS)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    Ok(allowed
        .into_iter()
        .filter(|candidate| candidates.contains(candidate))
        .collect::<Vec<_>>()
        .join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_common_cardigann_filter_chain() {
        let filters = vec![
            FilterBlock {
                name: "regexp".into(),
                args: Some(serde_yaml::Value::String("size=(.+)".into())),
            },
            FilterBlock {
                name: "replace".into(),
                args: Some(serde_yaml::from_str("[' GB', ' GiB']").unwrap()),
            },
            FilterBlock {
                name: "append".into(),
                args: Some(serde_yaml::Value::String(" verified".into())),
            },
        ];
        assert_eq!(
            apply_filters("size=2 GB", &filters, &Variables::new()).unwrap(),
            "2 GiB verified"
        );
    }

    #[test]
    fn implements_cardigann_jsonjoinarray_validate_and_debug_filters() {
        let filters = vec![FilterBlock {
            name: "jsonjoinarray".into(),
            args: Some(serde_yaml::from_str("['$.items[*].name', ' / ']").unwrap()),
        }];
        assert_eq!(
            apply_filters(
                r#"{"items":[{"name":"one"},{"name":"two"}]}"#,
                &filters,
                &Variables::new(),
            )
            .unwrap(),
            "one / two"
        );

        let filters = vec![FilterBlock {
            name: "validate".into(),
            args: Some(serde_yaml::Value::String("free, trusted, internal".into())),
        }];
        assert_eq!(
            apply_filters("internal | unknown | free", &filters, &Variables::new()).unwrap(),
            "free, internal"
        );

        let filters = vec![FilterBlock {
            name: "hexdump".into(),
            args: None,
        }];
        assert_eq!(
            apply_filters("a\n", &filters, &Variables::new()).unwrap(),
            "a\n"
        );
    }

    #[test]
    fn supports_single_digit_cardigann_date_tokens_and_requires_diacritics_replace() {
        let filters = vec![FilterBlock {
            name: "dateparse".into(),
            args: Some(serde_yaml::Value::String("MMM d yyyy h:mm tt".into())),
        }];
        assert!(
            apply_filters("Jan 2 2025 3:04 PM", &filters, &Variables::new())
                .unwrap()
                .starts_with("2025-01-02T15:04:00")
        );

        let filters = vec![FilterBlock {
            name: "diacritics".into(),
            args: Some(serde_yaml::Value::String("replace".into())),
        }];
        assert_eq!(
            apply_filters("Crème", &filters, &Variables::new()).unwrap(),
            "Creme"
        );
        let filters = vec![FilterBlock {
            name: "diacritics".into(),
            args: None,
        }];
        assert!(apply_filters("Crème", &filters, &Variables::new()).is_err());
    }

    #[test]
    fn matches_prowlarr_date_split_trim_and_encoding_edge_cases() {
        let dateparse = vec![FilterBlock {
            name: "dateparse".into(),
            args: Some(serde_yaml::Value::String("yyyy-MM-dd".into())),
        }];
        assert_eq!(
            apply_filters("not a date", &dateparse, &Variables::new()).unwrap(),
            "not a date"
        );

        let timeparse = vec![FilterBlock {
            name: "timeparse".into(),
            args: Some(serde_yaml::Value::String("HH:mm".into())),
        }];
        assert_eq!(
            apply_filters("not a time", &timeparse, &Variables::new()).unwrap(),
            "not a time"
        );

        let fuzzytime = vec![FilterBlock {
            name: "fuzzytime".into(),
            args: None,
        }];
        assert!(
            apply_filters("2025-01-02 03:04:05", &fuzzytime, &Variables::new())
                .unwrap()
                .starts_with("2025-01-02T03:04:05")
        );

        let split = vec![FilterBlock {
            name: "split".into(),
            args: Some(serde_yaml::from_str("['::', '1']").unwrap()),
        }];
        assert_eq!(
            apply_filters("a::b", &split, &Variables::new()).unwrap(),
            ""
        );

        let trim = vec![FilterBlock {
            name: "trim".into(),
            args: Some(serde_yaml::Value::String("xy".into())),
        }];
        assert_eq!(
            apply_filters("xyhelloyx", &trim, &Variables::new()).unwrap(),
            "yhelloy"
        );

        let urlencode = vec![FilterBlock {
            name: "urlencode".into(),
            args: None,
        }];
        assert_eq!(
            apply_filters("a b/+", &urlencode, &Variables::new()).unwrap(),
            "a+b%2F%2B"
        );
        assert_eq!(
            apply_filters_with_encoding(
                "café",
                &urlencode,
                &Variables::new(),
                encoding_rs::WINDOWS_1252,
            )
            .unwrap(),
            "caf%E9"
        );
        let urldecode = vec![FilterBlock {
            name: "urldecode".into(),
            args: None,
        }];
        assert_eq!(
            apply_filters_with_encoding(
                "caf%E9",
                &urldecode,
                &Variables::new(),
                encoding_rs::WINDOWS_1252,
            )
            .unwrap(),
            "café"
        );
        let htmlencode = vec![FilterBlock {
            name: "htmlencode".into(),
            args: None,
        }];
        assert_eq!(
            apply_filters("<a 'x'>&", &htmlencode, &Variables::new()).unwrap(),
            "&lt;a &#39;x&#39;&gt;&amp;"
        );
    }

    #[test]
    fn supplies_current_date_context_for_partial_cardigann_layouts() {
        let now = Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap();
        assert_eq!(
            parse_date_at("01.02", Some("MM.dd"), now).unwrap(),
            "2025-01-02T00:00:00+00:00"
        );
        assert_eq!(
            parse_date_at("01/02 03:04", Some("MM/dd HH:mm"), now).unwrap(),
            "2025-01-02T03:04:00+00:00"
        );
        assert_eq!(
            parse_date_at("03:04 +02:00", Some("HH:mm zzz"), now).unwrap(),
            "2025-06-15T01:04:00+00:00"
        );
    }
}
