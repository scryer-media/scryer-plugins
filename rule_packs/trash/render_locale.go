package main

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
)

// normalizedFactScore ports managed_trash.rs upstream_score/pack_score.  The
// selected set order is primary, anime, then default; app ownership is chosen
// from the fact code and weakest equivalent upstream value wins.
func normalizedFactScore(snap snapshot, code string, selectedSets []string) (int64, bool, error) {
	var rows []factScore
	if err := json.Unmarshal(snap.FactScores, &rows); err != nil {
		return 0, false, err
	}
	var envelopes []scoreEnvelope
	if err := json.Unmarshal(snap.ScoreEnvelopes, &envelopes); err != nil {
		return 0, false, err
	}
	veto := map[string]int64{}
	for _, row := range envelopes {
		for _, value := range row.Vetoes {
			magnitude := abs64(value)
			if veto[row.ScoreSet] == 0 || magnitude < veto[row.ScoreSet] {
				veto[row.ScoreSet] = magnitude
			}
		}
		if veto[row.ScoreSet] == 0 {
			veto[row.ScoreSet] = 10000
		}
	}
	sets := append(append([]string{}, selectedSets...), "default")
	apps := []string{"radarr", "sonarr"}
	if contains(code, "anime") {
		apps = []string{"sonarr", "radarr"}
	}
	for _, set := range sets {
		for _, app := range apps {
			var chosen *int64
			for _, row := range rows {
				if row.Code == code && row.ScoreSet == set && row.App == app {
					value := row.Score
					if chosen == nil || abs64(value) < abs64(*chosen) || abs64(value) == abs64(*chosen) && value < *chosen {
						chosen = &value
					}
				}
			}
			if chosen != nil {
				return normalizeTrashScore(*chosen, veto[selectedSets[0]]), true, nil
			}
		}
	}
	return 0, false, nil
}
func normalizeTrashScore(value, veto int64) int64 {
	if value <= -10000 {
		return -10000
	}
	magnitude := abs64(value)
	scaled := magnitude
	if magnitude > 100 {
		span := veto - 100
		if span <= 0 {
			scaled = 1000
		} else {
			over := magnitude - 100
			if over > span {
				over = span
			}
			scaled = 100 + over*900/span
		}
	}
	if value < 0 {
		return -scaled
	}
	return scaled
}
func abs64(value int64) int64 {
	if value < 0 {
		return -value
	}
	return value
}
func contains(value, wanted string) bool {
	for i := 0; i+len(wanted) <= len(value); i++ {
		if value[i:i+len(wanted)] == wanted {
			return true
		}
	}
	return false
}

func localeScoreSets(name string) []string {
	switch name {
	case "french-vf":
		return []string{"french-multi-vf", "french-anime-multi"}
	case "french-vo":
		return []string{"french-multi-vo", "french-anime-multi"}
	case "french-vostfr":
		return []string{"french-vostfr", "french-anime-vostfr"}
	case "german":
		return []string{"german", "german-anime"}
	default:
		return []string{"default", "default"}
	}
}
func localeLanguageStems(name string) map[string]bool {
	if strings.HasPrefix(name, "french-") {
		return map[string]bool{"language-not-french": true, "language-not-original": true, "language-original-plus-french": true}
	}
	if name == "german" {
		return map[string]bool{"language-not-original": true, "not-german-or-english": true, "not-german-japanese-or-english": true, "not-german-japanese-korean-chinese-or-english": true}
	}
	return map[string]bool{}
}

// renderLocalePolicy replaces only score literals and the generated trailing
// language section.  Locale intent and regional native-score gates remain in
// the reviewed policy source.
func renderLocalePolicy(body string, snap snapshot, name string) (string, error) {
	sets := localeScoreSets(name)
	re := regexp.MustCompile(`score_entry\["([^"]+)"\] := (-?[0-9]+) if \{\n([\s\S]*?)has_fact\("([^"]+)"\)\n\}`)
	var replaceErr error
	body = re.ReplaceAllStringFunc(body, func(match string) string {
		if replaceErr != nil {
			return match
		}
		parts := re.FindStringSubmatch(match)
		score, ok, err := normalizedFactScore(snap, parts[4], sets)
		if err != nil {
			replaceErr = err
			return match
		}
		if !ok {
			return ""
		}
		return strings.Replace(match, " := "+parts[2]+" if", fmt.Sprintf(" := %d if", score), 1)
	})
	if replaceErr != nil {
		return "", replaceErr
	}
	marker := "has_audio_language(value) if {"
	if index := strings.Index(body, marker); index >= 0 {
		languages, err := renderLocaleLanguages(snap, name, sets)
		if err != nil {
			return "", err
		}
		body = strings.TrimRight(body[:index], "\n") + "\n\n" + languages + "\n"
	}
	return body, nil
}
func renderLocaleLanguages(snap snapshot, name string, sets []string) (string, error) {
	var rows []languageRow
	if err := json.Unmarshal(snap.LanguageRules, &rows); err != nil {
		return "", fmt.Errorf("decode language rules: %w", err)
	}
	allowed := localeLanguageStems(name)
	var out []string
	helpers := false
	seen := map[string]string{}
	for _, row := range rows {
		if !allowed[row.Stem] || row.App == "guide-only" {
			continue
		}
		identity, err := json.Marshal(row.Conditions)
		if err != nil {
			return "", fmt.Errorf("encode language conditions for %s: %w", row.Code, err)
		}
		if prior, ok := seen[row.Code]; ok {
			if prior != string(identity) {
				return "", fmt.Errorf("conflicting language conditions for %s", row.Code)
			}
			continue
		}
		seen[row.Code] = string(identity)
		score, ok, err := normalizedFactScore(snap, row.Code, sets)
		if err != nil {
			return "", err
		}
		if !ok {
			continue
		}
		if score == 0 {
			continue
		}
		if !helpers {
			out = append(out, "has_audio_language(value) if {\n    some language in input.release.languages_audio\n    lower(language) == value\n}\n\nhas_original_audio_language if {\n    some language in input.release.languages_audio\n    lower(language) == lower(input.context.inferred_original_audio_language)\n}")
			helpers = true
		}
		ident := strings.ReplaceAll(row.Code, ".", "_")
		var required []string
		for _, c := range row.Conditions {
			if c.Required {
				expr := languageExpr(c)
				if expr == "" {
					return "", fmt.Errorf("unsupported required language condition for %s", row.Code)
				}
				required = append(required, expr)
			}
		}
		var predicates string
		if len(required) == 0 {
			var optional []string
			for _, c := range row.Conditions {
				expr := languageExpr(c)
				if expr == "" {
					return "", fmt.Errorf("unsupported language condition for %s", row.Code)
				}
				optional = append(optional, ident+" if {\n    "+expr+"\n}")
			}
			if len(optional) == 0 {
				return "", fmt.Errorf("language rule %s has no conditions", row.Code)
			}
			predicates = strings.Join(optional, "\n\n")
		} else {
			predicates = ident + " if {\n    " + strings.Join(required, "\n    ") + "\n}"
		}
		out = append(out, "# "+row.Stem+" ("+row.App+")\n"+predicates+"\n\nscore_entry[\""+ident+"\"] := "+fmt.Sprint(score)+" if {\n    locale_intent\n    "+ident+"\n}")
	}
	return strings.Join(out, "\n\n"), nil
}
func languageExpr(c languageCondition) string {
	var value string
	switch v := c.Language.(type) {
	case string:
		if v == "original" {
			value = "has_original_audio_language"
		}
	case map[string]interface{}:
		if n, ok := v["named"].(string); ok {
			value = "has_audio_language(\"" + n + "\")"
		}
	}
	if value == "" {
		return ""
	}
	if c.Negate {
		return "not " + value
	}
	return value
}
