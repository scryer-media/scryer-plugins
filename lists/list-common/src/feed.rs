//! RSS 2.0, RSS 1.0 and Atom parsing for list feeds.
//!
//! Only what a list needs is kept: each entry's title, links, guids,
//! categories and any id-bearing elements. Ids are read from `imdb://`,
//! `tmdb://` and `tvdb://` guids (the Plex watchlist convention), from IMDb,
//! TMDb and TVDB page URLs, from elements named like `imdb_id`, `tmdbid` or
//! `tvdb`, and from newznab-style `<attr name="imdb" value="..."/>`.

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart, Event};
use scryer_plugin_sdk::{ListMediaKind, PluginError};

use crate::error::permanent;
use crate::ids::{Ids, normalize_imdb, positive_id, split_title_year, year_from_date};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeedEntry {
    pub title: Option<String>,
    pub year: Option<i32>,
    pub ids: Ids,
    pub kind: Option<ListMediaKind>,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feed {
    pub title: Option<String>,
    pub link: Option<String>,
    pub entries: Vec<FeedEntry>,
}

#[derive(Default)]
struct RawEntry {
    title: String,
    year: String,
    links: Vec<String>,
    guids: Vec<String>,
    categories: Vec<String>,
    imdb: Vec<String>,
    tmdb: Vec<String>,
    tvdb: Vec<String>,
}

fn local(event: &BytesStart<'_>) -> String {
    event.local_name().as_ref().to_ascii_lowercase()
}

fn attribute(event: &BytesStart<'_>, name: &str) -> Option<String> {
    event
        .attributes()
        .flatten()
        .find(|attribute| {
            attribute
                .key
                .local_name()
                .as_ref()
                .eq_ignore_ascii_case(name)
        })
        .and_then(|attribute| attribute.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_reference(reference: &BytesRef<'_>) -> String {
    if let Ok(Some(ch)) = reference.resolve_char_ref() {
        return ch.to_string();
    }
    match reference.xml_content(XmlVersion::Implicit1_0).as_ref() {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        other => format!("&{other};"),
    }
}

fn is_entry(name: &str) -> bool {
    name == "item" || name == "entry"
}

fn id_field(name: &str) -> Option<&'static str> {
    match name {
        "imdb" | "imdbid" | "imdb_id" | "imdb-id" => Some("imdb"),
        "tmdb" | "tmdbid" | "tmdb_id" | "tmdb-id" => Some("tmdb"),
        "tvdb" | "tvdbid" | "tvdb_id" | "tvdb-id" => Some("tvdb"),
        _ => None,
    }
}

fn push_id(entry: &mut RawEntry, which: &str, value: String) {
    match which {
        "imdb" => entry.imdb.push(value),
        "tmdb" => entry.tmdb.push(value),
        _ => entry.tvdb.push(value),
    }
}

/// Apply attribute-carried data from a start or empty element.
fn apply_attributes(entry: &mut RawEntry, name: &str, event: &BytesStart<'_>) {
    match name {
        "link" => {
            if let Some(href) = attribute(event, "href") {
                entry.links.push(href);
            }
        }
        "category" => {
            if let Some(term) = attribute(event, "term") {
                entry.categories.push(term);
            }
        }
        "attr" => {
            if let (Some(key), Some(value)) = (attribute(event, "name"), attribute(event, "value"))
                && let Some(which) = id_field(&key.to_ascii_lowercase())
            {
                push_id(entry, which, value);
            }
        }
        _ => {}
    }
}

fn apply_text(entry: &mut RawEntry, name: &str, text: String) {
    if text.is_empty() {
        return;
    }
    match name {
        "title" => entry.title = text,
        "year" => entry.year = text,
        "link" => entry.links.push(text),
        "guid" | "id" => entry.guids.push(text),
        "category" => entry.categories.push(text),
        other => {
            if let Some(which) = id_field(other) {
                push_id(entry, which, text);
            }
        }
    }
}

/// Parse a feed body. A document with no RSS or Atom root, or one that is not
/// well-formed XML, is a permanent failure.
pub fn parse_feed(body: &str) -> Result<Feed, PluginError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(false);

    let mut feed = Feed::default();
    let mut saw_root = false;
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut entry: Option<(usize, RawEntry)> = None;

    loop {
        let event = reader
            .read_event()
            .map_err(|_| permanent("the feed is not well-formed XML"))?;
        match event {
            Event::Start(start) => {
                let name = local(&start);
                if stack.is_empty() && matches!(name.as_str(), "rss" | "rdf" | "feed") {
                    saw_root = true;
                }
                if entry.is_none() && is_entry(&name) {
                    entry = Some((stack.len(), RawEntry::default()));
                } else if let Some((_, raw)) = entry.as_mut() {
                    apply_attributes(raw, &name, &start);
                } else if name == "link"
                    && feed.link.is_none()
                    && let Some(href) = attribute(&start, "href")
                {
                    feed.link = Some(href);
                }
                stack.push(name);
                text.clear();
            }
            Event::Empty(empty) => {
                let name = local(&empty);
                if let Some((_, raw)) = entry.as_mut() {
                    apply_attributes(raw, &name, &empty);
                } else if name == "link"
                    && feed.link.is_none()
                    && let Some(href) = attribute(&empty, "href")
                {
                    feed.link = Some(href);
                }
            }
            Event::Text(chunk) => text.push_str(&chunk.xml_content(XmlVersion::Implicit1_0)),
            Event::CData(chunk) => text.push_str(&chunk.xml_content(XmlVersion::Implicit1_0)),
            Event::GeneralRef(reference) => text.push_str(&resolve_reference(&reference)),
            Event::End(_) => {
                let Some(name) = stack.pop() else {
                    return Err(permanent("the feed is not well-formed XML"));
                };
                let value = text.trim().to_string();
                text.clear();
                match entry.as_mut() {
                    Some((depth, _)) if *depth == stack.len() && is_entry(&name) => {
                        if let Some((_, raw)) = entry.take()
                            && let Some(parsed) = finish_entry(raw)
                        {
                            feed.entries.push(parsed);
                        }
                    }
                    Some((_, raw)) => apply_text(raw, &name, value),
                    None => {
                        let parent = stack.last().map(String::as_str);
                        if matches!(parent, Some("channel" | "feed")) {
                            if name == "title" && feed.title.is_none() && !value.is_empty() {
                                feed.title = Some(value);
                            } else if name == "link" && feed.link.is_none() && !value.is_empty() {
                                feed.link = Some(value);
                            }
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !saw_root {
        return Err(permanent("the document is not an RSS or Atom feed"));
    }
    Ok(feed)
}

fn kind_from_category(category: &str) -> Option<ListMediaKind> {
    match category.trim().to_ascii_lowercase().as_str() {
        "movie" | "movies" | "film" | "films" => Some(ListMediaKind::Movie),
        "show" | "shows" | "tv" | "tvshow" | "tv show" | "series" => Some(ListMediaKind::Series),
        _ => None,
    }
}

/// Ids and a kind read from one guid or link value.
fn scan_reference(value: &str, ids: &mut Ids, kind: &mut Option<ListMediaKind>) {
    let lower = value.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("tmdb://") {
        *ids = std::mem::take(ids).with_tmdb(Some(rest.to_string()));
    } else if let Some(rest) = lower.strip_prefix("tvdb://") {
        *ids = std::mem::take(ids).with_tvdb(Some(rest.to_string()));
    } else if lower.contains("themoviedb.org/") {
        for (segment, segment_kind) in [
            ("/movie/", ListMediaKind::Movie),
            ("/tv/", ListMediaKind::Series),
        ] {
            if let Some((_, rest)) = lower.split_once(segment) {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if positive_id(&digits).is_some() {
                    *ids = std::mem::take(ids).with_tmdb(Some(digits));
                    kind.get_or_insert(segment_kind);
                }
            }
        }
    } else if lower.contains("thetvdb.com/") && lower.contains("id=") {
        if let Some((_, rest)) = lower.split_once("id=") {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            *ids = std::mem::take(ids).with_tvdb(Some(digits));
        }
    }
    if lower.starts_with("imdb://") || lower.contains("imdb.com/") || lower.starts_with("tt") {
        *ids = std::mem::take(ids).with_imdb(normalize_imdb(value));
    }
}

fn finish_entry(raw: RawEntry) -> Option<FeedEntry> {
    let mut ids = Ids::default();
    let mut kind = raw
        .categories
        .iter()
        .find_map(|category| kind_from_category(category));
    for value in raw.guids.iter().chain(raw.links.iter()) {
        scan_reference(value, &mut ids, &mut kind);
    }
    for value in raw.imdb {
        ids = ids.with_imdb(Some(value));
    }
    for value in raw.tmdb {
        ids = ids.with_tmdb(Some(value));
    }
    for value in raw.tvdb {
        ids = ids.with_tvdb(Some(value));
    }
    let (title, title_year) = split_title_year(&raw.title);
    let year = year_from_date(&raw.year).or(title_year);
    let title = (!title.is_empty()).then_some(title);
    if title.is_none() && ids.is_empty() {
        return None;
    }
    Some(FeedEntry {
        title,
        year,
        ids,
        kind,
        link: raw.links.into_iter().next(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/" xmlns:x="urn:fixture">
  <channel>
    <title>Fixture Watchlist &amp; Friends</title>
    <link>https://feeds.example.test/watchlist</link>
    <item>
      <title>Fixture Feature One (2030)</title>
      <category>movie</category>
      <guid isPermaLink="false">imdb://tt9900001</guid>
      <guid isPermaLink="false">tmdb://990001</guid>
      <media:content url="https://images.example.test/1.jpg"/>
    </item>
    <item>
      <title><![CDATA[Fixture Serial Two]]></title>
      <category>show</category>
      <guid>tvdb://880002</guid>
    </item>
    <item>
      <title>Fixture &#8220;Three&#8221; 2029</title>
      <link>https://www.themoviedb.org/movie/990003-fixture-three</link>
    </item>
    <item>
      <title>Fixture Four</title>
      <x:tmdbid>990004</x:tmdbid>
      <attr name="imdb" value="tt9900004"/>
    </item>
    <item><description>no title, no ids</description></item>
  </channel>
</rss>"#;

    #[test]
    fn rss_entries_carry_ids_kinds_and_years() {
        let feed = parse_feed(RSS).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Fixture Watchlist & Friends"));
        assert_eq!(
            feed.link.as_deref(),
            Some("https://feeds.example.test/watchlist")
        );
        assert_eq!(feed.entries.len(), 4);

        let one = &feed.entries[0];
        assert_eq!(one.title.as_deref(), Some("Fixture Feature One"));
        assert_eq!(one.year, Some(2030));
        assert_eq!(one.kind, Some(ListMediaKind::Movie));
        assert_eq!(one.ids.imdb.as_deref(), Some("tt9900001"));
        assert_eq!(one.ids.tmdb.as_deref(), Some("990001"));

        let two = &feed.entries[1];
        assert_eq!(two.title.as_deref(), Some("Fixture Serial Two"));
        assert_eq!(two.kind, Some(ListMediaKind::Series));
        assert_eq!(two.ids.tvdb.as_deref(), Some("880002"));

        let three = &feed.entries[2];
        assert_eq!(
            three.title.as_deref(),
            Some("Fixture \u{201c}Three\u{201d}")
        );
        assert_eq!(three.year, Some(2029));
        assert_eq!(three.ids.tmdb.as_deref(), Some("990003"));
        assert_eq!(three.kind, Some(ListMediaKind::Movie));

        let four = &feed.entries[3];
        assert_eq!(four.ids.tmdb.as_deref(), Some("990004"));
        assert_eq!(four.ids.imdb.as_deref(), Some("tt9900004"));
    }

    #[test]
    fn atom_entries_read_href_links_and_terms() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Fixture Atom</title>
  <link href="https://feeds.example.test/atom"/>
  <entry>
    <title>Fixture Atom Film</title>
    <id>urn:fixture:1</id>
    <link href="https://www.imdb.com/title/tt9900010/"/>
    <category term="movie"/>
  </entry>
</feed>"#;
        let feed = parse_feed(atom).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Fixture Atom"));
        assert_eq!(
            feed.link.as_deref(),
            Some("https://feeds.example.test/atom")
        );
        assert_eq!(feed.entries[0].ids.imdb.as_deref(), Some("tt9900010"));
        assert_eq!(feed.entries[0].kind, Some(ListMediaKind::Movie));
    }

    #[test]
    fn non_feeds_are_permanent_failures() {
        assert!(parse_feed("<html><body>nope</body></html>").is_err());
        assert!(parse_feed("{\"not\":\"xml\"}").is_err());
        assert!(parse_feed("<rss><channel><item><title>x</item></channel></rss>").is_err());
    }
}
