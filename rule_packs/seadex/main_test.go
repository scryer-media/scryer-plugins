package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func rawFixture(id, releaseID string) map[string]any {
	return map[string]any{
		"id":              id,
		"alID":            123,
		"updated":         "2024-11-25 18:43:31.608Z",
		"notes":           "reference notes",
		"theoreticalBest": "",
		"trs":             []string{releaseID},
		"expand": map[string]any{"trs": []any{map[string]any{
			"id": releaseID, "releaseGroup": "Group", "isBest": true,
			"updated": "2024-11-25 18:43:31.608Z",
			"files":   []any{map[string]any{"name": "[Group] Show - 01.mkv", "length": 100}},
		}}},
	}
}

func normalizedFixture(t *testing.T) Snapshot {
	t.Helper()
	data, err := json.Marshal(map[string]any{"items": []any{rawFixture("entry-a", "release-a")}})
	if err != nil {
		t.Fatal(err)
	}
	snapshot, err := normalizeSnapshot(data)
	if err != nil {
		t.Fatal(err)
	}
	return snapshot
}

func TestNormalizeRawSnapshotSortsAndValidatesRelations(t *testing.T) {
	data, err := json.Marshal(map[string]any{"items": []any{rawFixture("entry-z", "release-z"), rawFixture("entry-a", "release-a")}})
	if err != nil {
		t.Fatal(err)
	}
	snapshot, err := normalizeSnapshot(data)
	if err != nil {
		t.Fatal(err)
	}
	if got := snapshot.Entries[0].ID; got != "entry-a" {
		t.Fatalf("first sorted entry = %q", got)
	}
	if snapshot.Entries[0].TheoreticalBest != nil {
		t.Fatal("empty theoretical best should normalize to null")
	}
	if got := snapshot.Entries[0].Releases[0].Files[0]; got.Name != "[Group] Show - 01.mkv" || got.Size != 100 {
		t.Fatalf("unexpected file %#v", got)
	}

	broken := rawFixture("entry-b", "release-b")
	broken["trs"] = []string{"different"}
	data, _ = json.Marshal(map[string]any{"items": []any{broken}})
	if _, err := normalizeSnapshot(data); err == nil || !strings.Contains(err.Error(), "expanded") {
		t.Fatalf("expected relation validation error, got %v", err)
	}
}

func TestRejectsMalformedAndPartialInputs(t *testing.T) {
	broken := rawFixture("entry-a", "release-a")
	broken["expand"].(map[string]any)["trs"].([]any)[0].(map[string]any)["files"].([]any)[0].(map[string]any)["length"] = "100"
	data, _ := json.Marshal(map[string]any{"items": []any{broken}})
	if _, err := normalizeSnapshot(data); err == nil {
		t.Fatal("string file length was accepted")
	}

	data, _ = json.Marshal(map[string]any{"page": 1, "perPage": 500, "totalPages": 2, "totalItems": 2, "items": []any{rawFixture("entry-a", "release-a")}})
	if _, err := normalizeSnapshot(data); err == nil || !strings.Contains(err.Error(), "complete") {
		t.Fatalf("expected partial raw response rejection, got %v", err)
	}

	directory := t.TempDir()
	path := filepath.Join(directory, "overrides.json")
	if err := os.WriteFile(path, []byte(`{"schema_version":1,"releases":{"release-a":{"exlcude":true}}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := loadOverrides(path); err == nil {
		t.Fatal("misspelled override field was accepted")
	}
}

func TestRejectsMissingAndNullRequiredWireFields(t *testing.T) {
	invalidRaw := []struct {
		name   string
		mutate func(map[string]any)
	}{
		{"missing trs", func(entry map[string]any) { delete(entry, "trs") }},
		{"null trs", func(entry map[string]any) { entry["trs"] = nil }},
		{"missing isBest", func(entry map[string]any) {
			delete(entry["expand"].(map[string]any)["trs"].([]any)[0].(map[string]any), "isBest")
		}},
		{"null file length", func(entry map[string]any) {
			entry["expand"].(map[string]any)["trs"].([]any)[0].(map[string]any)["files"].([]any)[0].(map[string]any)["length"] = nil
		}},
		{"missing files", func(entry map[string]any) {
			delete(entry["expand"].(map[string]any)["trs"].([]any)[0].(map[string]any), "files")
		}},
		{"null files", func(entry map[string]any) {
			entry["expand"].(map[string]any)["trs"].([]any)[0].(map[string]any)["files"] = nil
		}},
		{"null expansion", func(entry map[string]any) { entry["expand"] = nil }},
	}
	for _, test := range invalidRaw {
		t.Run(test.name, func(t *testing.T) {
			entry := rawFixture("entry-a", "release-a")
			test.mutate(entry)
			data, _ := json.Marshal(map[string]any{"items": []any{entry}})
			if _, err := normalizeSnapshot(data); err == nil {
				t.Fatal("malformed raw input was accepted")
			}
		})
	}

	base, _ := json.Marshal(normalizedFixture(t))
	invalidNormalized := []struct {
		name   string
		mutate func(map[string]any)
	}{
		{"missing is_best", func(snapshot map[string]any) {
			delete(snapshot["entries"].([]any)[0].(map[string]any)["releases"].([]any)[0].(map[string]any), "is_best")
		}},
		{"null size", func(snapshot map[string]any) {
			snapshot["entries"].([]any)[0].(map[string]any)["releases"].([]any)[0].(map[string]any)["files"].([]any)[0].(map[string]any)["size"] = nil
		}},
		{"null entries", func(snapshot map[string]any) { snapshot["entries"] = nil }},
		{"null releases", func(snapshot map[string]any) { snapshot["entries"].([]any)[0].(map[string]any)["releases"] = nil }},
	}
	for _, test := range invalidNormalized {
		t.Run(test.name, func(t *testing.T) {
			var value map[string]any
			if err := json.Unmarshal(base, &value); err != nil {
				t.Fatal(err)
			}
			test.mutate(value)
			data, _ := json.Marshal(value)
			if _, err := normalizeSnapshot(data); err == nil {
				t.Fatal("malformed normalized input was accepted")
			}
		})
	}
}

func TestArtifactsDeterministicAndReportChanges(t *testing.T) {
	snapshot := normalizedFixture(t)
	previousData, _ := json.Marshal(map[string]any{"items": []any{rawFixture("entry-old", "release-old")}})
	previous, err := normalizeSnapshot(previousData)
	if err != nil {
		t.Fatal(err)
	}
	overrides := Overrides{SchemaVersion: 1, Releases: map[string]Override{"release-a": {Aliases: []string{"Show alias"}}, "stale": {Exclude: true}}}
	first, err := buildArtifacts(snapshot, overrides, &previous, defaultPackVersion)
	if err != nil {
		t.Fatal(err)
	}
	second, err := buildArtifacts(snapshot, overrides, &previous, defaultPackVersion)
	if err != nil {
		t.Fatal(err)
	}
	for _, name := range artifactNames {
		if !bytes.Equal(first[name], second[name]) {
			t.Fatalf("%s was not deterministic", name)
		}
	}
	var report struct {
		StaleOverrides []string                       `json:"stale_overrides"`
		Changes        map[string]map[string][]string `json:"changes"`
	}
	if err := json.Unmarshal(first["seadex-coverage.json"], &report); err != nil {
		t.Fatal(err)
	}
	if len(report.StaleOverrides) != 1 || report.StaleOverrides[0] != "stale" {
		t.Fatalf("stale overrides = %#v", report.StaleOverrides)
	}
	if got := report.Changes["releases"]["removed"]; len(got) != 1 || got[0] != "release-old" {
		t.Fatalf("removed releases = %#v", got)
	}
	var generated pack
	if err := json.Unmarshal(first["seadex-scoring.json"], &generated); err != nil {
		t.Fatal(err)
	}
	if generated.ID != packID || len(generated.Rules) != 1 || generated.Rules[0].AppliedFacets[0] != "anime" {
		t.Fatalf("unexpected pack %#v", generated)
	}
}

func TestFetchFailureDoesNotReplaceSnapshot(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.URL.Query().Get("sort") != "id" || request.Header.Get("Accept") != "application/json" {
			t.Error("fetch request missing stable API parameters")
		}
		if request.URL.Query().Get("page") == "1" {
			_ = json.NewEncoder(writer).Encode(pageResponse{Page: 1, PerPage: 1, TotalPages: 2, TotalItems: 2, Items: []rawEntry{mustRawEntry(t, "entry-a", "release-a")}})
			return
		}
		writer.WriteHeader(http.StatusServiceUnavailable)
	}))
	defer server.Close()
	directory := t.TempDir()
	output := filepath.Join(directory, "snapshot.json.gz")
	if err := os.WriteFile(output, []byte("previous"), 0o600); err != nil {
		t.Fatal(err)
	}
	err := run([]string{"fetch", "--api-url", server.URL, "--per-page", "1", "--output", output, "--timeout", "1s"})
	if err == nil {
		t.Fatal("partial fetch unexpectedly succeeded")
	}
	content, err := os.ReadFile(output)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "previous" {
		t.Fatal("failed fetch replaced output")
	}
}

func TestGenerateInvalidInputDoesNotReplaceArtifacts(t *testing.T) {
	directory := t.TempDir()
	snapshotPath := filepath.Join(directory, "invalid.json")
	if err := os.WriteFile(snapshotPath, []byte(`{"schema_version":1,"entries":null}`), 0o600); err != nil {
		t.Fatal(err)
	}
	for _, name := range artifactNames {
		if err := os.WriteFile(filepath.Join(directory, name), []byte("previous"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if err := run([]string{"generate", "--snapshot", snapshotPath, "--output-dir", directory}); err == nil {
		t.Fatal("invalid generate input was accepted")
	}
	for _, name := range artifactNames {
		content, err := os.ReadFile(filepath.Join(directory, name))
		if err != nil || string(content) != "previous" {
			t.Fatalf("invalid generate replaced %s", name)
		}
	}
}

func TestGzipSnapshotIsDeterministicAndReadable(t *testing.T) {
	snapshot := normalizedFixture(t)
	first := snapshotFileBytes(snapshot, "snapshot.json.gz")
	second := snapshotFileBytes(snapshot, "snapshot.json.gz")
	if !bytes.Equal(first, second) {
		t.Fatal("gzip output was not deterministic")
	}
	path := filepath.Join(t.TempDir(), "snapshot.json.gz")
	if err := os.WriteFile(path, first, 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err := loadSnapshot(path)
	if err != nil {
		t.Fatal(err)
	}
	if snapshotChecksum(loaded) != snapshotChecksum(snapshot) {
		t.Fatal("gzip snapshot did not round trip")
	}
}

func TestFetchSaveLoadRoundTripKeepsEmptyArrays(t *testing.T) {
	emptyReleases := rawEntry{ID: "entry-empty-releases", AniListID: 1, UpdatedAt: "2026-01-01T00:00:00Z", Notes: "", TRs: json.RawMessage(`[]`)}
	emptyReleases.Expand.TRs = []rawRelease{}
	emptyFiles := rawEntry{ID: "entry-empty-files", AniListID: 2, UpdatedAt: "2026-01-01T00:00:00Z", Notes: "", TRs: json.RawMessage(`["release-empty-files"]`)}
	emptyFiles.Expand.TRs = []rawRelease{{ID: "release-empty-files", Group: "Group", Best: true, UpdatedAt: "2026-01-01T00:00:00Z", Files: []rawFile{}}}
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		_ = json.NewEncoder(writer).Encode(pageResponse{Page: 1, PerPage: 2, TotalPages: 1, TotalItems: 2, Items: []rawEntry{emptyReleases, emptyFiles}})
	}))
	defer server.Close()
	snapshot, err := fetchRecords(server.URL, time.Second, 2, server.Client())
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "snapshot.json.gz")
	if err := os.WriteFile(path, snapshotFileBytes(snapshot, path), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err := loadSnapshot(path)
	if err != nil {
		t.Fatal(err)
	}
	var releasesEntry, filesEntry *Entry
	for index := range loaded.Entries {
		entry := &loaded.Entries[index]
		switch entry.ID {
		case "entry-empty-releases":
			releasesEntry = entry
		case "entry-empty-files":
			filesEntry = entry
		}
	}
	if releasesEntry == nil || filesEntry == nil || releasesEntry.Releases == nil || len(filesEntry.Releases) != 1 || filesEntry.Releases[0].Files == nil {
		t.Fatalf("empty arrays became null: %#v", loaded)
	}
}

func TestSourceRevisionUsesEmbeddedSources(t *testing.T) {
	digest := sha256.New()
	for _, source := range []struct {
		name string
		data []byte
	}{
		{"main.go", converterSource},
		{"matcher.go", matcherSource},
		{"encoding.go", encodingSource},
		{"policy.rego", []byte(policyTemplate)},
	} {
		digest.Write([]byte(source.name))
		digest.Write([]byte{0})
		digest.Write(source.data)
		digest.Write([]byte{0})
	}
	want := hex.EncodeToString(digest.Sum(nil))
	if got := sourceRevision(); got != want {
		t.Fatalf("source revision = %q, want %q", got, want)
	}
	workingDirectory, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chdir(t.TempDir()); err != nil {
		t.Fatal(err)
	}
	defer os.Chdir(workingDirectory)
	if got := sourceRevision(); got != want {
		t.Fatalf("source revision changed after chdir: %q, want %q", got, want)
	}
}

func mustRawEntry(t *testing.T, id, releaseID string) rawEntry {
	t.Helper()
	data, err := json.Marshal(rawFixture(id, releaseID))
	if err != nil {
		t.Fatal(err)
	}
	var entry rawEntry
	if err := json.Unmarshal(data, &entry); err != nil {
		t.Fatal(err)
	}
	return entry
}

func TestCheckDoesNotWrite(t *testing.T) {
	snapshot := normalizedFixture(t)
	artifacts, err := buildArtifacts(snapshot, Overrides{SchemaVersion: 1, Releases: map[string]Override{}}, nil, defaultPackVersion)
	if err != nil {
		t.Fatal(err)
	}
	directory := t.TempDir()
	snapshotPath := filepath.Join(directory, "snapshot.json")
	data, _ := json.Marshal(snapshot)
	if err := os.WriteFile(snapshotPath, data, 0o600); err != nil {
		t.Fatal(err)
	}
	for name, content := range artifacts {
		if err := os.WriteFile(filepath.Join(directory, name), content, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if err := run([]string{"check", "--snapshot", snapshotPath, "--output-dir", directory}); err != nil {
		t.Fatal(err)
	}
	regoPath := filepath.Join(directory, "seadex-scoring.rego")
	if err := os.WriteFile(regoPath, []byte("outdated\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := run([]string{"check", "--snapshot", snapshotPath, "--output-dir", directory}); err == nil {
		t.Fatal("outdated output was accepted")
	}
	content, _ := os.ReadFile(regoPath)
	if string(content) != "outdated\n" {
		t.Fatal("check modified an artifact")
	}
}

func TestPackVersionGenerateAndCheck(t *testing.T) {
	snapshot := normalizedFixture(t)
	directory := t.TempDir()
	snapshotPath := filepath.Join(directory, "snapshot.json")
	data, err := json.Marshal(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(snapshotPath, data, 0o600); err != nil {
		t.Fatal(err)
	}
	version := "2.3.4-rc.1+build.5"
	if err := run([]string{"generate", "--snapshot", snapshotPath, "--output-dir", directory, "--pack-version", version}); err != nil {
		t.Fatal(err)
	}
	packBytes, err := os.ReadFile(filepath.Join(directory, "seadex-scoring.json"))
	if err != nil {
		t.Fatal(err)
	}
	var generated pack
	if err := json.Unmarshal(packBytes, &generated); err != nil {
		t.Fatal(err)
	}
	if generated.Version != version {
		t.Fatalf("pack version = %q, want %q", generated.Version, version)
	}
	coverageBytes, err := os.ReadFile(filepath.Join(directory, "seadex-coverage.json"))
	if err != nil {
		t.Fatal(err)
	}
	var report coverage
	if err := json.Unmarshal(coverageBytes, &report); err != nil {
		t.Fatal(err)
	}
	if report.PackVersion != version {
		t.Fatalf("coverage pack version = %q, want %q", report.PackVersion, version)
	}
	if err := run([]string{"check", "--snapshot", snapshotPath, "--output-dir", directory, "--pack-version", version}); err != nil {
		t.Fatalf("same version check failed: %v", err)
	}
	if err := run([]string{"check", "--snapshot", snapshotPath, "--output-dir", directory}); err == nil {
		t.Fatal("default version check accepted custom-version artifacts")
	}
	if err := run([]string{"generate", "--snapshot", snapshotPath, "--output-dir", directory, "--pack-version", "01.2.3"}); err == nil {
		t.Fatal("invalid version was accepted")
	}
	unchanged, err := os.ReadFile(filepath.Join(directory, "seadex-scoring.json"))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(unchanged, packBytes) {
		t.Fatal("invalid version replaced artifacts")
	}
}

func TestValidateSemVer(t *testing.T) {
	for _, version := range []string{"0.0.0", "1.2.3", "1.2.3-alpha.1", "1.2.3-01a+build.01", "1.2.3+build.1", "18446744073709551615.0.0"} {
		if err := validateSemVer(version); err != nil {
			t.Fatalf("valid version %q was rejected: %v", version, err)
		}
	}
	for _, version := range []string{"", "1.2", "01.2.3", "1.02.3", "1.2.03", "18446744073709551616.0.0", "1.2.3-", "1.2.3-01", "1.2.3-alpha..1", "1.2.3+", "1.2.3+build+metadata", "v1.2.3", "1.2.3-α"} {
		if err := validateSemVer(version); err == nil {
			t.Fatalf("invalid version %q was accepted", version)
		}
	}
}

func TestFetchRequiresStablePaginationMetadata(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		_ = json.NewEncoder(writer).Encode(pageResponse{Page: 1, PerPage: 1, TotalPages: 1, TotalItems: 2, Items: []rawEntry{mustRawEntry(t, "entry-a", "release-a")}})
	}))
	defer server.Close()
	_, err := fetchRecords(server.URL, time.Second, 1, server.Client())
	if err == nil || !strings.Contains(err.Error(), "fetched") {
		t.Fatalf("expected totalItems mismatch, got %v", err)
	}
}

func TestFetchRejectsTrailingJSONAndZeroTimeout(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		data, _ := json.Marshal(pageResponse{Page: 1, PerPage: 1, TotalPages: 1, TotalItems: 1, Items: []rawEntry{mustRawEntry(t, "entry-a", "release-a")}})
		_, _ = writer.Write(append(data, []byte(`\n{}`)...))
	}))
	defer server.Close()
	if _, err := fetchRecords(server.URL, time.Second, 1, server.Client()); err == nil {
		t.Fatal("trailing HTTP JSON was accepted")
	}
	if _, err := fetchRecords(server.URL, 0, 1, server.Client()); err == nil {
		t.Fatal("zero timeout was accepted")
	}
}
