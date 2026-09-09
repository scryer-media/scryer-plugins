package main

import (
	"encoding/json"
	"path"
	"sort"
	"strings"
)

type localeGroupRow struct {
	Code          string `json:"code"`
	Matcher       string `json:"matcher"`
	MatchKind     string `json:"match_kind"`
	Facet         string `json:"facet"`
	SourceContext string `json:"source_context"`
}

// distillLocaleGroupRows is the lossless group-only locale path. Records whose
// group expression cannot be reduced exactly remain explicit ignored rows.
func distillLocaleGroupRows(raw rawUpstreamSnapshot) ([]localeGroupRow, []string) {
	var rows []localeGroupRow
	var ignored []string
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		if stem == "obfuscated" {
			facet := localeFacet(file.Path, stem)
			for _, record := range file.Records {
				for _, spec := range record.Specifications {
					if spec.Implementation != "ReleaseTitleSpecification" || isTrue(spec.Negate) {
						continue
					}
					pattern, err := groupPattern(spec.Fields)
					if err != nil {
						continue
					}
					if matcher, ok := terminalObfuscatedMatcher(pattern); ok {
						rows = append(rows, localeGroupRow{Code: "trash.obfuscated", Matcher: matcher, MatchKind: "exact", Facet: facet, SourceContext: "any"})
					}
				}
			}
			continue
		}
		if stem == "scene" {
			facet := localeFacet(file.Path, stem)
			for _, record := range file.Records {
				for _, spec := range record.Specifications {
					if spec.Implementation != "ReleaseTitleSpecification" || isTrue(spec.Negate) {
						continue
					}
					pattern, err := groupPattern(spec.Fields)
					if err != nil {
						continue
					}
					if start := strings.Index(pattern, `|\b(-`); start >= 0 {
						values, _ := finiteGroupLiterals(pattern[start+1:])
						for _, value := range values {
							if matcher, ok := strings.CutPrefix(value, "-"); ok {
								rows = append(rows, localeGroupRow{Code: "trash.scene", Matcher: matcher, MatchKind: "exact", Facet: facet, SourceContext: "any"})
							}
						}
					}
				}
			}
			continue
		}
		if !strings.HasPrefix(stem, "french-") && !strings.HasPrefix(stem, "german-") && !strings.HasPrefix(stem, "asian-") {
			continue
		}
		context, ok := localeGroupContext(stem)
		if !ok {
			continue
		}
		facet := localeFacet(file.Path, stem)
		if context == "anime" || context == "anime_bd" || context == "anime_web" {
			facet = "anime"
		}
		for _, record := range file.Records {
			for _, spec := range record.Specifications {
				if isTrue(spec.Negate) {
					ignored = append(ignored, file.Path+":"+spec.Name+":negated")
					continue
				}
				var matchers []distilledGroupMatcher
				var err error
				switch spec.Implementation {
				case "ReleaseGroupSpecification":
					matchers, err = groupMatchers(spec.Fields)
				case "ReleaseTitleSpecification":
					var pattern string
					pattern, err = groupPattern(spec.Fields)
					if err == nil {
						var matcher distilledGroupMatcher
						var match bool
						matcher, match = titleSpecGroupMatcher(spec.Name, pattern)
						if !match {
							err = errNonLosslessTitle
						}
						matchers = []distilledGroupMatcher{matcher}
					}
				default:
					continue
				}
				if err != nil {
					ignored = append(ignored, file.Path+":"+spec.Name+":"+err.Error())
					continue
				}
				for _, matcher := range matchers {
					rows = append(rows, localeGroupRow{Code: localeCode(stem), Matcher: matcher.value, MatchKind: matcher.kind, Facet: facet, SourceContext: context})
				}
			}
		}
	}
	unique := map[string]localeGroupRow{}
	for _, row := range rows {
		unique[localeKey(row)] = row
	}
	rows = rows[:0]
	for _, row := range unique {
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool { return localeKey(rows[i]) < localeKey(rows[j]) })
	return rows, ignored
}
func terminalObfuscatedMatcher(value string) (string, bool) {
	value = strings.TrimSpace(strings.TrimSuffix(value, `\b`))
	if strings.HasPrefix(value, "-") {
		value = strings.TrimPrefix(value, "-")
	} else if strings.HasPrefix(value, "_") {
		value = strings.TrimPrefix(value, "_")
	} else {
		return "", false
	}
	if value == "" {
		return "", false
	}
	for _, r := range value {
		if !((r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9')) {
			return "", false
		}
	}
	return value, true
}

var errNonLosslessTitle = &localeError{"non_lossless_title_regex"}

type localeError struct{ value string }

func (e *localeError) Error() string { return e.value }
func localeGroupContext(stem string) (string, bool) {
	if strings.Contains(stem, "anime-bd-tier-") || strings.Contains(stem, "anime-bluray-tier-") {
		return "anime_bd", true
	}
	if strings.Contains(stem, "anime-web-tier-") {
		return "anime_web", true
	}
	if strings.Contains(stem, "anime-") && strings.Contains(stem, "tier-") {
		return "anime", true
	}
	if strings.Contains(stem, "remux-tier-") {
		return "remux", true
	}
	if strings.Contains(stem, "uhd-bluray-tier-") {
		return "uhd_bluray", true
	}
	if strings.Contains(stem, "bluray-tier-") {
		return "bluray", true
	}
	if strings.Contains(stem, "web-tier-") {
		return "web", true
	}
	if strings.HasPrefix(stem, "asian-tier-") || strings.HasSuffix(stem, "-lq") || strings.HasSuffix(stem, "-scene") {
		return "any", true
	}
	return "", false
}
func localeFacet(file, stem string) string {
	if strings.HasPrefix(stem, "anime-") {
		return "anime"
	}
	if strings.Contains(file, "/radarr/") {
		return "movie"
	}
	return "series"
}
func localeCode(stem string) string {
	parts := strings.SplitN(stem, "-", 2)
	semantic := parts[1]
	switch {
	case strings.Contains(semantic, "tier-01"):
		semantic = "group.tier1"
	case strings.Contains(semantic, "tier-02"):
		semantic = "group.tier2"
	case strings.Contains(semantic, "tier-03"):
		semantic = "group.tier3"
	case strings.Contains(semantic, "lq"):
		semantic = "lq"
	case strings.Contains(semantic, "scene"):
		semantic = "scene"
	default:
		semantic = "marker." + strings.ReplaceAll(semantic, "-", "_")
	}
	return "trash.locale." + parts[0] + "." + semantic
}
func localeKey(row localeGroupRow) string {
	return strings.Join([]string{row.Code, row.Matcher, row.MatchKind, row.Facet, row.SourceContext}, "|")
}
func localeRowsFromReference(data []byte) ([]localeGroupRow, error) {
	var snapshot detectionSnapshot
	if err := json.Unmarshal(data, &snapshot); err != nil {
		return nil, err
	}
	raw := snapshot.Tables["locale_group_fact_rules"].([]interface{})
	out := make([]localeGroupRow, 0, len(raw))
	for _, value := range raw {
		row := value.(map[string]interface{})
		out = append(out, localeGroupRow{Code: row["code"].(string), Matcher: row["matcher"].(string), MatchKind: row["match_kind"].(string), Facet: row["facet"].(string), SourceContext: row["source_context"].(string)})
	}
	return out, nil
}
