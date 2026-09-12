package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func refreshFixture(t *testing.T) (Snapshot, string, string, string) {
	t.Helper()
	snapshot := normalizedFixture(t)
	dir := t.TempDir()
	previous := filepath.Join(dir, "previous.json.gz")
	manifest := filepath.Join(dir, "pack.json")
	artifacts, err := buildArtifacts(snapshot, Overrides{SchemaVersion: 1, Releases: map[string]Override{}}, nil, "1.2.3")
	if err != nil {
		t.Fatal(err)
	}
	writeRefreshFile(t, previous, snapshotFileBytes(snapshot, previous))
	writeRefreshFile(t, manifest, artifacts["seadex-scoring.json"])
	return snapshot, previous, manifest, filepath.Join(dir, "staged")
}

func writeRefreshFile(t *testing.T, path string, data []byte) {
	t.Helper()
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
}

func TestRefreshIgnoresReferenceOnlyChanges(t *testing.T) {
	for _, metadataOnly := range []bool{false, true} {
		snapshot, previous, manifest, output := refreshFixture(t)
		if metadataOnly {
			snapshot.Entries[0].Notes = "new reference notes"
			snapshot.Entries[0].UpdatedAt = "2026-09-08T00:00:00Z"
		}
		incoming := filepath.Join(filepath.Dir(output), "incoming.json.gz")
		writeRefreshFile(t, incoming, snapshotFileBytes(snapshot, incoming))
		result, err := refreshArtifacts(incoming, previous, manifest, "", output)
		if err != nil || result.Changed || result.Version != "1.2.3" {
			t.Fatalf("no-op refresh: %#v, %v", result, err)
		}
		if _, err := os.Stat(output); !os.IsNotExist(err) {
			t.Fatalf("no-op created output: %v", err)
		}
	}
}

func TestRefreshStagesDeterministicPatchAndPreservesInputs(t *testing.T) {
	snapshot, previous, manifest, output := refreshFixture(t)
	oldSnapshot, _ := os.ReadFile(previous)
	oldManifest, _ := os.ReadFile(manifest)
	snapshot.Entries[0].Releases[0].Best = false
	incoming := filepath.Join(filepath.Dir(output), "incoming.json.gz")
	writeRefreshFile(t, incoming, snapshotFileBytes(snapshot, incoming))
	for _, destination := range []string{output, output + "-again"} {
		result, err := refreshArtifacts(incoming, previous, manifest, "", destination)
		if err != nil || !result.Changed || result.Version != "1.2.4" || result.SnapshotSHA256 != snapshotChecksum(snapshot) {
			t.Fatalf("changed refresh: %#v, %v", result, err)
		}
		if err := run([]string{"check", "--snapshot", filepath.Join(destination, "snapshot.json.gz"), "--previous-snapshot", previous, "--output-dir", destination, "--pack-version", "1.2.4"}); err != nil {
			t.Fatal(err)
		}
	}
	for _, name := range append(append([]string{}, artifactNames...), "snapshot.json.gz") {
		first, err := os.ReadFile(filepath.Join(output, name))
		if err != nil {
			t.Fatal(err)
		}
		second, err := os.ReadFile(filepath.Join(output+"-again", name))
		if err != nil || !bytes.Equal(first, second) {
			t.Fatalf("nondeterministic %s: %v", name, err)
		}
	}
	for path, expected := range map[string][]byte{previous: oldSnapshot, manifest: oldManifest} {
		actual, err := os.ReadFile(path)
		if err != nil || !bytes.Equal(actual, expected) {
			t.Fatalf("input modified: %s", path)
		}
	}
	if _, err := refreshArtifacts(incoming, previous, manifest, "", output); err == nil {
		t.Fatal("existing staged output was overwritten")
	}
}

func TestRefreshRejectsUnsafeInputs(t *testing.T) {
	for _, test := range []string{"malformed", "empty", "stale override", "wrong pack"} {
		t.Run(test, func(t *testing.T) {
			snapshot, previous, manifest, output := refreshFixture(t)
			incoming := filepath.Join(filepath.Dir(output), "incoming.json")
			overrides := ""
			switch test {
			case "malformed":
				writeRefreshFile(t, incoming, []byte(`{"schema_version":1,"entries":null}`))
			case "empty":
				snapshot.Entries = []Entry{}
				writeRefreshFile(t, incoming, snapshotFileBytes(snapshot, incoming))
			case "stale override":
				overrides = filepath.Join(filepath.Dir(output), "overrides.json")
				writeRefreshFile(t, overrides, []byte(`{"schema_version":1,"releases":{"gone":{"exclude":true}}}`))
				writeRefreshFile(t, incoming, snapshotFileBytes(snapshot, incoming))
			case "wrong pack":
				data, _ := os.ReadFile(manifest)
				writeRefreshFile(t, manifest, bytes.Replace(data, []byte(packID), []byte("unrelated-pack"), 1))
				writeRefreshFile(t, incoming, snapshotFileBytes(snapshot, incoming))
			}
			if _, err := refreshArtifacts(incoming, previous, manifest, overrides, output); err == nil {
				t.Fatal("unsafe input accepted")
			}
			if _, err := os.Stat(output); !os.IsNotExist(err) {
				t.Fatalf("failed refresh created output: %v", err)
			}
		})
	}
}

func TestRefreshReportsRemovedRecords(t *testing.T) {
	snapshot, _, manifest, output := refreshFixture(t)
	older := normalizedFixture(t)
	extra := normalizedFixture(t).Entries[0]
	extra.ID, extra.AniListID = "entry-b", 456
	extra.Releases[0].ID = "release-b"
	extra.Releases[0].Files[0].Name = "[Group] Different - 01.mkv"
	older.Entries = append(older.Entries, extra)
	previous := filepath.Join(filepath.Dir(output), "older.json.gz")
	writeRefreshFile(t, previous, snapshotFileBytes(older, previous))
	artifacts, err := buildArtifacts(older, Overrides{SchemaVersion: 1, Releases: map[string]Override{}}, nil, "1.2.3")
	if err != nil {
		t.Fatal(err)
	}
	writeRefreshFile(t, manifest, artifacts["seadex-scoring.json"])
	incoming := filepath.Join(filepath.Dir(output), "incoming.json.gz")
	writeRefreshFile(t, incoming, snapshotFileBytes(snapshot, incoming))
	result, err := refreshArtifacts(incoming, previous, manifest, "", output)
	if err != nil || !result.Changed {
		t.Fatalf("deletion: %#v, %v", result, err)
	}
	data, err := os.ReadFile(filepath.Join(output, "seadex-coverage.json"))
	if err != nil {
		t.Fatal(err)
	}
	var report coverage
	if err := json.Unmarshal(data, &report); err != nil {
		t.Fatal(err)
	}
	if removed := report.Changes["releases"]["removed"]; len(removed) != 1 || removed[0] != "release-b" {
		t.Fatalf("removed = %v", removed)
	}
}

func TestRefreshWorkflowOutputs(t *testing.T) {
	_, previous, manifest, output := refreshFixture(t)
	githubOutput := filepath.Join(filepath.Dir(output), "github-output")
	if err := run([]string{"refresh", "--snapshot", previous, "--previous-snapshot", previous, "--current-pack", manifest, "--output-dir", output, "--github-output", githubOutput}); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(githubOutput)
	if err != nil || !strings.HasPrefix(string(data), "changed=false\nversion=1.2.3\nsnapshot_sha256=") || len(strings.Split(strings.TrimSpace(string(data)), "\n")) != 3 {
		t.Fatalf("workflow output = %q, %v", data, err)
	}
}

func TestNextPatch(t *testing.T) {
	if got, err := nextPatch("0.20.9"); err != nil || got != "0.20.10" {
		t.Fatalf("patch = %q, %v", got, err)
	}
	for _, version := range []string{"v1.2.3", "1.2", "1.2.3-rc.1", "1.2.3+build", "1.2.18446744073709551615"} {
		if _, err := nextPatch(version); err == nil {
			t.Fatalf("unsafe version accepted: %s", version)
		}
	}
}
