use chardetng::EncodingDetector;
use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

use super::simd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubtitleFormat {
    Srt,
    Vtt,
    Ass,
    Ssa,
}

impl SubtitleFormat {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "srt" => Ok(Self::Srt),
            "vtt" | "webvtt" => Ok(Self::Vtt),
            "ass" => Ok(Self::Ass),
            "ssa" => Ok(Self::Ssa),
            other => Err(format!("unsupported subtitle sync format '{other}'")),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Ass => "ass",
            Self::Ssa => "ssa",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubtitleCue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub content: String,
}

impl SubtitleCue {
    #[allow(dead_code)]
    pub(crate) fn merge_with(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        if merged.content.is_empty() {
            merged.content = other.content.clone();
        } else if !other.content.is_empty() {
            merged.content.push('\n');
            merged.content.push_str(&other.content);
        }
        merged
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SubtitleDocument {
    pub format: SubtitleFormat,
    pub content: String,
    pub cues: Vec<SubtitleCue>,
    encoding: SubtitleEncoding,
}

#[derive(Debug, Clone)]
struct SubtitleEncoding {
    input_label: String,
    encoding: &'static Encoding,
}

pub(crate) fn parse_document(
    format: SubtitleFormat,
    bytes: &[u8],
    encoding_hint: Option<&str>,
) -> Result<(SubtitleDocument, Vec<String>), String> {
    let (content, encoding, mut warnings) = decode_subtitle(bytes, encoding_hint);
    let cues = match format {
        SubtitleFormat::Srt => parse_srt_cues(&content),
        SubtitleFormat::Vtt => parse_vtt_cues(&content),
        SubtitleFormat::Ass | SubtitleFormat::Ssa => parse_ass_cues(&content),
    };
    Ok((
        SubtitleDocument {
            format,
            content,
            cues,
            encoding,
        },
        std::mem::take(&mut warnings),
    ))
}

pub(crate) fn rewrite_document(
    document: &SubtitleDocument,
    ratio: f64,
    offset_ms: i64,
    output_encoding: &str,
) -> Result<(Vec<u8>, Vec<String>), String> {
    let rewritten = match document.format {
        SubtitleFormat::Srt => rewrite_srt_content(&document.content, ratio, offset_ms),
        SubtitleFormat::Vtt => rewrite_vtt_content(&document.content, ratio, offset_ms),
        SubtitleFormat::Ass | SubtitleFormat::Ssa => {
            rewrite_ass_content(&document.content, ratio, offset_ms)
        }
    };
    Ok(encode_subtitle(
        &rewritten,
        &document.encoding,
        output_encoding,
    ))
}

pub(crate) fn parse_srt_ts(value: &str) -> Option<i64> {
    let (time, millis) = value.trim().split_once([',', '.'])?;
    let parts = time.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let hours = parts[0].parse::<i64>().ok()?;
    let minutes = parts[1].parse::<i64>().ok()?;
    let seconds = parts[2].parse::<i64>().ok()?;
    let millis = parse_fractional_ms(millis)?;
    Some((((hours * 60 + minutes) * 60 + seconds) * 1000) + millis)
}

pub(crate) fn format_srt_ts(ms: i64) -> String {
    let ms = ms.max(0);
    let total_seconds = ms / 1000;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60,
        ms % 1000
    )
}

pub(crate) fn format_vtt_ts(ms: i64) -> String {
    let ms = ms.max(0);
    let total_seconds = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60,
        ms % 1000
    )
}

pub(crate) fn format_ass_ts(ms: i64) -> String {
    let ms = ms.max(0);
    let total_cs = (ms + 5) / 10;
    let total_seconds = total_cs / 100;
    format!(
        "{}:{:02}:{:02}.{:02}",
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60,
        total_cs % 100
    )
}

fn decode_subtitle(
    bytes: &[u8],
    encoding_hint: Option<&str>,
) -> (String, SubtitleEncoding, Vec<String>) {
    let mut warnings = Vec::new();
    let encoding = encoding_hint
        .filter(|hint| !hint.eq_ignore_ascii_case("infer"))
        .and_then(|hint| Encoding::for_label(hint.as_bytes()))
        .or_else(|| Encoding::for_bom(bytes).map(|(encoding, _)| encoding))
        .or_else(|| std::str::from_utf8(bytes).ok().map(|_| UTF_8))
        .unwrap_or_else(|| {
            let mut detector = EncodingDetector::new();
            detector.feed(bytes, true);
            detector.guess(None, true)
        });

    let (decoded, actual_encoding, had_errors) = encoding.decode(bytes);
    if had_errors {
        warnings.push(format!(
            "subtitle bytes contained invalid sequences for {}; replacement characters were inserted",
            actual_encoding.name()
        ));
    }

    (
        decoded.into_owned(),
        SubtitleEncoding {
            input_label: actual_encoding.name().to_ascii_lowercase(),
            encoding: actual_encoding,
        },
        warnings,
    )
}

fn encode_subtitle(
    content: &str,
    input_encoding: &SubtitleEncoding,
    output_encoding: &str,
) -> (Vec<u8>, Vec<String>) {
    let mut warnings = Vec::new();
    let encoding = if output_encoding.eq_ignore_ascii_case("same") {
        input_encoding.encoding
    } else {
        Encoding::for_label(output_encoding.as_bytes()).unwrap_or_else(|| {
            warnings.push(format!(
                "unsupported output encoding '{}'; using utf-8",
                output_encoding
            ));
            UTF_8
        })
    };

    let (encoded, actual_encoding, had_errors) = encoding.encode(content);
    if had_errors {
        warnings.push(format!(
            "rewritten subtitle could not be represented as {}; using utf-8",
            actual_encoding.name()
        ));
        return (content.as_bytes().to_vec(), warnings);
    }
    if output_encoding.eq_ignore_ascii_case("same")
        && actual_encoding
            .name()
            .eq_ignore_ascii_case(WINDOWS_1252.name())
        && input_encoding.input_label != "windows-1252"
    {
        warnings.push("subtitle encoding normalized to windows-1252".to_string());
    }
    (encoded.into_owned(), warnings)
}

fn parse_srt_cues(content: &str) -> Vec<SubtitleCue> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut cues = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        if let Some((start, end)) = parse_srt_timing_line(lines[index]) {
            let mut content_lines = Vec::new();
            index += 1;
            while index < lines.len()
                && !lines[index].trim().is_empty()
                && parse_srt_timing_line(lines[index]).is_none()
            {
                content_lines.push(lines[index]);
                index += 1;
            }
            cues.push(SubtitleCue {
                start_ms: start,
                end_ms: end,
                content: content_lines.join("\n"),
            });
        } else {
            index += 1;
        }
    }
    cues
}

fn parse_srt_timing_line(line: &str) -> Option<(i64, i64)> {
    let (start, rest) = line.split_once("-->")?;
    let (end, _) = split_timestamp_token(rest.trim_start())?;
    Some((parse_srt_ts(start.trim())?, parse_srt_ts(end)?))
}

fn parse_vtt_cues(content: &str) -> Vec<SubtitleCue> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut cues = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        if line_starts_with_ignore_ascii_case(trimmed, "NOTE") {
            index += 1;
            while index < lines.len() && !lines[index].trim().is_empty() {
                index += 1;
            }
            continue;
        }

        if let Some((start, end)) = parse_vtt_timing_line(lines[index]) {
            let mut content_lines = Vec::new();
            index += 1;
            while index < lines.len()
                && !lines[index].trim().is_empty()
                && parse_vtt_timing_line(lines[index]).is_none()
            {
                content_lines.push(lines[index]);
                index += 1;
            }
            cues.push(SubtitleCue {
                start_ms: start,
                end_ms: end,
                content: content_lines.join("\n"),
            });
        } else {
            index += 1;
        }
    }
    cues
}

fn parse_vtt_timing_line(line: &str) -> Option<(i64, i64)> {
    let (start, rest) = line.split_once("-->")?;
    let (end, _) = split_timestamp_token(rest.trim_start())?;
    Some((parse_vtt_ts(start.trim())?, parse_vtt_ts(end)?))
}

fn parse_vtt_ts(value: &str) -> Option<i64> {
    let (time, millis) = value.trim().split_once('.')?;
    let parts = time.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (
            0,
            minutes.parse::<i64>().ok()?,
            seconds.parse::<i64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<i64>().ok()?,
            minutes.parse::<i64>().ok()?,
            seconds.parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    let millis = parse_fixed_millis(millis)?;
    Some((((hours * 60 + minutes) * 60 + seconds) * 1000) + millis)
}

fn parse_ass_cues(content: &str) -> Vec<SubtitleCue> {
    let mut cues = Vec::new();
    let mut in_events = false;
    let mut event_format = AssEventFormat::default();
    for line in content.lines() {
        let trimmed = line.trim();
        if is_section_header(trimmed) {
            in_events = trimmed.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if line_starts_with_ignore_ascii_case(trimmed, "Format:") {
            event_format = parse_ass_event_format(trimmed).unwrap_or_else(AssEventFormat::default);
            continue;
        }
        if !is_ass_timed_event_line(trimmed) {
            continue;
        }
        let Some(fields) = split_ass_fields(trimmed, &event_format) else {
            continue;
        };
        let Some(start) = parse_ass_ts(fields[event_format.start_index].trim()) else {
            continue;
        };
        let Some(end) = parse_ass_ts(fields[event_format.end_index].trim()) else {
            continue;
        };
        let content = fields
            .get(event_format.text_index)
            .copied()
            .unwrap_or_default()
            .to_string();
        cues.push(SubtitleCue {
            start_ms: start,
            end_ms: end,
            content,
        });
    }
    cues
}

fn rewrite_srt_content(content: &str, ratio: f64, offset_ms: i64) -> String {
    rewrite_lines_preserving_endings(content, |line| {
        let Some((start_raw, rest)) = line.split_once("-->") else {
            return None;
        };
        let start = parse_srt_ts(start_raw.trim())?;
        let (end_raw, suffix) = split_timestamp_token(rest.trim_start())?;
        let end = parse_srt_ts(end_raw)?;
        let (start, end) = simd::transform_ms_pair(start, end, ratio, offset_ms);
        let leading = &start_raw[..start_raw.len() - start_raw.trim_start().len()];
        Some(format!(
            "{}{} --> {}{}",
            leading,
            format_srt_ts(start),
            format_srt_ts(end),
            suffix
        ))
    })
}

fn rewrite_vtt_content(content: &str, ratio: f64, offset_ms: i64) -> String {
    rewrite_lines_preserving_endings(content, |line| {
        let Some((start_raw, rest)) = line.split_once("-->") else {
            return None;
        };
        let start = parse_vtt_ts(start_raw.trim())?;
        let (end_raw, suffix) = split_timestamp_token(rest.trim_start())?;
        let end = parse_vtt_ts(end_raw)?;
        let (start, end) = simd::transform_ms_pair(start, end, ratio, offset_ms);
        let leading = &start_raw[..start_raw.len() - start_raw.trim_start().len()];
        Some(format!(
            "{}{} --> {}{}",
            leading,
            format_vtt_ts(start),
            format_vtt_ts(end),
            suffix
        ))
    })
}

fn rewrite_ass_content(content: &str, ratio: f64, offset_ms: i64) -> String {
    let mut in_events = false;
    let mut event_format = AssEventFormat::default();
    rewrite_lines_preserving_endings(content, |line| {
        let trimmed = line.trim();
        if is_section_header(trimmed) {
            in_events = trimmed.eq_ignore_ascii_case("[Events]");
            return None;
        }
        if !in_events {
            return None;
        }
        if line_starts_with_ignore_ascii_case(trimmed, "Format:") {
            event_format = parse_ass_event_format(trimmed).unwrap_or_else(AssEventFormat::default);
            return None;
        }
        if is_ass_timed_event_line(trimmed) {
            rewrite_ass_event_line(line, &event_format, ratio, offset_ms)
        } else {
            None
        }
    })
}

fn rewrite_lines_preserving_endings(
    content: &str,
    mut rewrite: impl FnMut(&str) -> Option<String>,
) -> String {
    let mut output = String::with_capacity(content.len());
    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = raw_line
            .strip_suffix("\r\n")
            .map(|line| (line, "\r\n"))
            .or_else(|| raw_line.strip_suffix('\n').map(|line| (line, "\n")))
            .unwrap_or((raw_line, ""));
        if let Some(rewritten) = rewrite(line) {
            output.push_str(&rewritten);
        } else {
            output.push_str(line);
        }
        output.push_str(ending);
    }
    if !content.ends_with('\n') && content.is_empty() {
        output.clear();
    }
    output
}

fn rewrite_ass_event_line(
    line: &str,
    event_format: &AssEventFormat,
    ratio: f64,
    offset_ms: i64,
) -> Option<String> {
    let prefix_len = line.find(':')? + 1;
    let prefix = &line[..prefix_len];
    let body = line[prefix_len..].trim_start();
    let leading = &line[prefix_len..line.len() - body.len()];
    let mut fields = split_ass_fields(body, event_format)?
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let start = parse_ass_ts(fields[event_format.start_index].trim())?;
    let end = parse_ass_ts(fields[event_format.end_index].trim())?;
    let (start, end) = simd::transform_ms_pair(start, end, ratio, offset_ms);
    fields[event_format.start_index] = format_ass_ts(start);
    fields[event_format.end_index] = format_ass_ts(end);
    Some(format!("{prefix}{leading}{}", fields.join(",")))
}

#[derive(Debug, Clone, Copy)]
struct AssEventFormat {
    start_index: usize,
    end_index: usize,
    text_index: usize,
    field_count: usize,
}

impl Default for AssEventFormat {
    fn default() -> Self {
        Self {
            start_index: 1,
            end_index: 2,
            text_index: 9,
            field_count: 10,
        }
    }
}

fn parse_ass_event_format(line: &str) -> Option<AssEventFormat> {
    let (_, fields) = line.split_once(':')?;
    let fields = fields
        .split(',')
        .map(|field| field.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let start_index = fields.iter().position(|field| field == "start")?;
    let end_index = fields.iter().position(|field| field == "end")?;
    let text_index = fields
        .iter()
        .position(|field| field == "text")
        .unwrap_or(fields.len().saturating_sub(1));
    Some(AssEventFormat {
        start_index,
        end_index,
        text_index,
        field_count: fields.len().max(text_index + 1),
    })
}

fn split_ass_fields<'a>(body: &'a str, event_format: &AssEventFormat) -> Option<Vec<&'a str>> {
    let max_splits = event_format.field_count.saturating_sub(1);
    let fields = body.splitn(max_splits + 1, ',').collect::<Vec<_>>();
    (fields.len() > event_format.start_index && fields.len() > event_format.end_index)
        .then_some(fields)
}

fn split_timestamp_token(value: &str) -> Option<(&str, &str)> {
    let trimmed = value.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    Some((&trimmed[..end], &trimmed[end..]))
}

fn parse_ass_ts(value: &str) -> Option<i64> {
    let (time, fraction) = value.trim().split_once('.')?;
    let parts = time.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let hours = parts[0].parse::<i64>().ok()?;
    let minutes = parts[1].parse::<i64>().ok()?;
    let seconds = parts[2].parse::<i64>().ok()?;
    let centiseconds = parse_fractional_ms(fraction)?;
    Some((((hours * 60 + minutes) * 60 + seconds) * 1000) + centiseconds * 10)
}

fn parse_fractional_ms(value: &str) -> Option<i64> {
    let digits = value
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok()
}

fn parse_fixed_millis(value: &str) -> Option<i64> {
    let digits = value
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .take(3)
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let value = digits.parse::<i64>().ok()?;
    Some(match digits.len() {
        1 => value * 100,
        2 => value * 10,
        _ => value,
    })
}

fn is_section_header(line: &str) -> bool {
    line.starts_with('[') && line.ends_with(']')
}

fn line_starts_with_ignore_ascii_case(line: &str, prefix: &str) -> bool {
    line.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn is_ass_timed_event_line(line: &str) -> bool {
    [
        "Dialogue:",
        "Comment:",
        "Picture:",
        "Sound:",
        "Movie:",
        "Command:",
    ]
    .into_iter()
    .any(|prefix| line_starts_with_ignore_ascii_case(line, prefix))
}

#[cfg(test)]
mod tests {
    use crate::subtitle_sync::transformers::{
        SubtitleTransformOptions, max_time_seconds, preprocess_cues, subtitle_speech_spans,
    };

    use super::*;

    const FAKE_SRT: &[u8] = b"1
00:00:00,178 --> 00:00:01,1416
<i>Previously on \"Your favorite TV show...\"</i>

2
00:00:01,1828 --> 00:00:04,549
Oh hi, Mark.

3
00:00:04,653 --> 00:00:03,3062
You are tearing me apart, Lisa!
";

    #[test]
    fn parses_srt_with_long_millisecond_fields_like_upstream() {
        let (doc, _) = parse_document(SubtitleFormat::Srt, FAKE_SRT, Some("utf-8")).unwrap();

        assert_eq!(doc.cues[0].end_ms, 2_416);
        assert_eq!(doc.cues[2].end_ms, 6_062);
    }

    #[test]
    fn preprocesses_start_duration_and_max_time_like_upstream() {
        let (doc, _) = parse_document(SubtitleFormat::Srt, FAKE_SRT, Some("utf-8")).unwrap();
        for start_seconds in [0, 2, 4, 6] {
            let filtered = preprocess_cues(
                &doc.cues,
                SubtitleTransformOptions {
                    start_seconds,
                    max_subtitle_duration_ms: 10_000,
                },
            );
            assert!(
                filtered
                    .iter()
                    .all(|cue| cue.start_ms >= i64::from(start_seconds) * 1000)
            );
        }

        for max_subtitle_duration_ms in [1_000, 1_500, 2_000, 2_500] {
            let clamped = preprocess_cues(
                &doc.cues,
                SubtitleTransformOptions {
                    start_seconds: 0,
                    max_subtitle_duration_ms,
                },
            );
            assert!(
                clamped
                    .iter()
                    .all(|cue| cue.end_ms - cue.start_ms <= max_subtitle_duration_ms)
            );
        }

        let spans = subtitle_speech_spans(&doc.cues, SubtitleTransformOptions::default());
        assert!((max_time_seconds(&spans) - 6.062).abs() < 0.001);
    }

    #[test]
    fn rewrites_srt_with_offset_and_scale() {
        let (doc, _) = parse_document(SubtitleFormat::Srt, FAKE_SRT, Some("utf-8")).unwrap();
        let (rewritten, _) = rewrite_document(&doc, 1.05, 998, "same").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert!(rewritten.contains("00:00:01,185 --> 00:00:03,535"));
    }

    #[test]
    fn parses_and_rewrites_vtt_with_cue_settings_notes_and_crlf() {
        let vtt = b"WEBVTT\r\nKind: captions\r\n\r\nNOTE this block should be ignored\r\n00:00:00.000 --> 00:00:01.000\r\nnot a cue\r\n\r\nintro-cue\r\n00:00:02.500 --> 00:00:04.000 align:start position:10%\r\nHello there\r\nsecond line\r\n\r\n00:00:05.000 --> 00:00:06.250\r\nFinal cue\r\n";
        let (doc, _) = parse_document(SubtitleFormat::Vtt, vtt, Some("utf-8")).unwrap();

        assert_eq!(doc.cues.len(), 2);
        assert_eq!(doc.cues[0].start_ms, 2_500);
        assert_eq!(doc.cues[0].end_ms, 4_000);
        assert_eq!(doc.cues[0].content, "Hello there\nsecond line");

        let (rewritten, _) = rewrite_document(&doc, 1.0, 1_250, "same").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert!(rewritten.starts_with("WEBVTT\r\n"));
        assert!(rewritten.contains("NOTE this block should be ignored\r\n"));
        assert!(rewritten.contains("00:00:03.750 --> 00:00:05.250 align:start position:10%\r\n"));
        assert!(rewritten.contains("intro-cue\r\n"));
        assert!(rewritten.contains("00:00:06.250 --> 00:00:07.500\r\n"));
    }

    #[test]
    fn vtt_timestamps_accept_minute_form_and_malformed_cues_are_ignored() {
        let vtt = b"WEBVTT\n\nbad\n00:bad --> 00:00:02.000\nignored\n\n00:02.500 --> 00:04.000\nshort form\n";
        let (doc, _) = parse_document(SubtitleFormat::Vtt, vtt, Some("utf-8")).unwrap();

        assert_eq!(doc.cues.len(), 1);
        assert_eq!(doc.cues[0].start_ms, 2_500);
        assert_eq!(doc.cues[0].end_ms, 4_000);
        assert_eq!(format_vtt_ts(doc.cues[0].start_ms), "00:00:02.500");
    }

    #[test]
    fn preserves_latin1_output_when_requested() {
        let bytes = b"1\n00:00:00,000 --> 00:00:01,000\ncaf\xe9\n";
        let (doc, _) = parse_document(SubtitleFormat::Srt, bytes, Some("windows-1252")).unwrap();
        let (rewritten, _) = rewrite_document(&doc, 1.0, 1_000, "same").unwrap();

        assert!(rewritten.contains(&0xe9));
    }

    #[test]
    fn rewrites_ass_dialogue_timestamps_and_preserves_commas() {
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Text\nDialogue: 0,0:00:02.00,0:00:03.00,Default,Hi, comma\nComment: 0,0:00:04.00,0:00:05.00,Default,note\n";
        let (doc, _) = parse_document(SubtitleFormat::Ass, ass.as_bytes(), Some("utf-8")).unwrap();
        let (rewritten, _) = rewrite_document(&doc, 1.0, -1_000, "same").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert_eq!(doc.cues.len(), 2);
        assert!(rewritten.contains("Dialogue: 0,0:00:01.00,0:00:02.00,Default,Hi, comma"));
        assert!(rewritten.contains("Comment: 0,0:00:03.00,0:00:04.00,Default,note"));
    }

    #[test]
    fn rewrites_ssa_timestamps_with_scale_and_preserves_format_line() {
        let ssa = "[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: Marked=0,0:00:10.00,0:00:12.00,Default,,0000,0000,0000,,Scaled\n";
        let (doc, _) = parse_document(SubtitleFormat::Ssa, ssa.as_bytes(), Some("utf-8")).unwrap();
        let (rewritten, _) = rewrite_document(&doc, 1.05, -500, "same").unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert_eq!(doc.cues[0].content, "Scaled");
        assert!(rewritten.contains("Format: Marked, Start, End"));
        assert!(
            rewritten.contains(
                "Dialogue: Marked=0,0:00:10.00,0:00:12.10,Default,,0000,0000,0000,,Scaled"
            )
        );
    }
}
