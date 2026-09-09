package main

// refreshCommands is intentionally data-only so tests can pin the converter
// contract without contacting upstream or executing a release operation.
func refreshCommands(revision, version string) [][]string {
	return [][]string{
		{"fetch", "--revision", revision, "--output", "snapshot/upstream-raw.json"},
		{"distill", "--snapshot", "snapshot/upstream-raw.json", "--output-dir", "snapshot"},
		{"generate", "--snapshot", "snapshot/core-snapshot.json", "--output-dir", "..", "--pack-version", version},
		{"check", "--snapshot", "snapshot/core-snapshot.json", "--output-dir", "..", "--pack-version", version},
	}
}
