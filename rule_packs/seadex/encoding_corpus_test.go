package main

import (
	"os"
	"path/filepath"
	"testing"
)

// These expectations are authored independently of the encoder. In particular,
// seasons, episodes, versions and CRCs cannot be recombined across stored tuples.
func TestEncodingEngineCorpus(t *testing.T) {
	tables := Tables{
		Strict: map[string]int{
			"[g] correlated s01e02v1 [1080p x264] [1234abcd]": 400,
			"[g] correlated s02e01v2 [1080p x264] [abcd1234]": 400,
			"[g] listed s01e01v1 1080p":                       200,
			"[g] listed s02e02v2 1080p":                       200,
			"compact ¦§42":                                    400,
			"compact ¦§43":                                    400,
			"literal %60 and `":                               400,
		},
		StrictCollisions: map[string]bool{
			"[g] collision s01e01": true,
			"[g] collision s01e02": true,
		},
		Tolerant: map[string]int{
			"g|collision s01e01": 400,
			"g|collision s01e02": 400,
		},
	}
	cases := []map[string]any{
		{"title": "[g] correlated s01e02v1 [1080p x264] [1234abcd]", "score": 400},
		{"title": "[g] correlated s02e01v2 [1080p x264] [abcd1234]", "score": 400},
		{"title": "[g] correlated s01e01v2 [1080p x264] [abcd1234]", "score": 0},
		{"title": "[g] correlated s01e02v1 [720p x264] [1234abcd]", "score": 0},
		{"title": "[g] correlated s01e02v3 [1080p x264] [1234abcd]", "score": 0},
		{"title": "[g] correlated s01e02v1 [1080p x264] [abcd1234]", "score": 0},
		{"title": "[g] listed s01e01v1 1080p", "score": 200},
		{"title": "[g] listed s02e02v2 1080p", "score": 200},
		{"title": "[g] listed s01e02v2 1080p", "score": 0},
		{"title": "compact ¦§42", "score": 400},
		{"title": "compact ¦§43", "score": 400},
		{"title": "compact §¦42", "score": 0},
		{"title": "compact ¦§44", "score": 0},
		{"title": "literal %60 and `", "score": 400},
		{"title": "literal ` and %60", "score": 0},
		{"title": "[g] collision s01e01", "group": "G", "score": 0},
		{"title": "[g] collision s01e02", "group": "G", "score": 0},
		{"title": "collision.s01e01-G", "group": "G", "score": 400, "code": "seadex_tolerant_best"},
	}
	output := os.Getenv("SEADEX_ENGINE_CORPUS_DIR")
	if output == "" {
		output = t.TempDir()
	}
	output, err := filepath.Abs(output)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(output, 0755); err != nil {
		t.Fatal(err)
	}
	policy, err := renderPolicy(tables)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(output, "encoding-adversarial.rego")
	if err := os.WriteFile(path, []byte(policy), 0644); err != nil {
		t.Fatal(err)
	}
	writeCorpusJobs(t, filepath.Join(output, "encoding-adversarial-jobs.json"), []any{
		map[string]any{"name": "encoding-adversarial", "source": path, "cases": cases},
	})
}
