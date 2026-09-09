package main

import (
	"path"
	"sort"
	"strings"
)

// distillFactRows translates the parser-visible facts directly from the raw
// custom-format tree.  It deliberately shares tokenPatterns with detector
// signal extraction so a refresh cannot make the two tables disagree.
func distillFactRows(raw rawUpstreamSnapshot) (facts []detectorRow, noGroup []string) {
	signals, blocked := distillDetectorRows(raw)
	for _, signal := range signals {
		for _, code := range factCodesForSignal(signal.Kind) {
			facts = append(facts, detectorRow{Code: code, Facet: signal.Facet, Category: "any", Pattern: signal.Pattern})
		}
	}
	for _, row := range blocked {
		if row.Code == "trash_guides_fansub" || row.Code == "trash_guides_fastsub" {
			facts = append(facts,
				detectorRow{Code: "trash.hardcoded_subs", Facet: "anime", Category: "anime", Pattern: row.Pattern},
				detectorRow{Code: "trash." + strings.TrimPrefix(row.Code, "trash_guides_"), Facet: "anime", Category: "anime", Pattern: row.Pattern},
			)
		}
	}
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
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
				if isLocaleMarkerStem(stem) && (localeMarkerSufficient(record, spec) || stem == "french-vff" || stem == "french-vfq" || stem == "german-subbed") {
					patterns := tokenPatterns(pattern)
					patterns = appendNamedPatternSpecials(stem, spec.Name, patterns)
					for _, p := range patterns {
						facts = append(facts, detectorRow{Code: localeCode(stem), Facet: facet, Category: "any", Pattern: p})
					}
				}
				if stem == "retags" {
					for _, p := range tokenPatterns(pattern) {
						facts = append(facts, detectorRow{Code: "trash.retagged", Facet: facet, Category: "any", Pattern: p})
					}
				}
			}
			if stem == "no-rlsgroup" {
				for _, spec := range record.Specifications {
					if spec.Implementation == "ReleaseGroupSpecification" {
						if value, err := groupPattern(spec.Fields); err == nil && strings.TrimSpace(value) == "." {
							noGroup = append(noGroup, facet)
						}
					}
				}
			}
		}
	}
	return dedupeDetector(facts), dedupeStrings(noGroup)
}

func factCodesForSignal(kind string) []string {
	switch kind {
	case "ai_enhanced":
		return []string{"trash.ai_enhanced"}
	case "proper":
		return []string{"trash.proper"}
	case "repack":
		return []string{"trash.proper", "trash.repack"}
	case "dubs_only":
		return []string{"trash.dubs_only"}
	default:
		return nil
	}
}

func isLocaleMarkerStem(stem string) bool {
	if !(strings.HasPrefix(stem, "french-") || strings.HasPrefix(stem, "german-") || strings.HasPrefix(stem, "asian-")) {
		return false
	}
	_, group := localeGroupContext(stem)
	return !group
}

func localeMarkerSufficient(record rawCustomFormat, spec rawSpecification) bool {
	if strings.EqualFold(spec.Name, "") {
		return false
	}
	if strings.HasPrefix(record.Name, "") { // keep the rule local to this custom format below
		// A marker is sufficient if the format has no required specifications, or
		// it is its sole required specification.  The raw format owns all specs.
		required := 0
		for _, candidate := range record.Specifications {
			if isTrue(candidate.Required) {
				required++
				if candidate.Name != spec.Name || candidate.Implementation != spec.Implementation || string(candidate.Fields) != string(spec.Fields) {
					return false
				}
			}
		}
		return required == 0 || required == 1
	}
	return false
}

func appendNamedPatternSpecials(stem, name string, patterns []detectorPattern) []detectorPattern {
	if stem == "french-vfq" {
		patterns = append(patterns, detectorPattern{Kind: "sequence", Tokens: []string{"VFQ"}})
	}
	if stem == "german-subbed" {
		for _, language := range []string{"GER", "GERMAN"} {
			for _, subtitle := range []string{"OMU", "SUB", "SUBBED", "SUBS"} {
				patterns = append(patterns, detectorPattern{Kind: "required_tokens", Tokens: []string{language, subtitle}})
			}
		}
	}
	return patterns
}

func dedupeStrings(values []string) []string {
	seen := map[string]bool{}
	for _, value := range values {
		seen[value] = true
	}
	out := make([]string, 0, len(seen))
	for value := range seen {
		out = append(out, value)
	}
	sort.Strings(out)
	return out
}
