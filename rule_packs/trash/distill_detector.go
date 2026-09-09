package main

import (
	"encoding/json"
	"path"
	"sort"
	"strings"
)

type detectorPattern struct {
	Kind   string   `json:"kind"`
	Tokens []string `json:"tokens"`
}
type detectorRow struct {
	Kind     string          `json:"kind,omitempty"`
	Code     string          `json:"code,omitempty"`
	Facet    string          `json:"facet,omitempty"`
	Category string          `json:"category,omitempty"`
	Pattern  detectorPattern `json:"pattern"`
}

func tokenPatterns(value string) []detectorPattern {
	seen := map[string]detectorPattern{}
	add := func(kind string, tokens ...string) {
		for i := range tokens {
			tokens[i] = strings.ToUpper(tokens[i])
		}
		key := kind + "|" + strings.Join(tokens, "|")
		seen[key] = detectorPattern{kind, tokens}
	}
	v := strings.TrimSpace(value)
	if strings.HasPrefix(v, `\b`) && strings.HasSuffix(v, `\b`) {
		inner := strings.TrimSuffix(strings.TrimPrefix(v, `\b`), `\b`)
		inner = strings.TrimPrefix(inner, "(")
		inner = strings.TrimSuffix(inner, ")")
		inner = strings.TrimPrefix(inner, "?:")
		valid := true
		for _, part := range strings.Split(inner, "|") {
			part = strings.TrimSpace(part)
			if part == "" || !asciiAlphaNumeric(part) {
				valid = false
				break
			}
			add("sequence", part)
		}
		if !valid {
			// This is deliberately all-or-nothing, matching simple_boundary_patterns.
			for key, pattern := range seen {
				if pattern.Kind == "sequence" && len(pattern.Tokens) == 1 {
					delete(seen, key)
				}
			}
		}
	}
	values, _ := finiteGroupLiterals(v)
	for _, value := range values {
		if asciiAlphaNumeric(value) {
			add("sequence", value)
		}
	}
	if strings.Contains(v, "(?=.*") {
		if tokens := extractBoundaryTokens(v); len(tokens) >= 2 {
			add("required_tokens", tokens...)
		}
	}
	if strings.Contains(v, "[ ._-]?") || strings.Contains(v, "[ ._-]") {
		cleaned := strings.NewReplacer(
			`\b`, " ",
			`[ ._-]?`, " ",
			`[ ._-]`, " ",
			"(", " ", ")", " ", "[", " ", "]", " ", "^", " ", "$", " ", "?", " ", "*", " ", "|", " ",
		).Replace(v)
		var tokens []string
		for _, part := range strings.Fields(cleaned) {
			if token := sanitizeDetectorToken(part); token != "" {
				tokens = append(tokens, token)
			}
		}
		if len(tokens) >= 2 {
			add("sequence", tokens...)
		}
	}
	var out []detectorPattern
	for _, row := range seen {
		out = append(out, row)
	}
	sort.Slice(out, func(i, j int) bool {
		return out[i].Kind+strings.Join(out[i].Tokens, "|") < out[j].Kind+strings.Join(out[j].Tokens, "|")
	})
	return out
}

func asciiAlphaNumeric(value string) bool {
	if value == "" {
		return false
	}
	for _, ch := range value {
		if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9') {
			return false
		}
	}
	return true
}

func sanitizeDetectorToken(value string) string {
	var out strings.Builder
	for _, ch := range value {
		if ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' {
			out.WriteRune(ch)
		}
	}
	return strings.ToUpper(out.String())
}

func extractBoundaryTokens(value string) []string {
	var tokens []string
	for input := value; ; {
		start := strings.Index(input, `\b`)
		if start < 0 {
			break
		}
		afterStart := input[start+2:]
		end := strings.Index(afterStart, `\b`)
		if end < 0 {
			break
		}
		if token := sanitizeDetectorToken(afterStart[:end]); token != "" {
			found := false
			for _, existing := range tokens {
				if existing == token {
					found = true
					break
				}
			}
			if !found {
				tokens = append(tokens, token)
			}
		}
		input = afterStart[end+2:]
	}
	return tokens
}
func distillDetectorRows(raw rawUpstreamSnapshot) (signals, blocked []detectorRow) {
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		facet := "series"
		if strings.Contains(file.Path, "/radarr/") {
			facet = "movie"
		}
		if strings.HasPrefix(stem, "anime-") || stem == "fansub" || stem == "fastsub" || stem == "dubs-only" {
			facet = "anime"
		}
		for _, record := range file.Records {
			for _, spec := range record.Specifications {
				if spec.Implementation != "ReleaseTitleSpecification" || isTrue(spec.Negate) {
					continue
				}
				pattern, err := groupPattern(spec.Fields)
				if err != nil {
					continue
				}
				patterns := tokenPatterns(pattern)
				switch stem {
				case "upscaled":
					if strings.EqualFold(spec.Name, "AI Upscales") {
						patterns = append(patterns, detectorPattern{"required_tokens", []string{"AI", "ENHANCED"}})
					}
					if strings.EqualFold(spec.Name, "Upscaled") {
						patterns = append(patterns, detectorPattern{"sequence", []string{"UPSCALED"}}, detectorPattern{"sequence", []string{"UPREZ"}})
					}
					for _, p := range patterns {
						signals = append(signals, detectorRow{Kind: "ai_enhanced", Facet: facet, Pattern: p})
					}
				case "repack-proper", "repack2", "repack3":
					if strings.EqualFold(spec.Name, "Repack/Proper/Rerip") {
						patterns = append(patterns, detectorPattern{"sequence", []string{"PROPER"}}, detectorPattern{"sequence", []string{"REPACK"}}, detectorPattern{"sequence", []string{"RERIP"}})
					}
					if strings.EqualFold(spec.Name, "Not Higher Version Repack/Proper") {
						patterns = append(patterns, detectorPattern{"sequence", []string{"REPACK2"}}, detectorPattern{"sequence", []string{"REPACK3"}}, detectorPattern{"required_tokens", []string{"REAL", "PROPER"}}, detectorPattern{"required_tokens", []string{"REAL", "REPACK"}})
					}
					if stem == "repack2" {
						patterns = append(patterns, detectorPattern{"sequence", []string{"PROPER2"}}, detectorPattern{"sequence", []string{"REPACK2"}}, detectorPattern{"required_tokens", []string{"REAL", "PROPER"}}, detectorPattern{"required_tokens", []string{"REAL", "REPACK"}})
					}
					if stem == "repack3" {
						patterns = append(patterns, detectorPattern{"sequence", []string{"PROPER3"}}, detectorPattern{"sequence", []string{"REPACK3"}})
					}
					for _, p := range patterns {
						kind := "proper"
						for _, token := range p.Tokens {
							if token == "REPACK" || token == "RERIP" {
								kind = "repack"
							}
						}
						signals = append(signals, detectorRow{Kind: kind, Facet: facet, Pattern: p})
					}
				case "dubs-only":
					// The source regexes include exclusions for dual audio.  The
					// upstream converter intentionally keeps only its explicit Dubbed
					// detector vocabulary, so do not infer signals from other specs.
					if strings.EqualFold(spec.Name, "Dubbed") {
						for _, p := range []detectorPattern{
							{"sequence", []string{"DUB"}},
							{"sequence", []string{"DUBBED"}},
							{"required_tokens", []string{"ENG", "DUB"}},
							{"required_tokens", []string{"FUNI", "DUB"}},
						} {
							signals = append(signals, detectorRow{Kind: "dubs_only", Facet: facet, Pattern: p})
						}
					}
				case "anime-raws":
					for _, p := range patterns {
						blocked = append(blocked, detectorRow{Code: "trash_guides_anime_raws", Facet: "anime", Category: "anime", Pattern: p})
					}
				case "lq-release-title":
					for _, p := range patterns {
						blocked = append(blocked, detectorRow{Code: "trash_guides_lq_release_title", Facet: facet, Category: "any", Pattern: p})
					}
				case "fansub":
					for _, p := range patterns {
						blocked = append(blocked, detectorRow{Code: "trash_guides_fansub", Facet: "anime", Category: "anime", Pattern: p})
					}
				case "fastsub":
					for _, p := range patterns {
						blocked = append(blocked, detectorRow{Code: "trash_guides_fastsub", Facet: "anime", Category: "anime", Pattern: p})
					}
				}
			}
		}
	}
	return dedupeDetector(signals), dedupeDetector(blocked)
}
func dedupeDetector(rows []detectorRow) []detectorRow {
	seen := map[string]detectorRow{}
	for _, row := range rows {
		data, _ := json.Marshal(row)
		seen[string(data)] = row
	}
	rows = rows[:0]
	for _, row := range seen {
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool {
		a, _ := json.Marshal(rows[i])
		b, _ := json.Marshal(rows[j])
		return string(a) < string(b)
	})
	return rows
}
