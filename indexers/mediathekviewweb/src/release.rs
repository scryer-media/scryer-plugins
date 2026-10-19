//! Turning one MediathekViewWeb film-list entry into a Scryer HTTP-family
//! release.

use std::collections::HashMap;

use scryer_plugin_sdk::{
    HttpAudioRole, HttpAvailability, HttpRangeSupport, HttpResourceRole, HttpTransport,
    IndexerProtocol, IndexerSourceKind, PluginHttpAudioTrack, PluginHttpChannel, PluginHttpRelease,
    PluginHttpResource, PluginHttpSubtitleTrack, PluginHttpValidators, PluginHttpVideoTrack,
    PluginSearchResult,
};
use serde_json::{Value, json};

use crate::api::Entry;

pub const PROVIDER: &str = "mediathekviewweb";

/// Frame heights a broadcaster file name may carry. A number outside this
/// set is a bitrate, a date or an id, never a height.
const KNOWN_HEIGHTS: &[u32] = &[
    180, 216, 234, 240, 270, 288, 360, 396, 432, 480, 540, 576, 720, 1080, 1440, 2160,
];

pub fn guid(item_id: &str) -> String {
    format!("{PROVIDER}:{item_id}")
}

/// Accessibility variants the broadcasters publish as separate entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Variant {
    pub audio_description: bool,
    pub sign_language: bool,
}

/// Read the variant from the title, the topic and the programme page.
/// Broadcasters mark it in words (`Audiodeskription`, `Hörfassung`,
/// `Gebärdensprache`) or with a bracketed abbreviation (`(AD)`, `(DGS)`).
/// ARD often leaves the title plain and marks the variant only in the
/// content id its page URL carries (see [`ard_page_variant`]).
pub fn variant(entry: &Entry) -> Variant {
    let text = format!("{} {}", entry.title, entry.topic);
    let lower = text.to_lowercase();
    let page = ard_page_variant(&entry.url_website);
    Variant {
        audio_description: page.audio_description
            || lower.contains("audiodeskription")
            || lower.contains("hörfassung")
            || has_bracketed_tag(&text, "AD"),
        sign_language: page.sign_language
            || lower.contains("gebärdensprache")
            || has_bracketed_tag(&text, "DGS")
            || has_bracketed_tag(&text, "ÖGS"),
    }
}

/// The variant an ARD Mediathek page URL names. Its last path segment is a
/// base64 content id such as
/// `crid://wdr.de/Beitrag-sophora-<uuid>/audiodeskription`; the final
/// segment of that id names the accessibility version.
fn ard_page_variant(url: &str) -> Variant {
    let Some(crid) = ard_page_crid(url) else {
        return Variant::default();
    };
    let crid = crid.trim_end_matches('/').to_ascii_lowercase();
    let last = crid.rsplit('/').next().unwrap_or_default();
    Variant {
        audio_description: matches!(last, "audiodeskription" | "hoerfassung"),
        sign_language: matches!(last, "gebaerdensprache" | "dgs"),
    }
}

/// The decoded `crid://` id of an `ardmediathek.de/video/…` page URL.
fn ard_page_crid(url: &str) -> Option<String> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = without_scheme.split_once('/')?;
    let host = host.to_ascii_lowercase();
    if host != "ardmediathek.de" && host != "www.ardmediathek.de" {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let rest = path.strip_prefix("video/")?;
    let segment = rest.split('/').rfind(|segment| !segment.is_empty())?;
    let decoded = decode_base64(segment)?;
    let crid = String::from_utf8(decoded).ok()?;
    crid.starts_with("crid://").then_some(crid)
}

/// Standard or URL-safe base64, padding optional. `None` for anything else.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Where a release's media files may be downloaded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Geo {
    /// Germany only; the CDN refuses requests from elsewhere.
    Germany,
    Worldwide,
}

impl Geo {
    /// The short hint in `provider_extra.geo`.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Germany => "de",
            Self::Worldwide => "worldwide",
        }
    }

    /// The region in `provider_extra.geo_region`.
    pub fn region(self) -> &'static str {
        match self {
            Self::Germany => "DE",
            Self::Worldwide => "worldwide",
        }
    }
}

/// The rights region one media URL encodes in its path. ARD's CDN files
/// Germany-only media under a `/de/` segment (`/media/p/public/de/…`,
/// `/medp/ondemand/de/…`) or a `geoavailability_D` segment, and worldwide
/// media under `/weltweit/`. A `/de/` segment counts only on ARD's
/// `ard-mcdn.de` hosts, where it is known to mean the rights region.
pub fn media_geo(url: &str) -> Option<Geo> {
    if !is_http_url(url) {
        return None;
    }
    let host = url_host(url);
    let path = url_path(url);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.contains(&"weltweit") {
        return Some(Geo::Worldwide);
    }
    if segments.contains(&"geoavailability_d") {
        return Some(Geo::Germany);
    }
    if host.ends_with(".ard-mcdn.de") && segments.contains(&"de") {
        return Some(Geo::Germany);
    }
    None
}

/// The release's region: Germany when any media file is Germany-only,
/// worldwide when every media file is worldwide, otherwise unknown.
fn release_geo(resources: &[PluginHttpResource]) -> Option<Geo> {
    let geos: Vec<Option<Geo>> = resources
        .iter()
        .filter(|resource| resource.role == HttpResourceRole::Muxed)
        .map(|resource| media_geo(&resource.url))
        .collect();
    if geos.contains(&Some(Geo::Germany)) {
        Some(Geo::Germany)
    } else if !geos.is_empty() && geos.iter().all(|geo| *geo == Some(Geo::Worldwide)) {
        Some(Geo::Worldwide)
    } else {
        None
    }
}

fn has_bracketed_tag(text: &str, tag: &str) -> bool {
    [("(", ")"), ("[", "]")]
        .iter()
        .any(|(open, close)| text.contains(&format!("{open}{tag}{close}")))
}

/// `<topic> - <title> [<channel>]`. The topic is left out when the title
/// already starts with it, so a title is never doubled.
pub fn release_title(entry: &Entry) -> String {
    let topic = entry.topic.trim();
    let title = entry.title.trim();
    let base = if title.is_empty() {
        topic.to_string()
    } else if topic.is_empty() || title.to_lowercase().starts_with(&topic.to_lowercase()) {
        title.to_string()
    } else {
        format!("{topic} - {title}")
    };
    let channel = entry.channel.trim();
    if channel.is_empty() {
        base
    } else {
        format!("{base} [{channel}]")
    }
}

/// The URL's host, lowercased, without port.
fn url_host(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    host.split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The URL's path, lowercased, without query or fragment.
fn url_path(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = without_scheme
        .find('/')
        .map_or("", |index| &without_scheme[index..]);
    path.split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}

fn extension(path: &str) -> Option<&str> {
    let name = file_name(path);
    name.rsplit_once('.').map(|(_, extension)| extension)
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// The frame height a broadcaster encoded in the file name:
/// `_AVC-720.mp4`, `-1080.mp4`, `_960x540-50p-1600kbit.mp4`, `_720p.mp4`.
pub fn infer_height(url: &str) -> Option<u32> {
    let path = url_path(url);
    let name = file_name(&path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let known = |value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|height| KNOWN_HEIGHTS.contains(height))
    };

    // `<width>x<height>`.
    for (index, _) in stem.match_indices('x') {
        let before = &stem[..index];
        let after = &stem[index + 1..];
        let width_digits = before
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .count();
        let height: String = after.chars().take_while(char::is_ascii_digit).collect();
        if width_digits >= 3
            && let Some(height) = known(&height)
        {
            return Some(height);
        }
    }

    // A height token: `AVC-720`, `-1080`, `_720p`.
    stem.split(['-', '_', '.'])
        .rev()
        .find_map(|token| known(token).or_else(|| token.strip_suffix('p').and_then(known)))
}

/// The subtitle format a sidecar URL names: `(format, container)`.
pub fn subtitle_format(url: &str) -> Option<(&'static str, &'static str)> {
    let path = url_path(url);
    match extension(&path) {
        Some("ttml" | "xml" | "dfxp") => return Some(("ttml", "ttml")),
        Some("vtt" | "webvtt") => return Some(("webvtt", "vtt")),
        Some("srt") => return Some(("srt", "srt")),
        _ => {}
    }
    // ARD serves EBU-TT-D, a TTML profile, from extension-less paths.
    if path.contains("/ebutt/") || path.contains("ebu-tt") {
        return Some(("ttml", "ttml"));
    }
    None
}

fn media_resource(
    url: &str,
    variant: Variant,
    size_estimate: Option<i64>,
) -> Option<PluginHttpResource> {
    if !is_http_url(url) {
        return None;
    }
    let path = url_path(url);
    let extension = extension(&path);
    let (transport, container) = match extension {
        Some("m3u8") => (HttpTransport::Hls, None),
        Some(extension @ ("mp4" | "m4v" | "webm" | "mkv" | "ts" | "mov")) => {
            (HttpTransport::DirectFile, Some(extension.to_string()))
        }
        _ => (HttpTransport::DirectFile, None),
    };
    Some(PluginHttpResource {
        transport,
        role: HttpResourceRole::Muxed,
        url: url.to_string(),
        expires_at: None,
        headers: Vec::new(),
        auth_ref: None,
        size_bytes: None,
        size_estimate,
        validators: PluginHttpValidators::default(),
        container,
        video: infer_height(url).map(|height| PluginHttpVideoTrack {
            height: Some(height),
            ..PluginHttpVideoTrack::default()
        }),
        audio: variant.audio_description.then(|| PluginHttpAudioTrack {
            role: Some(HttpAudioRole::AudioDescription),
            ..PluginHttpAudioTrack::default()
        }),
        subtitle: None,
        separate_tracks: false,
        range_support: HttpRangeSupport::Unknown,
    })
}

fn subtitle_resource(url: &str) -> Option<PluginHttpResource> {
    if !is_http_url(url) {
        return None;
    }
    let format = subtitle_format(url);
    Some(PluginHttpResource {
        transport: HttpTransport::DirectFile,
        role: HttpResourceRole::Subtitle,
        url: url.to_string(),
        expires_at: None,
        headers: Vec::new(),
        auth_ref: None,
        size_bytes: None,
        size_estimate: None,
        validators: PluginHttpValidators::default(),
        container: format.map(|(_, container)| container.to_string()),
        video: None,
        audio: None,
        subtitle: Some(PluginHttpSubtitleTrack {
            language: None,
            role: None,
            format: format.map(|(format, _)| format.to_string()),
        }),
        separate_tracks: false,
        range_support: HttpRangeSupport::Unknown,
    })
}

fn provider_page_resource(page_url: &str) -> PluginHttpResource {
    PluginHttpResource {
        transport: HttpTransport::ProviderPage,
        role: HttpResourceRole::Muxed,
        url: page_url.to_string(),
        expires_at: None,
        headers: Vec::new(),
        auth_ref: None,
        size_bytes: None,
        size_estimate: None,
        validators: PluginHttpValidators::default(),
        container: None,
        video: None,
        audio: None,
        subtitle: None,
        separate_tracks: false,
        range_support: HttpRangeSupport::Unknown,
    }
}

/// The release for one entry, or `None` when it names nothing to fetch.
pub fn build_result(entry: &Entry) -> Option<PluginSearchResult> {
    let item_id = entry.id.trim();
    if item_id.is_empty() {
        return None;
    }
    let variant = variant(entry);
    let size_estimate = entry
        .size
        .filter(|size| *size > 0)
        .and_then(|size| i64::try_from(size).ok());

    // Lowest quality first, each URL once. `size` describes `url_video`.
    let mut resources: Vec<PluginHttpResource> = Vec::new();
    for (url, estimate) in [
        (&entry.url_video_low, None),
        (&entry.url_video, size_estimate),
        (&entry.url_video_hd, None),
    ] {
        if resources.iter().any(|resource| resource.url == *url) {
            continue;
        }
        if let Some(resource) = media_resource(url, variant, estimate) {
            resources.push(resource);
        }
    }
    if resources.is_empty() {
        return None;
    }
    let geo = release_geo(&resources);
    if let Some(subtitle) = subtitle_resource(&entry.url_subtitle) {
        resources.push(subtitle);
    }

    let website = is_http_url(&entry.url_website).then(|| entry.url_website.clone());
    // The programme page is the durable identity. An entry without one is
    // identified by its default media URL instead, and offers no page.
    let page_url = website.clone().unwrap_or_else(|| {
        resources
            .iter()
            .find(|resource| resource.role == HttpResourceRole::Muxed)
            .map(|resource| resource.url.clone())
            .unwrap_or_default()
    });
    if let Some(website) = &website {
        resources.push(provider_page_resource(website));
    }

    let duration_seconds = entry.duration.filter(|seconds| *seconds > 0);
    let channel = entry.channel.trim();
    let mut extra: HashMap<String, Value> = HashMap::new();
    extra.insert("raw_title".into(), json!(entry.title));
    extra.insert("topic".into(), json!(entry.topic));
    if !channel.is_empty() {
        extra.insert("channel".into(), json!(channel));
    }
    if let Some(duration) = duration_seconds {
        extra.insert("duration_seconds".into(), json!(duration));
    }
    if let Some(size) = size_estimate {
        extra.insert("size_estimate".into(), json!(size));
    }
    if let Some(timestamp) = entry.filmliste_timestamp {
        extra.insert("filmliste_timestamp".into(), json!(timestamp));
    }
    extra.insert("audio_description".into(), json!(variant.audio_description));
    extra.insert("sign_language".into(), json!(variant.sign_language));
    if let Some(geo) = geo {
        extra.insert("geo".into(), json!(geo.hint()));
        extra.insert("geo_region".into(), json!(geo.region()));
    }
    let description: String = entry.description.chars().take(500).collect();
    if !description.is_empty() {
        extra.insert("description".into(), json!(description));
    }
    extra.insert(
        "has_subtitles".into(),
        json!(is_http_url(&entry.url_subtitle)),
    );

    Some(PluginSearchResult {
        title: release_title(entry),
        link: Some(page_url.clone()),
        published_at: entry.timestamp.and_then(rfc3339_from_unix),
        provider_extra: extra,
        guid: Some(guid(item_id)),
        info_url: website,
        source_kind: Some(IndexerSourceKind::Http),
        protocol: Some(IndexerProtocol::Http),
        http: Some(PluginHttpRelease {
            provider: PROVIDER.to_string(),
            item_id: item_id.to_string(),
            page_url,
            duration_seconds,
            channel: (!channel.is_empty()).then(|| PluginHttpChannel {
                id: None,
                name: channel.to_string(),
            }),
            availability: match geo {
                Some(Geo::Worldwide) => HttpAvailability::Available,
                Some(Geo::Germany) => HttpAvailability::GeoRestricted,
                None => HttpAvailability::Unknown,
            },
            rights_window: None,
            playlist: None,
            resources,
            resolve: None,
        }),
        ..PluginSearchResult::default()
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix timestamp. `None` past year 9999.
pub fn rfc3339_from_unix(seconds: u64) -> Option<String> {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(days).ok()?);
    if year > 9999 {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    ))
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
