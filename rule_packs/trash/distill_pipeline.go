package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strings"
)

type upstreamCoverage struct {
	SchemaVersion    int               `json:"schema_version"`
	SourceRevision   string            `json:"source_revision"`
	InputFiles       []upstreamFileRef `json:"input_files"`
	TableCounts      map[string]int    `json:"table_counts"`
	CoreSHA256       string            `json:"core_sha256"`
	DetectionSHA256  string            `json:"detection_sha256"`
	IgnoredRecords   []string          `json:"ignored_records"`
	StaleMemberships []string          `json:"stale_memberships"`
}
type upstreamFileRef struct {
	Path   string `json:"path"`
	SHA256 string `json:"sha256"`
}

// distill writes compact generated snapshots from the immutable raw snapshot.
// It never consumes the pinned compiled references used by parity tests.
func distill(args []string) error {
	fs := flag.NewFlagSet("distill", flag.ContinueOnError)
	input := fs.String("snapshot", "snapshot/upstream-raw-31a2716d.json", "normalized raw upstream snapshot")
	inputAlias := fs.String("input", "", "alias for --snapshot")
	output := fs.String("output-dir", "snapshot", "generated snapshot directory")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *inputAlias != "" {
		input = inputAlias
	}
	data, err := os.ReadFile(*input)
	if err != nil {
		return err
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		return fmt.Errorf("decode raw upstream snapshot: %w", err)
	}
	if err := validateRawSnapshot(raw); err != nil {
		return err
	}
	core, detection, err := distillRawSnapshots(raw)
	if err != nil {
		return err
	}
	coreBytes, err := json.MarshalIndent(core, "", "  ")
	if err != nil {
		return err
	}
	detectionBytes, err := json.MarshalIndent(detection, "", "  ")
	if err != nil {
		return err
	}
	coverageBytes, err := json.MarshalIndent(rawCoverage(raw, detection, coreBytes, detectionBytes), "", "  ")
	if err != nil {
		return err
	}
	for _, artifact := range []struct {
		name string
		body []byte
	}{
		{"core-snapshot.json", coreBytes},
		{"detection-snapshot.json", detectionBytes},
		{"upstream-coverage.json", coverageBytes},
	} {
		if err := atomicWrite(filepath.Join(*output, artifact.name), append(artifact.body, '\n')); err != nil {
			return err
		}
	}
	return nil
}

func validateRawSnapshot(raw rawUpstreamSnapshot) error {
	if raw.SchemaVersion != 1 || len(raw.Files) == 0 || !isHex(raw.SourceRevision, 40) {
		return fmt.Errorf("invalid raw upstream snapshot")
	}
	paths := map[string]bool{}
	for _, file := range raw.Files {
		if file.Path == "" || paths[file.Path] || !isHex(file.SHA256, 64) || !isHex(file.CanonicalSHA256, 64) || canonicalRawRecords(file.Records) != file.CanonicalSHA256 || len(file.Records) == 0 {
			return fmt.Errorf("invalid raw upstream file %q", file.Path)
		}
		paths[file.Path] = true
		for _, record := range file.Records {
			if record.Name == "" || record.TrashID == "" || len(record.Specifications) == 0 {
				return fmt.Errorf("invalid raw record in %s", file.Path)
			}
			for _, spec := range record.Specifications {
				if spec.Name == "" || spec.Implementation == "" || !json.Valid(spec.Fields) {
					return fmt.Errorf("invalid raw specification in %s", file.Path)
				}
			}
		}
	}
	return nil
}
func canonicalizeRaw(args []string) error {
	fs := flag.NewFlagSet("canonicalize-raw", flag.ContinueOnError)
	input := fs.String("snapshot", "snapshot/upstream-raw.json", "")
	if err := fs.Parse(args); err != nil {
		return err
	}
	data, err := os.ReadFile(*input)
	if err != nil {
		return err
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	for i := range raw.Files {
		raw.Files[i].CanonicalSHA256 = canonicalRawRecords(raw.Files[i].Records)
	}
	out, err := json.MarshalIndent(raw, "", "  ")
	if err != nil {
		return err
	}
	return atomicWrite(*input, append(out, '\n'))
}
func isHex(value string, length int) bool {
	if len(value) != length {
		return false
	}
	for _, ch := range value {
		if !(ch >= '0' && ch <= '9' || ch >= 'a' && ch <= 'f' || ch >= 'A' && ch <= 'F') {
			return false
		}
	}
	return true
}

func rawCoverage(raw rawUpstreamSnapshot, detection detectionSnapshot, coreBytes, detectionBytes []byte) upstreamCoverage {
	files := make([]upstreamFileRef, 0, len(raw.Files))
	for _, file := range raw.Files {
		checksum := file.SHA256
		if checksum == "" {
			body, _ := json.Marshal(file.Records)
			digest := sha256.Sum256(body)
			checksum = hex.EncodeToString(digest[:])
		}
		files = append(files, upstreamFileRef{Path: file.Path, SHA256: checksum})
	}
	_, groups, _ := distillGroupSnapshot(raw)
	_, locale := distillLocaleGroupRows(raw)
	_, services := distillServiceAliases(raw)
	ignored := append(append(groups, locale...), services...)
	sort.Strings(ignored)
	coreHash := sha256.Sum256(coreBytes)
	detectionHash := sha256.Sum256(detectionBytes)
	return upstreamCoverage{SchemaVersion: 1, SourceRevision: raw.SourceRevision, InputFiles: files, TableCounts: detection.Counts, CoreSHA256: hex.EncodeToString(coreHash[:]), DetectionSHA256: hex.EncodeToString(detectionHash[:]), IgnoredRecords: ignored, StaleMemberships: staleCuratedMemberships(raw)}
}
func staleCuratedMemberships(raw rawUpstreamSnapshot) []string {
	present := map[string]bool{}
	overrideTokens := map[string]map[string]bool{}
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		present[stem] = true
		spec, tracked := serviceSpecs[stem]
		if !tracked || len(spec.overrides) == 0 {
			continue
		}
		for _, record := range file.Records {
			for _, rule := range record.Specifications {
				if rule.Implementation != "ReleaseTitleSpecification" || isTrue(rule.Negate) {
					continue
				}
				pattern, err := groupPattern(rule.Fields)
				if err != nil || strings.Contains(pattern, "(?") {
					continue
				}
				values, err := finiteGroupLiterals(pattern)
				if err != nil {
					continue
				}
				for _, value := range values {
					token := aliasToken(value)
					if token == "" {
						continue
					}
					if overrideTokens[stem] == nil {
						overrideTokens[stem] = map[string]bool{}
					}
					overrideTokens[stem][token] = true
				}
			}
		}
	}
	var stale []string
	for stem, spec := range serviceSpecs {
		if !present[stem] {
			stale = append(stale, "service_stem:"+stem)
			continue
		}
		for token := range spec.overrides {
			if !overrideTokens[stem][token] {
				stale = append(stale, "service_token_override:"+stem+":"+token)
			}
		}
	}
	sort.Strings(stale)
	return stale
}

// distillRawSnapshots is the only path from a normalized upstream tree to the
// two generated inputs.  It intentionally does not read either checked-in
// compiled snapshot; those files are pinned parity references, not sources.
func distillRawSnapshots(raw rawUpstreamSnapshot) (snapshot, detectionSnapshot, error) {
	core, ignoredGroups, err := distillGroupSnapshot(raw)
	if err != nil {
		return snapshot{}, detectionSnapshot{}, err
	}
	locale, ignoredLocale := distillLocaleGroupRows(raw)
	if len(ignoredGroups) > 0 || len(ignoredLocale) > 0 {
		// Ignored rows are retained by the coverage report in the caller.  The
		// distillers already reject active group rows that cannot be translated.
	}
	services, _ := distillServiceAliases(raw)
	signals, blocked := distillDetectorRows(raw)
	facts, noGroup := distillFactRows(raw)
	language := distillLanguageRows(raw)
	core.LanguageRules, err = json.Marshal(language)
	if err != nil {
		return snapshot{}, detectionSnapshot{}, err
	}
	core.FactScores, err = json.Marshal(distillFactScores(raw))
	if err != nil {
		return snapshot{}, detectionSnapshot{}, err
	}
	core.ScoreEnvelopes, err = json.Marshal(scoreEnvelopes(raw))
	if err != nil {
		return snapshot{}, detectionSnapshot{}, err
	}
	tables := map[string]interface{}{
		"service_alias_rules":          serviceRowsForSnapshot(services),
		"fact_rules":                   detectorRowsForSnapshot(facts),
		"locale_group_fact_rules":      localeRowsForSnapshot(locale),
		"token_signal_rules":           detectorRowsForSnapshot(signals),
		"blocked_title_rules":          detectorRowsForSnapshot(blocked),
		"no_release_group_fact_facets": stringRowsForSnapshot(noGroup),
	}
	counts := make(map[string]int, len(tables))
	for name, table := range tables {
		rows, ok := table.([]interface{})
		if !ok {
			return snapshot{}, detectionSnapshot{}, fmt.Errorf("internal non-row table %q", name)
		}
		counts[name] = len(rows)
	}
	return core, detectionSnapshot{SchemaVersion: 1, SourceRevision: raw.SourceRevision, Counts: counts, Tables: tables}, nil
}

type scoreEnvelope struct {
	ScoreSet string  `json:"score_set"`
	Min      int64   `json:"min"`
	Max      int64   `json:"max"`
	Vetoes   []int64 `json:"vetoes"`
}
type factScore struct {
	Code     string `json:"code"`
	App      string `json:"app"`
	ScoreSet string `json:"score_set"`
	Score    int64  `json:"score"`
}

func distillFactScores(raw rawUpstreamSnapshot) []factScore {
	seen := map[string]factScore{}
	facts, _ := distillFactRows(raw)
	localeRows, _ := distillLocaleGroupRows(raw)
	emitted := map[string]bool{}
	for _, row := range facts {
		emitted[row.Facet+"|"+row.Code] = true
	}
	for _, row := range localeRows {
		emitted[row.Facet+"|"+row.Code] = true
	}
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		app := "guide-only"
		if strings.Contains(file.Path, "/sonarr/") {
			app = "sonarr"
		}
		if strings.Contains(file.Path, "/radarr/") {
			app = "radarr"
		}
		codes := scoreCodesForStem(stem)
		if localeStemHasManagedScore(stem) || stem == "french-adn" || stem == "french-anime-fansub" {
			codes = append(codes, localeCode(stem))
		}
		if (app == "guide-only" || strings.HasPrefix(stem, "language-") || strings.HasPrefix(stem, "not-") && strings.HasSuffix(stem, "-or-english")) && stem != "language-original-plus-french" {
			codes = append(codes, "trash.lang."+strings.ReplaceAll(strings.TrimPrefix(stem, "language-"), "-", "_"))
		}
		factFacet := localeFacet(file.Path, stem)
		if strings.HasPrefix(stem, "anime-") || stem == "fansub" || stem == "fastsub" || stem == "dubs-only" {
			factFacet = "anime"
		}
		for _, record := range file.Records {
			for _, code := range codes {
				if !strings.HasPrefix(code, "trash.lang.") && code != "trash.locale.french.marker.adn" && code != "trash.locale.french.marker.anime_fansub" && !emitted[factFacet+"|"+code] {
					continue
				}
				for set, score := range record.Scores {
					row := factScore{code, app, set, score}
					seen[code+"|"+app+"|"+set+"|"+fmt.Sprint(score)] = row
				}
			}
		}
	}
	out := make([]factScore, 0, len(seen))
	for _, row := range seen {
		out = append(out, row)
	}
	sort.Slice(out, func(i, j int) bool {
		return out[i].Code+out[i].App+out[i].ScoreSet+fmt.Sprint(out[i].Score) < out[j].Code+out[j].App+out[j].ScoreSet+fmt.Sprint(out[j].Score)
	})
	return out
}
func localeStemHasManagedScore(stem string) bool {
	return strings.Contains(stem, "tier-01") || strings.Contains(stem, "tier-02") || strings.Contains(stem, "tier-03") || strings.HasSuffix(stem, "-lq") || strings.HasSuffix(stem, "-scene") || map[string]bool{"french-vostfr": true, "french-vff": true, "french-vfi": true, "french-vof": true, "french-vfq": true, "french-vq": true, "french-voq": true, "german-subbed": true}[stem]
}
func localeGroupContextExists(stem string) bool { _, ok := localeGroupContext(stem); return ok }
func scoreCodesForStem(stem string) []string {
	switch stem {
	case "upscaled":
		return []string{"trash.ai_enhanced"}
	case "repack-proper":
		return []string{"trash.proper", "trash.repack"}
	case "repack2":
		return []string{"trash.proper", "trash.repack"}
	case "repack3":
		return []string{"trash.proper"}
	case "dubs-only":
		return []string{"trash.dubs_only"}
	case "fansub":
		return []string{"trash.hardcoded_subs", "trash.fansub"}
	case "fastsub":
		return []string{"trash.hardcoded_subs", "trash.fastsub"}
	case "retags":
		return []string{"trash.retagged"}
	case "scene":
		return []string{"trash.scene"}
	case "obfuscated":
		return []string{"trash.obfuscated"}
	case "no-rlsgroup":
		return []string{"trash.no_release_group"}
	}
	return nil
}

func scoreEnvelopes(raw rawUpstreamSnapshot) []scoreEnvelope {
	bySet := map[string]*scoreEnvelope{}
	for _, file := range raw.Files {
		for _, record := range file.Records {
			for set, score := range record.Scores {
				row := bySet[set]
				if row == nil {
					row = &scoreEnvelope{ScoreSet: set, Min: score, Max: score}
					bySet[set] = row
				}
				if score < row.Min {
					row.Min = score
				}
				if score > row.Max {
					row.Max = score
				}
				if score <= -10000 {
					found := false
					for _, v := range row.Vetoes {
						if v == score {
							found = true
						}
					}
					if !found {
						row.Vetoes = append(row.Vetoes, score)
					}
				}
			}
		}
	}
	out := make([]scoreEnvelope, 0, len(bySet))
	for _, row := range bySet {
		sort.Slice(row.Vetoes, func(i, j int) bool { return row.Vetoes[i] < row.Vetoes[j] })
		out = append(out, *row)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ScoreSet < out[j].ScoreSet })
	return out
}

func stringRowsForSnapshot(rows []string) []interface{} {
	out := make([]interface{}, len(rows))
	for index, row := range rows {
		out[index] = row
	}
	return out
}

func detectorRowsForSnapshot(rows []detectorRow) []interface{} {
	out := make([]interface{}, 0, len(rows))
	for index, row := range rows {
		entry := map[string]interface{}{"order": index, "pattern": map[string]interface{}{"kind": row.Pattern.Kind, "tokens": row.Pattern.Tokens}}
		if row.Kind != "" {
			entry["kind"] = row.Kind
		}
		if row.Code != "" {
			entry["code"] = row.Code
		}
		if row.Facet != "" {
			entry["facet"] = row.Facet
		}
		if row.Category != "" {
			entry["category"] = row.Category
		}
		out = append(out, entry)
	}
	return out
}

func localeRowsForSnapshot(rows []localeGroupRow) []interface{} {
	out := make([]interface{}, 0, len(rows))
	for index, row := range rows {
		out = append(out, map[string]interface{}{"order": index, "code": row.Code, "matcher": row.Matcher, "match_kind": row.MatchKind, "facet": row.Facet, "source_context": row.SourceContext})
	}
	return out
}

func serviceRowsForSnapshot(rows []serviceAliasRow) []interface{} {
	out := make([]interface{}, 0, len(rows))
	for index, row := range rows {
		out = append(out, map[string]interface{}{"order": index, "token": row.Token, "service": row.Service, "requires_web_adjacency": row.RequiresWebAdjacency})
	}
	return out
}

func marshalDistilledSnapshots(raw rawUpstreamSnapshot) ([]byte, []byte, error) {
	core, detection, err := distillRawSnapshots(raw)
	if err != nil {
		return nil, nil, err
	}
	coreBytes, err := json.MarshalIndent(core, "", "  ")
	if err != nil {
		return nil, nil, err
	}
	detectionBytes, err := json.MarshalIndent(detection, "", "  ")
	if err != nil {
		return nil, nil, err
	}
	return append(coreBytes, '\n'), append(detectionBytes, '\n'), nil
}
