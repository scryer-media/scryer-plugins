package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

// Expectations in fixtures/cases.json are independently authored, not computed
// by the matcher. Full-data cases below check compiler/runtime parity at scale.
func TestEngineCorpus(t *testing.T) {
	snapshot, err := loadSnapshot("fixtures/snapshot.json")
	if err != nil {
		t.Fatal(err)
	}
	artifacts, err := buildArtifacts(snapshot, Overrides{SchemaVersion: 1, Releases: map[string]Override{}}, nil, defaultPackVersion)
	if err != nil {
		t.Fatal(err)
	}
	cases, err := os.ReadFile("fixtures/cases.json")
	if err != nil {
		t.Fatal(err)
	}
	var corpus []map[string]any
	if err := json.Unmarshal(cases, &corpus); err != nil {
		t.Fatal(err)
	}
	if len(corpus) < 40 {
		t.Fatal("adversarial coverage unexpectedly shrank")
	}
	output := os.Getenv("SEADEX_ENGINE_CORPUS_DIR")
	if output == "" {
		output = t.TempDir()
	}
	output, err = filepath.Abs(output)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(output, 0755); err != nil {
		t.Fatal(err)
	}
	packPath := filepath.Join(output, "adversarial-pack.json")
	if err := os.WriteFile(packPath, artifacts["seadex-scoring.json"], 0644); err != nil {
		t.Fatal(err)
	}
	writeCorpusJobs(t, filepath.Join(output, "adversarial-jobs.json"), []any{map[string]any{"name": "adversarial", "pack": packPath, "cases": corpus}})
	if path := os.Getenv("SEADEX_FULL_SNAPSHOT"); path != "" {
		full, err := loadSnapshot(path)
		if err != nil {
			t.Fatal(err)
		}
		tables, _, err := compileSnapshot(full, Overrides{SchemaVersion: 1, Releases: map[string]Override{}})
		if err != nil {
			t.Fatal(err)
		}
		fullArtifacts, err := buildArtifacts(full, Overrides{SchemaVersion: 1, Releases: map[string]Override{}}, nil, defaultPackVersion)
		if err != nil {
			t.Fatal(err)
		}
		fullPath := filepath.Join(output, "full-pack.json")
		if err := os.WriteFile(fullPath, fullArtifacts["seadex-scoring.json"], 0644); err != nil {
			t.Fatal(err)
		}
		fullCases := fullEngineCases(tables)
		writeCorpusJobs(t, filepath.Join(output, "full-jobs.json"), []any{map[string]any{"name": "full-snapshot", "pack": fullPath, "cases": fullCases}})
		t.Logf("full snapshot: %d cases, %d strict keys, %d tolerant keys", len(fullCases), len(tables.Strict), len(tables.Tolerant))
	}
}

func writeCorpusJobs(t *testing.T, path string, jobs []any) {
	t.Helper()
	b, err := json.MarshalIndent(jobs, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, append(b, '\n'), 0644); err != nil {
		t.Fatal(err)
	}
}

func fullEngineCases(tables Tables) []map[string]any {
	cases := []map[string]any{}
	step := func(count, samples int) int {
		if os.Getenv("SEADEX_EXHAUSTIVE_CORPUS") == "1" {
			return 1
		}
		return max(1, count/samples)
	}
	keys := func(values map[string]int) []string {
		k := []string{}
		for key := range values {
			k = append(k, key)
		}
		sort.Strings(k)
		return k
	}
	strict := keys(tables.Strict)
	for i := 0; i < len(strict); i += step(len(strict), 500) {
		key := strict[i]
		// A stored stem can itself end in a video extension (e.g. .mkv.mkv
		// upstream). Restore one extension so normalization removes it once.
		cases = append(cases, map[string]any{"title": key + ".mkv", "group": nil, "score": tables.Strict[key]})
	}
	tolerant := keys(tables.Tolerant)
	for i := 0; i < len(tolerant); i += step(len(tolerant), 500) {
		key := tolerant[i]
		parts := strings.SplitN(key, "|", 2)
		raw := parts[1] + "-" + parts[0]
		score := tables.Tolerant[key]
		if tables.StrictCollisions[strictSignature(raw)] {
			score = 0
		} else if strictScore, ok := tables.Strict[strictSignature(raw)]; ok {
			score = strictScore
		}
		cases = append(cases, map[string]any{"title": raw, "group": parts[0], "score": score})
	}
	collisions := []string{}
	for key := range tables.StrictCollisions {
		collisions = append(collisions, key)
	}
	sort.Strings(collisions)
	for i := 0; i < len(collisions); i += step(len(collisions), 100) {
		cases = append(cases, map[string]any{"title": collisions[i] + ".mkv", "group": "G", "score": 0})
	}
	for i := 0; i < 100; i++ {
		cases = append(cases, map[string]any{"title": "Never Listed Fixture Galaxy - 01v2 1080p-G", "group": "G", "score": 0})
	}
	for i := 0; i < min(20, len(strict)); i++ {
		cases = append(cases, map[string]any{"title": strict[i], "group": nil, "score": 0, "facet": "movie"})
	}
	return cases
}
