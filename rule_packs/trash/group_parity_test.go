package main

import (
	"encoding/json"
	"os"
	"sort"
	"testing"
)

// This is intentionally a pinned-source regression gate. It must not be
// removed while the raw distiller is being completed.
func TestPinnedGroupKeyParity(t *testing.T) {
	rawBytes, err := os.ReadFile("snapshot/upstream-raw-31a2716d.json")
	if err != nil {
		t.Fatal(err)
	}
	var raw rawUpstreamSnapshot
	if err := json.Unmarshal(rawBytes, &raw); err != nil {
		t.Fatal(err)
	}
	actual, ignored, err := distillGroupSnapshot(raw)
	if err != nil {
		t.Fatalf("distill pinned source: %v; ignored=%d", err, len(ignored))
	}
	expectedBytes, err := os.ReadFile("snapshot/core-snapshot-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	expected, err := validateSnapshot(expectedBytes)
	if err != nil {
		t.Fatal(err)
	}
	// The native oracle collapsed BD/WEB tiers. Preserve its membership check;
	// source-specific scores are independently checked in review_regression_test.go.
	missing, added := groupKeyDiff(expected, legacyAnimeGroupProjection(t, actual))
	if len(missing) != 0 || len(added) != 0 {
		t.Fatalf("pinned group parity: missing=%d added=%d ignored=%d missing_keys=%v added_keys=%v", len(missing), len(added), len(ignored), firstGroupKeys(missing, 20), firstGroupKeys(added, 20))
	}
}
func groupKeyDiff(expected, actual snapshot) ([]string, []string) {
	var left, right []groupRule
	_ = json.Unmarshal(expected.GroupRules, &left)
	_ = json.Unmarshal(actual.GroupRules, &right)
	a, b := map[string]bool{}, map[string]bool{}
	for _, row := range left {
		a[groupKey(row)] = true
	}
	for _, row := range right {
		b[groupKey(row)] = true
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
func groupKey(row groupRule) string {
	return row.Matcher + "|" + row.MatchKind + "|" + row.Tier + "|" + row.Facet + "|" + row.SourceContext
}
func firstGroupKeys(values []string, limit int) []string {
	if len(values) > limit {
		return values[:limit]
	}
	return values
}

func legacyAnimeGroupProjection(t *testing.T, snap snapshot) snapshot {
	t.Helper()
	var rows []groupRule
	if err := json.Unmarshal(snap.GroupRules, &rows); err != nil {
		t.Fatal(err)
	}
	collapsed := map[string]groupRule{}
	for _, row := range rows {
		if row.SourceContext == "anime_bd" || row.SourceContext == "anime_web" {
			row.SourceContext = "anime"
		}
		key := row.Matcher + "|" + row.MatchKind + "|" + row.Facet + "|" + row.SourceContext
		prior, found := collapsed[key]
		if !found || groupTierRank(row.Tier) < groupTierRank(prior.Tier) {
			collapsed[key] = row
		}
	}
	rows = nil
	for _, row := range collapsed {
		rows = append(rows, row)
	}
	snap.GroupRules = mustMarshalJSON(t, rows)
	return snap
}
