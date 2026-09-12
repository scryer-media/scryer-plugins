package main

import (
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	_ "embed"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
)

//go:embed main.go
var converterSource []byte

//go:embed matcher.go
var matcherSource []byte

//go:embed encoding.go
var encodingSource []byte

const (
	schemaVersion      = 1
	packID             = "seadex-scoring-pack"
	defaultPackVersion = "1.0.0"
	ruleID             = "seadex-preferences"
	defaultAPIURL      = "https://releases.moe/api/collections/entries/records"
	defaultPerPage     = 500
)

var artifactNames = []string{"seadex-scoring.json", "seadex-scoring.rego", "seadex-coverage.json"}
var errOutdated = errors.New("outdated artifacts")

// Snapshot is the compact, deterministic source form stored in the pack.
type Snapshot struct {
	SchemaVersion int     `json:"schema_version"`
	Entries       []Entry `json:"entries"`
}

type Entry struct {
	ID              string    `json:"id"`
	AniListID       int64     `json:"anilist_id"`
	UpdatedAt       string    `json:"updated_at"`
	Notes           string    `json:"notes"`
	TheoreticalBest *string   `json:"theoretical_best"`
	Releases        []Release `json:"releases"`
}

type Release struct {
	ID        string `json:"id"`
	Group     string `json:"release_group"`
	Best      bool   `json:"is_best"`
	UpdatedAt string `json:"updated_at"`
	Files     []File `json:"files"`
}

type File struct {
	Name string `json:"name"`
	Size int64  `json:"size"`
}

type Overrides struct {
	SchemaVersion int                 `json:"schema_version"`
	Releases      map[string]Override `json:"releases"`
}

type Override struct {
	Exclude bool     `json:"exclude"`
	Aliases []string `json:"aliases"`
}

type rawEntry struct {
	ID              string          `json:"id"`
	AniListID       int64           `json:"alID"`
	UpdatedAt       string          `json:"updated"`
	Notes           string          `json:"notes"`
	TheoreticalBest *string         `json:"theoreticalBest"`
	TRs             json.RawMessage `json:"trs"`
	Expand          struct {
		TRs []rawRelease `json:"trs"`
	} `json:"expand"`
}

type rawRelease struct {
	ID        string    `json:"id"`
	Group     string    `json:"releaseGroup"`
	Best      bool      `json:"isBest"`
	UpdatedAt string    `json:"updated"`
	Files     []rawFile `json:"files"`
}

type rawFile struct {
	Name string `json:"name"`
	Size int64  `json:"length"`
}

type pageResponse struct {
	Page       int        `json:"page"`
	PerPage    int        `json:"perPage"`
	TotalPages int        `json:"totalPages"`
	TotalItems int        `json:"totalItems"`
	Items      []rawEntry `json:"items"`
}

type pack struct {
	SchemaVersion int        `json:"schema_version"`
	ID            string     `json:"id"`
	Name          string     `json:"name"`
	Description   string     `json:"description"`
	Author        string     `json:"author"`
	Version       string     `json:"version"`
	Customizable  bool       `json:"customizable"`
	Rules         []packRule `json:"rules"`
}

type packRule struct {
	ID            string   `json:"id"`
	Title         string   `json:"title"`
	Description   string   `json:"description"`
	Category      string   `json:"category"`
	AppliedFacets []string `json:"appliedFacets"`
	RegoSource    string   `json:"regoSource"`
}

type coverage struct {
	SchemaVersion     int                            `json:"schema_version"`
	PackID            string                         `json:"pack_id"`
	PackVersion       string                         `json:"pack_version"`
	SnapshotSHA256    string                         `json:"snapshot_sha256"`
	ConverterRevision string                         `json:"converter_revision"`
	Included          json.RawMessage                `json:"included"`
	Skipped           json.RawMessage                `json:"skipped"`
	Ambiguous         json.RawMessage                `json:"ambiguous"`
	StaleOverrides    []string                       `json:"stale_overrides"`
	Changes           map[string]map[string][]string `json:"changes"`
	Matching          map[string]json.RawMessage     `json:"matching"`
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		if errors.Is(err, errOutdated) {
			os.Exit(1)
		}
		os.Exit(2)
	}
}

func run(args []string) error {
	if len(args) == 0 {
		return errors.New("expected fetch, generate, check, or refresh")
	}
	switch args[0] {
	case "refresh":
		return runRefresh(args[1:])
	case "fetch":
		flags := flag.NewFlagSet("fetch", flag.ContinueOnError)
		flags.SetOutput(io.Discard)
		apiURL := flags.String("api-url", defaultAPIURL, "SeaDex API endpoint")
		output := flags.String("output", "", "normalized snapshot path")
		timeout := flags.Duration("timeout", 30*time.Second, "request timeout")
		perPage := flags.Int("per-page", defaultPerPage, "API page size")
		if err := flags.Parse(args[1:]); err != nil {
			return err
		}
		if *output == "" {
			return errors.New("fetch requires --output")
		}
		snapshot, err := fetchRecords(*apiURL, *timeout, *perPage, http.DefaultClient)
		if err != nil {
			return err
		}
		return atomicWrite(*output, snapshotFileBytes(snapshot, *output))
	case "generate", "check":
		flags := flag.NewFlagSet(args[0], flag.ContinueOnError)
		flags.SetOutput(io.Discard)
		snapshotPath := flags.String("snapshot", "", "normalized SeaDex snapshot")
		outputDir := flags.String("output-dir", "", "artifact directory")
		version := flags.String("pack-version", defaultPackVersion, "pack SemVer version")
		overridesPath := flags.String("overrides", "", "reviewed overrides")
		previousPath := flags.String("previous-snapshot", "", "previous normalized snapshot")
		if err := flags.Parse(args[1:]); err != nil {
			return err
		}
		if *snapshotPath == "" || *outputDir == "" {
			return fmt.Errorf("%s requires --snapshot and --output-dir", args[0])
		}
		if err := validateSemVer(*version); err != nil {
			return fmt.Errorf("invalid --pack-version: %w", err)
		}
		snapshot, err := loadSnapshot(*snapshotPath)
		if err != nil {
			return err
		}
		overrides, err := loadOverrides(*overridesPath)
		if err != nil {
			return err
		}
		var previous *Snapshot
		if *previousPath != "" {
			value, err := loadSnapshot(*previousPath)
			if err != nil {
				return err
			}
			previous = &value
		}
		artifacts, err := buildArtifacts(snapshot, overrides, previous, *version)
		if err != nil {
			return err
		}
		if args[0] == "generate" {
			return writeArtifacts(*outputDir, artifacts)
		}
		var stale []string
		for _, name := range artifactNames {
			content, err := os.ReadFile(filepath.Join(*outputDir, name))
			if err != nil || !bytes.Equal(content, artifacts[name]) {
				stale = append(stale, name)
			}
		}
		if len(stale) > 0 {
			return fmt.Errorf("%w: %s", errOutdated, strings.Join(stale, ", "))
		}
		return nil
	default:
		return fmt.Errorf("unknown command %q", args[0])
	}
}

func loadSnapshot(path string) (Snapshot, error) {
	data, err := readInput(path)
	if err != nil {
		return Snapshot{}, err
	}
	return normalizeSnapshot(data)
}

func loadOverrides(path string) (Overrides, error) {
	if path == "" {
		return Overrides{SchemaVersion: schemaVersion, Releases: map[string]Override{}}, nil
	}
	data, err := readInput(path)
	if err != nil {
		return Overrides{}, err
	}
	if err := validateOverridesWire(data); err != nil {
		return Overrides{}, err
	}
	var overrides Overrides
	if err := decodeStrict(data, &overrides); err != nil {
		return Overrides{}, fmt.Errorf("invalid overrides: %w", err)
	}
	if overrides.SchemaVersion != schemaVersion {
		return Overrides{}, errors.New("overrides.schema_version must equal 1")
	}
	if overrides.Releases == nil {
		return Overrides{}, errors.New("overrides.releases must be an object")
	}
	for releaseID, override := range overrides.Releases {
		if strings.TrimSpace(releaseID) == "" {
			return Overrides{}, errors.New("overrides release ID must be non-empty")
		}
		for _, alias := range override.Aliases {
			if strings.TrimSpace(alias) == "" {
				return Overrides{}, fmt.Errorf("override %q has an empty alias", releaseID)
			}
		}
		override.Aliases = uniqueSorted(override.Aliases)
		overrides.Releases[releaseID] = override
	}
	return overrides, nil
}

func readInput(path string) ([]byte, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if filepath.Ext(path) != ".gz" {
		return data, nil
	}
	reader, err := gzip.NewReader(bytes.NewReader(data))
	if err != nil {
		return nil, err
	}
	defer reader.Close()
	return io.ReadAll(reader)
}

func decodeStrict(data []byte, value any) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(value); err != nil {
		return err
	}
	if err := ensureEOF(decoder); err != nil {
		return err
	}
	return nil
}

func ensureEOF(decoder *json.Decoder) error {
	var extra any
	err := decoder.Decode(&extra)
	if err == io.EOF {
		return nil
	}
	if err == nil {
		return errors.New("multiple JSON values")
	}
	return err
}

func wireObject(data json.RawMessage) (map[string]json.RawMessage, error) {
	if bytes.Equal(bytes.TrimSpace(data), []byte("null")) {
		return nil, errors.New("must not be null")
	}
	var value map[string]json.RawMessage
	if err := decodeStrictMap(data, &value); err != nil || value == nil {
		if err == nil {
			err = errors.New("must be an object")
		}
		return nil, err
	}
	return value, nil
}

func wireArray(data json.RawMessage) ([]json.RawMessage, error) {
	if bytes.Equal(bytes.TrimSpace(data), []byte("null")) {
		return nil, errors.New("must not be null")
	}
	var value []json.RawMessage
	if err := decodeStrict(data, &value); err != nil || value == nil {
		if err == nil {
			err = errors.New("must be an array")
		}
		return nil, err
	}
	return value, nil
}

func requiredWire(object map[string]json.RawMessage, field string) (json.RawMessage, error) {
	value, ok := object[field]
	if !ok || bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
		return nil, fmt.Errorf("%s is required and must not be null", field)
	}
	return value, nil
}

func wireString(object map[string]json.RawMessage, field string, nullable bool) error {
	value, ok := object[field]
	if !ok {
		return fmt.Errorf("%s is required", field)
	}
	if nullable && bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
		return nil
	}
	if bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
		return fmt.Errorf("%s must not be null", field)
	}
	var result string
	if err := decodeStrict(value, &result); err != nil {
		return fmt.Errorf("%s must be a string", field)
	}
	return nil
}

func wireInt(object map[string]json.RawMessage, field string) error {
	value, err := requiredWire(object, field)
	if err != nil {
		return err
	}
	var result int64
	if err := decodeStrict(value, &result); err != nil {
		return fmt.Errorf("%s must be an integer", field)
	}
	return nil
}

func wireBool(object map[string]json.RawMessage, field string) error {
	value, err := requiredWire(object, field)
	if err != nil {
		return err
	}
	var result bool
	if err := decodeStrict(value, &result); err != nil {
		return fmt.Errorf("%s must be a boolean", field)
	}
	return nil
}

func validateRawEntriesWire(data json.RawMessage) error {
	entries, err := wireArray(data)
	if err != nil {
		return err
	}
	for index, rawEntry := range entries {
		if err := validateRawEntryWire(rawEntry); err != nil {
			return fmt.Errorf("raw entries[%d]: %w", index, err)
		}
	}
	return nil
}

func validateRawEntryWire(data json.RawMessage) error {
	entry, err := wireObject(data)
	if err != nil {
		return err
	}
	for _, field := range []string{"id", "updated", "notes"} {
		if err := wireString(entry, field, false); err != nil {
			return err
		}
	}
	if err := wireString(entry, "theoreticalBest", true); err != nil {
		return err
	}
	if err := wireInt(entry, "alID"); err != nil {
		return err
	}
	expandRaw, err := requiredWire(entry, "expand")
	if err != nil {
		return err
	}
	expand, err := wireObject(expandRaw)
	if err != nil {
		return fmt.Errorf("expand: %w", err)
	}
	releasesRaw, err := requiredWire(expand, "trs")
	if err != nil {
		return fmt.Errorf("expand.trs: %w", err)
	}
	releases, err := wireArray(releasesRaw)
	if err != nil {
		return fmt.Errorf("expand.trs: %w", err)
	}
	for index, rawRelease := range releases {
		if err := validateRawReleaseWire(rawRelease); err != nil {
			return fmt.Errorf("expand.trs[%d]: %w", index, err)
		}
	}
	listed, err := requiredWire(entry, "trs")
	if err != nil {
		return fmt.Errorf("trs: %w", err)
	}
	ids, err := wireArray(listed)
	if err != nil {
		return fmt.Errorf("trs: %w", err)
	}
	for _, id := range ids {
		var stringID string
		if err := decodeStrict(id, &stringID); err != nil {
			return errors.New("trs must contain string IDs")
		}
	}
	return nil
}

func validateRawReleaseWire(data json.RawMessage) error {
	release, err := wireObject(data)
	if err != nil {
		return err
	}
	for _, field := range []string{"id", "releaseGroup", "updated"} {
		if err := wireString(release, field, false); err != nil {
			return err
		}
	}
	if err := wireBool(release, "isBest"); err != nil {
		return err
	}
	filesRaw, err := requiredWire(release, "files")
	if err != nil {
		return err
	}
	files, err := wireArray(filesRaw)
	if err != nil {
		return err
	}
	for index, rawFile := range files {
		file, err := wireObject(rawFile)
		if err != nil {
			return fmt.Errorf("files[%d]: %w", index, err)
		}
		if err := wireString(file, "name", false); err != nil {
			return err
		}
		if err := wireInt(file, "length"); err != nil {
			return err
		}
	}
	return nil
}

func validateNormalizedWire(data []byte) error {
	top, err := wireObject(data)
	if err != nil {
		return err
	}
	if err := wireInt(top, "schema_version"); err != nil {
		return err
	}
	entriesRaw, err := requiredWire(top, "entries")
	if err != nil {
		return err
	}
	entries, err := wireArray(entriesRaw)
	if err != nil {
		return err
	}
	for entryIndex, rawEntry := range entries {
		entry, err := wireObject(rawEntry)
		if err != nil {
			return fmt.Errorf("entries[%d]: %w", entryIndex, err)
		}
		for _, field := range []string{"id", "updated_at", "notes"} {
			if err := wireString(entry, field, false); err != nil {
				return err
			}
		}
		if err := wireString(entry, "theoretical_best", true); err != nil {
			return err
		}
		if err := wireInt(entry, "anilist_id"); err != nil {
			return err
		}
		releasesRaw, err := requiredWire(entry, "releases")
		if err != nil {
			return err
		}
		releases, err := wireArray(releasesRaw)
		if err != nil {
			return err
		}
		for releaseIndex, rawRelease := range releases {
			release, err := wireObject(rawRelease)
			if err != nil {
				return fmt.Errorf("entries[%d].releases[%d]: %w", entryIndex, releaseIndex, err)
			}
			for _, field := range []string{"id", "release_group", "updated_at"} {
				if err := wireString(release, field, false); err != nil {
					return err
				}
			}
			if err := wireBool(release, "is_best"); err != nil {
				return err
			}
			filesRaw, err := requiredWire(release, "files")
			if err != nil {
				return err
			}
			files, err := wireArray(filesRaw)
			if err != nil {
				return err
			}
			for _, rawFile := range files {
				file, err := wireObject(rawFile)
				if err != nil {
					return err
				}
				if err := wireString(file, "name", false); err != nil {
					return err
				}
				if err := wireInt(file, "size"); err != nil {
					return err
				}
			}
		}
	}
	return nil
}

func validateOverridesWire(data []byte) error {
	top, err := wireObject(data)
	if err != nil {
		return err
	}
	if len(top) != 2 {
		return errors.New("overrides contains unknown or missing fields")
	}
	if err := wireInt(top, "schema_version"); err != nil {
		return err
	}
	releasesRaw, err := requiredWire(top, "releases")
	if err != nil {
		return err
	}
	releases, err := wireObject(releasesRaw)
	if err != nil {
		return err
	}
	for releaseID, rawOverride := range releases {
		if strings.TrimSpace(releaseID) == "" {
			return errors.New("overrides release ID must be non-empty")
		}
		override, err := wireObject(rawOverride)
		if err != nil {
			return err
		}
		for field, value := range override {
			switch field {
			case "exclude":
				if bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
					return errors.New("override exclude must not be null")
				}
				if err := wireBool(override, field); err != nil {
					return err
				}
			case "aliases":
				if bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
					return errors.New("override aliases must not be null")
				}
				aliases, err := wireArray(value)
				if err != nil {
					return err
				}
				for _, alias := range aliases {
					var text string
					if err := decodeStrict(alias, &text); err != nil {
						return errors.New("override aliases must be strings")
					}
				}
			default:
				return fmt.Errorf("override %q has unknown field %q", releaseID, field)
			}
		}
	}
	return nil
}

func normalizeSnapshot(data []byte) (Snapshot, error) {
	trimmed := bytes.TrimSpace(data)
	if len(trimmed) == 0 {
		return Snapshot{}, errors.New("empty snapshot")
	}
	if trimmed[0] == '[' {
		return decodeRawEntries(trimmed)
	}
	var top map[string]json.RawMessage
	if err := decodeStrictMap(trimmed, &top); err != nil {
		return Snapshot{}, fmt.Errorf("invalid snapshot: %w", err)
	}
	if _, hasSchema := top["schema_version"]; hasSchema {
		if err := validateNormalizedWire(trimmed); err != nil {
			return Snapshot{}, fmt.Errorf("invalid normalized snapshot: %w", err)
		}
		var snapshot Snapshot
		if err := decodeStrict(trimmed, &snapshot); err != nil {
			return Snapshot{}, fmt.Errorf("invalid normalized snapshot: %w", err)
		}
		if err := validateSnapshot(&snapshot); err != nil {
			return Snapshot{}, err
		}
		return snapshot, nil
	}
	items, ok := top["items"]
	if !ok {
		return Snapshot{}, errors.New("snapshot must be normalized or contain raw items")
	}
	if hasPaginationMetadata(top) {
		if err := validateSinglePageRaw(top, items); err != nil {
			return Snapshot{}, err
		}
	}
	return decodeRawEntries(items)
}

func decodeRawEntries(data json.RawMessage) (Snapshot, error) {
	if err := validateRawEntriesWire(data); err != nil {
		return Snapshot{}, fmt.Errorf("invalid raw snapshot: %w", err)
	}
	var entries []rawEntry
	if err := decodeStrict(data, &entries); err != nil {
		return Snapshot{}, fmt.Errorf("invalid raw snapshot: %w", err)
	}
	return normalizeRawEntries(entries)
}

func decodeStrictMap(data []byte, value *map[string]json.RawMessage) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	if err := decoder.Decode(value); err != nil {
		return err
	}
	return ensureEOF(decoder)
}

func hasPaginationMetadata(top map[string]json.RawMessage) bool {
	for _, key := range []string{"page", "perPage", "totalPages", "totalItems"} {
		if _, ok := top[key]; ok {
			return true
		}
	}
	return false
}

func validateSinglePageRaw(top map[string]json.RawMessage, items json.RawMessage) error {
	var metadata struct{ Page, PerPage, TotalPages, TotalItems int }
	for key, target := range map[string]*int{
		"page": &metadata.Page, "perPage": &metadata.PerPage, "totalPages": &metadata.TotalPages, "totalItems": &metadata.TotalItems,
	} {
		raw, ok := top[key]
		if !ok {
			return errors.New("a paginated raw snapshot must include complete page metadata")
		}
		if err := wireInt(top, key); err != nil {
			return errors.New("a paginated raw snapshot has invalid metadata")
		}
		if err := json.Unmarshal(raw, target); err != nil {
			return errors.New("a paginated raw snapshot has invalid metadata")
		}
	}
	rawItems, err := wireArray(items)
	if err != nil {
		return errors.New("raw items must be an array")
	}
	if metadata.Page != 1 || metadata.TotalPages != 1 || metadata.TotalItems != len(rawItems) || metadata.PerPage < len(rawItems) {
		return errors.New("a paginated SeaDex response is not a complete snapshot; use fetch")
	}
	return nil
}

func normalizeRawEntries(rawEntries []rawEntry) (Snapshot, error) {
	entries := make([]Entry, 0, len(rawEntries))
	for index, raw := range rawEntries {
		entry, err := normalizeRawEntry(raw)
		if err != nil {
			return Snapshot{}, fmt.Errorf("raw entries[%d]: %w", index, err)
		}
		entries = append(entries, entry)
	}
	snapshot := Snapshot{SchemaVersion: schemaVersion, Entries: entries}
	if err := validateSnapshot(&snapshot); err != nil {
		return Snapshot{}, err
	}
	return snapshot, nil
}

func normalizeRawEntry(raw rawEntry) (Entry, error) {
	entry := Entry{ID: raw.ID, AniListID: raw.AniListID, UpdatedAt: raw.UpdatedAt, Notes: raw.Notes, TheoreticalBest: raw.TheoreticalBest, Releases: make([]Release, 0, len(raw.Expand.TRs))}
	if entry.TheoreticalBest != nil && *entry.TheoreticalBest == "" {
		entry.TheoreticalBest = nil
	}
	for _, rawRelease := range raw.Expand.TRs {
		release := Release{ID: rawRelease.ID, Group: rawRelease.Group, Best: rawRelease.Best, UpdatedAt: rawRelease.UpdatedAt, Files: make([]File, 0, len(rawRelease.Files))}
		for _, rawFile := range rawRelease.Files {
			release.Files = append(release.Files, File{Name: rawFile.Name, Size: rawFile.Size})
		}
		entry.Releases = append(entry.Releases, release)
	}
	if len(raw.TRs) != 0 {
		var listed []string
		if err := json.Unmarshal(raw.TRs, &listed); err != nil {
			return Entry{}, errors.New("trs must be an array of release IDs")
		}
		expanded := make([]string, 0, len(entry.Releases))
		for _, release := range entry.Releases {
			expanded = append(expanded, release.ID)
		}
		if !sameUniqueStrings(listed, expanded) {
			return Entry{}, errors.New("trs must exactly identify expanded records")
		}
	}
	return entry, nil
}

func validateSnapshot(snapshot *Snapshot) error {
	if snapshot.SchemaVersion != schemaVersion {
		return errors.New("snapshot.schema_version must equal 1")
	}
	if len(snapshot.Entries) == 0 {
		return errors.New("snapshot.entries must not be empty")
	}
	seenEntries := map[string]bool{}
	for entryIndex := range snapshot.Entries {
		entry := &snapshot.Entries[entryIndex]
		if strings.TrimSpace(entry.ID) == "" || entry.AniListID <= 0 || strings.TrimSpace(entry.UpdatedAt) == "" {
			return fmt.Errorf("entry %d has invalid required fields", entryIndex)
		}
		if seenEntries[entry.ID] {
			return fmt.Errorf("duplicate entry ID %q", entry.ID)
		}
		seenEntries[entry.ID] = true
		if entry.TheoreticalBest != nil && *entry.TheoreticalBest == "" {
			entry.TheoreticalBest = nil
		}
		seenReleases := map[string]bool{}
		for releaseIndex := range entry.Releases {
			release := &entry.Releases[releaseIndex]
			if strings.TrimSpace(release.ID) == "" || strings.TrimSpace(release.Group) == "" || strings.TrimSpace(release.UpdatedAt) == "" {
				return fmt.Errorf("entry %q has an invalid release", entry.ID)
			}
			if seenReleases[release.ID] {
				return fmt.Errorf("entry %q has duplicate release ID %q", entry.ID, release.ID)
			}
			seenReleases[release.ID] = true
			for _, file := range release.Files {
				if strings.TrimSpace(file.Name) == "" || file.Size < 0 {
					return fmt.Errorf("release %q has an invalid file", release.ID)
				}
			}
			sort.Slice(release.Files, func(i, j int) bool {
				if release.Files[i].Name == release.Files[j].Name {
					return release.Files[i].Size < release.Files[j].Size
				}
				return release.Files[i].Name < release.Files[j].Name
			})
		}
		sort.Slice(entry.Releases, func(i, j int) bool { return entry.Releases[i].ID < entry.Releases[j].ID })
	}
	sort.Slice(snapshot.Entries, func(i, j int) bool { return snapshot.Entries[i].ID < snapshot.Entries[j].ID })
	return nil
}

func decodePageResponse(data []byte) (pageResponse, error) {
	top, err := wireObject(data)
	if err != nil {
		return pageResponse{}, err
	}
	for _, field := range []string{"page", "perPage", "totalPages", "totalItems"} {
		if err := wireInt(top, field); err != nil {
			return pageResponse{}, err
		}
	}
	items, err := requiredWire(top, "items")
	if err != nil {
		return pageResponse{}, err
	}
	if err := validateRawEntriesWire(items); err != nil {
		return pageResponse{}, err
	}
	var response pageResponse
	if err := json.Unmarshal(data, &response); err != nil {
		return pageResponse{}, err
	}
	return response, nil
}

func fetchRecords(apiURL string, timeout time.Duration, perPage int, client *http.Client) (Snapshot, error) {
	if perPage < 1 {
		return Snapshot{}, errors.New("per-page must be positive")
	}
	if timeout <= 0 {
		return Snapshot{}, errors.New("timeout must be positive")
	}
	if client == nil {
		client = http.DefaultClient
	}
	requestPage := func(page int) (pageResponse, error) {
		parsed, err := url.Parse(apiURL)
		if err != nil {
			return pageResponse{}, err
		}
		query := parsed.Query()
		query.Set("perPage", fmt.Sprint(perPage))
		query.Set("expand", "trs")
		query.Set("sort", "id")
		query.Set("page", fmt.Sprint(page))
		parsed.RawQuery = query.Encode()
		req, err := http.NewRequest(http.MethodGet, parsed.String(), nil)
		if err != nil {
			return pageResponse{}, err
		}
		req.Header.Set("Accept", "application/json")
		req.Header.Set("User-Agent", "Scryer-SeaDex-Converter/1.0")
		activeClient := *client
		activeClient.Timeout = timeout
		response, err := activeClient.Do(req)
		if err != nil {
			return pageResponse{}, err
		}
		defer response.Body.Close()
		if response.StatusCode < 200 || response.StatusCode >= 300 {
			return pageResponse{}, fmt.Errorf("SeaDex returned HTTP %d", response.StatusCode)
		}
		data, err := io.ReadAll(response.Body)
		if err != nil {
			return pageResponse{}, err
		}
		return decodePageResponse(data)
	}
	first, err := requestPage(1)
	if err != nil {
		return Snapshot{}, err
	}
	if first.Page != 1 || first.PerPage != perPage || first.TotalPages < 1 || first.TotalItems < 1 {
		return Snapshot{}, errors.New("invalid first pagination response")
	}
	pages := []pageResponse{first}
	for page := 2; page <= first.TotalPages; page++ {
		result, err := requestPage(page)
		if err != nil {
			return Snapshot{}, err
		}
		if result.Page != page || result.PerPage != perPage || result.TotalPages != first.TotalPages || result.TotalItems != first.TotalItems {
			return Snapshot{}, fmt.Errorf("pagination metadata changed on page %d", page)
		}
		pages = append(pages, result)
	}
	var rawEntries []rawEntry
	for pageNumber, page := range pages {
		if len(page.Items) == 0 || (pageNumber < len(pages)-1 && len(page.Items) != perPage) || len(page.Items) > perPage {
			return Snapshot{}, fmt.Errorf("page %d is empty or truncated", pageNumber+1)
		}
		rawEntries = append(rawEntries, page.Items...)
	}
	if len(rawEntries) != first.TotalItems {
		return Snapshot{}, fmt.Errorf("fetched %d entries but SeaDex reported %d", len(rawEntries), first.TotalItems)
	}
	return normalizeRawEntries(rawEntries)
}

func buildArtifacts(snapshot Snapshot, overrides Overrides, previous *Snapshot, version string) (map[string][]byte, error) {
	if err := validateSnapshot(&snapshot); err != nil {
		return nil, err
	}
	if err := validateSemVer(version); err != nil {
		return nil, fmt.Errorf("invalid pack version: %w", err)
	}
	tables, report, err := compileSnapshot(snapshot, overrides)
	if err != nil {
		return nil, err
	}
	policy, err := renderPolicy(tables)
	if err != nil {
		return nil, err
	}
	reportBytes, err := json.Marshal(report)
	if err != nil {
		return nil, err
	}
	var reportMap map[string]json.RawMessage
	if err := json.Unmarshal(reportBytes, &reportMap); err != nil {
		return nil, err
	}
	packBytes, err := json.MarshalIndent(pack{SchemaVersion: schemaVersion, ID: packID, Name: "SeaDex Scoring Pack", Description: "Prefer exact SeaDex-recommended anime releases.", Author: "community", Version: version, Customizable: false, Rules: []packRule{{ID: ruleID, Title: "Prefer SeaDex recommendations", Description: "Boost listed SeaDex releases, with higher priority for best releases.", Category: "Anime", AppliedFacets: []string{"anime"}, RegoSource: policy}}}, "", "  ")
	if err != nil {
		return nil, err
	}
	matching := map[string]json.RawMessage{}
	for key, value := range reportMap {
		if key != "included" && key != "skipped" && key != "ambiguous" {
			matching[key] = value
		}
	}
	encoderStats, err := encodingStats(tables)
	if err != nil {
		return nil, err
	}
	encoderStatsBytes, err := json.Marshal(encoderStats)
	if err != nil {
		return nil, err
	}
	matching["encoding"] = encoderStatsBytes
	coverageBytes, err := json.MarshalIndent(coverage{SchemaVersion: schemaVersion, PackID: packID, PackVersion: version, SnapshotSHA256: snapshotChecksum(snapshot), ConverterRevision: sourceRevision(), Included: arrayOrEmpty(reportMap["included"]), Skipped: arrayOrEmpty(reportMap["skipped"]), Ambiguous: arrayOrEmpty(reportMap["ambiguous"]), StaleOverrides: staleOverrides(snapshot, overrides), Changes: snapshotChanges(snapshot, previous), Matching: matching}, "", "  ")
	if err != nil {
		return nil, err
	}
	return map[string][]byte{"seadex-scoring.json": append(packBytes, '\n'), "seadex-scoring.rego": []byte(strings.TrimRight(policy, "\n") + "\n"), "seadex-coverage.json": append(coverageBytes, '\n')}, nil
}

func validateSemVer(version string) error {
	coreAndPre, build, hasBuild := strings.Cut(version, "+")
	if hasBuild && !validBuildMetadata(build) {
		return errors.New("build metadata must contain dot-separated ASCII alphanumeric or hyphen identifiers")
	}
	core, prerelease, hasPrerelease := strings.Cut(coreAndPre, "-")
	if hasPrerelease && !validPrerelease(prerelease) {
		return errors.New("prerelease must contain dot-separated ASCII alphanumeric or hyphen identifiers")
	}
	parts := strings.Split(core, ".")
	if len(parts) != 3 {
		return errors.New("must have major.minor.patch")
	}
	for _, part := range parts {
		if !validNumericIdentifier(part) {
			return errors.New("major, minor, and patch must be non-negative integers without leading zeroes")
		}
	}
	return nil
}

func validPrerelease(value string) bool {
	if value == "" {
		return false
	}
	for _, identifier := range strings.Split(value, ".") {
		if !validVersionIdentifier(identifier) {
			return false
		}
		if numericVersionIdentifier(identifier) && !validUnboundedNumericIdentifier(identifier) {
			return false
		}
	}
	return true
}

func validBuildMetadata(value string) bool {
	if value == "" {
		return false
	}
	for _, identifier := range strings.Split(value, ".") {
		if !validVersionIdentifier(identifier) {
			return false
		}
	}
	return true
}

func validNumericIdentifier(value string) bool {
	if !validUnboundedNumericIdentifier(value) {
		return false
	}
	_, err := strconv.ParseUint(value, 10, 64)
	return err == nil
}

func validUnboundedNumericIdentifier(value string) bool {
	return value != "" && (len(value) == 1 || value[0] != '0') && numericVersionIdentifier(value)
}

func numericVersionIdentifier(value string) bool {
	for _, character := range value {
		if character < '0' || character > '9' {
			return false
		}
	}
	return value != ""
}

func validVersionIdentifier(value string) bool {
	if value == "" {
		return false
	}
	for _, character := range value {
		if !(character >= '0' && character <= '9' || character >= 'A' && character <= 'Z' || character >= 'a' && character <= 'z' || character == '-') {
			return false
		}
	}
	return true
}

func arrayOrEmpty(value json.RawMessage) json.RawMessage {
	if len(value) == 0 {
		return json.RawMessage("[]")
	}
	return value
}

func snapshotChecksum(snapshot Snapshot) string {
	data, _ := json.Marshal(snapshot)
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

func sourceRevision() string {
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
	return hex.EncodeToString(digest.Sum(nil))
}

func snapshotChanges(snapshot Snapshot, previous *Snapshot) map[string]map[string][]string {
	oldEntries, oldReleases := map[string]bool{}, map[string]bool{}
	if previous != nil {
		for _, entry := range previous.Entries {
			oldEntries[entry.ID] = true
			for _, release := range entry.Releases {
				oldReleases[release.ID] = true
			}
		}
	}
	newEntries, newReleases := map[string]bool{}, map[string]bool{}
	for _, entry := range snapshot.Entries {
		newEntries[entry.ID] = true
		for _, release := range entry.Releases {
			newReleases[release.ID] = true
		}
	}
	return map[string]map[string][]string{"entries": {"added": setDifference(newEntries, oldEntries), "removed": setDifference(oldEntries, newEntries)}, "releases": {"added": setDifference(newReleases, oldReleases), "removed": setDifference(oldReleases, newReleases)}}
}

func staleOverrides(snapshot Snapshot, overrides Overrides) []string {
	releases := map[string]bool{}
	for _, entry := range snapshot.Entries {
		for _, release := range entry.Releases {
			releases[release.ID] = true
		}
	}
	stale := []string{}
	for id := range overrides.Releases {
		if !releases[id] {
			stale = append(stale, id)
		}
	}
	sort.Strings(stale)
	return stale
}
func setDifference(left, right map[string]bool) []string {
	values := []string{}
	for value := range left {
		if !right[value] {
			values = append(values, value)
		}
	}
	sort.Strings(values)
	return values
}
func uniqueSorted(values []string) []string {
	set := map[string]bool{}
	for _, value := range values {
		set[value] = true
	}
	return setDifference(set, map[string]bool{})
}
func sameUniqueStrings(left, right []string) bool {
	if len(left) != len(right) {
		return false
	}
	leftSet, rightSet := map[string]bool{}, map[string]bool{}
	for _, value := range left {
		if value == "" || leftSet[value] {
			return false
		}
		leftSet[value] = true
	}
	for _, value := range right {
		if value == "" || rightSet[value] {
			return false
		}
		rightSet[value] = true
	}
	if len(leftSet) != len(rightSet) {
		return false
	}
	for value := range leftSet {
		if !rightSet[value] {
			return false
		}
	}
	return true
}

func snapshotFileBytes(snapshot Snapshot, path string) []byte {
	data, _ := json.MarshalIndent(snapshot, "", "  ")
	data = append(data, '\n')
	if filepath.Ext(path) != ".gz" {
		return data
	}
	var buffer bytes.Buffer
	writer, _ := gzip.NewWriterLevel(&buffer, gzip.BestCompression)
	writer.Name = ""
	writer.ModTime = time.Unix(0, 0)
	writer.OS = 255
	_, _ = writer.Write(data)
	_ = writer.Close()
	return buffer.Bytes()
}

func atomicWrite(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	temporary, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".*")
	if err != nil {
		return err
	}
	temporaryName := temporary.Name()
	defer os.Remove(temporaryName)
	if _, err := temporary.Write(data); err != nil {
		temporary.Close()
		return err
	}
	if err := temporary.Sync(); err != nil {
		temporary.Close()
		return err
	}
	if err := temporary.Close(); err != nil {
		return err
	}
	return os.Rename(temporaryName, path)
}
func writeArtifacts(directory string, artifacts map[string][]byte) error {
	for _, name := range artifactNames {
		data, ok := artifacts[name]
		if !ok {
			return fmt.Errorf("missing generated artifact %s", name)
		}
		if err := atomicWrite(filepath.Join(directory, name), data); err != nil {
			return err
		}
	}
	return nil
}
