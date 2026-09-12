package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestSemanticFingerprintIgnoresVersionAndRuleOrder(t *testing.T) {
	first := []byte(`{"id":"trash-guides-scoring-pack","version":"1.0.0","rules":[{"id":"b","regoSource":"b","appliedFacets":["series","movie"]},{"id":"a","regoSource":"a"}]}`)
	second := []byte(`{"id":"trash-guides-scoring-pack","version":"1.0.1","rules":[{"id":"a","regoSource":"a"},{"id":"b","regoSource":"b","appliedFacets":["movie","series"]}]}`)
	one, err := semanticFingerprint(profiles["trash"], first)
	if err != nil {
		t.Fatal(err)
	}
	two, err := semanticFingerprint(profiles["trash"], second)
	if err != nil {
		t.Fatal(err)
	}
	if one != two {
		t.Fatalf("metadata changed semantic fingerprint: %s != %s", one, two)
	}
}

func TestSemanticFingerprintIncludesRego(t *testing.T) {
	one, err := semanticFingerprint(profiles["trash"], []byte(`{"id":"trash-guides-scoring-pack","rules":[{"id":"a","regoSource":"score_entry[\"a\"] := 1"}]}`))
	if err != nil {
		t.Fatal(err)
	}
	two, err := semanticFingerprint(profiles["trash"], []byte(`{"id":"trash-guides-scoring-pack","rules":[{"id":"a","regoSource":"score_entry[\"a\"] := 2"}]}`))
	if err != nil {
		t.Fatal(err)
	}
	if one == two {
		t.Fatal("Rego change did not alter semantic fingerprint")
	}
}

func TestSemanticFingerprintRejectsWrongPack(t *testing.T) {
	if _, err := semanticFingerprint(profiles["trash"], []byte(`{"id":"other","rules":[]}`)); err == nil {
		t.Fatal("accepted wrong pack")
	}
	if _, err := semanticFingerprint(profiles["seadex"], []byte(`{"id":"trash-guides-scoring-pack","rules":[]}`)); err == nil {
		t.Fatal("seadex profile accepted the TRaSH pack")
	}
	if _, err := lookupProfile("other"); err == nil {
		t.Fatal("accepted unknown profile")
	}
}

func TestAllowedChangesContainsOnlyGeneratedRefreshOutputs(t *testing.T) {
	for _, path := range []string{
		"rule_packs/trash-scoring.json", "rule_packs/trash-scoring-coverage.json",
		"rule_packs/trash/snapshot/core-snapshot.json",
		"rule_packs/trash/snapshot/detection-snapshot.json",
		"rule_packs/trash/snapshot/upstream-coverage.json",
		"rule_packs/trash/generated/source-video.rego",
		"rule_packs/trash/generated/editions-anime.rego",
	} {
		if _, ok := profiles["trash"].Allowed[path]; !ok {
			t.Fatalf("missing allowlisted output %s", path)
		}
	}
	if _, ok := profiles["trash"].Allowed["rule_packs/trash/render.go"]; ok {
		t.Fatal("converter source is allowlisted")
	}
	for _, path := range []string{
		"rule_packs/seadex-scoring.json", "rule_packs/seadex-scoring.rego",
		"rule_packs/seadex-coverage.json", "rule_packs/seadex/snapshot.json.gz",
	} {
		if _, ok := profiles["seadex"].Allowed[path]; !ok {
			t.Fatalf("missing allowlisted SeaDex output %s", path)
		}
	}
	for _, path := range []string{"rule_packs/seadex/main.go", "rule_packs/seadex/refresh.go", "rule_packs/trash-scoring.json"} {
		if _, ok := profiles["seadex"].Allowed[path]; ok {
			t.Fatalf("SeaDex profile allowlists %s", path)
		}
	}
	for name, profile := range profiles {
		for path := range profile.Allowed {
			if strings.HasSuffix(path, ".go") || strings.HasPrefix(path, ".github/") {
				t.Fatalf("%s profile allowlists source path %s", name, path)
			}
		}
	}
}

func TestNULPathsPreservesWhitespaceInPathNames(t *testing.T) {
	paths := nulPaths([]byte("rule_packs/trash/generated/audio.rego\x00space name.rego\x00"))
	if len(paths) != 2 || paths[1] != "space name.rego" {
		t.Fatalf("unexpected NUL path parsing: %#v", paths)
	}
}

func TestSemanticFingerprintFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "pack.json")
	if err := os.WriteFile(path, []byte(`{"id":"trash-guides-scoring-pack","rules":[]}`), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := semanticFingerprintFile(profiles["trash"], path); err != nil {
		t.Fatal(err)
	}
}

func TestRefreshCommandsPinTheConverterContract(t *testing.T) {
	got := refreshCommands("0123456789012345678901234567890123456789", "1.2.3")
	want := [][]string{
		{"fetch", "--revision", "0123456789012345678901234567890123456789", "--output", "snapshot/upstream-raw.json"},
		{"distill", "--snapshot", "snapshot/upstream-raw.json", "--output-dir", "snapshot"},
		{"generate", "--snapshot", "snapshot/core-snapshot.json", "--output-dir", "..", "--pack-version", "1.2.3"},
		{"check", "--snapshot", "snapshot/core-snapshot.json", "--output-dir", "..", "--pack-version", "1.2.3"},
	}
	if len(got) != len(want) {
		t.Fatalf("command count = %d", len(got))
	}
	for i := range want {
		if len(got[i]) != len(want[i]) {
			t.Fatalf("command %d length = %d", i, len(got[i]))
		}
		for j := range want[i] {
			if got[i][j] != want[i][j] {
				t.Fatalf("command %d arg %d = %q, want %q", i, j, got[i][j], want[i][j])
			}
		}
	}
}
