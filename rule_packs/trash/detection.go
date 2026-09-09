package main

// This translator intentionally has no package declaration in its output.
// Callers prepend their own package and may compose it with score rules.

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

type detectionSnapshot struct {
	SchemaVersion  int                    `json:"schema_version"`
	SourceRevision string                 `json:"source_revision"`
	Counts         map[string]int         `json:"counts"`
	Tables         map[string]interface{} `json:"tables"`
}

func parseDetectionSnapshot(data []byte) (detectionSnapshot, error) {
	var snapshot detectionSnapshot
	if err := json.Unmarshal(data, &snapshot); err != nil {
		return snapshot, fmt.Errorf("decode detection snapshot: %w", err)
	}
	if snapshot.SchemaVersion != 1 || len(snapshot.SourceRevision) != 40 {
		return snapshot, fmt.Errorf("unsupported detection snapshot schema=%d revision=%q", snapshot.SchemaVersion, snapshot.SourceRevision)
	}
	for _, name := range requiredDetectionTables {
		value, ok := snapshot.Tables[name]
		if !ok {
			return snapshot, fmt.Errorf("detection snapshot missing table %q", name)
		}
		rows, ok := value.([]interface{})
		if !ok || snapshot.Counts[name] != len(rows) {
			return snapshot, fmt.Errorf("detection snapshot table %q count mismatch", name)
		}
	}
	for _, name := range []string{"fact_rules", "token_signal_rules", "blocked_title_rules"} {
		for index, row := range snapshot.Tables[name].([]interface{}) {
			record, ok := row.(map[string]interface{})
			if !ok {
				return snapshot, fmt.Errorf("%s[%d] is not an object", name, index)
			}
			pattern, ok := record["pattern"].(map[string]interface{})
			if !ok {
				return snapshot, fmt.Errorf("%s[%d] has no pattern", name, index)
			}
			kind, _ := pattern["kind"].(string)
			if kind != "sequence" && kind != "required_tokens" {
				return snapshot, fmt.Errorf("%s[%d] unsupported pattern kind %q", name, index, kind)
			}
		}
	}
	return snapshot, nil
}

var requiredDetectionTables = []string{
	"service_alias_rules", "fact_rules", "locale_group_fact_rules",
	"token_signal_rules", "blocked_title_rules", "no_release_group_fact_facets",
}

// generateDetection translates the complete Rust snapshot written by the
// ignored parser test. It refuses incomplete snapshots: no table or pattern
// variant is silently omitted from the generated policy.
func generateDetection(dir string) error {
	data, err := os.ReadFile(filepath.Join(dir, "detection-snapshot.json"))
	if err != nil {
		return fmt.Errorf("read detection snapshot: %w", err)
	}
	var snapshot detectionSnapshot
	if err := json.Unmarshal(data, &snapshot); err != nil {
		return fmt.Errorf("decode detection snapshot: %w", err)
	}
	if snapshot.SchemaVersion != 1 || snapshot.SourceRevision == "" {
		return fmt.Errorf("unsupported detection snapshot schema=%d revision=%q", snapshot.SchemaVersion, snapshot.SourceRevision)
	}
	for _, name := range requiredDetectionTables {
		value, ok := snapshot.Tables[name]
		if !ok {
			return fmt.Errorf("detection snapshot missing table %q", name)
		}
		rows, isRows := value.([]interface{})
		if !isRows || snapshot.Counts[name] != len(rows) {
			return fmt.Errorf("detection snapshot table %q count mismatch", name)
		}
	}
	for _, name := range []string{"fact_rules", "token_signal_rules", "blocked_title_rules"} {
		for index, row := range snapshot.Tables[name].([]interface{}) {
			record, ok := row.(map[string]interface{})
			if !ok {
				return fmt.Errorf("%s[%d] is not an object", name, index)
			}
			pattern, ok := record["pattern"].(map[string]interface{})
			if !ok {
				return fmt.Errorf("%s[%d] has no pattern", name, index)
			}
			kind, _ := pattern["kind"].(string)
			if kind != "sequence" && kind != "required_tokens" {
				return fmt.Errorf("%s[%d] unsupported pattern kind %q", name, index, kind)
			}
		}
	}
	regoTables, err := indexedDetectionTables(snapshot.Tables)
	if err != nil {
		return err
	}
	table, err := json.MarshalIndent(regoTables, "", "  ")
	if err != nil {
		return fmt.Errorf("encode Rego table: %w", err)
	}
	var out bytes.Buffer
	fmt.Fprintf(&out, "# Generated from parser revision %s. Do not edit.\n", snapshot.SourceRevision)
	fmt.Fprintf(&out, "# Required input: release.normalized_tokens is the parser-normalized token array.\n")
	fmt.Fprintf(&out, "# Alias candidates are translated here; parser-selected release.streaming_service remains authoritative.\n")
	fmt.Fprintf(&out, "trash_detection_tables := %s\n\n", table)
	out.WriteString(regoDetectionHelpers)
	if err := os.WriteFile(filepath.Join(dir, "detection.rego"), out.Bytes(), 0644); err != nil {
		return fmt.Errorf("write detection.rego: %w", err)
	}
	return nil
}

func indexedDetectionTables(tables map[string]interface{}) (map[string]interface{}, error) {
	// The snapshot retains full records and provenance for review. Runtime Rego
	// receives only the indexed fields its helpers read, so it does not carry a
	// second unindexed copy of every active record.
	indexed := map[string]interface{}{
		"no_release_group_fact_facets": tables["no_release_group_fact_facets"],
	}
	for _, name := range []string{"fact_rules", "token_signal_rules", "blocked_title_rules"} {
		byAnchor := map[string][]interface{}{}
		for _, raw := range tables[name].([]interface{}) {
			rule := raw.(map[string]interface{})
			pattern := rule["pattern"].(map[string]interface{})
			tokens := pattern["tokens"].([]interface{})
			if len(tokens) == 0 {
				return nil, fmt.Errorf("%s has empty pattern", name)
			}
			anchor := tokens[0].(string)
			if pattern["kind"] == "required_tokens" {
				words := make([]string, len(tokens))
				for i, token := range tokens {
					words[i] = token.(string)
				}
				sort.Strings(words)
				anchor = words[0]
			}
			fields := []string{"pattern"}
			switch name {
			case "fact_rules":
				fields = append(fields, "code", "facet", "category")
			case "token_signal_rules":
				fields = append(fields, "kind")
			case "blocked_title_rules":
				fields = append(fields, "code", "facet", "category", "order")
			}
			byAnchor[anchor] = append(byAnchor[anchor], compactDetectionRecord(rule, fields...))
		}
		indexed[name+"_by_anchor"] = byAnchor
	}
	services := map[string][]interface{}{}
	for _, raw := range tables["service_alias_rules"].([]interface{}) {
		rule := raw.(map[string]interface{})
		token := strings.ToUpper(rule["token"].(string))
		services[token] = append(services[token], compactDetectionRecord(rule, "service", "requires_web_adjacency"))
	}
	indexed["service_aliases_by_token"] = services
	exact := map[string][]interface{}{}
	prefix := map[string][]interface{}{}
	for _, raw := range tables["locale_group_fact_rules"].([]interface{}) {
		rule := raw.(map[string]interface{})
		key := rule["facet"].(string) + "|" + asciiFold(rule["matcher"].(string))
		if rule["match_kind"] == "exact" {
			// The exact-map key already carries the matcher, so runtime needs
			// only [code, source_context].
			exact[key] = append(exact[key], []interface{}{rule["code"], rule["source_context"]})
		} else {
			// Prefix matching still needs its matcher: [matcher, code, context].
			prefix[rule["facet"].(string)] = append(prefix[rule["facet"].(string)], []interface{}{rule["matcher"], rule["code"], rule["source_context"]})
		}
	}
	indexed["locale_group_exact"] = exact
	indexed["locale_group_prefix"] = prefix
	return indexed, nil
}

func asciiFold(value string) string {
	return strings.Map(func(r rune) rune {
		if r >= 'A' && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, value)
}

func compactDetectionRecord(record map[string]interface{}, fields ...string) map[string]interface{} {
	compact := make(map[string]interface{}, len(fields))
	for _, field := range fields {
		compact[field] = record[field]
	}
	return compact
}

const regoDetectionHelpers = `# No raw-title fallback exists. Token positions and Unicode normalization belong to the parser.
normalized_tokens := input.release.normalized_tokens if {
  is_array(input.release.normalized_tokens)
}

detection_available if { normalized_tokens }

# Rust uses to_ascii_lowercase / eq_ignore_ascii_case for these comparisons.
# Keep non-ASCII lookalikes distinct from ASCII release-group spellings.
trash_detection_ascii_fold(value) := result if {
  a := replace(value, "A", "a"); b := replace(a, "B", "b"); c := replace(b, "C", "c"); d := replace(c, "D", "d"); e := replace(d, "E", "e"); f := replace(e, "F", "f"); g := replace(f, "G", "g"); h := replace(g, "H", "h"); i := replace(h, "I", "i"); j := replace(i, "J", "j"); k := replace(j, "K", "k"); l := replace(k, "L", "l"); m := replace(l, "M", "m"); n := replace(m, "N", "n"); o := replace(n, "O", "o"); p := replace(o, "P", "p"); q := replace(p, "Q", "q"); r := replace(q, "R", "r"); s := replace(r, "S", "s"); t := replace(s, "T", "t"); u := replace(t, "U", "u"); v := replace(u, "V", "v"); w := replace(v, "W", "w"); x := replace(w, "X", "x"); y := replace(x, "Y", "y"); result := replace(y, "Z", "z")
}
trash_detection_ascii_upper(value) := result if {
  a := replace(value, "a", "A"); b := replace(a, "b", "B"); c := replace(b, "c", "C"); d := replace(c, "d", "D"); e := replace(d, "e", "E"); f := replace(e, "f", "F"); g := replace(f, "g", "G"); h := replace(g, "h", "H"); i := replace(h, "i", "I"); j := replace(i, "j", "J"); k := replace(j, "k", "K"); l := replace(k, "l", "L"); m := replace(l, "m", "M"); n := replace(m, "n", "N"); o := replace(n, "o", "O"); p := replace(o, "p", "P"); q := replace(p, "q", "Q"); r := replace(q, "r", "R"); s := replace(r, "s", "S"); t := replace(s, "t", "T"); u := replace(t, "u", "U"); v := replace(u, "v", "V"); w := replace(v, "w", "W"); x := replace(w, "x", "X"); y := replace(x, "y", "Y"); result := replace(y, "z", "Z")
}

category_value := value if { value := object.get(input.context, "category", ""); is_string(value) }
category_value := "" if { not is_string(object.get(input.context, "category", "")) }
detection_facet := "anime" if { trash_detection_ascii_fold(trim_space(category_value)) == "anime" }
detection_facet := "series" if { trash_detection_ascii_fold(trim_space(category_value)) == "series" }
detection_facet := "movie" if { not trash_detection_ascii_fold(trim_space(category_value)) == "anime"; not trash_detection_ascii_fold(trim_space(category_value)) == "series" }
detection_scope := "anime" if { detection_facet == "anime" }
detection_scope := "any" if { not detection_facet == "anime" }

pattern_matches(pattern, tokens) if {
  pattern.kind == "required_tokens"
  every wanted in pattern.tokens { some candidate in tokens; candidate == wanted }
}
pattern_matches(pattern, tokens) if {
  pattern.kind == "sequence"
  count(pattern.tokens) > 0
  some start
  tokens[start] == pattern.tokens[0]
  start + count(pattern.tokens) <= count(tokens)
  every offset, wanted in pattern.tokens { tokens[start + offset] == wanted }
}

rule_applies(rule) if { rule.facet == detection_facet; rule.category == "any" }
rule_applies(rule) if { rule.facet == detection_facet; rule.category == "anime"; detection_scope == "anime" }

signal_codes(rule) := ["trash.ai_enhanced"] if { rule.kind == "ai_enhanced" }
signal_codes(rule) := ["trash.proper"] if { rule.kind == "proper" }
signal_codes(rule) := ["trash.proper", "trash.repack"] if { rule.kind == "repack" }
signal_codes(rule) := ["trash.dubs_only"] if { rule.kind == "dubs_only" }
signal_codes(rule) := ["trash.hardcoded_subs"] if { rule.kind == "hardcoded_subs" }

blocked_fact(rule) := "trash.blocked.anime_raws" if { rule.code == "trash_guides_anime_raws" }
blocked_fact(rule) := "trash.blocked.lq_release_title" if { rule.code == "trash_guides_lq_release_title" }
blocked_fact(rule) := "trash.blocked.fansub" if { rule.code == "trash_guides_fansub" }
blocked_fact(rule) := "trash.blocked.fastsub" if { rule.code == "trash_guides_fastsub" }
blocked_fact(rule) := "trash.blocked.legacy" if { not rule.code == "trash_guides_anime_raws"; not rule.code == "trash_guides_lq_release_title"; not rule.code == "trash_guides_fansub"; not rule.code == "trash_guides_fastsub" }

french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VF2($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF[.]VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF[.]VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ[.]VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ[.]VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }

german_invalid_gap(language, subtitle) if {
  some gap_index
  gap_index > language
  gap_index < subtitle
  gap := normalized_tokens[gap_index]
  not regex.match("^[A-Z]+$", gap)
}
german_invalid_gap(language, subtitle) if {
  some gap_index
  gap_index > language
  gap_index < subtitle
  contains(normalized_tokens[gap_index], "DUB")
}
german_invalid_gap(language, subtitle) if {
  some gap_index
  gap_index > language
  gap_index < subtitle
  normalized_tokens[gap_index] in {"DL", "ML"}
}
german_subbed if {
  some language
  normalized_tokens[language] in {"GER", "GERMAN"}
  some subtitle
  subtitle > language
  normalized_tokens[subtitle] in {"OMU", "SUB", "SUBBED", "SUBS"}
  not german_invalid_gap(language, subtitle)
}

locale_context_matches(context) if { context == "any" }
locale_context_matches(context) if { context == "anime"; detection_facet == "anime" }
locale_context_matches(context) if { context == "anime_bd"; detection_facet == "anime"; trash_detection_ascii_fold(release_source) in {"bluray", "br-disk", "brdisk"} }
locale_context_matches(context) if { context == "anime_web"; detection_facet == "anime"; trash_detection_ascii_fold(release_source) in {"web-dl", "webrip"} }
release_source := value if { value := object.get(input.release, "source", ""); is_string(value) }
release_group_value := value if { value := object.get(input.release, "release_group", ""); is_string(value) }
release_quality := value if { value := object.get(input.release, "quality", ""); is_string(value) }
release_group_folded := trash_detection_ascii_fold(release_group_value) if { release_group_value }
locale_context_matches(context) if { context == "web"; trash_detection_ascii_fold(release_source) in {"web-dl", "webrip"} }
locale_context_matches(context) if { context == "remux"; input.release.is_remux }
locale_context_matches(context) if { context == "bluray"; trash_detection_ascii_fold(release_source) == "bluray"; not input.release.is_remux; not contains(release_quality, "2160") }
locale_context_matches(context) if { context == "uhd_bluray"; trash_detection_ascii_fold(release_source) == "bluray"; not input.release.is_remux; contains(release_quality, "2160") }
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.token_signal_rules_by_anchor[token]
  pattern_matches(rule.pattern, normalized_tokens)
  some code in signal_codes(rule)
}
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.blocked_title_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := blocked_fact(rule)
}
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.fact_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := rule.code
  code != "trash.locale.german.marker.subbed"
  not french_vf2_exclusion
}
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.fact_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := rule.code
  code != "trash.locale.french.marker.vff"
  code != "trash.locale.french.marker.vfq"
  code != "trash.locale.german.marker.subbed"
  french_vf2_exclusion
}
detected_facts["trash.locale.german.marker.subbed"] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.fact_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  rule.code == "trash.locale.german.marker.subbed"
  german_subbed
}
detected_facts[code] if {
  normalized_tokens
  group := release_group_value
  group != ""
  some rule in trash_detection_tables.locale_group_exact[concat("|", [detection_facet, release_group_folded])]
  locale_context_matches(rule[1])
  code := rule[0]
}
detected_facts[code] if {
  normalized_tokens
  group := release_group_value
  group != ""
  some rule in trash_detection_tables.locale_group_prefix[detection_facet]
  startswith(release_group_folded, trash_detection_ascii_fold(rule[0]))
  locale_context_matches(rule[2])
  code := rule[1]
}
detected_facts["trash.no_release_group"] if {
  normalized_tokens
  object.get(input.release, "release_group", null) in {null, ""}
  some facet in trash_detection_tables.no_release_group_fact_facets
  facet == detection_facet
}

has_detected_fact(code) if { detected_facts[code] }
detected_fact_bool(code) := true if { detected_facts[code] }
detected_fact_bool(code) := false if { not detected_facts[code] }

# This diagnostic mirrors Rust detect_token_signals, rather than all derived
# facts: blocked/fact rules may independently project hardcoded-subs.
detected_token_signal(kind) if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.token_signal_rules_by_anchor[token]
  pattern_matches(rule.pattern, normalized_tokens)
  rule.kind == kind
}
detected_token_signal_bool(kind) := true if { detected_token_signal(kind) }
detected_token_signal_bool(kind) := false if { not detected_token_signal(kind) }
detected_proper_signal if { detected_token_signal("proper") }
detected_proper_signal if { detected_token_signal("repack") }
detected_proper_signal_bool := true if { detected_proper_signal }
detected_proper_signal_bool := false if { not detected_proper_signal }

detected_services[service] if {
  normalized_tokens
  some index
  token := normalized_tokens[index]
  some rule in trash_detection_tables.service_aliases_by_token[token]
  not rule.requires_web_adjacency
  service := rule.service
}
detected_services[service] if {
  normalized_tokens
  some index
  token := normalized_tokens[index]
  some rule in trash_detection_tables.service_aliases_by_token[token]
  rule.requires_web_adjacency
  next := normalized_tokens[index + 1]
  next in {"WEB", "WEBDL", "WEBRIP"}
  service := rule.service
}

# The parser assigns StreamingService roles before selecting this scalar. Alias
# candidates above are diagnostic only; scoring must use this parser-selected
# input when it needs the current first-role precedence.
selected_service := service if { service := object.get(input.release, "streaming_service", ""); is_string(service); service != "" }

detected_signals := {"ai_enhanced": detected_token_signal_bool("ai_enhanced"), "proper": detected_proper_signal_bool, "repack": detected_token_signal_bool("repack"), "dubs_only": detected_token_signal_bool("dubs_only"), "hardcoded_subs": detected_token_signal_bool("hardcoded_subs")}
detected_blocked_title := code if {
  normalized_tokens
  matching_orders := [rule.order | some token in normalized_tokens; some rule in trash_detection_tables.blocked_title_rules_by_anchor[token]; rule_applies(rule); pattern_matches(rule.pattern, normalized_tokens)]
  first_order := min(matching_orders)
  some token in normalized_tokens
  some rule in trash_detection_tables.blocked_title_rules_by_anchor[token]
  rule.order == first_order
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := rule.code
}
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": selected_service, "signals": detected_signals, "blocked": detected_blocked_title} if { detection_available; detected_blocked_title; selected_service }
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": null, "signals": detected_signals, "blocked": detected_blocked_title} if { detection_available; detected_blocked_title; not selected_service }
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": selected_service, "signals": detected_signals, "blocked": null} if { detection_available; not detected_blocked_title; selected_service }
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": null, "signals": detected_signals, "blocked": null} if { detection_available; not detected_blocked_title; not selected_service }
detector_result := {"available": false, "reason": "release.normalized_tokens is required"} if { not detection_available }
`
