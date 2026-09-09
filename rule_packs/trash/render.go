package main

import (
	"encoding/json"
	"fmt"
	"strings"
)

type groupRule struct {
	Index         int    `json:"index"`
	Matcher       string `json:"matcher"`
	MatchKind     string `json:"match_kind"`
	Tier          string `json:"tier"`
	Facet         string `json:"facet"`
	SourceContext string `json:"source_context"`
}
type groupRuntime struct {
	Index int    `json:"i"`
	Tier  string `json:"t"`
}
type prefixRuntime struct {
	Matcher string `json:"m"`
	Index   int    `json:"i"`
	Tier    string `json:"t"`
}

func renderDetection(snapshot detectionSnapshot) (string, error) {
	return renderDetectionScope(snapshot, "")
}

// renderDetectionScope keeps each customizable contribution self-contained
// without embedding unrelated locale rows in every module.
func renderDetectionScope(snapshot detectionSnapshot, locale string) (string, error) {
	tables, err := indexedDetectionTables(scopedDetectionTables(snapshot.Tables, locale))
	if err != nil {
		return "", err
	}
	encoded, err := json.MarshalIndent(tables, "", "  ")
	if err != nil {
		return "", err
	}
	return "import rego.v1\n\n# Generated from parser snapshot " + snapshot.SourceRevision + ".\ntrash_detection_tables := " + string(encoded) + "\n\n" + regoDetectionHelpers + "\n", nil
}

func scopedDetectionTables(source map[string]interface{}, locale string) map[string]interface{} {
	out := map[string]interface{}{}
	copyRows := func(name string, keep func(map[string]interface{}) bool) {
		var rows []interface{}
		for _, raw := range source[name].([]interface{}) {
			row := raw.(map[string]interface{})
			if keep(row) {
				rows = append(rows, row)
			}
		}
		out[name] = rows
	}
	if locale == "" {
		copyRows("service_alias_rules", func(map[string]interface{}) bool { return true })
		copyRows("token_signal_rules", func(map[string]interface{}) bool { return true })
		copyRows("blocked_title_rules", func(map[string]interface{}) bool { return true })
		copyRows("fact_rules", func(row map[string]interface{}) bool {
			return !strings.HasPrefix(detectionTableString(row, "code"), "trash.locale.")
		})
		copyRows("locale_group_fact_rules", func(map[string]interface{}) bool { return false })
		out["no_release_group_fact_facets"] = source["no_release_group_fact_facets"]
		return out
	}
	prefix := "trash.locale." + locale + "."
	copyRows("service_alias_rules", func(map[string]interface{}) bool { return false })
	copyRows("token_signal_rules", func(map[string]interface{}) bool { return false })
	copyRows("blocked_title_rules", func(map[string]interface{}) bool { return false })
	copyRows("fact_rules", func(row map[string]interface{}) bool {
		return strings.HasPrefix(detectionTableString(row, "code"), prefix)
	})
	copyRows("locale_group_fact_rules", func(row map[string]interface{}) bool {
		return strings.HasPrefix(detectionTableString(row, "code"), prefix)
	})
	out["no_release_group_fact_facets"] = []interface{}{}
	return out
}
func detectionTableString(row map[string]interface{}, key string) string {
	value, _ := row[key].(string)
	return value
}
func renderGroups(snapshot snapshot) (string, error) {
	var rows []groupRule
	if err := json.Unmarshal(snapshot.GroupRules, &rows); err != nil {
		return "", err
	}
	if len(rows) == 0 {
		return "", fmt.Errorf("no group rules")
	}
	exact := map[string]map[string]map[string]groupRuntime{}
	prefix := map[string]map[string][]prefixRuntime{}
	for index, row := range rows {
		if row.Index != index || row.Matcher == "" || (row.MatchKind != "exact" && row.MatchKind != "prefix") || row.Tier == "" || row.Facet == "" || row.SourceContext == "" {
			return "", fmt.Errorf("invalid group_rules[%d]", index)
		}
		if row.MatchKind == "exact" {
			if exact[row.Facet] == nil {
				exact[row.Facet] = map[string]map[string]groupRuntime{}
			}
			if exact[row.Facet][row.SourceContext] == nil {
				exact[row.Facet][row.SourceContext] = map[string]groupRuntime{}
			}
			exact[row.Facet][row.SourceContext][asciiUpper(row.Matcher)] = groupRuntime{row.Index, row.Tier}
		} else {
			if prefix[row.Facet] == nil {
				prefix[row.Facet] = map[string][]prefixRuntime{}
			}
			prefix[row.Facet][row.SourceContext] = append(prefix[row.Facet][row.SourceContext], prefixRuntime{asciiUpper(row.Matcher), row.Index, row.Tier})
		}
	}
	e, err := json.MarshalIndent(exact, "", "  ")
	if err != nil {
		return "", err
	}
	p, err := json.MarshalIndent(prefix, "", "  ")
	if err != nil {
		return "", err
	}
	return "import rego.v1\n\n# Generated from compiled TRaSH group rows; source order is decisive.\ntrash_group_exact := " + string(e) + "\ntrash_group_prefix := " + string(p) + "\n\n" + groupRego, nil
}
func asciiUpper(v string) string {
	return strings.Map(func(r rune) rune {
		if r >= 'a' && r <= 'z' {
			return r - 32
		}
		return r
	}, v)
}

const groupRego = `trash_upper(value) := result if { result := replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(value,"a","A"),"b","B"),"c","C"),"d","D"),"e","E"),"f","F"),"g","G"),"h","H"),"i","I"),"j","J"),"k","K"),"l","L"),"m","M"),"n","N"),"o","O"),"p","P"),"q","Q"),"r","R"),"s","S"),"t","T"),"u","U"),"v","V"),"w","W"),"x","X"),"y","Y"),"z","Z") }
trash_group_string(field) := value if { value := object.get(input.release,field,""); is_string(value) }
trash_group_string(field) := "" if { not is_string(object.get(input.release,field,"")) }
trash_group_context := "anime" if { lower(object.get(input.context,"category","")) == "anime" }
trash_group_context := "remux" if { lower(object.get(input.context,"category","")) != "anime"; input.release.is_remux }
trash_group_context := "web" if { lower(object.get(input.context,"category","")) != "anime"; not input.release.is_remux; trash_upper(trash_group_string("source")) in {"WEB-DL","WEBRIP"} }
trash_group_context := "uhd_bluray" if { lower(object.get(input.context,"category","")) != "anime"; not input.release.is_remux; trash_upper(trash_group_string("source")) in {"BLURAY","BRDISK"}; trash_upper(trash_group_string("quality")) == "2160P" }
trash_group_context := "bluray" if { lower(object.get(input.context,"category","")) != "anime"; not input.release.is_remux; trash_upper(trash_group_string("source")) in {"BLURAY","BRDISK"}; trash_upper(trash_group_string("quality")) != "2160P" }
trash_group_context := "any" if { lower(object.get(input.context,"category","")) != "anime"; not input.release.is_remux; not trash_upper(trash_group_string("source")) in {"WEB-DL","WEBRIP","BLURAY","BRDISK"} }
trash_group_facets := ["anime"] if { lower(object.get(input.context,"category","")) == "anime" }
trash_group_facets := ["series"] if { lower(object.get(input.context,"category","")) == "series" }
trash_group_facets := ["movie"] if { lower(object.get(input.context,"category","")) == "movie" }
trash_group_facets := ["movie","series"] if { not lower(object.get(input.context,"category","")) in {"anime","series","movie"} }
trash_group_candidate(facet, context) := rule if { by_facet := object.get(trash_group_exact,facet,{}); by_context := object.get(by_facet,context,{}); group := trash_upper(trash_group_string("release_group")); rule := by_context[group] }
trash_group_candidate(facet, context) := rule if { some rule in object.get(object.get(trash_group_prefix,facet,{}),context,[]); startswith(trash_upper(trash_group_string("release_group")),rule.m) }
trash_group_best(facet, context) := rule if { candidates := [value | value := trash_group_candidate(facet,context)]; count(candidates)>0; rule := candidates[_]; rule.i == min([other.i | other := candidates[_]]) }
trash_group_slots contains {"p":0,"rule":rule} if { facets:=trash_group_facets; rule:=trash_group_best(facets[0],trash_group_context) }
trash_group_slots contains {"p":1,"rule":rule} if { facets:=trash_group_facets; rule:=trash_group_best(facets[0],"any") }
trash_group_slots contains {"p":2,"rule":rule} if { facets:=trash_group_facets; count(facets)>1; rule:=trash_group_best(facets[1],trash_group_context) }
trash_group_slots contains {"p":3,"rule":rule} if { facets:=trash_group_facets; count(facets)>1; rule:=trash_group_best(facets[1],"any") }
trash_group_selected := candidate.rule if { candidates := trash_group_slots; count(candidates)>0; candidate:=candidates[_]; candidate.p == min([other.p|other:=candidates[_]]) }
trash_group_weight(tier) := value if { weights := {"balanced":{"gold":300,"silver":150,"bronze":50,"banned":-10000,"unknown":-30},"audiophile":{"gold":500,"silver":250,"bronze":80,"banned":-10000,"unknown":-60},"efficient":{"gold":150,"silver":80,"bronze":30,"banned":-10000,"unknown":-15},"compatible":{"gold":200,"silver":100,"bronze":40,"banned":-10000,"unknown":-20}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),tier,0) }
score_entry[sprintf("group_%s",[trash_group_selected.t])] := trash_group_weight(trash_group_selected.t) if { trash_group_selected; trash_group_weight(trash_group_selected.t) != 0 }
score_entry["group_unknown"] := trash_group_weight("unknown") if { not trash_group_selected; trash_group_weight("unknown") != 0 }`

func renderStreaming() string {
	return `import rego.v1
trash_stream_weight(tier) := value if { weights:={"balanced":{"one":30,"two":20,"three":10,"anime":20},"audiophile":{"one":20,"two":15,"three":5,"anime":15},"efficient":{"one":40,"two":30,"three":20,"anime":25},"compatible":{"one":30,"two":20,"three":15,"anime":20}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),tier,0) }
trash_stream_tier := "one" if { input.release.streaming_service in {"Netflix","Apple TV+","Amazon","Disney+"} }
trash_stream_tier := "two" if { input.release.streaming_service in {"HBO Max","Paramount+","Hulu","Peacock"} }
trash_stream_tier := "anime" if { input.release.streaming_service in {"Crunchyroll","Funimation","HIDIVE"} }
trash_stream_tier := "three" if { input.release.streaming_service != null; input.release.streaming_service != ""; not input.release.streaming_service in {"Netflix","Apple TV+","Amazon","Disney+","HBO Max","Paramount+","Hulu","Peacock","Crunchyroll","Funimation","HIDIVE"} }
score_entry["streaming_service"] := trash_stream_weight(trash_stream_tier) if { trash_stream_tier; trash_stream_weight(trash_stream_tier) != 0 }`
}
func renderUnwanted() string {
	return `
trash_unwanted_weight(name) := value if { weights:={"balanced":{"scene":-30,"obfuscated":-90,"retagged":-60,"hardcoded":-300},"audiophile":{"scene":-60,"obfuscated":-180,"retagged":-120,"hardcoded":-400},"efficient":{"scene":-15,"obfuscated":-45,"retagged":-30,"hardcoded":-200},"compatible":{"scene":-20,"obfuscated":-60,"retagged":-40,"hardcoded":-300}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),name,0) }
score_entry["trash.scene"] := trash_unwanted_weight("scene") if { has_detected_fact("trash.scene") }
score_entry["trash.obfuscated"] := trash_unwanted_weight("obfuscated") if { has_detected_fact("trash.obfuscated") }
score_entry["trash.retagged"] := trash_unwanted_weight("retagged") if { has_detected_fact("trash.retagged") }
score_entry["hardcoded_subs"] := trash_unwanted_weight("hardcoded") if { input.release.is_hardcoded_subs == true }
score_entry["trash_guides_anime_raws"] := -10000 if { has_detected_fact("trash.blocked.anime_raws") }
score_entry["trash_guides_lq_release_title"] := -10000 if { has_detected_fact("trash.blocked.lq_release_title") }
score_entry["trash_guides_fansub"] := -10000 if { has_detected_fact("trash.blocked.fansub") }
score_entry["trash_guides_fastsub"] := -10000 if { has_detected_fact("trash.blocked.fastsub") }`
}
func renderEditionAnime() string {
	return `import rego.v1
trash_ea_weight(name) := value if { weights:={"balanced":{"imax":80,"extended":40,"hybrid":30,"criterion":20,"remaster":20,"v2":20,"bit":40,"uncensored":30,"dubs":-100},"audiophile":{"imax":120,"extended":60,"hybrid":50,"criterion":40,"remaster":30,"v2":25,"bit":50,"uncensored":40,"dubs":-150},"efficient":{"imax":40,"extended":20,"hybrid":20,"criterion":10,"remaster":10,"v2":20,"bit":60,"uncensored":20,"dubs":-60},"compatible":{"imax":60,"extended":30,"hybrid":25,"criterion":15,"remaster":15,"v2":15,"bit":20,"uncensored":20,"dubs":-80}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),name,0) }
trash_edition := "imax" if { input.release.edition in {"IMAX","IMAX Enhanced"} }
trash_edition := "extended" if { input.release.edition in {"Extended","Unrated","Director's Cut"} }
trash_edition := "hybrid" if { input.release.edition == "Hybrid" }
trash_edition := "criterion" if { input.release.edition == "Criterion" }
trash_edition := "remaster" if { input.release.edition == "Remaster" }
score_entry["edition_bonus"] := trash_ea_weight(trash_edition) if { trash_edition }
score_entry["anime_version_bonus"] := trash_ea_weight("v2") if { lower(object.get(input.context,"category","")) == "anime"; input.release.anime_version >= 2 }
score_entry["anime_10bit_bonus"] := trash_ea_weight("bit") if { lower(object.get(input.context,"category","")) == "anime"; input.release.is_10bit == true }
score_entry["anime_uncensored_bonus"] := trash_ea_weight("uncensored") if { lower(object.get(input.context,"category","")) == "anime"; input.release.is_uncensored == true }
score_entry["anime_dubs_only"] := trash_ea_weight("dubs") if { lower(object.get(input.context,"category","")) == "anime"; input.release.is_dubs_only == true }`
}
