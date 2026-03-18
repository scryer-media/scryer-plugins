use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    execute_full_search, standard_config_fields, Capabilities, NewznabConfig, PluginDescriptor,
    SearchRequest,
};

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    Ok(build_descriptor_json()?)
}

fn build_descriptor_json() -> Result<String, Error> {
    let descriptor = PluginDescriptor {
        name: "Torznab Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "torrent_indexer".to_string(),
        provider_type: "torznab".to_string(),
        provider_aliases: vec!["jackett".to_string(), "prowlarr".to_string()],
        capabilities: Capabilities {
            supported_ids: HashMap::from([
                ("movie".into(), vec!["imdb_id".into()]),
                ("series".into(), vec!["tvdb_id".into()]),
                ("anime".into(), vec!["tvdb_id".into()]),
            ]),
            deduplicates_aliases: false,
            season_param: Some("season".into()),
            episode_param: Some("ep".into()),
            query_param: Some("q".into()),
            search: true,
            imdb_search: true,
            tvdb_search: true,
        },
        scoring_policies: vec![],
        config_fields: standard_config_fields(),
        allowed_hosts: vec![],
        rate_limit_seconds: Some(2),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let config = NewznabConfig::from_extism()?;
    let response = execute_full_search(&config, &req, torznab_metadata_extractor)?;
    Ok(serde_json::to_string(&response)?)
}

fn torznab_metadata_extractor(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut grabs: Option<i64> = None;
    let mut seeders: Option<i64> = None;
    let mut peers: Option<i64> = None;
    let mut downloads: Option<i64> = None;
    let mut downloadvolumefactor: Option<f64> = None;
    let mut uploadvolumefactor: Option<f64> = None;
    let mut minimumratio: Option<f64> = None;
    let mut minimumseedtime: Option<i64> = None;
    let mut info_hash: Option<String> = None;
    let mut magnet_uri: Option<String> = None;
    let mut genres: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    let mut languages: Vec<String> = Vec::new();

    for (name, value) in pairs {
        let normalized = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let trimmed = value.trim();

        match normalized.as_str() {
            "language" => {
                languages.extend(split_multi_value(trimmed));
            }
            "grabs" => {
                grabs = parse_i64(trimmed);
            }
            "seeders" => {
                seeders = parse_i64(trimmed);
            }
            "peers" | "leechers" => {
                peers = parse_i64(trimmed);
            }
            "downloads" => {
                downloads = parse_i64(trimmed);
            }
            "downloadvolumefactor" => {
                downloadvolumefactor = parse_f64(trimmed);
            }
            "uploadvolumefactor" => {
                uploadvolumefactor = parse_f64(trimmed);
            }
            "minimumratio" => {
                minimumratio = parse_f64(trimmed);
            }
            "minimumseedtime" => {
                minimumseedtime = parse_i64(trimmed);
            }
            "infohash" => {
                let normalized_hash = normalize_info_hash(trimmed);
                if !normalized_hash.is_empty() {
                    info_hash = Some(normalized_hash);
                }
            }
            "magneturl" => {
                if !trimmed.is_empty() {
                    magnet_uri = Some(trimmed.to_string());
                }
            }
            "genre" => {
                genres.extend(split_multi_value(trimmed));
            }
            "tag" => {
                tags.extend(split_multi_value(trimmed));
            }
            _ => {}
        }
    }

    let mut extra = HashMap::new();
    if let Some(value) = seeders {
        extra.insert("seeders".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = peers {
        extra.insert("peers".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = downloads {
        extra.insert("downloads".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = downloadvolumefactor {
        extra.insert(
            "downloadvolumefactor".to_string(),
            serde_json::Value::from(value),
        );
        if (value - 0.0).abs() < f64::EPSILON {
            extra.insert("freeleech".to_string(), serde_json::Value::from(true));
        }
    }
    if let Some(value) = uploadvolumefactor {
        extra.insert(
            "uploadvolumefactor".to_string(),
            serde_json::Value::from(value),
        );
    }
    if let Some(value) = minimumratio {
        extra.insert("minimumratio".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = minimumseedtime {
        extra.insert(
            "minimumseedtime".to_string(),
            serde_json::Value::from(value),
        );
    }
    if let Some(ref value) = info_hash {
        extra.insert("info_hash".to_string(), serde_json::Value::from(value.as_str()));
        // Auto-generate magnet URI if tracker didn't provide one
        if magnet_uri.is_none() {
            extra.insert(
                "magnet_uri".to_string(),
                serde_json::Value::from(build_magnet_uri(value)),
            );
        }
    }
    if let Some(value) = magnet_uri {
        extra.insert("magnet_uri".to_string(), serde_json::Value::from(value));
    }
    if !genres.is_empty() {
        extra.insert(
            "genres".to_string(),
            serde_json::to_value(dedupe(genres)).unwrap_or_default(),
        );
    }
    if !tags.is_empty() {
        extra.insert(
            "tags".to_string(),
            serde_json::to_value(dedupe(tags)).unwrap_or_default(),
        );
    }

    (dedupe(languages), grabs, extra)
}

fn split_multi_value(value: &str) -> Vec<String> {
    value
        .split(['/', '|'])
        .flat_map(|part| part.split(" - "))
        .flat_map(|part| part.split(','))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_i64(value: &str) -> Option<i64> {
    value.replace(',', "").parse::<i64>().ok()
}

fn parse_f64(value: &str) -> Option<f64> {
    value.replace(',', "").parse::<f64>().ok()
}

fn normalize_info_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn build_magnet_uri(info_hash: &str) -> String {
    const TRACKERS: &[&str] = &[
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://tracker.bittor.pw:1337/announce",
        "udp://public.popcorn-tracker.org:6969/announce",
        "udp://tracker.dler.org:6969/announce",
        "udp://exodus.desync.com:6969",
        "udp://open.demonii.com:1337/announce",
    ];

    let mut uri = format!("magnet:?xt=urn:btih:{info_hash}");
    for tracker in TRACKERS {
        uri.push_str("&tr=");
        uri.push_str(tracker);
    }
    uri
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if out
            .iter()
            .all(|existing: &String| !existing.eq_ignore_ascii_case(&value))
        {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn descriptor_is_torznab() {
        let json = build_descriptor_json().unwrap();
        assert!(json.contains("torznab"));
    }

    #[test]
    fn extracts_torrent_metadata() {
        let p = pairs(&[
            ("seeders", "42"),
            ("peers", "9"),
            ("infohash", "ABCDEF1234567890ABCDEF1234567890ABCDEF12"),
            ("magneturl", "magnet:?xt=urn:btih:abcdef"),
            ("downloadvolumefactor", "0"),
        ]);
        let (_, _, extra) = torznab_metadata_extractor(&p);
        assert_eq!(extra.get("seeders"), Some(&serde_json::Value::from(42)));
        assert_eq!(extra.get("peers"), Some(&serde_json::Value::from(9)));
        assert_eq!(
            extra.get("info_hash"),
            Some(&serde_json::Value::from(
                "abcdef1234567890abcdef1234567890abcdef12"
            ))
        );
        assert_eq!(
            extra.get("magnet_uri"),
            Some(&serde_json::Value::from("magnet:?xt=urn:btih:abcdef"))
        );
        assert_eq!(extra.get("freeleech"), Some(&serde_json::Value::from(true)));
    }

    #[test]
    fn extracts_languages_genres_and_tags() {
        let p = pairs(&[
            ("language", "English - Japanese"),
            ("genre", "Action / Sci-Fi"),
            ("tag", "remux, internal"),
        ]);
        let (languages, _, extra) = torznab_metadata_extractor(&p);
        assert_eq!(languages, vec!["English", "Japanese"]);
        assert_eq!(
            serde_json::from_value::<Vec<String>>(extra.get("genres").unwrap().clone()).unwrap(),
            vec!["Action", "Sci-Fi"]
        );
        assert_eq!(
            serde_json::from_value::<Vec<String>>(extra.get("tags").unwrap().clone()).unwrap(),
            vec!["remux", "internal"]
        );
    }

    #[test]
    fn extracts_grabs_and_ratio_rules() {
        let p = pairs(&[
            ("grabs", "1,234"),
            ("minimumratio", "1.5"),
            ("minimumseedtime", "7200"),
        ]);
        let (_, grabs, extra) = torznab_metadata_extractor(&p);
        assert_eq!(grabs, Some(1234));
        assert_eq!(
            extra.get("minimumratio"),
            Some(&serde_json::Value::from(1.5))
        );
        assert_eq!(
            extra.get("minimumseedtime"),
            Some(&serde_json::Value::from(7200))
        );
    }
}
