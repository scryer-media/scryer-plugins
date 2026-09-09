package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestFetchRawSnapshotCompleteAndDeterministic(t *testing.T) {
	server := upstreamFixture(t, false)
	defer server.Close()
	first, err := fetchRawSnapshot("main", server.URL, server.URL+"/raw")
	if err != nil {
		t.Fatal(err)
	}
	second, err := fetchRawSnapshot("0123456789012345678901234567890123456789", server.URL, server.URL+"/raw")
	if err != nil {
		t.Fatal(err)
	}
	if string(first) != string(second) {
		t.Fatal("explicit SHA changed normalized output")
	}
	var snapshot rawUpstreamSnapshot
	if err := json.Unmarshal(first, &snapshot); err != nil {
		t.Fatal(err)
	}
	if snapshot.SourceRevision != "0123456789012345678901234567890123456789" || len(snapshot.Files) != 3 {
		t.Fatalf("unexpected snapshot: %#v", snapshot)
	}
	for _, file := range snapshot.Files {
		if file.SHA256 == "" || len(file.Records) != 1 {
			t.Fatalf("bad file %#v", file)
		}
	}
}
func TestFetchRawSnapshotRejectsPartialTree(t *testing.T) {
	server := upstreamFixture(t, true)
	defer server.Close()
	if _, err := fetchRawSnapshot("main", server.URL, server.URL+"/raw"); err == nil {
		t.Fatal("accepted partial upstream tree")
	}
}
func TestRawFetchFailurePreservesOutput(t *testing.T) {
	server := upstreamFixture(t, true)
	defer server.Close()
	target := filepath.Join(t.TempDir(), "raw.json")
	if err := os.WriteFile(target, []byte("preserve"), 0644); err != nil {
		t.Fatal(err)
	}
	err := fetch([]string{"--revision", "main", "--github-api-base", server.URL, "--github-raw-base", server.URL + "/raw", "--output", target})
	if err == nil {
		t.Fatal("accepted partial tree")
	}
	got, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "preserve" {
		t.Fatal("failed raw fetch replaced output")
	}
}
func upstreamFixture(t *testing.T, partial bool) *httptest.Server {
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.HasPrefix(r.URL.Path, "/repos/TRaSH-Guides/Guides/commits/") {
			_, _ = w.Write([]byte(`{"sha":"0123456789012345678901234567890123456789"}`))
			return
		}
		if strings.Contains(r.URL.Path, "/contents/docs/json/") {
			dir := strings.TrimPrefix(r.URL.Path, "/repos/TRaSH-Guides/Guides/contents/docs/json/")
			if partial && dir == "guide-only" {
				_, _ = w.Write([]byte(`[]`))
				return
			}
			_, _ = w.Write([]byte(`[{"name":"example.json","type":"file"}]`))
			return
		}
		if strings.HasPrefix(r.URL.Path, "/raw/") {
			_, _ = w.Write([]byte(`{"name":"Example","trash_id":"id","trash_scores":{"default":10},"specifications":[{"name":"x","implementation":"ReleaseTitleSpecification","required":false,"negate":false,"fields":{"value":"Example"}}]}`))
			return
		}
		http.NotFound(w, r)
	}))
}
