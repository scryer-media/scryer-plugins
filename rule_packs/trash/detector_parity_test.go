package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

func TestPinnedTokenAndBlockedParity(t *testing.T) {
	data, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	signals, blocked := distillDetectorRows(raw)
	reference, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	var detection detectionSnapshot
	if err := json.Unmarshal(reference, &detection); err != nil {
		t.Fatal(err)
	}
	assertDetectorTableParity(t, "token", detection.Tables["token_signal_rules"].([]interface{}), signals)
	assertDetectorTableParity(t, "blocked", detection.Tables["blocked_title_rules"].([]interface{}), blocked)
}
func TestPinnedFactAndNoReleaseGroupParity(t *testing.T) {
	raw, detection := pinnedRawAndDetection(t)
	facts, noGroup := distillFactRows(raw)
	assertDetectorTableParity(t, "fact", detection.Tables["fact_rules"].([]interface{}), facts)
	expected := map[string]bool{}
	for _, value := range detection.Tables["no_release_group_fact_facets"].([]interface{}) {
		expected[value.(string)] = true
	}
	got := map[string]bool{}
	for _, value := range noGroup {
		got[value] = true
	}
	assertKeys(t, "no_release_group", expected, got)
}
func TestPinnedRawSnapshotPipelineParity(t *testing.T) {
	raw, reference := pinnedRawAndDetection(t)
	core, actual, err := distillRawSnapshots(raw)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := parseDetectionSnapshot(mustMarshalJSON(t, actual)); err != nil {
		t.Fatalf("raw output is not a valid detection snapshot: %v", err)
	}
	expectedCore, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	parsedCore, err := validateSnapshot(expectedCore)
	if err != nil {
		t.Fatal(err)
	}
	missing, added := groupKeyDiff(parsedCore, core)
	if len(missing) != 0 || len(added) != 0 {
		t.Fatalf("raw core parity missing=%d added=%d", len(missing), len(added))
	}
	assertDetectorTableParity(t, "pipeline fact", reference.Tables["fact_rules"].([]interface{}), snapshotDetectorRows(actual.Tables["fact_rules"].([]interface{})))
	assertDetectorTableParity(t, "pipeline token", reference.Tables["token_signal_rules"].([]interface{}), snapshotDetectorRows(actual.Tables["token_signal_rules"].([]interface{})))
	assertDetectorTableParity(t, "pipeline blocked", reference.Tables["blocked_title_rules"].([]interface{}), snapshotDetectorRows(actual.Tables["blocked_title_rules"].([]interface{})))
}
func TestRawPipelineReflectsOfflineInputChanges(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	_, before, err := distillRawSnapshots(raw)
	if err != nil {
		t.Fatal(err)
	}
	changed := raw
	changed.Files = append([]rawUpstreamFile(nil), raw.Files...)
	for index := range changed.Files {
		if strings.HasSuffix(changed.Files[index].Path, "/retags.json") {
			changed.Files[index].Records = append([]rawCustomFormat(nil), changed.Files[index].Records...)
			changed.Files[index].Records[0].Specifications = append(changed.Files[index].Records[0].Specifications, rawSpecification{Name: "offline mutation", Implementation: "ReleaseTitleSpecification", Fields: json.RawMessage(`{"value":"\\bOFFLINECHANGE\\b"}`), Required: json.RawMessage("false"), Negate: json.RawMessage("false")})
			changed.Files[index].Records[0].Scores["default"] = -12345
			break
		}
	}
	core, after, err := distillRawSnapshots(changed)
	if err != nil {
		t.Fatal(err)
	}
	beforeFacts := len(before.Tables["fact_rules"].([]interface{}))
	afterFacts := len(after.Tables["fact_rules"].([]interface{}))
	if afterFacts <= beforeFacts {
		t.Fatalf("raw fact addition did not reach output: %d -> %d", beforeFacts, afterFacts)
	}
	if !strings.Contains(string(core.ScoreEnvelopes), "-12345") {
		t.Fatal("raw score change did not reach score envelopes")
	}
}
func TestPinnedLanguageParity(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	actual := distillLanguageRows(raw)
	data, err := os.ReadFile("snapshot/language-rules-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	var expected []languageRow
	if err := json.Unmarshal(data, &expected); err != nil {
		t.Fatal(err)
	}
	want, got := map[string]bool{}, map[string]bool{}
	for _, row := range expected {
		want[languageKey(t, row)] = true
	}
	for _, row := range actual {
		got[languageKey(t, row)] = true
	}
	assertKeys(t, "language", want, got)
}
func TestPinnedFactScoreParity(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	data, err := os.ReadFile("snapshot/fact-scores-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	var expected []factScore
	if err := json.Unmarshal(data, &expected); err != nil {
		t.Fatal(err)
	}
	want, got := map[string]bool{}, map[string]bool{}
	for _, row := range expected {
		want[factScoreKey(row)] = true
	}
	for _, row := range distillFactScores(raw) {
		got[factScoreKey(row)] = true
	}
	assertKeys(t, "fact_scores", want, got)
}
func TestDetectionScopesExcludeUnrelatedRows(t *testing.T) {
	_, detection := pinnedRawAndDetection(t)
	unwanted, err := renderDetectionScope(detection, "")
	if err != nil {
		t.Fatal(err)
	}
	french, err := renderDetectionScope(detection, "french")
	if err != nil {
		t.Fatal(err)
	}
	german, err := renderDetectionScope(detection, "german")
	if err != nil {
		t.Fatal(err)
	}
	_ = unwanted
	for _, row := range scopedDetectionTables(detection.Tables, "")["fact_rules"].([]interface{}) {
		if strings.HasPrefix(stringValue(row.(map[string]interface{}), "code"), "trash.locale.") {
			t.Fatal("unwanted scope retained locale table rows")
		}
	}
	_ = french
	_ = german
	for _, scope := range []struct{ name, prefix string }{{"french", "trash.locale.french."}, {"german", "trash.locale.german."}} {
		for _, table := range []string{"fact_rules", "locale_group_fact_rules"} {
			for _, row := range scopedDetectionTables(detection.Tables, scope.name)[table].([]interface{}) {
				if !strings.HasPrefix(stringValue(row.(map[string]interface{}), "code"), scope.prefix) {
					t.Fatalf("%s scope retained unrelated %s row", scope.name, table)
				}
			}
		}
	}
	fullJSON := mustMarshalJSON(t, indexedTablesMust(t, detection.Tables))
	frenchJSON := mustMarshalJSON(t, indexedTablesMust(t, scopedDetectionTables(detection.Tables, "french")))
	germanJSON := mustMarshalJSON(t, indexedTablesMust(t, scopedDetectionTables(detection.Tables, "german")))
	if len(frenchJSON) >= len(fullJSON) || len(germanJSON) >= len(fullJSON) {
		t.Fatal("locale scope did not reduce rendered table")
	}
	t.Logf("indexed detection bytes full=%d french=%d german=%d", len(fullJSON), len(frenchJSON), len(germanJSON))
}
func TestNormalizedLocaleFactScoresMatchPinnedPolicies(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	snap, _, err := distillRawSnapshots(raw)
	if err != nil {
		t.Fatal(err)
	}
	for _, sample := range []struct {
		code string
		sets []string
		want int64
	}{
		{"trash.locale.french.group.tier1", []string{"french-multi-vf", "french-anime-multi"}, 245},
		{"trash.locale.french.group.tier1", []string{"french-vostfr", "french-anime-vostfr"}, 0},
		{"trash.locale.german.group.tier1", []string{"german", "german-anime"}, 151},
		{"trash.locale.asian.group.tier3", []string{"default", "default"}, 100},
	} {
		got, ok, err := normalizedFactScore(snap, sample.code, sample.sets)
		if err != nil || !ok || got != sample.want {
			t.Fatalf("%s %#v: got=%d ok=%v err=%v want=%d", sample.code, sample.sets, got, ok, err, sample.want)
		}
	}
}
func TestLocalePolicyUsesDistilledScoresAndLanguages(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	snap, _, err := distillRawSnapshots(raw)
	if err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile("sources/german-policy.rego")
	if err != nil {
		t.Fatal(err)
	}
	base, err := renderLocalePolicy(string(body), snap, "german")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(base, `score_entry["trash_tier_1"] := 151`) || !strings.Contains(base, "trash_lang_not_german_or_english") {
		t.Fatal("distilled German score/language clauses missing")
	}
	frenchBody, err := os.ReadFile("sources/french-vf-policy.rego")
	if err != nil {
		t.Fatal(err)
	}
	french, err := renderLocalePolicy(string(frenchBody), snap, "french-vf")
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(french, `score_entry["trash_lang_not_french"]`) != 1 || !strings.Contains(french, `score_entry["trash_lang_not_french"] := -10000`) {
		t.Fatal("distilled French language veto missing or duplicated")
	}
	var scores []factScore
	if err := json.Unmarshal(snap.FactScores, &scores); err != nil {
		t.Fatal(err)
	}
	for i := range scores {
		if scores[i].Code == "trash.locale.german.group.tier1" && scores[i].ScoreSet == "default" {
			scores[i].Score = 11000
		}
	}
	snap.FactScores = mustMarshalJSON(t, scores)
	changed, err := renderLocalePolicy(string(body), snap, "german")
	if err != nil {
		t.Fatal(err)
	}
	if changed == base || !strings.Contains(changed, `score_entry["trash_tier_1"] := 381`) {
		t.Fatal("raw score mutation did not alter rendered policy")
	}
}
func TestLocalePolicyDropsDeletedUpstreamScore(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	snap, _, err := distillRawSnapshots(raw)
	if err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile("sources/german-policy.rego")
	if err != nil {
		t.Fatal(err)
	}
	var scores []factScore
	if err := json.Unmarshal(snap.FactScores, &scores); err != nil {
		t.Fatal(err)
	}
	kept := scores[:0]
	for _, row := range scores {
		if row.Code != "trash.locale.german.group.tier1" {
			kept = append(kept, row)
		}
	}
	snap.FactScores = mustMarshalJSON(t, kept)
	rendered, err := renderLocalePolicy(string(body), snap, "german")
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(rendered, `score_entry["trash_tier_1"]`) {
		t.Fatal("deleted upstream score retained frozen entry")
	}
}
func TestLocaleLanguageRendererRejectsConflictingDuplicateRows(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	snap, _, err := distillRawSnapshots(raw)
	if err != nil {
		t.Fatal(err)
	}
	var rows []languageRow
	if err := json.Unmarshal(snap.LanguageRules, &rows); err != nil {
		t.Fatal(err)
	}
	for _, row := range rows {
		if row.Code == "trash.lang.not_french" && row.App == "radarr" {
			conflict := row
			conflict.App = "sonarr"
			conflict.Conditions = []languageCondition{{Language: "original", Negate: true}}
			rows = append(rows, conflict)
			break
		}
	}
	snap.LanguageRules = mustMarshalJSON(t, rows)
	if _, err := renderLocaleLanguages(snap, "french-vf", localeScoreSets("french-vf")); err == nil {
		t.Fatal("accepted conflicting duplicate language rows")
	}
}
func TestRawDeletionDistillsWithoutRetainingScoresAndReportsStaleMembership(t *testing.T) {
	data, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	kept := raw.Files[:0]
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(filepath.Base(file.Path), ".json")
		if stem != "german-subbed" && stem != "amzn" {
			kept = append(kept, file)
		}
	}
	raw.Files = kept
	for i := range raw.Files {
		raw.Files[i].CanonicalSHA256 = canonicalRawRecords(raw.Files[i].Records)
	}
	dir := t.TempDir()
	input := filepath.Join(dir, "raw.json")
	if err := os.WriteFile(input, mustMarshalJSON(t, raw), 0644); err != nil {
		t.Fatal(err)
	}
	if err := distill([]string{"--snapshot", input, "--output-dir", dir}); err != nil {
		t.Fatal(err)
	}
	coreData, err := os.ReadFile(filepath.Join(dir, "core-snapshot.json"))
	if err != nil {
		t.Fatal(err)
	}
	core, err := validateSnapshot(coreData)
	if err != nil {
		t.Fatal(err)
	}
	var scores []factScore
	if err := json.Unmarshal(core.FactScores, &scores); err != nil {
		t.Fatal(err)
	}
	for _, row := range scores {
		if row.Code == "trash.locale.german.marker.subbed" {
			t.Fatal("deleted raw record remained scored")
		}
	}
	coverageData, err := os.ReadFile(filepath.Join(dir, "upstream-coverage.json"))
	if err != nil {
		t.Fatal(err)
	}
	var coverage upstreamCoverage
	if err := json.Unmarshal(coverageData, &coverage); err != nil {
		t.Fatal(err)
	}
	found := false
	for _, entry := range coverage.StaleMemberships {
		if entry == "service_stem:amzn" {
			found = true
		}
	}
	if !found {
		t.Fatal("deleted curated service stem not reported stale")
	}
}
func TestStaleServiceTokenOverrideIsReported(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	for fileIndex := range raw.Files {
		if !strings.HasSuffix(raw.Files[fileIndex].Path, "/ip.json") {
			continue
		}
		for recordIndex := range raw.Files[fileIndex].Records {
			for specIndex := range raw.Files[fileIndex].Records[recordIndex].Specifications {
				spec := &raw.Files[fileIndex].Records[recordIndex].Specifications[specIndex]
				if spec.Implementation == "ReleaseTitleSpecification" {
					spec.Fields = json.RawMessage(`{"value":"\\bip\\b"}`)
				}
			}
		}
	}
	stale := staleCuratedMemberships(raw)
	found := false
	for _, entry := range stale {
		if entry == "service_token_override:ip:IPLAYER" {
			found = true
		}
	}
	if !found {
		t.Fatalf("missing stale token override: %v", stale)
	}
}
func TestDistillRejectsMalformedRawWithoutReplacingOutputs(t *testing.T) {
	data, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	for _, mutate := range []func(*rawUpstreamSnapshot){func(r *rawUpstreamSnapshot) { r.SourceRevision = "bad" }, func(r *rawUpstreamSnapshot) { r.Files = append(r.Files, r.Files[0]) }, func(r *rawUpstreamSnapshot) { r.Files[0].SHA256 = "bad" }} {
		changed := raw
		changed.Files = append([]rawUpstreamFile(nil), raw.Files...)
		mutate(&changed)
		input := filepath.Join(t.TempDir(), "raw.json")
		output := t.TempDir()
		if err := os.WriteFile(input, mustMarshalJSON(t, changed), 0644); err != nil {
			t.Fatal(err)
		}
		target := filepath.Join(output, "core-snapshot.json")
		if err := os.WriteFile(target, []byte("preserve"), 0644); err != nil {
			t.Fatal(err)
		}
		if err := distill([]string{"--snapshot", input, "--output-dir", output}); err == nil {
			t.Fatal("accepted malformed raw")
		}
		got, _ := os.ReadFile(target)
		if string(got) != "preserve" {
			t.Fatal("invalid raw replaced output")
		}
	}
}
func TestDistillRejectsCanonicalChecksumTamper(t *testing.T) {
	data, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	raw.Files[0].CanonicalSHA256 = strings.Repeat("0", 64)
	input := filepath.Join(t.TempDir(), "raw.json")
	if err := os.WriteFile(input, mustMarshalJSON(t, raw), 0644); err != nil {
		t.Fatal(err)
	}
	if err := distill([]string{"--snapshot", input, "--output-dir", t.TempDir()}); err == nil {
		t.Fatal("accepted canonical checksum tamper")
	}
}
func renderDetectionMust(t *testing.T, snapshot detectionSnapshot) string {
	t.Helper()
	value, err := renderDetection(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	return value
}
func indexedTablesMust(t *testing.T, tables map[string]interface{}) map[string]interface{} {
	t.Helper()
	result, err := indexedDetectionTables(tables)
	if err != nil {
		t.Fatal(err)
	}
	return result
}
func factScoreKey(row factScore) string {
	return row.Code + "|" + row.App + "|" + row.ScoreSet + "|" + fmt.Sprint(row.Score)
}
func languageKey(t *testing.T, row languageRow) string {
	t.Helper()
	conditions := append([]languageCondition(nil), row.Conditions...)
	sort.Slice(conditions, func(i, j int) bool {
		return string(mustMarshalJSON(t, conditions[i])) < string(mustMarshalJSON(t, conditions[j]))
	})
	return row.Code + "|" + row.App + "|" + row.Stem + "|" + string(mustMarshalJSON(t, conditions))
}

func mustMarshalJSON(t *testing.T, value interface{}) []byte {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func snapshotDetectorRows(rows []interface{}) []detectorRow {
	out := make([]detectorRow, 0, len(rows))
	for _, value := range rows {
		row := value.(map[string]interface{})
		pattern := row["pattern"].(map[string]interface{})
		tokens := pattern["tokens"].([]string)
		out = append(out, detectorRow{Kind: stringValue(row, "kind"), Code: stringValue(row, "code"), Facet: stringValue(row, "facet"), Category: stringValue(row, "category"), Pattern: detectorPattern{Kind: pattern["kind"].(string), Tokens: tokens}})
	}
	return out
}

func pinnedRawAndDetection(t *testing.T) (rawUpstreamSnapshot, detectionSnapshot) {
	t.Helper()
	data, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	reference, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	var detection detectionSnapshot
	if err := json.Unmarshal(reference, &detection); err != nil {
		t.Fatal(err)
	}
	return raw, detection
}
func TestPinnedServiceAliasParity(t *testing.T) {
	data, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	actual, _ := distillServiceAliases(raw)
	reference, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	var detection detectionSnapshot
	if err := json.Unmarshal(reference, &detection); err != nil {
		t.Fatal(err)
	}
	expected := map[string]bool{}
	for _, value := range detection.Tables["service_alias_rules"].([]interface{}) {
		row := value.(map[string]interface{})
		expected[serviceKey(row["token"].(string), row["service"].(string), row["requires_web_adjacency"].(bool))] = true
	}
	got := map[string]bool{}
	for _, row := range actual {
		got[serviceKey(row.Token, row.Service, row.RequiresWebAdjacency)] = true
	}
	assertKeys(t, "service", expected, got)
}
func serviceKey(token, service string, web bool) string {
	if web {
		return token + "|" + service + "|web"
	}
	return token + "|" + service + "|standalone"
}
func assertDetectorTableParity(t *testing.T, name string, expectedRaw []interface{}, actual []detectorRow) {
	t.Helper()
	expected := map[string]bool{}
	for _, value := range expectedRaw {
		row := value.(map[string]interface{})
		pattern := row["pattern"].(map[string]interface{})
		tokens := pattern["tokens"].([]interface{})
		values := make([]string, len(tokens))
		for i, v := range tokens {
			values[i] = v.(string)
		}
		expected[detectorKey(stringValue(row, "kind"), stringValue(row, "code"), stringValue(row, "facet"), stringValue(row, "category"), pattern["kind"].(string), values)] = true
	}
	got := map[string]bool{}
	for _, row := range actual {
		got[detectorKey(row.Kind, row.Code, row.Facet, row.Category, row.Pattern.Kind, row.Pattern.Tokens)] = true
	}
	assertKeys(t, name, expected, got)
}
func stringValue(row map[string]interface{}, key string) string {
	if value, ok := row[key].(string); ok {
		return value
	}
	return ""
}
func detectorKey(kind, code, facet, category, pattern string, tokens []string) string {
	return kind + "|" + code + "|" + facet + "|" + category + "|" + pattern + "|" + joinTokens(tokens)
}
func joinTokens(tokens []string) string {
	out := ""
	for _, token := range tokens {
		out += "/" + token
	}
	return out
}
func assertKeys(t *testing.T, name string, expected, got map[string]bool) {
	t.Helper()
	var missing, added []string
	for key := range expected {
		if !got[key] {
			missing = append(missing, key)
		}
	}
	for key := range got {
		if !expected[key] {
			added = append(added, key)
		}
	}
	sort.Strings(missing)
	sort.Strings(added)
	if len(missing) > 0 || len(added) > 0 {
		missingCount, addedCount := len(missing), len(added)
		if len(missing) > 12 {
			missing = missing[:12]
		}
		if len(added) > 12 {
			added = added[:12]
		}
		t.Fatalf("%s parity missing=%d added=%d missing_keys=%v added_keys=%v", name, missingCount, addedCount, missing, added)
	}
}
