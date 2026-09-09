package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"path"
	"sort"
	"strings"
	"time"
)

const (
	defaultGitHubAPIBase       = "https://api.github.com"
	defaultGitHubRawBase       = "https://raw.githubusercontent.com/TRaSH-Guides/Guides"
	maxUpstreamFileBytes int64 = 2 << 20
)

var upstreamDirectories = []string{"sonarr/cf", "radarr/cf", "guide-only"}

type rawUpstreamSnapshot struct {
	SchemaVersion  int               `json:"schema_version"`
	SourceRevision string            `json:"source_revision"`
	Files          []rawUpstreamFile `json:"files"`
}
type rawUpstreamFile struct {
	Path            string            `json:"path"`
	SHA256          string            `json:"sha256"` // immutable upstream source bytes
	CanonicalSHA256 string            `json:"canonical_sha256"`
	Records         []rawCustomFormat `json:"records"`
}
type rawCustomFormat struct {
	Name           string             `json:"name"`
	TrashID        string             `json:"trash_id"`
	Scores         map[string]int64   `json:"trash_scores,omitempty"`
	Specifications []rawSpecification `json:"specifications"`
}
type rawSpecification struct {
	Name           string          `json:"name"`
	Implementation string          `json:"implementation"`
	Required       json.RawMessage `json:"required"`
	Negate         json.RawMessage `json:"negate"`
	Fields         json.RawMessage `json:"fields"`
}
type githubCommit struct {
	SHA string `json:"sha"`
}
type githubEntry struct {
	Name string `json:"name"`
	Type string `json:"type"`
}

func fetchRawSnapshot(revision, apiBase, rawBase string) ([]byte, error) {
	client := &http.Client{Timeout: 30 * time.Second}
	sha, err := resolveUpstreamSHA(client, apiBase, revision)
	if err != nil {
		return nil, err
	}
	var files []rawUpstreamFile
	for _, dir := range upstreamDirectories {
		entries, err := getUpstreamJSON[[]githubEntry](client, fmt.Sprintf("%s/repos/TRaSH-Guides/Guides/contents/docs/json/%s?ref=%s", strings.TrimRight(apiBase, "/"), dir, url.QueryEscape(sha)))
		if err != nil {
			return nil, fmt.Errorf("list %s: %w", dir, err)
		}
		if len(entries) == 0 {
			return nil, fmt.Errorf("list %s: empty directory", dir)
		}
		if len(entries) >= 1000 {
			return nil, fmt.Errorf("list %s: GitHub contents listing may be truncated", dir)
		}
		sort.Slice(entries, func(i, j int) bool { return entries[i].Name < entries[j].Name })
		foundJSON := 0
		for _, entry := range entries {
			if entry.Type != "file" || !strings.HasSuffix(entry.Name, ".json") {
				continue
			}
			foundJSON++
			filePath := path.Join("docs/json", dir, entry.Name)
			body, err := getUpstreamBytes(client, strings.TrimRight(rawBase, "/")+"/"+sha+"/"+filePath)
			if err != nil {
				return nil, fmt.Errorf("fetch %s: %w", filePath, err)
			}
			formats, err := decodeRawCustomFormats(body)
			if err != nil {
				return nil, fmt.Errorf("decode %s: %w", filePath, err)
			}
			digest := sha256.Sum256(body)
			canonical := canonicalRawRecords(formats)
			files = append(files, rawUpstreamFile{Path: filePath, SHA256: hex.EncodeToString(digest[:]), CanonicalSHA256: canonical, Records: formats})
		}
		if foundJSON == 0 {
			return nil, fmt.Errorf("list %s: no JSON custom formats", dir)
		}
	}
	if len(files) == 0 {
		return nil, fmt.Errorf("upstream tree contains no JSON custom formats")
	}
	sort.Slice(files, func(i, j int) bool { return files[i].Path < files[j].Path })
	encoded, err := json.MarshalIndent(rawUpstreamSnapshot{SchemaVersion: 1, SourceRevision: sha, Files: files}, "", "  ")
	if err != nil {
		return nil, err
	}
	return append(encoded, '\n'), nil
}
func canonicalRawRecords(records []rawCustomFormat) string {
	body, _ := json.Marshal(records)
	digest := sha256.Sum256(body)
	return hex.EncodeToString(digest[:])
}
func resolveUpstreamSHA(client *http.Client, apiBase, revision string) (string, error) {
	commit, err := getUpstreamJSON[githubCommit](client, strings.TrimRight(apiBase, "/")+"/repos/TRaSH-Guides/Guides/commits/"+url.PathEscape(revision))
	if err != nil {
		return "", fmt.Errorf("resolve revision %q: %w", revision, err)
	}
	if len(commit.SHA) != 40 {
		return "", fmt.Errorf("resolved revision %q is not a 40-character SHA", commit.SHA)
	}
	return commit.SHA, nil
}
func getUpstreamJSON[T any](client *http.Client, endpoint string) (T, error) {
	var value T
	body, err := getUpstreamBytes(client, endpoint)
	if err != nil {
		return value, err
	}
	if err := json.Unmarshal(body, &value); err != nil {
		return value, err
	}
	return value, nil
}
func getUpstreamBytes(client *http.Client, endpoint string) ([]byte, error) {
	request, err := http.NewRequest(http.MethodGet, endpoint, nil)
	if err != nil {
		return nil, err
	}
	request.Header.Set("Accept", "application/json")
	request.Header.Set("User-Agent", "scryer-trash-pack-fetch")
	response, err := client.Do(request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("HTTP %s", response.Status)
	}
	body, err := io.ReadAll(io.LimitReader(response.Body, maxUpstreamFileBytes+1))
	if err != nil {
		return nil, err
	}
	if int64(len(body)) > maxUpstreamFileBytes {
		return nil, fmt.Errorf("response exceeds %d bytes", maxUpstreamFileBytes)
	}
	return body, nil
}
func decodeRawCustomFormats(body []byte) ([]rawCustomFormat, error) {
	var one rawCustomFormat
	if err := json.Unmarshal(body, &one); err != nil {
		return nil, err
	}
	if one.Name == "" || len(one.Specifications) == 0 {
		return nil, fmt.Errorf("custom format requires name and specifications")
	}
	for i, s := range one.Specifications {
		if s.Name == "" || s.Implementation == "" || !json.Valid(s.Fields) {
			return nil, fmt.Errorf("specification %d missing required fields", i)
		}
		if len(s.Required) == 0 {
			s.Required = []byte("false")
		}
		if len(s.Negate) == 0 {
			s.Negate = []byte("false")
		}
	}
	return []rawCustomFormat{one}, nil
}
