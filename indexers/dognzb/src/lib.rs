use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    execute_full_search, standard_config_fields, Capabilities, NewznabConfig, PluginDescriptor,
    ScoringPolicy, SearchRequest,
};

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        name: "DogNZB Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "usenet_indexer".to_string(),
        provider_type: "dognzb".to_string(),
        provider_aliases: vec![],
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
        scoring_policies: vec![ScoringPolicy {
            name: "dognzb_rating_bonus".to_string(),
            rego_source: REGO_RATING_BONUS.to_string(),
            applied_facets: vec![],
        }],
        config_fields: standard_config_fields(),
        allowed_hosts: vec![],
        rate_limit_seconds: None,
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let mut config = NewznabConfig::from_extism()?;
    config.page_size = 100;
    let response = execute_full_search(&config, &req, dognzb_metadata_extractor)?;
    Ok(serde_json::to_string(&response)?)
}

// ---------------------------------------------------------------------------
// DogNZB-specific metadata extraction
// ---------------------------------------------------------------------------

fn dognzb_metadata_extractor(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut rating: Option<i32> = None;
    let mut genres: Vec<String> = Vec::new();
    let mut comments: Option<i32> = None;
    let mut grabs: Option<i64> = None;

    for (name, value) in pairs {
        let normalized: String = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        match normalized.as_str() {
            "rating" => {
                rating = value.trim().replace(',', "").parse().ok();
            }
            "genre" => {
                let items: Vec<String> = value
                    .split(", ")
                    .map(|v| v.trim())
                    .filter(|v| !v.is_empty())
                    .map(ToString::to_string)
                    .collect();
                genres.extend(items);
            }
            "comments" => {
                comments = value.trim().replace(',', "").parse().ok();
            }
            "grabs" => {
                grabs = value.trim().replace(',', "").parse().ok();
            }
            _ => {}
        }
    }

    let mut extra = HashMap::new();
    if let Some(v) = rating {
        extra.insert("rating".to_string(), serde_json::Value::from(v));
    }
    if !genres.is_empty() {
        extra.insert(
            "genres".to_string(),
            serde_json::to_value(&genres).unwrap_or_default(),
        );
    }
    if let Some(v) = comments {
        extra.insert("comments".to_string(), serde_json::Value::from(v));
    }

    (vec![], grabs, extra)
}

// ---------------------------------------------------------------------------
// Rego scoring policies
// ---------------------------------------------------------------------------

const REGO_RATING_BONUS: &str = r#"package scryer.rules.user.plugin_dognzb_rating_bonus
import rego.v1

score_entry["dognzb_high_rating"] := 150 if {
    input.release.extra.rating >= 80
}

score_entry["dognzb_mid_rating"] := 50 if {
    input.release.extra.rating >= 60
    input.release.extra.rating < 80
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
    fn extracts_rating() {
        let p = pairs(&[("rating", "90")]);
        let (_, _, extra) = dognzb_metadata_extractor(&p);
        assert_eq!(extra.get("rating"), Some(&serde_json::Value::from(90)));
    }

    #[test]
    fn extracts_genres() {
        let p = pairs(&[("genre", "Adventure, Animation, Anime, Drama, Fantasy")]);
        let (_, _, extra) = dognzb_metadata_extractor(&p);
        let genres: Vec<String> =
            serde_json::from_value(extra.get("genres").unwrap().clone()).unwrap();
        assert_eq!(
            genres,
            vec!["Adventure", "Animation", "Anime", "Drama", "Fantasy"]
        );
    }

    #[test]
    fn extracts_comments() {
        let p = pairs(&[("comments", "12")]);
        let (_, _, extra) = dognzb_metadata_extractor(&p);
        assert_eq!(extra.get("comments"), Some(&serde_json::Value::from(12)));
    }

    #[test]
    fn extracts_grabs_with_comma() {
        let p = pairs(&[("grabs", "1,234")]);
        let (_, grabs, _) = dognzb_metadata_extractor(&p);
        assert_eq!(grabs, Some(1234));
    }

    #[test]
    fn returns_empty_languages() {
        let p = pairs(&[("rating", "80")]);
        let (languages, _, _) = dognzb_metadata_extractor(&p);
        assert!(languages.is_empty());
    }

    #[test]
    fn omits_missing_fields() {
        let p = pairs(&[]);
        let (languages, grabs, extra) = dognzb_metadata_extractor(&p);
        assert!(languages.is_empty());
        assert!(grabs.is_none());
        assert!(extra.is_empty());
    }
}
