use std::collections::BTreeMap;

use encoding_rs::Encoding;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

pub type ScalarMap = BTreeMap<String, serde_yaml::Value>;

pub const COMPILED_IR_VERSION: u16 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompiledIr {
    pub ir_version: u16,
    pub definition: Definition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEncoding {
    label: String,
}

impl SourceEncoding {
    #[cfg(test)]
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn encoding(&self) -> &'static Encoding {
        Encoding::for_label(self.label.as_bytes())
            .expect("SourceEncoding is constructed only from supported labels")
    }
}

impl Serialize for SourceEncoding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.label)
    }
}

impl<'de> Deserialize<'de> for SourceEncoding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let label = String::deserialize(deserializer)?;
        let encoding = Encoding::for_label(label.trim().as_bytes())
            .ok_or_else(|| D::Error::custom(format!("unsupported Cardigann encoding `{label}`")))?;
        Ok(Self {
            label: encoding.name().to_string(),
        })
    }
}

fn default_source_encoding() -> SourceEncoding {
    SourceEncoding {
        label: "UTF-8".to_string(),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub id: String,
    #[serde(default)]
    pub replaces: Vec<String>,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub language: String,
    #[serde(default = "default_source_encoding")]
    pub encoding: SourceEncoding,
    #[serde(rename = "type")]
    pub definition_type: String,
    pub links: Vec<String>,
    #[serde(default, rename = "legacylinks")]
    pub legacy_links: Vec<String>,
    #[serde(default)]
    pub certificates: Vec<String>,
    #[serde(default, rename = "requestDelay")]
    pub request_delay: Option<f64>,
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub caps: serde_yaml::Value,
    #[serde(default)]
    pub login: Option<LoginBlock>,
    #[serde(default)]
    pub ratio: Option<RatioBlock>,
    pub search: SearchBlock,
    #[serde(default)]
    pub download: Option<DownloadBlock>,
    #[serde(default)]
    pub followredirect: bool,
    #[serde(default = "default_true", rename = "testlinktorrent")]
    pub test_link_torrent: bool,
}

impl Definition {
    pub fn validate_supported(&self) -> Result<(), String> {
        validate_caps(&self.caps)?;
        if let Some(login) = &self.login {
            validate_error_paths(&login.error, "login.error")?;
            validate_captcha(login.captcha.as_ref())?;
        }
        validate_error_paths(&self.search.error, "search.error")?;
        Ok(())
    }
}

fn validate_captcha(captcha: Option<&CaptchaBlock>) -> Result<(), String> {
    if let Some(captcha) = captcha
        && !captcha.captcha_type.eq_ignore_ascii_case("image")
    {
        return Err(format!(
            "unsupported CAPTCHA type `{}` at `login.captcha.type`",
            captcha.captcha_type
        ));
    }
    Ok(())
}

fn validate_error_paths(errors: &[ErrorBlock], field_path: &str) -> Result<(), String> {
    for (index, error) in errors.iter().enumerate() {
        if error.path.is_some() {
            return Err(format!(
                "error.path is unsupported at `{field_path}[{index}].path`"
            ));
        }
    }
    Ok(())
}

fn validate_caps(caps: &serde_yaml::Value) -> Result<(), String> {
    let Some(caps) = caps.as_mapping() else {
        return Err("caps must be a mapping at `caps`".to_string());
    };
    for key in caps.keys() {
        let Some(key) = key.as_str() else {
            return Err("caps contains a non-string key at `caps`".to_string());
        };
        if !matches!(
            key,
            "categories" | "categorymappings" | "modes" | "allowrawsearch" | "allowtvsearchimdb"
        ) {
            return Err(format!("unsupported caps property at `caps.{key}`"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub name: String,
    #[serde(rename = "type")]
    pub setting_type: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, rename = "default")]
    pub default_value: Option<serde_yaml::Value>,
    #[serde(default)]
    pub defaults: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Option<serde_yaml::Value>,
    #[serde(default, alias = "help")]
    pub help_text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoginBlock {
    #[serde(default = "default_form")]
    pub method: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub form: Option<String>,
    #[serde(default, rename = "submitpath")]
    pub submit_path: Option<String>,
    #[serde(default)]
    pub inputs: ScalarMap,
    #[serde(default)]
    pub selectors: bool,
    #[serde(default, rename = "selectorinputs")]
    pub selector_inputs: BTreeMap<String, SelectorField>,
    #[serde(default, rename = "getselectorinputs")]
    pub get_selector_inputs: BTreeMap<String, SelectorField>,
    #[serde(default)]
    pub cookies: Vec<String>,
    #[serde(default)]
    pub headers: ScalarMap,
    #[serde(default)]
    pub test: Option<PageTestBlock>,
    #[serde(default)]
    pub error: Vec<ErrorBlock>,
    #[serde(default)]
    pub captcha: Option<CaptchaBlock>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptchaBlock {
    #[serde(rename = "type")]
    pub captcha_type: String,
    pub selector: String,
    pub input: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PageTestBlock {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBlock {
    #[serde(default)]
    pub path: Option<String>,
    pub selector: String,
    #[serde(default)]
    pub message: Option<SelectorField>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub paths: Vec<SearchPath>,
    #[serde(default)]
    pub inputs: ScalarMap,
    #[serde(default)]
    pub rows: RowsBlock,
    #[serde(default)]
    pub fields: IndexMap<String, SelectorField>,
    #[serde(default)]
    pub headers: ScalarMap,
    #[serde(default, rename = "allowemptyinputs", alias = "allowEmptyInputs")]
    pub allow_empty_inputs: bool,
    #[serde(default, rename = "keywordsfilters")]
    pub keyword_filters: Vec<FilterBlock>,
    #[serde(default, rename = "preprocessingfilters")]
    pub preprocessing_filters: Vec<FilterBlock>,
    #[serde(default)]
    pub error: Vec<ErrorBlock>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchBlockWire {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    paths: Vec<SearchPath>,
    #[serde(default)]
    inputs: ScalarMap,
    #[serde(default)]
    rows: RowsBlock,
    #[serde(default)]
    fields: IndexMap<String, SelectorField>,
    #[serde(default)]
    headers: ScalarMap,
    #[serde(default, rename = "allowemptyinputs", alias = "allowEmptyInputs")]
    allow_empty_inputs: bool,
    #[serde(default, rename = "keywordsfilters")]
    keyword_filters: Vec<FilterBlock>,
    #[serde(default, rename = "preprocessingfilters")]
    preprocessing_filters: Vec<FilterBlock>,
    #[serde(default)]
    error: Vec<ErrorBlock>,
}

impl<'de> Deserialize<'de> for SearchBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SearchBlockWire::deserialize(deserializer)?;
        let mut paths = wire.paths;
        if paths.is_empty()
            && let Some(path) = wire.path.clone()
        {
            paths.push(SearchPath {
                path,
                ..SearchPath::default()
            });
        }
        if paths.is_empty() {
            return Err(D::Error::custom("search requires `paths` or `path`"));
        }
        Ok(Self {
            path: wire.path,
            paths,
            inputs: wire.inputs,
            rows: wire.rows,
            fields: wire.fields,
            headers: wire.headers,
            allow_empty_inputs: wire.allow_empty_inputs,
            keyword_filters: wire.keyword_filters,
            preprocessing_filters: wire.preprocessing_filters,
            error: wire.error,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPath {
    pub path: String,
    #[serde(default = "default_get")]
    pub method: String,
    #[serde(default)]
    pub inputs: ScalarMap,
    #[serde(default = "default_true", rename = "inheritinputs")]
    pub inherit_inputs: bool,
    #[serde(default, deserialize_with = "deserialize_scalar_strings")]
    pub categories: Vec<String>,
    #[serde(default, rename = "queryseparator")]
    pub query_separator: Option<String>,
    #[serde(default)]
    pub response: ResponseBlock,
    #[serde(default, rename = "followredirect")]
    pub follow_redirect: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResponseType {
    #[default]
    Html,
    Json,
    Xml,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseBlock {
    #[serde(default, rename = "type")]
    pub response_type: ResponseType,
    #[serde(default, rename = "noresultsmessage", alias = "noResultsMessage")]
    pub no_results_message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RowsBlock {
    pub selector: String,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub after: usize,
    #[serde(default)]
    pub count: Option<SelectorField>,
    #[serde(default, rename = "missingAttributeEqualsNoResults")]
    pub missing_attribute_equals_no_results: bool,
    #[serde(default)]
    pub filters: Vec<FilterBlock>,
    #[serde(default, rename = "dateheaders")]
    pub date_headers: Option<SelectorField>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectorField {
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub text: Option<serde_yaml::Value>,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub remove: Option<serde_yaml::Value>,
    #[serde(default)]
    pub filters: Vec<FilterBlock>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, rename = "default")]
    pub default_value: Option<serde_yaml::Value>,
    #[serde(default, deserialize_with = "deserialize_optional_string_map")]
    pub case: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterBlock {
    pub name: String,
    #[serde(default)]
    pub args: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadBlock {
    #[serde(default = "default_get")]
    pub method: String,
    #[serde(default)]
    pub headers: ScalarMap,
    #[serde(default)]
    pub before: Option<RequestBlock>,
    #[serde(default)]
    pub selectors: Vec<DownloadSelector>,
    #[serde(default)]
    pub infohash: Option<InfoHashBlock>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBlock {
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_get")]
    pub method: String,
    #[serde(default)]
    pub inputs: ScalarMap,
    #[serde(default, rename = "queryseparator")]
    pub query_separator: Option<String>,
    #[serde(default, rename = "pathselector")]
    pub path_selector: Option<SelectorField>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadSelector {
    pub selector: String,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub filters: Vec<FilterBlock>,
    #[serde(default, rename = "usebeforeresponse")]
    pub use_before_response: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InfoHashBlock {
    pub hash: SelectorField,
    pub title: SelectorField,
    #[serde(default, rename = "usebeforeresponse")]
    pub use_before_response: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RatioBlock {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(flatten)]
    pub field: SelectorField,
}

fn deserialize_scalar_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<serde_yaml::Value>::deserialize(deserializer)?
        .into_iter()
        .map(|value| {
            scalar_to_string(&value).ok_or_else(|| D::Error::custom("expected a scalar value"))
        })
        .collect()
}

fn deserialize_optional_string_map<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<BTreeMap<String, serde_yaml::Value>>::deserialize(deserializer)?.map_or(
        Ok(None),
        |map| {
            map.into_iter()
                .map(|(key, value)| {
                    scalar_to_string(&value)
                        .map(|value| (key, value))
                        .ok_or_else(|| D::Error::custom("case values must be scalars"))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Some)
        },
    )
}

fn default_get() -> String {
    "get".to_string()
}

fn default_form() -> String {
    "form".to_string()
}

fn default_true() -> bool {
    true
}

pub fn scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::Bool(value) => Some(value.to_string()),
        serde_yaml::Value::Number(value) => Some(value.to_string()),
        serde_yaml::Value::String(value) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_legacy_search_path_and_scalar_schema_values() {
        let definition: Definition = serde_yaml::from_str(
            r#"
id: legacy
name: Legacy
description: Legacy definition
language: en-US
encoding: UTF-8
type: public
links: [https://example.test/]
requestDelay: 2.5
caps: {}
settings:
  - name: category
    type: select
    defaults: [all]
search:
  path: api/search
  rows:
    selector: item
  fields:
    title:
      case: {1: One}
"#,
        )
        .unwrap();

        assert_eq!(definition.search.paths.len(), 1);
        assert_eq!(definition.search.paths[0].path, "api/search");
        let xml_path: SearchPath =
            serde_yaml::from_str("path: api/search\nresponse:\n  type: xml").unwrap();
        assert_eq!(xml_path.response.response_type, ResponseType::Xml);
        assert_eq!(
            definition.search.fields["title"].case.as_ref().unwrap()["1"],
            "One"
        );
        let path: SearchPath =
            serde_yaml::from_str("path: api/search\ncategories: [1, \"2\"]").unwrap();
        assert_eq!(path.categories, ["1", "2"]);
        assert_eq!(definition.request_delay, Some(2.5));
        assert!(definition.test_link_torrent);
    }

    #[test]
    fn validates_non_utf8_cardigann_encoding_labels() {
        let encoding: SourceEncoding = serde_yaml::from_str("windows-1252").unwrap();
        assert_eq!(encoding.label(), "windows-1252");
        assert_eq!(encoding.encoding().name(), "windows-1252");

        let error = serde_yaml::from_str::<SourceEncoding>("unknown-legacy-encoding").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported Cardigann encoding `unknown-legacy-encoding`")
        );
    }

    #[test]
    fn accepts_image_captcha_and_rejects_other_captcha_types() {
        let image: CaptchaBlock =
            serde_yaml::from_str("type: image\nselector: img.captcha\ninput: captcha").unwrap();
        validate_captcha(Some(&image)).unwrap();

        let unsupported: CaptchaBlock =
            serde_yaml::from_str("type: recaptcha\nselector: div.captcha\ninput: captcha").unwrap();
        assert_eq!(
            validate_captcha(Some(&unsupported)).unwrap_err(),
            "unsupported CAPTCHA type `recaptcha` at `login.captcha.type`"
        );
    }

    #[test]
    fn rejects_search_without_a_request_path() {
        let error = serde_yaml::from_str::<SearchBlock>(
            r#"
rows:
  selector: item
fields: {}
"#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("search requires `paths` or `path`")
        );
    }
}
