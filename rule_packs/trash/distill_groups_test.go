package main

import (
	"encoding/json"
	"testing"
)

func TestDistillGroupSnapshotRepresentativeFacetsAndChanges(t *testing.T) {
	makeFile := func(path, stem, value string) rawUpstreamFile {
		return rawUpstreamFile{Path: path + stem + ".json", Records: []rawCustomFormat{{Specifications: []rawSpecification{{Name: "group", Implementation: "ReleaseGroupSpecification", Fields: json.RawMessage(`{"value":"` + value + `"}`), Negate: json.RawMessage("false")}}}}}
	}
	raw := rawUpstreamSnapshot{SchemaVersion: 1, SourceRevision: "0123456789012345678901234567890123456789", Files: []rawUpstreamFile{makeFile("docs/json/radarr/cf/", "web-tier-01", "MovieGroup"), makeFile("docs/json/sonarr/cf/", "web-tier-02", "SeriesGroup"), makeFile("docs/json/sonarr/cf/", "anime-web-tier-03", "AnimeGroup")}}
	snap, ignored, err := distillGroupSnapshot(raw)
	if err != nil {
		t.Fatal(err)
	}
	if len(ignored) != 0 {
		t.Fatal(ignored)
	}
	var rows []groupRule
	if err := json.Unmarshal(snap.GroupRules, &rows); err != nil {
		t.Fatal(err)
	}
	if len(rows) < 3 {
		t.Fatal(rows)
	}
	source, err := renderGroups(snap)
	if err != nil {
		t.Fatal(err)
	}
	if source == "" {
		t.Fatal("empty Rego")
	}
	raw.Files = raw.Files[:2]
	deleted, _, err := distillGroupSnapshot(raw)
	if err != nil {
		t.Fatal(err)
	}
	if added, removed, err := groupChangesFromSnapshots(deleted, snap); err != nil || added != 0 || removed != 1 {
		t.Fatalf("coverage %d/%d %v", added, removed, err)
	}
}
func TestDistillGroupSnapshotRecordsUnsupportedRegex(t *testing.T) {
	raw := rawUpstreamSnapshot{SourceRevision: "0123456789012345678901234567890123456789", Files: []rawUpstreamFile{{Path: "docs/json/radarr/cf/web-tier-01.json", Records: []rawCustomFormat{{Specifications: []rawSpecification{{Name: "group", Implementation: "ReleaseGroupSpecification", Fields: json.RawMessage(`{"value":"(?=bad)"}`)}}}}}}}
	if _, ignored, err := distillGroupSnapshot(raw); err != nil || len(ignored) != 1 {
		t.Fatalf("ignored=%v err=%v", ignored, err)
	}
}
