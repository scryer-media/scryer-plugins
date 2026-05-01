use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    current_sdk_constraint, execute_full_search, Capabilities, IndexerCategoryModel,
    IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities,
    IndexerProtocol, IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, NewznabConfig,
    PluginDescriptor, PluginResult, ProviderDescriptor, ScoringPolicy, SearchRequest, SDK_VERSION,
};

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "nzbgeek".to_string(),
        name: "NZBGeek Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "nzbgeek".to_string(),
            provider_aliases: vec![],
            source_kind: IndexerSourceKind::Usenet,
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
                anidb_search: false,
                rss: true,
                protocols: vec![IndexerProtocol::Usenet],
                feed_modes: vec![
                    IndexerFeedMode::Recent,
                    IndexerFeedMode::Rss,
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![
                    IndexerSearchInput::TitleQuery,
                    IndexerSearchInput::IdQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::Category,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec![
                    "imdb_id".into(),
                    "tvdb_id".into(),
                    "tmdb_id".into(),
                    "tvmaze_id".into(),
                    "tvrage_id".into(),
                ],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(100),
                    max_page_size: Some(100),
                    max_pages: Some(10),
                    api_quota_supported: true,
                    grab_quota_supported: true,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: None,
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    subtitles: true,
                    grabs: true,
                    comments: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    password_hint: true,
                    protection_hint: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![
                ScoringPolicy {
                    name: "nzbgeek_vote_penalty".to_string(),
                    rego_source: REGO_VOTE_PENALTY.to_string(),
                    applied_facets: vec![],
                },
                ScoringPolicy {
                    name: "nzbgeek_language_bonus".to_string(),
                    rego_source: REGO_LANGUAGE_BONUS.to_string(),
                    applied_facets: vec![],
                },
            ],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            rate_limit_seconds: None,
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let config = NewznabConfig::from_extism()?;
    let response = execute_full_search(&config, &req, nzbgeek_metadata_extractor)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

// ---------------------------------------------------------------------------
// NZBGeek-specific metadata extraction
// ---------------------------------------------------------------------------

fn nzbgeek_metadata_extractor(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut thumbs_up: Option<i32> = None;
    let mut thumbs_down: Option<i32> = None;
    let mut languages = Vec::new();
    let mut subtitles: Vec<String> = Vec::new();
    let mut grabs: Option<i64> = None;
    let mut password: Option<String> = None;

    for (name, value) in pairs {
        let normalized: String = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        match normalized.as_str() {
            "thumbsup" | "thumbup" => {
                thumbs_up = value.trim().replace(',', "").parse().ok();
            }
            "thumbsdown" | "thumbdown" => {
                thumbs_down = value.trim().replace(',', "").parse().ok();
            }
            "language" => {
                let items: Vec<String> = value
                    .split(" - ")
                    .map(|v| v.trim())
                    .filter(|v| !v.is_empty())
                    .map(ToString::to_string)
                    .collect();
                languages.extend(items);
            }
            "subs" => {
                let items: Vec<String> = value
                    .split(" - ")
                    .map(|v| v.trim())
                    .filter(|v| !v.is_empty())
                    .map(ToString::to_string)
                    .collect();
                subtitles.extend(items);
            }
            "grabs" => {
                grabs = value.trim().replace(',', "").parse().ok();
            }
            "password" => {
                let trimmed = value.trim();
                if !trimmed.is_empty() && trimmed != "0" {
                    password = Some(trimmed.to_string());
                }
            }
            _ => {}
        }
    }

    let mut extra = HashMap::new();
    if let Some(v) = thumbs_up {
        extra.insert("thumbs_up".to_string(), serde_json::Value::from(v));
    }
    if let Some(v) = thumbs_down {
        extra.insert("thumbs_down".to_string(), serde_json::Value::from(v));
    }
    if !subtitles.is_empty() {
        extra.insert(
            "subtitles".to_string(),
            serde_json::to_value(&subtitles).unwrap_or_default(),
        );
    }
    if let Some(ref pw) = password {
        extra.insert("password".to_string(), serde_json::Value::from(pw.as_str()));
    }

    (languages, grabs, extra)
}

// ---------------------------------------------------------------------------
// Rego scoring policies
// ---------------------------------------------------------------------------

const REGO_VOTE_PENALTY: &str = r#"package scryer.rules.user.plugin_nzbgeek_vote_penalty
import rego.v1

score_entry["nzbgeek_thumbs_down"] := penalty if {
    td := input.release.extra.thumbs_down
    td > 5
    extra := min([td - 5, 10])
    penalty := -2400 - (extra * 300)
}
"#;

const REGO_LANGUAGE_BONUS: &str = r#"package scryer.rules.user.plugin_nzbgeek_language_bonus
import rego.v1

score_entry["nzbgeek_english_confirmed"] := 200 if {
    langs := input.release.extra.languages
    count(langs) > 0
    some lang in langs
    lower(lang) == "english"
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn extracts_thumbs_up_and_down() {
        let p = pairs(&[("thumbs_up", "42"), ("thumbs_down", "3")]);
        let (_, _, extra) = nzbgeek_metadata_extractor(&p);
        assert_eq!(extra.get("thumbs_up"), Some(&serde_json::Value::from(42)));
        assert_eq!(extra.get("thumbs_down"), Some(&serde_json::Value::from(3)));
    }

    #[test]
    fn extracts_language() {
        let p = pairs(&[("language", "English - French")]);
        let (languages, _, _) = nzbgeek_metadata_extractor(&p);
        assert_eq!(languages, vec!["English", "French"]);
    }

    #[test]
    fn extracts_subs() {
        let p = pairs(&[("subs", "English - Spanish")]);
        let (_, _, extra) = nzbgeek_metadata_extractor(&p);
        let subs = extra.get("subtitles").unwrap();
        let arr: Vec<String> = serde_json::from_value(subs.clone()).unwrap();
        assert_eq!(arr, vec!["English", "Spanish"]);
    }

    #[test]
    fn extracts_grabs_with_comma() {
        let p = pairs(&[("grabs", "1,234")]);
        let (_, grabs, _) = nzbgeek_metadata_extractor(&p);
        assert_eq!(grabs, Some(1234));
    }

    #[test]
    fn extracts_password() {
        let p = pairs(&[("password", "1")]);
        let (_, _, extra) = nzbgeek_metadata_extractor(&p);
        assert_eq!(extra.get("password"), Some(&serde_json::Value::from("1")));
    }

    #[test]
    fn ignores_password_zero() {
        let p = pairs(&[("password", "0")]);
        let (_, _, extra) = nzbgeek_metadata_extractor(&p);
        assert!(extra.get("password").is_none());
    }
}
