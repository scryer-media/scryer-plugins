package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

type refreshResult struct {
	Changed        bool   `json:"changed"`
	Version        string `json:"version"`
	SnapshotSHA256 string `json:"snapshot_sha256"`
}

// refresh stages a new immutable patch only when the scoring policy changes.
// It never fetches, modifies the source checkout, commits, or publishes.
func runRefresh(args []string) error {
	flags := flag.NewFlagSet("refresh", flag.ContinueOnError)
	flags.SetOutput(io.Discard)
	snapshot := flags.String("snapshot", "", "new fetched snapshot")
	previous := flags.String("previous-snapshot", "", "current committed snapshot")
	currentPack := flags.String("current-pack", "", "current SeaDex pack manifest")
	output := flags.String("output-dir", "", "new staging directory; must not exist")
	overrides := flags.String("overrides", "", "reviewed overrides")
	githubOutput := flags.String("github-output", "", "append workflow outputs to this file")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || *snapshot == "" || *previous == "" || *currentPack == "" || *output == "" {
		return errors.New("refresh requires --snapshot, --previous-snapshot, --current-pack and --output-dir, with no positional arguments")
	}
	result, err := refreshArtifacts(*snapshot, *previous, *currentPack, *overrides, *output)
	if err != nil {
		return err
	}
	if *githubOutput != "" {
		file, err := os.OpenFile(*githubOutput, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0600)
		if err != nil {
			return err
		}
		_, writeErr := fmt.Fprintf(file, "changed=%t\nversion=%s\nsnapshot_sha256=%s\n", result.Changed, result.Version, result.SnapshotSHA256)
		closeErr := file.Close()
		if writeErr != nil {
			return writeErr
		}
		if closeErr != nil {
			return closeErr
		}
	}
	return json.NewEncoder(os.Stdout).Encode(result)
}

func nextPatch(version string) (string, error) {
	if err := validateSemVer(version); err != nil {
		return "", err
	}
	if strings.ContainsAny(version, "-+") {
		return "", errors.New("daily refresh requires a stable major.minor.patch version")
	}
	parts := strings.Split(version, ".")
	patch, err := strconv.ParseUint(parts[2], 10, 64)
	if err != nil || patch == ^uint64(0) {
		return "", errors.New("patch version overflow")
	}
	return parts[0] + "." + parts[1] + "." + strconv.FormatUint(patch+1, 10), nil
}

func refreshArtifacts(snapshotPath, previousPath, packPath, overridesPath, outputDir string) (refreshResult, error) {
	var result refreshResult
	data, err := os.ReadFile(packPath)
	if err != nil {
		return result, err
	}
	var current pack
	if err := decodeStrict(data, &current); err != nil {
		return result, fmt.Errorf("current pack: %w", err)
	}
	if current.SchemaVersion != schemaVersion || current.ID != packID || len(current.Rules) != 1 || current.Rules[0].ID != ruleID || strings.TrimSpace(current.Rules[0].RegoSource) == "" {
		return result, errors.New("current manifest must be the single-rule SeaDex scoring pack")
	}
	version, err := nextPatch(current.Version)
	if err != nil {
		return result, err
	}
	snapshot, err := loadSnapshot(snapshotPath)
	if err != nil {
		return result, err
	}
	previous, err := loadSnapshot(previousPath)
	if err != nil {
		return result, err
	}
	// An empty upstream response can be structurally valid but must never erase
	// the entire recommendation pack through unattended automation.
	if len(snapshot.Entries) == 0 {
		return result, errors.New("refusing an empty SeaDex refresh")
	}
	overrides, err := loadOverrides(overridesPath)
	if err != nil {
		return result, err
	}
	if stale := staleOverrides(snapshot, overrides); len(stale) != 0 {
		return result, fmt.Errorf("review stale SeaDex overrides before publishing: %s", strings.Join(stale, ", "))
	}
	artifacts, err := buildArtifacts(snapshot, overrides, &previous, version)
	if err != nil {
		return result, err
	}
	result = refreshResult{Version: current.Version, SnapshotSHA256: snapshotChecksum(snapshot)}
	if bytes.Equal(bytes.TrimSpace(artifacts["seadex-scoring.rego"]), bytes.TrimSpace([]byte(current.Rules[0].RegoSource))) {
		return result, nil
	}
	// Build and validate everything before touching the destination. A dedicated
	// new directory makes failed writes harmless to the last valid source data.
	if _, err := os.Stat(outputDir); !errors.Is(err, os.ErrNotExist) {
		if err != nil {
			return result, err
		}
		return result, errors.New("refresh output directory already exists")
	}
	parent := filepath.Dir(filepath.Clean(outputDir))
	if err := os.MkdirAll(parent, 0755); err != nil {
		return result, err
	}
	stage, err := os.MkdirTemp(parent, ".seadex-refresh-*")
	if err != nil {
		return result, err
	}
	defer os.RemoveAll(stage)
	if err := writeArtifacts(stage, artifacts); err != nil {
		return result, err
	}
	if err := atomicWrite(filepath.Join(stage, "snapshot.json.gz"), snapshotFileBytes(snapshot, "snapshot.json.gz")); err != nil {
		return result, err
	}
	if err := os.Rename(stage, outputDir); err != nil {
		return result, err
	}
	result.Changed, result.Version = true, version
	return result, nil
}
