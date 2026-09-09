package main

import (
	"encoding/json"
	"fmt"
	"path"
	"sort"
	"strings"
)

// distillGroupSnapshot is phase B's deliberately narrow bridge from the
// normalized raw tree. Unsupported regexes are reported instead of broadened.
func distillGroupSnapshot(raw rawUpstreamSnapshot) (snapshot, []string, error) {
	var rows []groupRule
	var ignored []string
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		tier, context, active := groupStem(stem)
		if !active {
			continue
		}
		facet := "series"
		if strings.Contains(file.Path, "/radarr/") {
			facet = "movie"
		}
		if strings.HasPrefix(stem, "anime-") {
			facet = "anime"
		}
		for _, record := range file.Records {
			for _, spec := range record.Specifications {
				if isTrue(spec.Negate) {
					ignored = append(ignored, file.Path+":"+spec.Name+":negated_group")
					continue
				}
				var matchers []distilledGroupMatcher
				var err error
				switch spec.Implementation {
				case "ReleaseGroupSpecification":
					matchers, err = groupMatchers(spec.Fields)
				case "ReleaseTitleSpecification":
					var raw string
					raw, err = groupPattern(spec.Fields)
					if err == nil {
						var matcher distilledGroupMatcher
						var ok bool
						matcher, ok = titleSpecGroupMatcher(spec.Name, raw)
						if !ok {
							err = fmt.Errorf("non_lossless_title_regex")
						} else {
							matchers = []distilledGroupMatcher{matcher}
						}
					}
				default:
					ignored = append(ignored, file.Path+":"+spec.Name+":unsupported_implementation")
					continue
				}
				if err != nil {
					ignored = append(ignored, file.Path+":"+spec.Name+":"+err.Error())
					continue
				}
				for _, matcher := range matchers {
					rows = append(rows, groupRule{Matcher: matcher.value, MatchKind: matcher.kind, Tier: tier, Facet: facet, SourceContext: context})
				}
			}
		}
	}
	rows = append(rows, legacyGroupRows()...)
	// Mirror xtask's group-tier conflict collapse: a single matcher/context
	// keeps Banned, then Gold, Silver, or Bronze irrespective of file order.
	collapsed := map[string]groupRule{}
	for _, row := range rows {
		key := strings.Join([]string{row.Matcher, row.MatchKind, row.Facet, row.SourceContext}, "\x00")
		current, exists := collapsed[key]
		if !exists || groupTierRank(row.Tier) < groupTierRank(current.Tier) {
			collapsed[key] = row
		}
	}
	rows = rows[:0]
	for _, row := range collapsed {
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool {
		a, b := rows[i], rows[j]
		return strings.Join([]string{a.Facet, a.SourceContext, a.Tier, a.Matcher}, "\x00") < strings.Join([]string{b.Facet, b.SourceContext, b.Tier, b.Matcher}, "\x00")
	})
	unique := rows[:0]
	seen := map[string]bool{}
	for _, row := range rows {
		key := strings.Join([]string{row.Matcher, row.Tier, row.Facet, row.SourceContext}, "\x00")
		if !seen[key] {
			seen[key] = true
			row.Index = len(unique)
			unique = append(unique, row)
		}
	}
	if len(unique) == 0 {
		return snapshot{}, ignored, fmt.Errorf("raw snapshot emitted no active group rows")
	}
	encoded, err := json.Marshal(unique)
	if err != nil {
		return snapshot{}, ignored, err
	}
	return snapshot{SchemaVersion: 1, SourceRevision: raw.SourceRevision, GroupRules: encoded}, ignored, nil
}
func groupTierRank(tier string) int {
	switch tier {
	case "banned":
		return 0
	case "gold":
		return 1
	case "silver":
		return 2
	default:
		return 3
	}
}
func legacyGroupRows() []groupRule {
	seeds := []struct{ name, tier, context string }{
		{"BDMV", "banned", "anime"}, {"BDVD", "banned", "anime"}, {"BakedFish", "banned", "anime"}, {"D3US", "banned", "any"}, {"DaddySubs", "banned", "anime"}, {"DeadFish", "banned", "anime"}, {"Deadmau RAWS", "banned", "anime"}, {"Erai-raws", "bronze", "anime"}, {"GSK_kun", "silver", "anime"}, {"Iznjie Biznjie", "silver", "anime"}, {"Judgment", "bronze", "anime"}, {"M2TS", "banned", "anime"}, {"Mr.Deadpool", "banned", "anime"}, {"NAN0", "gold", "anime"}, {"NoGrop", "banned", "any"}, {"NoobSubs", "banned", "anime"}, {"PiRaTeS", "banned", "any"}, {"PMR", "gold", "anime"}, {"QAS", "banned", "anime"}, {"SpaceFish", "banned", "anime"}, {"SubsPlus+", "silver", "anime"}, {"tenshi", "silver", "anime"}, {"VISIONPLUSHDR-X", "banned", "any"}, {"VISIONPLUSHDR1000", "banned", "any"}, {"WtF Anime", "banned", "anime"}, {"YTS.AG", "banned", "any"}, {"YTS.LT", "banned", "any"}, {"YTS.MX", "banned", "any"}, {"jennaortega", "banned", "any"}, {"mal lu zen", "banned", "anime"},
	}
	var rows []groupRule
	for _, seed := range seeds {
		facets := []string{"anime"}
		if seed.context == "any" {
			facets = []string{"movie", "series", "anime"}
		}
		for _, facet := range facets {
			rows = append(rows, groupRule{Matcher: seed.name, MatchKind: "exact", Tier: seed.tier, Facet: facet, SourceContext: seed.context})
		}
	}
	return rows
}
func groupStem(stem string) (tier, context string, active bool) {
	mapTier := func(value string) string {
		if value == "01" || value == "1" {
			return "gold"
		}
		if value == "02" || value == "2" {
			return "silver"
		}
		return "bronze"
	}
	for _, prefix := range []struct{ prefix, context string }{{"web-tier-", "web"}, {"hd-bluray-tier-", "bluray"}, {"uhd-bluray-tier-", "uhd_bluray"}, {"remux-tier-", "remux"}, {"anime-bd-tier-", "anime_bd"}, {"anime-web-tier-", "anime_web"}} {
		if value, ok := strings.CutPrefix(stem, prefix.prefix); ok {
			return mapTier(value), prefix.context, true
		}
	}
	if stem == "lq" || stem == "bad-dual-groups" {
		return "banned", "any", true
	}
	if stem == "anime-lq-groups" {
		return "banned", "anime", true
	}
	return "", "", false
}

func groupPattern(fields json.RawMessage) (string, error) {
	var value map[string]any
	if err := json.Unmarshal(fields, &value); err != nil {
		return "", fmt.Errorf("invalid_fields")
	}
	raw, ok := value["value"].(string)
	if !ok || raw == "" {
		return "", fmt.Errorf("missing_value")
	}
	return raw, nil
}
func groupMatchers(fields json.RawMessage) ([]distilledGroupMatcher, error) {
	raw, err := groupPattern(fields)
	if err != nil {
		return nil, err
	}
	return distillGroupMatchers(raw)
}
func isTrue(value json.RawMessage) bool {
	var result bool
	return json.Unmarshal(value, &result) == nil && result
}
