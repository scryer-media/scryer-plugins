package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestFetchRejectsInvalidInputWithoutReplacingSnapshot(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "snapshot.json")
	if err := os.WriteFile(target, []byte("preserve"), 0644); err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(dir, "bad.json")
	if err := os.WriteFile(source, []byte("{}"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := fetch([]string{"--source", source, "--output", target}); err == nil {
		t.Fatal("fetch accepted invalid input")
	}
	got, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "preserve" {
		t.Fatalf("target changed: %q", got)
	}
}
func TestFactScoreChangesAreSortedAndExact(t *testing.T) {
	old := snapshot{SchemaVersion: 1, SourceRevision: "0123456789012345678901234567890123456789", GroupRules: json.RawMessage(`[{"index":0,"matcher":"x","match_kind":"exact","tier":"gold","facet":"movie","source_context":"any"}]`), FactScores: json.RawMessage(`[{"code":"b","app":"sonarr","score_set":"default","score":2},{"code":"c","app":"radarr","score_set":"default","score":3}]`)}
	path := filepath.Join(t.TempDir(), "old.json")
	body, _ := json.Marshal(old)
	if err := os.WriteFile(path, body, 0644); err != nil {
		t.Fatal(err)
	}
	current := old
	current.FactScores = json.RawMessage(`[{"code":"a","app":"radarr","score_set":"default","score":1},{"code":"b","app":"sonarr","score_set":"default","score":4}]`)
	changes, err := factScoreChanges(path, current)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(changes.Added, ",") != "a|radarr|default=1" || strings.Join(changes.Removed, ",") != "c|radarr|default=3" || strings.Join(changes.Changed, ",") != "b|sonarr|default=2->4" {
		t.Fatalf("unexpected changes %#v", changes)
	}
}

func TestGenerateIsDeterministic(t *testing.T) {
	data := []byte(`{"schema_version":1,"source_revision":"0123456789012345678901234567890123456789","group_rules":[{"index":0,"matcher":"Example","match_kind":"exact","tier":"gold","facet":"movie","source_context":"any"}]}`)
	snap, err := validateSnapshot(data)
	if err != nil {
		t.Fatal(err)
	}
	localeData, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	localeSnap, err := validateSnapshot(localeData)
	if err != nil {
		t.Fatal(err)
	}
	snap.FactScores, snap.LanguageRules, snap.ScoreEnvelopes = localeSnap.FactScores, localeSnap.LanguageRules, localeSnap.ScoreEnvelopes
	detectionData, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	detection, err := parseDetectionSnapshot(detectionData)
	if err != nil {
		t.Fatal(err)
	}
	first, err := generate(snap, detection, data, "", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	second, err := generate(snap, detection, data, "", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	for name, want := range first {
		if !bytes.Equal(want, second[name]) {
			t.Fatalf("%s changed across identical generations", name)
		}
	}
	var result pack
	if err := json.Unmarshal(first["trash-scoring.json"], &result); err != nil {
		t.Fatal(err)
	}
	if result.ID != packID || result.MinScryerVersion != minHostVersion {
		t.Fatalf("unexpected generated pack identity: %#v", result)
	}
	for _, item := range result.Rules {
		name := filepath.Join("trash", "generated", strings.TrimPrefix(item.ID, "trash-guides-")+".rego")
		if got := string(first[name]); got != item.RegoSource+"\n" {
			t.Fatalf("readable source %s differs from pack", name)
		}
	}
}

func TestGeneratedRegoHonorsSourceColumnLimit(t *testing.T) {
	core, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	snap, err := validateSnapshot(core)
	if err != nil {
		t.Fatal(err)
	}
	detectionData, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	detection, err := parseDetectionSnapshot(detectionData)
	if err != nil {
		t.Fatal(err)
	}
	rules, err := generatedRules(snap, detection)
	if err != nil {
		t.Fatal(err)
	}
	for _, rule := range rules {
		for line, text := range bytes.Split([]byte(rule.RegoSource), []byte("\n")) {
			if len(text) > 1024 {
				t.Fatalf("%s line %d is %d bytes", rule.ID, line+1, len(text))
			}
		}
	}
}

func TestGroupSnapshotMutationAndDeletionChangeGeneratedPolicy(t *testing.T) {
	detectionData, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	detection, err := parseDetectionSnapshot(detectionData)
	if err != nil {
		t.Fatal(err)
	}
	coreData, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	reference, err := validateSnapshot(coreData)
	if err != nil {
		t.Fatal(err)
	}
	makeSnapshot := func(rows string) snapshot {
		data := []byte(`{"schema_version":1,"source_revision":"0123456789012345678901234567890123456789","group_rules":` + rows + `}`)
		snap, err := validateSnapshot(data)
		if err != nil {
			t.Fatal(err)
		}
		snap.FactScores, snap.LanguageRules, snap.ScoreEnvelopes = reference.FactScores, reference.LanguageRules, reference.ScoreEnvelopes
		return snap
	}
	base := makeSnapshot(`[{"index":0,"matcher":"Alpha","match_kind":"exact","tier":"gold","facet":"movie","source_context":"any"}]`)
	mutated := makeSnapshot(`[{"index":0,"matcher":"Bravo","match_kind":"exact","tier":"gold","facet":"movie","source_context":"any"}]`)
	deleted := makeSnapshot(`[{"index":0,"matcher":"Alpha","match_kind":"exact","tier":"gold","facet":"movie","source_context":"any"},{"index":1,"matcher":"Bravo","match_kind":"exact","tier":"silver","facet":"movie","source_context":"any"}]`)
	baseSource, err := renderGroups(base)
	if err != nil {
		t.Fatal(err)
	}
	changedSource, err := renderGroups(mutated)
	if err != nil {
		t.Fatal(err)
	}
	if baseSource == changedSource || !strings.Contains(changedSource, "BRAVO") {
		t.Fatal("group mutation did not change generated Rego")
	}
	_, err = generatedRules(deleted, detection)
	if err != nil {
		t.Fatal(err)
	}
	if added, removed, err := groupChangesFromSnapshots(deleted, base); err != nil || added != 1 || removed != 0 {
		t.Fatalf("unexpected addition coverage %d/%d: %v", added, removed, err)
	}
	if added, removed, err := groupChangesFromSnapshots(base, deleted); err != nil || added != 0 || removed != 1 {
		t.Fatalf("unexpected deletion coverage %d/%d: %v", added, removed, err)
	}
}

func TestOfflineCheckComparesEveryArtifactWithoutWriting(t *testing.T) {
	core, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	snap, err := validateSnapshot(core)
	if err != nil {
		t.Fatal(err)
	}
	detectionData, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	detection, err := parseDetectionSnapshot(detectionData)
	if err != nil {
		t.Fatal(err)
	}
	artifacts, err := generate(snap, detection, core, "", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	dir := t.TempDir()
	for name, body := range artifacts {
		if err := os.MkdirAll(filepath.Dir(filepath.Join(dir, name)), 0755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, name), body, 0644); err != nil {
			t.Fatal(err)
		}
	}
	if err := check(dir, artifacts); err != nil {
		t.Fatal(err)
	}
	bad := filepath.Join(dir, "trash-scoring.json")
	if err := os.WriteFile(bad, []byte("outdated"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := check(dir, artifacts); !errors.Is(err, errOutdated) {
		t.Fatalf("check error=%v", err)
	}
	got, err := os.ReadFile(bad)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "outdated" {
		t.Fatal("check mutated an outdated artifact")
	}
}

func TestDetectionSnapshotRejectsMissingRequiredTable(t *testing.T) {
	data := []byte(`{"schema_version":1,"source_revision":"0123456789012345678901234567890123456789","counts":{},"tables":{}}`)
	if _, err := parseDetectionSnapshot(data); err == nil {
		t.Fatal("accepted an incomplete detection snapshot")
	}
}

func TestGenerateRejectsMalformedLanguageRulesWithoutReplacingArtifacts(t *testing.T) {
	core, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	snap, err := validateSnapshot(core)
	if err != nil {
		t.Fatal(err)
	}
	snap.LanguageRules, err = json.Marshal([]languageRow{{
		Code: "trash.lang.not_french", App: "radarr", Stem: "language-not-french",
		Conditions: []languageCondition{{Language: map[string]interface{}{"unknown": true}, Negate: true}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	dir := t.TempDir()
	corePath := filepath.Join(dir, "core-snapshot.json")
	coreBytes, err := json.Marshal(snap)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(corePath, coreBytes, 0644); err != nil {
		t.Fatal(err)
	}
	detection, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "detection-snapshot.json"), detection, 0644); err != nil {
		t.Fatal(err)
	}
	target := filepath.Join(dir, "trash-scoring.json")
	if err := os.WriteFile(target, []byte("preserve"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := build("generate", []string{"--snapshot", corePath, "--output-dir", dir}); err == nil {
		t.Fatal("accepted malformed language rule")
	}
	got, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "preserve" {
		t.Fatalf("generation replaced artifact: %q", got)
	}
}

func TestValidateSnapshotRejectsUnsafeLanguageMetadata(t *testing.T) {
	base := snapshot{SchemaVersion: 1, SourceRevision: "0123456789012345678901234567890123456789", GroupRules: json.RawMessage(`[{"index":0}]`)}
	for name, row := range map[string]languageRow{
		"code":  {Code: "trash.lang.bad-code", App: "radarr", Stem: "language-not-french", Conditions: []languageCondition{{Language: "original"}}},
		"app":   {Code: "trash.lang.not_french", App: "other", Stem: "language-not-french", Conditions: []languageCondition{{Language: "original"}}},
		"stem":  {Code: "trash.lang.not_french", App: "radarr", Stem: "language\ncomment", Conditions: []languageCondition{{Language: "original"}}},
		"named": {Code: "trash.lang.not_french", App: "radarr", Stem: "language-not-french", Conditions: []languageCondition{{Language: map[string]interface{}{"named": "French"}}}},
	} {
		t.Run(name, func(t *testing.T) {
			candidate := base
			candidate.LanguageRules = mustMarshalJSON(t, []languageRow{row})
			data := mustMarshalJSON(t, candidate)
			if _, err := validateSnapshot(data); err == nil {
				t.Fatal("accepted malformed language metadata")
			}
		})
	}
}

func TestGroupRendererRetainsOrderedContextSlots(t *testing.T) {
	data, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	snap, err := validateSnapshot(data)
	if err != nil {
		t.Fatal(err)
	}
	source, err := renderGroups(snap)
	if err != nil {
		t.Fatal(err)
	}
	for _, required := range []string{"trash_group_slots", "\"p\":0", "\"p\":3", "lower(object.get(input.context,\"category\",\"\")) != \"anime\"", "rule := by_context[group]"} {
		if !strings.Contains(source, required) {
			t.Fatalf("missing group selector helper %q", required)
		}
	}
}
