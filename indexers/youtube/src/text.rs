//! Text handling: ISO 8601 durations, HTML entities in `search.list`
//! titles, release-title cleanup and the local title filter used when
//! enumerating a channel's uploads.

/// Seconds in an ISO 8601 duration as the Data API writes it
/// (`PT1H2M3S`, `P1DT2H`, `P0D`, `PT45.5S`). Year and month components have
/// no fixed length in seconds, so a duration that uses them is `None`.
pub fn parse_iso8601_duration(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix('P')?;
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut in_time = false;
    let mut number = String::new();
    let mut saw_component = false;
    for ch in rest.chars() {
        match ch {
            'T' if !in_time && number.is_empty() => in_time = true,
            '0'..='9' | '.' | ',' => number.push(if ch == ',' { '.' } else { ch }),
            unit => {
                if number.is_empty() {
                    return None;
                }
                let multiplier: u64 = match (in_time, unit) {
                    (false, 'W') => 7 * 86_400,
                    (false, 'D') => 86_400,
                    (true, 'H') => 3_600,
                    (true, 'M') => 60,
                    (true, 'S') => 1,
                    _ => return None,
                };
                let seconds = if number.contains('.') {
                    // Only seconds may be fractional in practice; round down.
                    let parsed: f64 = number.parse().ok()?;
                    if !parsed.is_finite() || parsed < 0.0 {
                        return None;
                    }
                    (parsed * multiplier as f64) as u64
                } else {
                    number.parse::<u64>().ok()?.checked_mul(multiplier)?
                };
                total = total.checked_add(seconds)?;
                number.clear();
                saw_component = true;
            }
        }
    }
    (number.is_empty() && saw_component).then_some(total)
}

/// Decode the HTML entities `search.list` leaves in snippet titles.
pub fn html_unescape(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let decoded = after.find(';').filter(|end| *end <= 10).and_then(|end| {
            let entity = &after[..end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    if let Some(hex) = entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                    {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else if let Some(decimal) = entity.strip_prefix('#') {
                        decimal.parse::<u32>().ok().and_then(char::from_u32)
                    } else {
                        None
                    }
                }
            }?;
            Some((ch, end))
        });
        match decoded {
            Some((ch, end)) => {
                output.push(ch);
                rest = &after[end + 1..];
            }
            None => {
                output.push('&');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// Bracket contents made only of these words are promotional noise, never
/// season, episode, edition or language information.
const NOISE_WORDS: &[&str] = &[
    "official",
    "video",
    "full",
    "episode",
    "episodes",
    "hd",
    "hq",
    "4k",
    "uhd",
    "cc",
    "subtitles",
    "subtitled",
    "captions",
    "new",
    "free",
    "premiere",
    "upload",
    "uploaded",
];

const TITLE_SEPARATORS: &[&str] = &[" | ", " - ", " – ", " — ", " // "];

/// A release title the existing parser can read.
///
/// Only reversible edits are made, and the raw title is always kept beside
/// the result:
/// - a trailing ` | <channel>` (or ` - `, ` – `, ` — `, ` // `) suffix is
///   removed; when what remains starts with an episode marker the channel is
///   moved to the front instead (`Episode 2 | Show` becomes `Show - Episode
///   2`), because a channel named for its show is then the only series title
///   the parser would see;
/// - bracketed segments made only of promotional words (`[Official Video]`,
///   `(Full Episode)`, `【HD】`) are dropped;
/// - whitespace is collapsed.
///
/// Episode markers (`S01E02`, `Episode 2`, `Ep. 2`, `#2`, dates) are never
/// touched. An edit that would leave nothing returns the collapsed raw title.
pub fn clean_title(raw: &str, channel_title: Option<&str>) -> String {
    let collapsed = collapse_whitespace(raw);
    let mut title = strip_noise_brackets(&collapsed);

    if let Some(channel) = channel_title
        .map(collapse_whitespace)
        .filter(|channel| !channel.is_empty())
    {
        for separator in TITLE_SEPARATORS {
            let Some(index) = title.len().checked_sub(separator.len() + channel.len()) else {
                continue;
            };
            if !title.is_char_boundary(index) {
                continue;
            }
            let (prefix, suffix) = title.split_at(index);
            let Some(tail) = suffix.strip_prefix(separator) else {
                continue;
            };
            if !tail.eq_ignore_ascii_case(&channel) {
                continue;
            }
            let prefix = prefix.trim();
            if prefix.is_empty() {
                break;
            }
            title = if starts_with_episode_marker(prefix) {
                format!("{channel} - {prefix}")
            } else {
                prefix.to_string()
            };
            break;
        }
    }

    let title = collapse_whitespace(trim_dangling_separators(&title));
    if title.is_empty() { collapsed } else { title }
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trim_dangling_separators(value: &str) -> &str {
    value
        .trim()
        .trim_end_matches(['|', '-', '–', '—', '/'])
        .trim_start_matches(['|', '-', '–', '—', '/'])
        .trim()
}

fn strip_noise_brackets(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    'outer: while !rest.is_empty() {
        for (open, close) in [('[', ']'), ('(', ')'), ('【', '】')] {
            if rest.starts_with(open)
                && let Some(end) = rest[open.len_utf8()..].find(close)
            {
                let inner = &rest[open.len_utf8()..open.len_utf8() + end];
                let consumed = open.len_utf8() + end + close.len_utf8();
                if is_noise(inner) {
                    rest = &rest[consumed..];
                } else {
                    output.push_str(&rest[..consumed]);
                    rest = &rest[consumed..];
                }
                continue 'outer;
            }
        }
        let ch = rest.chars().next().unwrap_or_default();
        output.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    collapse_whitespace(&output)
}

fn is_noise(inner: &str) -> bool {
    let words = words(inner);
    !words.is_empty()
        && words
            .iter()
            .all(|word| NOISE_WORDS.contains(&word.as_str()))
}

fn starts_with_episode_marker(value: &str) -> bool {
    let lower = value.to_lowercase();
    let lower = lower.trim_start();
    if lower.starts_with('#') && lower[1..].starts_with(|ch: char| ch.is_ascii_digit()) {
        return true;
    }
    for word in ["episode", "ep.", "ep ", "part ", "pt. ", "chapter "] {
        if let Some(after) = lower.strip_prefix(word)
            && after
                .trim_start()
                .starts_with(|ch: char| ch.is_ascii_digit())
        {
            return true;
        }
    }
    let bytes = lower.as_bytes();
    // S01E02 / E02
    let after_s = if bytes.first() == Some(&b's') {
        let digits = bytes[1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            return false;
        }
        &bytes[1 + digits..]
    } else {
        bytes
    };
    after_s.first() == Some(&b'e') && after_s.get(1).is_some_and(u8::is_ascii_digit)
}

/// Lowercase alphanumeric words.
pub fn words(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether every word of `query` appears in `title` or in `channel`. Used to
/// keep a channel's uploads to the searched title; a channel named for the
/// show matches all of its uploads.
pub fn matches_query(query: &str, title: &str, channel: Option<&str>) -> bool {
    let wanted = words(query);
    if wanted.is_empty() {
        return true;
    }
    let contains_all = |haystack: &str| {
        let have = words(haystack);
        wanted.iter().all(|word| have.contains(word))
    };
    contains_all(title) || channel.is_some_and(contains_all)
}
