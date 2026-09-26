//! External ids and deterministic item keys.
//!
//! An item key must name the same title on every sync of a list, or the host
//! sees a departure and an arrival instead of an unchanged row. Keys are
//! therefore built from the strongest id the provider gave, in a fixed order:
//! TMDb (scoped by kind, because TMDb numbers movies and series separately),
//! then IMDb, then TVDB, then a normalized title and year.

use scryer_plugin_sdk::{ListExternalId, ListMediaKind, ListPluginItem};

pub const SOURCE_TMDB: &str = "tmdb";
pub const SOURCE_IMDB: &str = "imdb";
pub const SOURCE_TVDB: &str = "tvdb";

/// Ids a provider supplied for one entry, already normalized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ids {
    pub tmdb: Option<String>,
    pub imdb: Option<String>,
    pub tvdb: Option<String>,
}

impl Ids {
    pub fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.imdb.is_none() && self.tvdb.is_none()
    }

    pub fn with_tmdb(mut self, value: Option<String>) -> Self {
        if self.tmdb.is_none() {
            self.tmdb = value.and_then(|value| positive_id(&value));
        }
        self
    }

    pub fn with_imdb(mut self, value: Option<String>) -> Self {
        if self.imdb.is_none() {
            self.imdb = value.and_then(|value| normalize_imdb(&value));
        }
        self
    }

    pub fn with_tvdb(mut self, value: Option<String>) -> Self {
        if self.tvdb.is_none() {
            self.tvdb = value.and_then(|value| positive_id(&value));
        }
        self
    }
}

/// The host's `ExternalId.kind` vocabulary.
pub fn kind_str(kind: ListMediaKind) -> Option<&'static str> {
    match kind {
        ListMediaKind::Movie => Some("movie"),
        ListMediaKind::Series => Some("series"),
        ListMediaKind::Anime => None,
    }
}

pub fn external_ids(ids: &Ids, kind: Option<ListMediaKind>) -> Vec<ListExternalId> {
    let kind = kind.and_then(kind_str).map(str::to_string);
    let mut out = Vec::new();
    let mut push = |source: &str, id: &Option<String>| {
        if let Some(id) = id {
            out.push(ListExternalId {
                source: source.to_string(),
                kind: kind.clone(),
                id: id.clone(),
            });
        }
    };
    push(SOURCE_TMDB, &ids.tmdb);
    push(SOURCE_IMDB, &ids.imdb);
    push(SOURCE_TVDB, &ids.tvdb);
    out
}

pub fn item_key(
    ids: &Ids,
    kind: Option<ListMediaKind>,
    title: Option<&str>,
    year: Option<i32>,
) -> Option<String> {
    let kind = kind.and_then(kind_str);
    if let (Some(tmdb), Some(kind)) = (&ids.tmdb, kind) {
        return Some(format!("tmdb:{kind}:{tmdb}"));
    }
    if let Some(imdb) = &ids.imdb {
        return Some(format!("imdb:{imdb}"));
    }
    if let Some(tvdb) = &ids.tvdb {
        return Some(format!("tvdb:{}:{tvdb}", kind.unwrap_or("series")));
    }
    if let Some(tmdb) = &ids.tmdb {
        return Some(format!("tmdb:{tmdb}"));
    }
    let title = normalize_title(title?);
    if title.is_empty() {
        return None;
    }
    Some(match year {
        Some(year) => format!("title:{title}:{year}"),
        None => format!("title:{title}"),
    })
}

/// Build a list item from ids and hints. Returns `None` when the entry has
/// neither an id nor a title, because such a row can never be matched.
pub fn build_item(
    ids: &Ids,
    kind: Option<ListMediaKind>,
    title: Option<String>,
    year: Option<i32>,
) -> Option<ListPluginItem> {
    let title = title
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty());
    let item_key = item_key(ids, kind, title.as_deref(), year)?;
    Some(ListPluginItem {
        item_key,
        kind_hint: kind,
        title,
        year,
        external_ids: external_ids(ids, kind),
        ..ListPluginItem::default()
    })
}

/// Drop later duplicates of an item key, then number the survivors from 1.
pub fn dedupe_and_rank(items: Vec<ListPluginItem>, first_rank: u32) -> Vec<ListPluginItem> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        if seen.insert(item.item_key.clone()) {
            item.rank = Some(first_rank + out.len() as u32);
            out.push(item);
        }
    }
    out
}

/// Find an IMDb title id (`tt` and at least seven digits) anywhere in text.
pub fn normalize_imdb(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index].eq_ignore_ascii_case(&b't') && bytes[index + 1].eq_ignore_ascii_case(&b't')
        {
            let digits = bytes[index + 2..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let boundary_before = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
            if digits >= 7 && boundary_before {
                return Some(format!("tt{}", &value[index + 2..index + 2 + digits]));
            }
        }
        index += 1;
    }
    None
}

/// A positive integer id, from a number or numeric text.
pub fn positive_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let number = trimmed.parse::<u64>().ok()?;
    (number > 0).then(|| number.to_string())
}

pub fn json_id(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::Number(number) => {
            number.as_u64().and_then(|n| positive_id(&n.to_string()))
        }
        serde_json::Value::String(text) => positive_id(text),
        _ => None,
    }
}

pub fn json_text(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub fn json_year(value: Option<&serde_json::Value>) -> Option<i32> {
    match value? {
        serde_json::Value::Number(number) => number.as_i64().and_then(plausible_year),
        serde_json::Value::String(text) => year_from_date(text),
        _ => None,
    }
}

/// Lowercase ASCII words joined by `-`; anything else is dropped.
pub fn normalize_title(title: &str) -> String {
    title
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

fn plausible_year(year: i64) -> Option<i32> {
    (1870..=2200).contains(&year).then_some(year as i32)
}

/// The year of an ISO-style date (`2019-05-01`) or a bare year.
pub fn year_from_date(value: &str) -> Option<i32> {
    let head = value.trim().get(..4)?;
    if !head.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    plausible_year(head.parse().ok()?)
}

/// Split `Name (2019)` or `Name 2019` into a title and a year.
pub fn split_title_year(value: &str) -> (String, Option<i32>) {
    let trimmed = value.trim();
    if let Some(open) = trimmed.rfind('(')
        && trimmed.ends_with(')')
    {
        let inner = &trimmed[open + 1..trimmed.len() - 1];
        if inner.len() == 4
            && let Some(year) = year_from_date(inner)
        {
            return (trimmed[..open].trim().to_string(), Some(year));
        }
    }
    if let Some((head, tail)) = trimmed.rsplit_once(' ')
        && tail.len() == 4
        && !head.trim().is_empty()
        && let Some(year) = year_from_date(tail)
    {
        return (head.trim().to_string(), Some(year));
    }
    (trimmed.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_prefer_scoped_tmdb_then_imdb_then_tvdb_then_title() {
        let all = Ids::default()
            .with_tmdb(Some("12".into()))
            .with_imdb(Some("tt0000012".into()))
            .with_tvdb(Some("99".into()));
        assert_eq!(
            item_key(&all, Some(ListMediaKind::Movie), None, None).unwrap(),
            "tmdb:movie:12"
        );
        assert_eq!(item_key(&all, None, None, None).unwrap(), "imdb:tt0000012");
        let tvdb = Ids::default().with_tvdb(Some("99".into()));
        assert_eq!(item_key(&tvdb, None, None, None).unwrap(), "tvdb:series:99");
        assert_eq!(
            item_key(
                &Ids::default(),
                None,
                Some("A Synthetic Film: Part II"),
                Some(2031)
            )
            .unwrap(),
            "title:a-synthetic-film-part-ii:2031"
        );
        assert_eq!(item_key(&Ids::default(), None, Some("  "), None), None);
    }

    #[test]
    fn external_ids_carry_the_kind_vocabulary() {
        let ids = Ids::default().with_tmdb(Some("5".into()));
        let series = external_ids(&ids, Some(ListMediaKind::Series));
        assert_eq!(series[0].source, "tmdb");
        assert_eq!(series[0].kind.as_deref(), Some("series"));
        assert_eq!(external_ids(&ids, None)[0].kind, None);
    }

    #[test]
    fn imdb_ids_are_found_in_urls_and_guids() {
        assert_eq!(
            normalize_imdb("https://www.imdb.com/title/tt9900001/").as_deref(),
            Some("tt9900001")
        );
        assert_eq!(
            normalize_imdb("imdb://tt99000012").as_deref(),
            Some("tt99000012")
        );
        assert_eq!(normalize_imdb("att9900001"), None);
        assert_eq!(normalize_imdb("tt123"), None);
    }

    #[test]
    fn titles_and_years_split() {
        assert_eq!(
            split_title_year("Fixture Feature (2030)"),
            ("Fixture Feature".to_string(), Some(2030))
        );
        assert_eq!(
            split_title_year("Fixture Feature 2030"),
            ("Fixture Feature".to_string(), Some(2030))
        );
        assert_eq!(split_title_year("2030"), ("2030".to_string(), None));
        assert_eq!(
            split_title_year("Fixture (Director Cut)"),
            ("Fixture (Director Cut)".to_string(), None)
        );
        assert_eq!(year_from_date("2031-04-02"), Some(2031));
        assert_eq!(year_from_date(""), None);
    }

    #[test]
    fn dedupe_keeps_first_and_ranks_contiguously() {
        let item = |key: &str| ListPluginItem {
            item_key: key.to_string(),
            ..Default::default()
        };
        let ranked = dedupe_and_rank(vec![item("a"), item("b"), item("a"), item("c")], 1);
        let keys: Vec<_> = ranked
            .iter()
            .map(|item| (item.item_key.as_str(), item.rank))
            .collect();
        assert_eq!(keys, vec![("a", Some(1)), ("b", Some(2)), ("c", Some(3))]);
    }
}
