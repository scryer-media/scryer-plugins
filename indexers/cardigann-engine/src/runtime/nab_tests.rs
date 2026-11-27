use super::*;
use scryer_plugin_sdk::{PluginNabOperation as Op, PluginNabSearch};

const DEFINITION: &str = r#"
id: nab-fixture
name: Nab fixture
type: public
links: [https://tracker.example/]
caps:
  modes:
    search: [q]
    tv-search: [q, season, ep, imdbid, tvdbid, tmdbid, rid, tvmazeid, traktid, doubanid, year, genre]
    movie-search: [q, imdbid, tmdbid, traktid, doubanid, year, genre]
    music-search: [q, artist, album, label, track, year, genre]
    book-search: [q, author, title, publisher, year, genre]
  categorymappings:
    - { id: 42, cat: Movies, default: true }
search:
  paths:
    - path: /first
    - path: /second
  inputs:
    mode: "{{ .Query.Type }}"
    q: "{{ .Query.Q }}"
    keywords: "{{ .Keywords }}"
    author: "{{ .Query.Author }}"
    title: "{{ .Query.Title }}"
    artist: "{{ .Query.Artist }}"
    album: "{{ .Query.Album }}"
    imdb: "{{ .Query.IMDBID }}"
    episode: "{{ .Query.Episode }}"
    offset: "{{ .Query.Offset }}"
    limit: "{{ .Query.Limit }}"
  rows:
    selector: div.release
  fields:
    title: { selector: a }
    download: { selector: a, attribute: href }
"#;

fn request(operation: Op, params: &[(&str, &str)]) -> PluginSearchRequest {
    PluginSearchRequest {
        query: "  A & B  ".into(),
        limit: 20,
        facet: Some("series".into()),
        nab: Some(PluginNabSearch {
            operation,
            parameters: params
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        }),
        ..Default::default()
    }
}

fn begin_search(definition: Definition, request: PluginSearchRequest) -> Result<Step, EngineError> {
    begin(
        serde_json::to_string(&CompiledIr {
            ir_version: COMPILED_IR_VERSION,
            definition,
        })
        .unwrap(),
        Operation::Search(Box::new(request)),
        BTreeMap::new(),
    )
}

fn params(request: &PluginHttpRequest) -> BTreeMap<String, String> {
    Url::parse(&request.url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[test]
fn nab_operations_bind_prowlarr_modes_and_preserve_exact_selectors() {
    let definition = parse_definition(DEFINITION).unwrap();
    for (operation, selectors, mode, expected) in [
        (Op::Search, vec![], "search", vec![]),
        (
            Op::Movie,
            vec![("imdbid", "123"), ("year", "2020")],
            "movie",
            vec![
                (".Query.IMDBID", "tt0000123"),
                (".Query.IMDBIDShort", "123"),
                (".Query.Year", "2020"),
            ],
        ),
        (
            Op::Tv,
            vec![("season", "01"), ("ep", "002"), ("tvdbid", "567")],
            "tvsearch",
            vec![
                (".Query.Season", "01"),
                (".Query.Ep", "002"),
                (".Query.Episode", "S01E02"),
                (".Query.TVDBID", "567"),
            ],
        ),
        (
            Op::Music,
            vec![
                ("artist", "Artist & Guest"),
                ("album", "001.00"),
                ("track", "007"),
            ],
            "music",
            vec![
                (".Query.Artist", "Artist & Guest"),
                (".Query.Album", "001.00"),
                (".Query.Track", "007"),
            ],
        ),
        (
            Op::Book,
            vec![
                ("author", "Author & Guest"),
                ("title", "Exact title"),
                ("publisher", "Publisher"),
            ],
            "book",
            vec![
                (".Query.Author", "Author & Guest"),
                (".Query.Title", "Exact title"),
                (".Query.Publisher", "Publisher"),
            ],
        ),
    ] {
        let request = request(operation, &selectors);
        assert!(nab::request_is_supported(&definition, &request).unwrap());
        let variables = search_variables(&definition, &BTreeMap::new(), &request).unwrap();
        assert_eq!(variables[".Query.Type"], mode);
        assert_eq!(variables[".Query.Q"], "  A & B  ");
        assert_eq!(variables[".Query.Offset"], "0");
        for (field, value) in expected {
            assert_eq!(variables[field], value, "{field}");
        }
        let Step::NeedHttp { request, .. } = begin_search(definition.clone(), request).unwrap()
        else {
            panic!("missing tracker request")
        };
        assert_eq!(params(&request)["mode"], mode);
        assert_eq!(params(&request)["q"], "  A & B  ");
    }
}

#[test]
fn exact_tv_keywords_match_prowlarr_episode_and_year_rules() {
    let definition = parse_definition(DEFINITION).unwrap();
    for (season, ep, expected) in [
        ("1", "2", "S01E02"),
        ("1", "", "S01"),
        ("0", "3", ""),
        ("2026", "10/25", "2026.10.25"),
        ("1", "Special", "S01ESpecial"),
    ] {
        let mut request = request(Op::Tv, &[("season", season), ("ep", ep), ("year", "2020")]);
        request.query = "Fixture".into();
        let vars = search_variables(&definition, &BTreeMap::new(), &request).unwrap();
        assert_eq!(
            vars[".Query.Episode"],
            if expected.is_empty() {
                Value::Null
            } else {
                Value::String(expected.into())
            }
        );
        let keywords = format!("Fixture 2020 {expected}").trim_end().to_string();
        assert_eq!(vars[".Keywords"], keywords);
        assert_eq!(vars[".Query.Keywords"], keywords);
    }
}

#[test]
fn unsupported_mode_or_selector_never_becomes_an_unfiltered_tracker_query() {
    for selectors in [
        vec![("apikey", "secret")],
        vec![("offset", "20")],
        vec![("unknown", "value")],
        vec![("artist", "wrong operation")],
    ] {
        let def = parse_definition(DEFINITION).unwrap();
        let Step::Complete { output } = begin_search(def, request(Op::Book, &selectors)).unwrap()
        else {
            panic!("unsupported selector issued HTTP")
        };
        let response: PluginSearchResponse = serde_json::from_value(output).unwrap();
        assert!(response.results.is_empty());
    }
    let mut def = parse_definition(DEFINITION).unwrap();
    def.caps["modes"]
        .as_mapping_mut()
        .unwrap()
        .remove(serde_yaml::Value::String("book-search".into()));
    assert!(matches!(
        begin_search(def, request(Op::Book, &[("author", "Author")])).unwrap(),
        Step::Complete { .. }
    ));
    let mut def = parse_definition(DEFINITION).unwrap();
    def.caps["modes"]["book-search"] = serde_yaml::from_str("[q]").unwrap();
    assert!(matches!(
        begin_search(def, request(Op::Book, &[("author", "Author")])).unwrap(),
        Step::Complete { .. }
    ));
}

#[test]
fn recent_feed_is_explicit_and_cardigann_never_accepts_a_provider_cursor() {
    let definition = parse_definition(DEFINITION).unwrap();
    let mut request = PluginSearchRequest {
        recent_paging: true,
        facet: Some("series".into()),
        ..Default::default()
    };
    assert!(nab::request_is_supported(&definition, &request).unwrap());
    let vars = search_variables(&definition, &BTreeMap::new(), &request).unwrap();
    assert_eq!(vars[".Query.Type"], "search");
    assert_eq!(vars[".Query.Q"], Value::Null);
    request.page_cursor = Some("offset:20".into());
    assert!(begin_search(definition.clone(), request.clone()).is_err());
    request.page_cursor = None;
    request.rss_catch_up = Some(Default::default());
    assert!(begin_search(definition, request).is_err());
}

#[test]
fn exact_search_keeps_all_configured_paths_and_host_buffer_pagination() {
    let definition = parse_definition(DEFINITION).unwrap();
    let mut step = begin_search(definition, request(Op::Book, &[("author", "Author")])).unwrap();
    for path in ["/first", "/second"] {
        let Step::NeedHttp {
            request,
            continuation,
        } = step
        else {
            panic!("search path skipped")
        };
        assert_eq!(Url::parse(&request.url).unwrap().path(), path);
        assert_eq!(params(&request)["author"], "Author");
        step = resume(&continuation, ResumeInput::Http(PluginHttpResponse {
            status: 200, headers: BTreeMap::new(), set_cookie_headers: vec![],
            body: format!("<div class=release><a href='magnet:?xt=urn:btih:0123456789012345678901234567890123456789'>{path}</a></div>").into_bytes(),
        })).unwrap();
    }
    let Step::Complete { output } = step else {
        panic!("search chain did not finish")
    };
    let response: PluginSearchResponse = serde_json::from_value(output).unwrap();
    assert_eq!(response.results.len(), 2);
    assert!(response.next_cursor.is_none());
}

#[test]
fn derived_title_search_variables_keep_the_existing_contract() {
    let definition = parse_definition(DEFINITION).unwrap();
    let request = PluginSearchRequest {
        query: "Fixture".into(),
        facet: Some("series".into()),
        season: Some(1),
        episode: Some(2),
        ..Default::default()
    };
    let vars = search_variables(&definition, &BTreeMap::new(), &request).unwrap();
    assert_eq!(vars[".Query.Type"], "tvsearch");
    assert_eq!(vars[".Query.Episode"], "2");
    assert_eq!(vars[".Query.Keywords"], "Fixture");
    assert_eq!(vars[".Keywords"], "Fixture");
}

#[test]
fn descriptor_advertises_nab_support_without_inventing_provider_pagination() {
    let descriptor = crate::build_descriptor();
    assert_eq!(descriptor.sdk_version, "3.20.0");
    let scryer_plugin_sdk::ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
        panic!("not an indexer")
    };
    assert!(indexer.capabilities.raw_nab_search);
    assert!(
        !indexer
            .capabilities
            .limits
            .is_some_and(|limits| limits.paged_search)
    );
}
