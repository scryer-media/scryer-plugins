package main

import (
	"encoding/json"
	"path"
	"sort"
	"strings"
)

type languageCondition struct {
	Language interface{} `json:"language"`
	Negate   bool        `json:"negate"`
	Required bool        `json:"required"`
}
type languageRow struct {
	Code       string              `json:"code"`
	App        string              `json:"app"`
	Stem       string              `json:"stem"`
	Conditions []languageCondition `json:"conditions"`
}

var languageCodes = map[int]string{1: "eng", 2: "fra", 3: "spa", 4: "deu", 5: "ita", 6: "dan", 7: "nld", 8: "jpn", 9: "isl", 10: "zho", 11: "rus", 12: "pol", 13: "vie", 14: "swe", 15: "nor", 16: "fin", 17: "tur", 18: "por", 19: "nld", 20: "ell", 21: "kor", 22: "hun", 23: "heb", 24: "lit"}

func distillLanguageRows(raw rawUpstreamSnapshot) []languageRow {
	var rows []languageRow
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		app := "guide-only"
		if strings.Contains(file.Path, "/sonarr/") {
			app = "sonarr"
		}
		if strings.Contains(file.Path, "/radarr/") {
			app = "radarr"
		}
		if !(app == "guide-only" || strings.HasPrefix(stem, "language-") || strings.HasPrefix(stem, "not-") && strings.HasSuffix(stem, "-or-english")) || stem == "language-original-plus-french" {
			continue
		}
		for _, record := range file.Records {
			var conditions []languageCondition
			valid := true
			for _, spec := range record.Specifications {
				if spec.Implementation != "LanguageSpecification" {
					continue
				}
				var fields struct {
					Value          int  `json:"value"`
					ExceptLanguage bool `json:"exceptLanguage"`
				}
				if json.Unmarshal(spec.Fields, &fields) != nil || fields.ExceptLanguage {
					valid = false
					break
				}
				language := interface{}(map[string]string{"named": languageCodes[fields.Value]})
				if fields.Value == -2 {
					language = "original"
				}
				if fields.Value != -2 && languageCodes[fields.Value] == "" {
					valid = false
					break
				}
				conditions = append(conditions, languageCondition{Language: language, Negate: isTrue(spec.Negate), Required: isTrue(spec.Required)})
			}
			if !valid || len(conditions) == 0 {
				continue
			}
			unique := map[string]languageCondition{}
			for _, condition := range conditions {
				key, _ := json.Marshal(condition)
				unique[string(key)] = condition
			}
			conditions = conditions[:0]
			for _, condition := range unique {
				conditions = append(conditions, condition)
			}
			sort.Slice(conditions, func(i, j int) bool {
				a, _ := json.Marshal(conditions[i])
				b, _ := json.Marshal(conditions[j])
				return string(a) < string(b)
			})
			code := "trash.lang." + strings.ReplaceAll(strings.TrimPrefix(stem, "language-"), "-", "_")
			rows = append(rows, languageRow{Code: code, App: app, Stem: stem, Conditions: conditions})
		}
	}
	sort.Slice(rows, func(i, j int) bool { return rows[i].Code+rows[i].App < rows[j].Code+rows[j].App })
	return rows
}
