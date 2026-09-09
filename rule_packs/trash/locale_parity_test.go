package main

import (
	"encoding/json"
	"os"
	"sort"
	"testing"
)

func TestPinnedLocaleGroupParity(t *testing.T) {
	rawBytes, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(rawBytes, &raw); err != nil {
		t.Fatal(err)
	}
	actual, ignored := distillLocaleGroupRows(raw)
	reference, err := os.ReadFile("snapshot/detection-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	expected, err := localeRowsFromReference(reference)
	if err != nil {
		t.Fatal(err)
	}
	// Keep the native oracle's membership gate without reinstating its lost
	// source distinction or app-derived facet for anime tier rows.
	project := func(rows []localeGroupRow) []localeGroupRow {
		for i := range rows {
			if rows[i].SourceContext == "anime_bd" || rows[i].SourceContext == "anime_web" {
				rows[i].SourceContext = "anime"
			}
			if rows[i].SourceContext == "anime" {
				rows[i].Facet = "anime"
			}
		}
		return rows
	}
	missing, added := localeDiff(project(expected), project(actual))
	if len(missing) > 0 || len(added) > 0 {
		t.Fatalf("locale parity missing=%d added=%d ignored=%d missing_keys=%v added_keys=%v", len(missing), len(added), len(ignored), localeFirst(missing), localeFirst(added))
	}
}
func localeDiff(expected, actual []localeGroupRow) ([]string, []string) {
	a, b := map[string]bool{}, map[string]bool{}
	for _, row := range expected {
		a[localeKey(row)] = true
	}
	for _, row := range actual {
		b[localeKey(row)] = true
	}
	var missing, added []string
	for key := range a {
		if !b[key] {
			missing = append(missing, key)
		}
	}
	for key := range b {
		if !a[key] {
			added = append(added, key)
		}
	}
	sort.Strings(missing)
	sort.Strings(added)
	return missing, added
}
func localeFirst(values []string) []string {
	if len(values) > 15 {
		return values[:15]
	}
	return values
}
