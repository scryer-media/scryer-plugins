use std::sync::OnceLock;

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};
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
        .map(|regex| {
            regex
                .replace_all(value, cardigann_replacement(replacement).as_str())
                .into_owned()
        })
}

fn regexp_extract(value: &str, args: &[String]) -> Result<String, String> {
    let pattern = args.first().map(String::as_str).unwrap_or_default();
    let regex = cardigann_regex(pattern)
        .map_err(|error| format!("invalid regexp pattern `{pattern}`: {error}"))?;
    // Prowlarr reads `Groups[1].Value` of the match, which is the empty string
    // when nothing matched. Erroring here instead dropped whole rows through
    // the required-field path.
    let Some(captures) = regex
        .captures(value)
        .map_err(|error| format!("regexp `{pattern}` failed on `{value}`: {error}"))?
    else {
        return Ok(String::new());
    };
    let group = args
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| usize::from(captures.len() > 1));
    Ok(captures
        .get(group)
        .map(|capture| capture.as_str().to_string())
        .unwrap_or_default())
}

/// Compile a Cardigann pattern with the .NET dialect the corpus is written in.
///
/// `fancy-regex` supplies the lookaround, atomic groups and backreferences 38
/// definitions rely on and the plain `regex` crate rejects; the rewrite below
/// covers .NET's Unicode *block* names, which have no Rust equivalent but map
/// onto the script of the same name for every block the corpus names.
pub(crate) fn cardigann_regex(pattern: &str) -> Result<fancy_regex::Regex, fancy_regex::Error> {
    fancy_regex::Regex::new(&rewrite_unicode_blocks(pattern))
}

fn rewrite_unicode_blocks(pattern: &str) -> String {
    // `\p{IsCJKUnifiedIdeographs}` is the one block whose Rust spelling is not
    // simply the block name, so it keeps its own mapping.
    let pattern = pattern
        .replace(r"\p{IsCJKUnifiedIdeographs}", r"\p{Han}")
        .replace(r"\P{IsCJKUnifiedIdeographs}", r"\P{Han}");
    let blocks = Regex::new(r"\\([pP])\{Is(\w+)\}").expect("static Unicode block regex");
    blocks.replace_all(&pattern, r"\${1}{${2}}").into_owned()
}

/// Rewrite a .NET replacement string into the Rust dialect.
///
/// .NET reads `$1x` as group 1 followed by a literal `x`; Rust (and
/// `fancy-regex`) read it as the group named `1x`, which is always empty. 38
/// definitions write the .NET form, so brace the group number whenever a word
/// character follows it. `${1}`, `$$` and `$name` are already unambiguous and
/// are left alone.
pub(crate) fn cardigann_replacement(replacement: &str) -> String {
    let mut output = String::with_capacity(replacement.len());
    let mut characters = replacement.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '$' {
            output.push(character);
            continue;
        }
        match characters.peek() {
            Some('$') | Some('{') | None => {
                output.push('$');
                if let Some(next) = characters.next() {
                    output.push(next);
                }
            }
            Some(next) if next.is_ascii_digit() => {
                let mut digits = String::new();
                while characters.peek().is_some_and(char::is_ascii_digit) {
                    digits.push(characters.next().expect("peeked digit"));
                }
                let braced = characters
                    .peek()
                    .is_some_and(|next| next.is_alphanumeric() || *next == '_');
                if braced {
                    output.push_str(&format!("${{{digits}}}"));
                } else {
                    output.push('$');
                    output.push_str(&digits);
                }
            }
            Some(_) => output.push('$'),
        }
    }
    output
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
    parse_fuzzy_time_at(value, now_utc())
}

fn parse_fuzzy_time_at(value: &str, now: DateTime<Utc>) -> Result<String, String> {
    parse_relative_time_at(value, now)
        .or_else(|_| parse_unknown_datetime_at(value, now).map(|date| date.to_rfc3339()))
}

fn unknown_date_pattern(slot: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    slot.get_or_init(|| Regex::new(pattern).expect("static unknown-date regex"))
}

/// The named day and its offset in days from today, if this value opens with
/// one, together with the text that names it.
fn relative_day(value: &str) -> Option<(&str, i64)> {
    const NAMED_DAYS: [(&str, i64); 3] = [("today", 0), ("yesterday", -1), ("tomorrow", 1)];
    static PATTERNS: [OnceLock<Regex>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    NAMED_DAYS
        .iter()
        .zip(PATTERNS.iter())
        .find_map(|((name, offset), slot)| {
            let pattern =
                unknown_date_pattern(slot, &format!(r"(?i)\b{name}(?:[\s,]+(?:at)?\s*|[\s,]*|$)"));
            pattern
                .find(value)
                .map(|matched| (matched.as_str(), *offset))
        })
}

/// A bare time of day, the way `DateTimeUtil.ParseTimeSpan` reads the remainder
/// of a `Today …` or `<weekday> at …` value. No time at all is midnight.
fn parse_time_of_day(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(Duration::zero());
    }
    for pattern in ["%H:%M:%S", "%H:%M", "%I:%M:%S %p", "%I:%M %p"] {
        if let Ok(time) = NaiveTime::parse_from_str(value, pattern) {
            return Ok(time.signed_duration_since(NaiveTime::MIN));
        }
    }
    Err(format!("could not parse time of day `{value}`"))
}

/// Prowlarr's `DateTimeUtil.FromUnknown`, for the forms it resolves itself
/// before handing the rest to its fuzzy parser.
///
/// The order is Prowlarr's: RFC 1123, a bare unix timestamp, anything naming
/// `now`, anything naming `ago`, the named days, `<weekday> at <time>`, then the
/// two shapes that omit the year, and only then the fixed layouts.
fn parse_unknown_datetime_at(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(parsed) = DateTime::parse_from_rfc2822(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if !value.is_empty()
        && value.chars().all(|character| character.is_ascii_digit())
        && let Ok(timestamp) = value.parse::<i64>()
        && let Some(parsed) = Utc.timestamp_opt(timestamp, 0).single()
    {
        return Ok(parsed);
    }
    let lowered = value.to_lowercase();
    if lowered.contains("now") {
        return Ok(now);
    }
    static AGO: OnceLock<Regex> = OnceLock::new();
    if unknown_date_pattern(&AGO, r"(?i)\bago").is_match(value) {
        return relative_time_at(value, now);
    }
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| "the current date has no midnight".to_string())?;
    if let Some((matched, offset)) = relative_day(value) {
        let time = value.replace(matched, "");
        let parsed = midnight + parse_time_of_day(&time)? + Duration::days(offset);
        return Ok(Utc.from_utc_datetime(&parsed));
    }
    static WEEKDAY: OnceLock<Regex> = OnceLock::new();
    let weekday = unknown_date_pattern(
        &WEEKDAY,
        r"(?i)\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\s+at\s+",
    );
    if let Some(captures) = weekday.captures(value) {
        let matched = captures.get(0).expect("group 0 always matches").as_str();
        let named = captures
            .get(1)
            .expect("weekday group")
            .as_str()
            .to_lowercase();
        let time = value.replace(matched, "");
        let mut parsed = midnight + parse_time_of_day(&time)?;
        let target = match named.as_str() {
            "monday" => Weekday::Mon,
            "tuesday" => Weekday::Tue,
            "wednesday" => Weekday::Wed,
            "thursday" => Weekday::Thu,
            "friday" => Weekday::Fri,
            "saturday" => Weekday::Sat,
            _ => Weekday::Sun,
        };
        // Prowlarr steps back from today, so today itself counts.
        while parsed.weekday() != target {
            parsed -= Duration::days(1);
        }
        return Ok(Utc.from_utc_datetime(&parsed));
    }
    // The two shapes that leave the year out; both rewrite the value and fall
    // through to the layouts below.
    let mut value = value.to_string();
    static MISSING_YEAR: OnceLock<Regex> = OnceLock::new();
    if let Some(captures) =
        unknown_date_pattern(&MISSING_YEAR, r"^(\d{1,2}-\d{1,2})(\s|$)").captures(&value)
    {
        let date = captures.get(1).expect("date group").as_str().to_string();
        value = value.replace(&date, &format!("{}-{date}", now.year()));
    }
    static MISSING_YEAR_WITH_MONTH_NAME: OnceLock<Regex> = OnceLock::new();
    if let Some(captures) = unknown_date_pattern(
        &MISSING_YEAR_WITH_MONTH_NAME,
        r"^(\d{1,2}\s+\w{3})\s+(\d{1,2}:\d{1,2}.*)$",
    )
    .captures(&value.clone())
    {
        let date = captures.get(1).expect("date group").as_str();
        let time = captures.get(2).expect("time group").as_str();
        value = format!("{date} {} {time}", now.year());
    }
    for pattern in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%d/%m/%Y %H:%M",
        "%m/%d/%Y %H:%M",
        "%d.%m.%Y %H:%M",
        "%d %b %Y %H:%M",
        "%b %e, %Y %H:%M",
        "%B %e, %Y %H:%M",
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(&value, pattern) {
            return Ok(Utc.from_utc_datetime(&parsed));
        }
    }
    for pattern in [
        "%Y-%m-%d",
        "%d/%m/%Y",
        "%m/%d/%Y",
        "%d.%m.%Y",
        "%d %b %Y",
        "%b %d %Y",
        "%b %e, %Y",
        "%B %e, %Y",
    ] {
        if let Ok(parsed) = NaiveDate::parse_from_str(&value, pattern)
            && let Some(parsed) = parsed.and_hms_opt(0, 0, 0)
        {
            return Ok(Utc.from_utc_datetime(&parsed));
        }
    }
    Err(format!("could not parse fuzzy time `{value}`"))
}

pub(crate) fn normalize_unknown_date(value: &str) -> String {
    normalize_unknown_date_at(value, now_utc())
}

fn normalize_unknown_date_at(value: &str, now: DateTime<Utc>) -> String {
    parse_unknown_datetime_at(value, now)
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

/// Prowlarr's `DateTimeUtil.FromTimeAgo`.
///
/// The corpus writes relative times in every abbreviation a tracker template
/// happens to use — `3 hrs ago`, `5 mins ago`, `2 wks`, `1h` — so the unit is
/// matched by containment rather than by a fixed word list. Commas, `ago` and
/// `and` are noise; a value that mentions `now` is now; a month is 30 days and
/// a year 365.
fn parse_relative_time(value: &str) -> Result<String, String> {
    parse_relative_time_at(value, now_utc())
}

fn parse_relative_time_at(value: &str, now: DateTime<Utc>) -> Result<String, String> {
    relative_time_at(value, now).map(|parsed| parsed.to_rfc3339())
}

fn relative_time_at(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let normalized = value.trim().to_lowercase();
    if normalized.contains("now") {
        return Ok(now);
    }
    let normalized = normalized
        .replace(',', "")
        .replace("ago", "")
        .replace("and", "");
    let regex = Regex::new(r"([\d\.]+)\s*([^\d\s\.]+)").expect("static relative time regex");
    let mut seconds = 0f64;
    let mut matched = false;
    for captures in regex.captures_iter(&normalized) {
        let amount = captures[1].parse::<f64>().unwrap_or_default();
        let unit = &captures[2];
        let factor = if unit.contains("sec") || unit == "s" {
            1.0
        } else if unit.contains("min") || unit == "m" {
            60.0
        } else if unit.contains("hour") || unit.contains("hr") || unit == "h" {
            3_600.0
        } else if unit.contains("day") || unit == "d" {
            86_400.0
        } else if unit.contains("week") || unit.contains("wk") || unit == "w" {
            604_800.0
        } else if unit.contains("month") || unit == "mo" {
            2_592_000.0
        } else if unit.contains("year") || unit == "y" {
            31_536_000.0
        } else {
            return Err(format!(
                "could not parse relative time `{value}`: unknown unit `{unit}`"
            ));
        };
        seconds += amount * factor;
        matched = true;
    }
    if !matched {
        return Err(format!("could not parse relative time `{value}`"));
    }
    Ok(now - Duration::seconds(seconds.round() as i64))
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

    /// The corpus is written in the .NET dialect: lookaround, backreferences,
    /// `\p{IsXxx}` block names, and `$1x` meaning "group 1 then x". Each of
    /// these used to fail at compile time, which became a filter error, which
    /// dropped the row.
    #[test]
    fn accepts_the_dotnet_regex_dialect_the_corpus_is_written_in() {
        let re_replace = |pattern: &str, replacement: &str, value: &str| {
            apply_filters(
                value,
                &[FilterBlock {
                    name: "re_replace".into(),
                    args: Some(serde_yaml::Value::Sequence(vec![
                        serde_yaml::Value::String(pattern.into()),
                        serde_yaml::Value::String(replacement.into()),
                    ])),
                }],
                &Variables::new(),
            )
        };

        // Lookahead: strip a trailing marker only when a size unit follows.
        assert_eq!(
            re_replace(r"\s+(?=GB)", "", "12 GB").unwrap(),
            "12GB",
            "lookahead"
        );
        // Lookbehind.
        assert_eq!(
            re_replace(r"(?<=Season )0+", "", "Season 007").unwrap(),
            "Season 7"
        );
        // Backreference.
        assert_eq!(
            re_replace(r"(\w+) \1", "$1", "repeat repeat tail").unwrap(),
            "repeat tail"
        );
        // .NET Unicode block name.
        assert_eq!(
            re_replace(r"[\p{IsCyrillic}]+", "-", "abcДЕФghi").unwrap(),
            "abc-ghi"
        );
        // `$1x` is group 1 followed by a literal `x` in .NET, not the group
        // named `1x`.
        assert_eq!(
            re_replace(r"^(\d+)$", "$1x264", "1080").unwrap(),
            "1080x264"
        );
        // The unambiguous forms keep their meaning.
        assert_eq!(re_replace(r"^(\d+)$", "${1}p", "1080").unwrap(), "1080p");
        assert_eq!(re_replace(r"^\d+$", "$$", "1080").unwrap(), "$");
        assert_eq!(
            re_replace(r"^(?<width>\d+)$", "$width!", "1080").unwrap(),
            "1080!"
        );

        assert_eq!(cardigann_replacement("$1x"), "${1}x");
        assert_eq!(cardigann_replacement("$1 x"), "$1 x");
        assert_eq!(cardigann_replacement("$10"), "$10");
        assert_eq!(cardigann_replacement("${1}x"), "${1}x");
        assert_eq!(cardigann_replacement("$$1x"), "$$1x");
        assert_eq!(cardigann_replacement("$name"), "$name");
        assert_eq!(cardigann_replacement("trailing$"), "trailing$");

        // An unparseable pattern is still an error.
        assert!(re_replace("(unclosed", "", "value").is_err());
    }

    /// Prowlarr reads `Groups[1].Value` of a failed match, which is empty.
    #[test]
    fn a_regexp_filter_that_does_not_match_yields_an_empty_string() {
        let regexp = |pattern: &str, value: &str| {
            apply_filters(
                value,
                &[FilterBlock {
                    name: "regexp".into(),
                    args: Some(serde_yaml::Value::String(pattern.into())),
                }],
                &Variables::new(),
            )
        };
        assert_eq!(regexp(r"size=(\d+)", "no size here").unwrap(), "");
        assert_eq!(regexp(r"size=(\d+)", "size=42").unwrap(), "42");
        assert!(regexp("(unclosed", "size=42").is_err());
    }

    /// Prowlarr's `FromTimeAgo` matches any unit token containing the unit
    /// name, so `3 hrs ago` and `5 mins ago` are ordinary corpus values.
    #[test]
    fn parses_the_abbreviated_relative_times_prowlarr_accepts() {
        let ago = |value: &str| {
            let parsed = parse_relative_time(value).unwrap();
            let parsed = DateTime::parse_from_rfc3339(&parsed)
                .unwrap()
                .with_timezone(&Utc);
            (now_utc() - parsed).num_seconds()
        };
        assert!((ago("3 hrs ago") - 3 * 3600).abs() <= 2);
        assert!((ago("5 mins ago") - 5 * 60).abs() <= 2);
        assert!((ago("2 wks") - 14 * 86_400).abs() <= 2);
        assert!((ago("1h") - 3600).abs() <= 2);
        assert!((ago("30 secs ago") - 30).abs() <= 2);
        assert!((ago("1 mo") - 30 * 86_400).abs() <= 2);
        assert!((ago("1 year") - 365 * 86_400).abs() <= 2);
        assert!((ago("1 day, 2 hours and 3 minutes ago") - (86_400 + 7_200 + 180)).abs() <= 2);
        assert!(ago("just now").abs() <= 2);
        // Prowlarr throws on a unit it cannot name, and so does this.
        assert!(parse_relative_time("2 fortnights ago").is_err());
    }

    /// `DateTimeUtil.FromUnknown` resolves the named days, `<weekday> at <time>`
    /// and the two year-less shapes before it ever reaches a layout. Without
    /// them an untyped `date` field kept the tracker's own text, which the host
    /// then had no date for.
    #[test]
    fn resolves_the_untyped_date_forms_prowlarr_resolves() {
        // A Monday, so "Saturday at …" is two days back.
        let now = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        for (value, expected) in [
            ("Today 10:30", "2026-06-15T10:30:00+00:00"),
            ("Yesterday, 23:15", "2026-06-14T23:15:00+00:00"),
            ("Tomorrow at 08:00", "2026-06-16T08:00:00+00:00"),
            ("today", "2026-06-15T00:00:00+00:00"),
            ("Saturday at 14:22", "2026-06-13T14:22:00+00:00"),
            ("Monday at 09:00", "2026-06-15T09:00:00+00:00"),
            ("01-02 15:00", "2026-01-02T15:00:00+00:00"),
            ("1 Jan 10:30", "2026-01-01T10:30:00+00:00"),
            // The layouts the fallback list gained.
            ("2026-06-15T10:30:00", "2026-06-15T10:30:00+00:00"),
            ("15.06.2026 10:30", "2026-06-15T10:30:00+00:00"),
            ("15.06.2026", "2026-06-15T00:00:00+00:00"),
            ("15 Jun 2026 10:30", "2026-06-15T10:30:00+00:00"),
            ("15 Jun 2026", "2026-06-15T00:00:00+00:00"),
            ("Jun 15 2026", "2026-06-15T00:00:00+00:00"),
            ("Jun 5, 2026 10:30", "2026-06-05T10:30:00+00:00"),
            // A bare unix timestamp and an RFC 1123 value keep working.
            ("1781000000", "2026-06-09T10:13:20+00:00"),
            (
                "Sat, 13 Jun 2026 14:22:00 +0000",
                "2026-06-13T14:22:00+00:00",
            ),
        ] {
            assert_eq!(normalize_unknown_date_at(value, now), expected, "{value}");
        }

        // `ago` still routes through the relative-time parser.
        assert_eq!(
            normalize_unknown_date_at("3 hours ago", now),
            "2026-06-15T09:00:00+00:00"
        );
        // Anything naming `now` is now.
        assert_eq!(normalize_unknown_date_at("just now", now), now.to_rfc3339());
        // A value none of it resolves is still handed back unchanged.
        assert_eq!(normalize_unknown_date_at("  no date  ", now), "no date");
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
