package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestFactScoreCoverageJSON(t *testing.T) {
	data, err := os.ReadFile("snapshot/core-snapshot.json")
	if err != nil {
		t.Fatal(err)
	}
	before, err := validateSnapshot(data)
	if err != nil {
		t.Fatal(err)
	}
	before.FactScores = mustMarshalJSON(t, []factScore{{"removed", "sonarr", "default", 1}, {"changed", "sonarr", "default", 2}})
	previous := filepath.Join(t.TempDir(), "previous.json")
	if err := os.WriteFile(previous, mustMarshalJSON(t, before), 0600); err != nil {
		t.Fatal(err)
	}
	after := before
	after.FactScores = mustMarshalJSON(t, []factScore{{"added", "sonarr", "default", 3}, {"changed", "sonarr", "default", 4}})
	changes, err := factScoreChanges(previous, after)
	if err != nil {
		t.Fatal(err)
	}
	var got map[string][]string
	if err := json.Unmarshal(mustMarshalJSON(t, changes), &got); err != nil {
		t.Fatal(err)
	}
	want := map[string][]string{"added": {"added|sonarr|default=3"}, "removed": {"removed|sonarr|default=1"}, "changed": {"changed|sonarr|default=2->4"}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("coverage lost changes: got %v want %v", got, want)
	}
	if got := string(mustMarshalJSON(t, scoreCoverage{})); got != "{}" {
		t.Fatalf("empty coverage: %s", got)
	}
}

func TestPinnedAnimeSourceTiers(t *testing.T) {
	raw, _ := pinnedRawAndDetection(t)
	core, _, err := distillGroupSnapshot(raw)
	if err != nil {
		t.Fatal(err)
	}
	var rows []groupRule
	if err := json.Unmarshal(core.GroupRules, &rows); err != nil {
		t.Fatal(err)
	}
	arid := map[string]string{}
	for _, row := range rows {
		if row.Matcher == "Arid" && row.Facet == "anime" {
			arid[row.SourceContext] = row.Tier
		}
	}
	if want := (map[string]string{"anime_bd": "silver", "anime_web": "gold"}); !reflect.DeepEqual(arid, want) {
		t.Fatalf("Arid tiers: %v", arid)
	}
	locale, _ := distillLocaleGroupRows(raw)
	ao := map[string]string{}
	for _, row := range locale {
		if row.Matcher == "AO" && row.Facet == "anime" {
			ao[row.SourceContext] = row.Code
		}
	}
	if want := (map[string]string{"anime_bd": "trash.locale.german.group.tier1", "anime_web": "trash.locale.german.group.tier2"}); !reflect.DeepEqual(ao, want) {
		t.Fatalf("AO tiers: %v", ao)
	}
}
