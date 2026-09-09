// Command trash converts the reviewed TRaSH Guides snapshot into one rule pack.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
)

const (
	packID         = "trash-guides-scoring-pack"
	defaultVersion = "1.0.0"
	minHostVersion = "0.20.0"
)

var errOutdated = errors.New("outdated artifacts")

var (
	safeLanguageCode  = regexp.MustCompile(`^trash\.lang\.[a-z0-9_]+$`)
	safeLanguageStem  = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)
	safeLanguageNamed = regexp.MustCompile(`^[a-z]{3}$`)
)

type snapshot struct {
	SchemaVersion  int             `json:"schema_version"`
	SourceRevision string          `json:"source_revision"`
	GroupRules     json.RawMessage `json:"group_rules"`
	LanguageRules  json.RawMessage `json:"language_rules,omitempty"`
	FactScores     json.RawMessage `json:"fact_scores,omitempty"`
	ScoreEnvelopes json.RawMessage `json:"score_set_envelopes,omitempty"`
}

type rule struct {
	ID              string   `json:"id"`
	Title           string   `json:"title"`
	Description     string   `json:"description"`
	Category        string   `json:"category"`
	AppliedFacets   []string `json:"appliedFacets,omitempty"`
	RegoSource      string   `json:"regoSource"`
	EvaluationPhase string   `json:"evaluationPhase"`
	DefaultEnabled  bool     `json:"defaultEnabled"`
	ExclusiveGroup  string   `json:"exclusiveGroup,omitempty"`
	Customizable    bool     `json:"customizable"`
}

type pack struct {
	SchemaVersion    int    `json:"schema_version"`
	ID               string `json:"id"`
	Name             string `json:"name"`
	Description      string `json:"description"`
	Author           string `json:"author"`
	Version          string `json:"version"`
	MinScryerVersion string `json:"min_scryer_version"`
	Rules            []rule `json:"rules"`
}

type coverage struct {
	SchemaVersion        int            `json:"schema_version"`
	PackID               string         `json:"pack_id"`
	PackVersion          string         `json:"pack_version"`
	SourceRevision       string         `json:"source_revision"`
	SnapshotSHA256       string         `json:"snapshot_sha256"`
	MembershipProvenance string         `json:"membership_provenance"`
	GroupRuleCount       int            `json:"group_rule_count"`
	Changes              changeCoverage `json:"changes"`
	FactScoreChanges     scoreCoverage  `json:"fact_score_changes"`
}
type changeCoverage struct{ AddedGroups, RemovedGroups int }
type scoreCoverage struct {
	Added, Removed, Changed []string `json:"omitempty"`
}

func main() {
	if len(os.Args) < 2 {
		fatal("usage: trash fetch|compact|distill|generate|check")
	}
	var err error
	switch os.Args[1] {
	case "fetch":
		err = fetch(os.Args[2:])
	case "compact":
		err = compact(os.Args[2:])
	case "canonicalize-raw":
		err = canonicalizeRaw(os.Args[2:])
	case "distill":
		err = distill(os.Args[2:])
	case "generate", "check":
		err = build(os.Args[1], os.Args[2:])
	default:
		err = fmt.Errorf("unknown command %q", os.Args[1])
	}
	if err != nil {
		fatal(err.Error())
	}
}

// compact removes fixtures and diagnostic payloads from an exported core
// snapshot, retaining only the group rows and their pinned provenance.
func compact(args []string) error {
	fs := flag.NewFlagSet("compact", flag.ContinueOnError)
	input := fs.String("input", "", "exported core snapshot")
	output := fs.String("output", "snapshot/core-snapshot.json", "normalized snapshot")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *input == "" {
		return errors.New("compact requires --input")
	}
	data, err := os.ReadFile(*input)
	if err != nil {
		return err
	}
	snap, err := validateSnapshot(data)
	if err != nil {
		return err
	}
	normalized, err := json.MarshalIndent(snap, "", "  ")
	if err != nil {
		return err
	}
	return atomicWrite(*output, append(normalized, '\n'))
}
func fatal(message string) { fmt.Fprintln(os.Stderr, message); os.Exit(1) }

// fetch obtains a reviewed snapshot from an explicit URL or file. Validation
// completes before a same-directory rename, so a bad refresh never damages the
// checked-in snapshot.
func fetch(args []string) error {
	fs := flag.NewFlagSet("fetch", flag.ContinueOnError)
	source := fs.String("source", "", "https URL or local snapshot path")
	output := fs.String("output", "snapshot/core-snapshot.json", "snapshot output")
	revision := fs.String("revision", "", "TRaSH Git revision or branch for a raw upstream snapshot")
	apiBase := fs.String("github-api-base", defaultGitHubAPIBase, "GitHub API base (test hook)")
	rawBase := fs.String("github-raw-base", defaultGitHubRawBase, "GitHub raw-content base (test hook)")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *revision != "" {
		if *source != "" {
			return errors.New("fetch accepts either --revision or --source, not both")
		}
		data, err := fetchRawSnapshot(*revision, *apiBase, *rawBase)
		if err != nil {
			return err
		}
		return atomicWrite(*output, data)
	}
	if *source == "" {
		return errors.New("fetch requires --source for a reviewed snapshot or --revision for a raw upstream snapshot")
	}
	data, err := readSource(*source)
	if err != nil {
		return err
	}
	if _, err := validateSnapshot(data); err != nil {
		return fmt.Errorf("refusing invalid snapshot: %w", err)
	}
	return atomicWrite(*output, data)
}
func readSource(source string) ([]byte, error) {
	if strings.HasPrefix(source, "https://") {
		response, err := http.Get(source) // #nosec G107 -- explicit operator supplied immutable URL.
		if err != nil {
			return nil, err
		}
		defer response.Body.Close()
		if response.StatusCode != http.StatusOK {
			return nil, fmt.Errorf("snapshot fetch: %s", response.Status)
		}
		return io.ReadAll(io.LimitReader(response.Body, 128<<20))
	}
	return os.ReadFile(source)
}
func validateSnapshot(data []byte) (snapshot, error) {
	var value snapshot
	if err := json.Unmarshal(data, &value); err != nil {
		return value, err
	}
	if value.SchemaVersion != 1 || len(value.SourceRevision) != 40 || len(value.GroupRules) == 0 {
		return value, errors.New("expected schema-v1 compiled TRaSH snapshot with source revision and group rules")
	}
	var groups []json.RawMessage
	if err := json.Unmarshal(value.GroupRules, &groups); err != nil || len(groups) == 0 {
		return value, errors.New("group_rules is missing or empty")
	}
	if err := validateLanguageRows(value.LanguageRules); err != nil {
		return value, err
	}
	return value, nil
}

func validateLanguageRows(data json.RawMessage) error {
	if len(data) == 0 {
		return nil
	}
	var rows []languageRow
	if err := json.Unmarshal(data, &rows); err != nil {
		return fmt.Errorf("invalid language_rules: %w", err)
	}
	for _, row := range rows {
		if !safeLanguageCode.MatchString(row.Code) || !safeLanguageStem.MatchString(row.Stem) {
			return fmt.Errorf("invalid language rule metadata")
		}
		if row.App != "radarr" && row.App != "sonarr" && row.App != "guide-only" {
			return fmt.Errorf("invalid language rule app")
		}
		if len(row.Conditions) == 0 {
			return fmt.Errorf("language rule %s has no conditions", row.Code)
		}
		for _, condition := range row.Conditions {
			if !validLanguageValue(condition.Language) {
				return fmt.Errorf("invalid language condition for %s", row.Code)
			}
		}
	}
	return nil
}

func validLanguageValue(value interface{}) bool {
	if original, ok := value.(string); ok {
		return original == "original"
	}
	named, ok := value.(map[string]interface{})
	if !ok || len(named) != 1 {
		return false
	}
	code, ok := named["named"].(string)
	return ok && safeLanguageNamed.MatchString(code)
}

func build(command string, args []string) error {
	fs := flag.NewFlagSet(command, flag.ContinueOnError)
	snapshotPath := fs.String("snapshot", "snapshot/core-snapshot.json", "compiled TRaSH snapshot")
	previous := fs.String("previous-snapshot", "", "previous compiled snapshot")
	output := fs.String("output-dir", "..", "rule_packs directory")
	version := fs.String("pack-version", defaultVersion, "pack SemVer")
	if err := fs.Parse(args); err != nil {
		return err
	}
	data, err := os.ReadFile(*snapshotPath)
	if err != nil {
		return err
	}
	snap, err := validateSnapshot(data)
	if err != nil {
		return fmt.Errorf("invalid snapshot: %w", err)
	}
	detectionPath := filepath.Join(filepath.Dir(*snapshotPath), "detection-snapshot.json")
	detectionBytes, err := os.ReadFile(detectionPath)
	if err != nil {
		return fmt.Errorf("read detection snapshot: %w", err)
	}
	detection, err := parseDetectionSnapshot(detectionBytes)
	if err != nil {
		return err
	}
	if detection.SourceRevision != snap.SourceRevision {
		return fmt.Errorf("core and detection snapshot revisions differ: %s != %s", snap.SourceRevision, detection.SourceRevision)
	}
	artifacts, err := generate(snap, detection, data, *previous, *version)
	if err != nil {
		return err
	}
	if command == "check" {
		return check(*output, artifacts)
	}
	for name, body := range artifacts {
		if err := atomicWrite(filepath.Join(*output, name), body); err != nil {
			return err
		}
	}
	return nil
}

func generate(snap snapshot, detection detectionSnapshot, snapshotBytes []byte, previous, version string) (map[string][]byte, error) {
	if !validVersion(version) {
		return nil, fmt.Errorf("invalid pack version %q", version)
	}
	rules, err := generatedRules(snap, detection)
	if err != nil {
		return nil, err
	}
	for index := range rules {
		rules[index].RegoSource = strings.TrimRight(rules[index].RegoSource, "\n")
	}
	payload, err := json.MarshalIndent(pack{1, packID, "TRaSH Guides Scoring Pack", "Reviewed TRaSH Guides scoring, translated from a pinned compiled snapshot.", "scryer-media", version, minHostVersion, rules}, "", "  ")
	if err != nil {
		return nil, err
	}
	added, removed := 0, 0
	var scoreChanges scoreCoverage
	if previous != "" {
		added, removed, err = groupChanges(previous, snap)
		if err != nil {
			return nil, err
		}
		scoreChanges, err = factScoreChanges(previous, snap)
		if err != nil {
			return nil, err
		}
	}
	digest := sha256.Sum256(snapshotBytes)
	report, err := json.MarshalIndent(coverage{1, packID, version, snap.SourceRevision, hex.EncodeToString(digest[:]), "Compiled upstream rows retain source order, matcher kind, facet, source context, and curated membership rationale from the saved snapshot.", countGroups(snap), changeCoverage{added, removed}, scoreChanges}, "", "  ")
	if err != nil {
		return nil, err
	}
	artifacts := map[string][]byte{"trash-scoring.json": append(payload, '\n'), "trash-scoring-coverage.json": append(report, '\n')}
	for _, item := range rules {
		name := strings.TrimPrefix(item.ID, "trash-guides-")
		artifacts[filepath.Join("trash", "generated", name+".rego")] = append([]byte(strings.TrimRight(item.RegoSource, "\n")), '\n')
	}
	return artifacts, nil
}
func generatedRules(snap snapshot, detection detectionSnapshot) ([]rule, error) {
	detect, err := renderDetectionScope(detection, "")
	if err != nil {
		return nil, err
	}
	group, err := renderGroups(snap)
	if err != nil {
		return nil, err
	}
	rules := []rule{
		{ID: "trash-guides-groups", Title: "TRaSH Guides group reputation", Description: "Source-ordered TRaSH release-group reputation.", Category: "TRaSH Guides", RegoSource: moduleSource("groups", group), EvaluationPhase: "baseline", DefaultEnabled: true, Customizable: true},
		{ID: "trash-guides-streaming", Title: "TRaSH Guides streaming services", Description: "TRaSH streaming-service scoring using the parser-selected service.", Category: "TRaSH Guides", RegoSource: moduleSource("streaming", renderStreaming()), EvaluationPhase: "baseline", DefaultEnabled: true, Customizable: true},
		{ID: "trash-guides-unwanted", Title: "TRaSH Guides unwanted releases", Description: "TRaSH token detection and unwanted-release scoring.", Category: "TRaSH Guides", RegoSource: moduleSource("unwanted", detect+renderUnwanted()), EvaluationPhase: "baseline", DefaultEnabled: true, Customizable: true},
		{ID: "trash-guides-editions-anime", Title: "TRaSH Guides editions and anime", Description: "Edition and anime scoring from parsed release fields.", Category: "TRaSH Guides", RegoSource: moduleSource("editions_anime", renderEditionAnime()), EvaluationPhase: "baseline", DefaultEnabled: true, Customizable: true},
	}
	names := []string{"french-vf", "french-vo", "french-vostfr", "german", "asian"}
	for _, name := range names {
		locale := name
		if strings.HasPrefix(name, "french-") {
			locale = "french"
		}
		localeDetect, err := renderDetectionScope(detection, locale)
		if err != nil {
			return nil, err
		}
		body, err := os.ReadFile(filepath.Join("sources", name+"-policy.rego"))
		if err != nil {
			return nil, err
		}
		if strings.Contains(string(body), "input.release.guide_facts") {
			body = []byte(strings.Replace(string(body), "has_fact(value) if {\n    some fact in input.release.guide_facts\n    lower(fact) == value\n}", "has_fact(value) if {\n    has_detected_fact(value)\n}", 1))
		}
		if strings.Contains(string(body), "input.release.guide_facts") {
			return nil, fmt.Errorf("%s still depends on retired release.guide_facts", name)
		}
		policyBody, err := renderLocalePolicy(string(body), snap, name)
		if err != nil {
			return nil, err
		}
		item := rule{ID: "trash-guides-" + name, Title: "TRaSH Guides " + strings.ReplaceAll(name, "-", " "), Description: "Locale policy generated from the pinned TRaSH snapshot.", Category: "TRaSH Guides", RegoSource: moduleSource(strings.ReplaceAll(name, "-", "_"), localeDetect+policyBody), EvaluationPhase: "additional", DefaultEnabled: false, Customizable: true}
		if strings.HasPrefix(name, "french-") {
			item.ExclusiveGroup = "trash-guides-french-locale"
		}
		rules = append(rules, item)
	}
	files, err := filepath.Glob("templates/native-*.rego")
	if err != nil {
		return nil, err
	}
	sort.Strings(files)
	for _, file := range files {
		body, err := os.ReadFile(file)
		if err != nil {
			return nil, err
		}
		name := strings.TrimSuffix(strings.TrimPrefix(filepath.Base(file), "native-"), ".rego")
		rules = append(rules, rule{ID: "trash-guides-" + name, Title: "TRaSH Guides " + strings.ReplaceAll(name, "-", " "), Description: "Native replacement contribution.", Category: "TRaSH Guides", RegoSource: string(body), EvaluationPhase: "baseline", DefaultEnabled: true, Customizable: true})
	}
	return rules, nil
}

func moduleSource(name, source string) string {
	var body []string
	for _, line := range strings.Split(source, "\n") {
		trimmed := strings.TrimSpace(line)
		if strings.HasPrefix(trimmed, "package ") || trimmed == "import rego.v1" {
			continue
		}
		body = append(body, line)
	}
	return "package scryer.user.trash_guides_" + name + "\nimport rego.v1\n\n" + strings.Join(body, "\n")
}

func personaSource(body []byte) []byte {
	text := string(body)
	for _, key := range []string{"group_gold", "group_silver", "group_bronze", "group_banned", "group_unknown_penalty", "scene_penalty", "obfuscated_penalty", "retagged_penalty", "proper_bonus", "repack_bonus", "upscaled_penalty", "hardcoded_subs_penalty", "anime_dubs_only_penalty", "streaming_tier1", "streaming_tier2", "streaming_anime", "streaming_tier3"} {
		text = strings.ReplaceAll(text, "input.profile.scoring_weights."+key, "trash_weight(\""+key+"\")")
	}
	return []byte(text + "\n# Persona-owned scoring; optional boolean overrides disable a contribution when false.\ntrash_persona := lower(object.get(input.profile, \"scoring_persona\", \"balanced\"))\ntrash_weight(name) := value if { values := {\"balanced\": {\"group_gold\": 180, \"group_silver\": 90, \"group_bronze\": 30, \"group_banned\": -10000, \"group_unknown_penalty\": -10, \"scene_penalty\": -15, \"obfuscated_penalty\": -20, \"retagged_penalty\": -20, \"proper_bonus\": 5, \"repack_bonus\": 10, \"upscaled_penalty\": -100, \"hardcoded_subs_penalty\": -50, \"anime_dubs_only_penalty\": -50, \"streaming_tier1\": 30, \"streaming_tier2\": 20, \"streaming_anime\": 20, \"streaming_tier3\": 10}, \"audiophile\": {\"group_gold\": 240, \"group_silver\": 120, \"group_bronze\": 40, \"group_banned\": -10000, \"group_unknown_penalty\": -20, \"scene_penalty\": -25, \"obfuscated_penalty\": -30, \"retagged_penalty\": -30, \"proper_bonus\": 5, \"repack_bonus\": 15, \"upscaled_penalty\": -150, \"hardcoded_subs_penalty\": -75, \"anime_dubs_only_penalty\": -75, \"streaming_tier1\": 40, \"streaming_tier2\": 30, \"streaming_anime\": 30, \"streaming_tier3\": 15}, \"efficient\": {\"group_gold\": 100, \"group_silver\": 50, \"group_bronze\": 15, \"group_banned\": -10000, \"group_unknown_penalty\": 0, \"scene_penalty\": 0, \"obfuscated_penalty\": -10, \"retagged_penalty\": -10, \"proper_bonus\": 10, \"repack_bonus\": 15, \"upscaled_penalty\": -50, \"hardcoded_subs_penalty\": -25, \"anime_dubs_only_penalty\": -25, \"streaming_tier1\": 15, \"streaming_tier2\": 10, \"streaming_anime\": 10, \"streaming_tier3\": 5}, \"compatible\": {\"group_gold\": 120, \"group_silver\": 60, \"group_bronze\": 20, \"group_banned\": -10000, \"group_unknown_penalty\": 0, \"scene_penalty\": 0, \"obfuscated_penalty\": 0, \"retagged_penalty\": 0, \"proper_bonus\": 0, \"repack_bonus\": 0, \"upscaled_penalty\": 0, \"hardcoded_subs_penalty\": 0, \"anime_dubs_only_penalty\": 0, \"streaming_tier1\": 0, \"streaming_tier2\": 0, \"streaming_anime\": 0, \"streaming_tier3\": 0}}; value := object.get(object.get(values, trash_persona, values[\"balanced\"]), name, 0); object.get(object.get(input.profile, \"scoring_overrides\", {}), name, true) }\n")
}
func groupChanges(path string, current snapshot) (int, int, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return 0, 0, err
	}
	old, err := validateSnapshot(data)
	if err != nil {
		return 0, 0, err
	}
	return groupChangesFromSnapshots(current, old)
}
func groupChangesFromSnapshots(current, old snapshot) (int, int, error) {
	if _, err := validateGroupRows(current); err != nil {
		return 0, 0, err
	}
	if _, err := validateGroupRows(old); err != nil {
		return 0, 0, err
	}
	return countGroups(current) - countCommon(old, current), countGroups(old) - countCommon(old, current), nil
}
func validateGroupRows(value snapshot) ([]groupRule, error) {
	var rows []groupRule
	if err := json.Unmarshal(value.GroupRules, &rows); err != nil {
		return nil, err
	}
	for i, row := range rows {
		if row.Index != i || row.Matcher == "" || row.MatchKind == "" || row.Tier == "" || row.Facet == "" || row.SourceContext == "" {
			return nil, fmt.Errorf("invalid group_rules[%d]", i)
		}
	}
	return rows, nil
}
func countGroups(value snapshot) int {
	var rows []json.RawMessage
	_ = json.Unmarshal(value.GroupRules, &rows)
	return len(rows)
}
func countCommon(a, b snapshot) int {
	var left, right []groupRule
	_ = json.Unmarshal(a.GroupRules, &left)
	_ = json.Unmarshal(b.GroupRules, &right)
	set := map[string]bool{}
	for _, row := range left {
		set[row.Matcher+"\x00"+row.MatchKind+"\x00"+row.Tier+"\x00"+row.Facet+"\x00"+row.SourceContext] = true
	}
	total := 0
	for _, row := range right {
		if set[row.Matcher+"\x00"+row.MatchKind+"\x00"+row.Tier+"\x00"+row.Facet+"\x00"+row.SourceContext] {
			total++
		}
	}
	return total
}
func factScoreChanges(previous string, current snapshot) (scoreCoverage, error) {
	data, err := os.ReadFile(previous)
	if err != nil {
		return scoreCoverage{}, err
	}
	old, err := validateSnapshot(data)
	if err != nil {
		return scoreCoverage{}, err
	}
	decode := func(raw json.RawMessage) (map[string]int64, error) {
		var rows []factScore
		if len(raw) == 0 {
			return map[string]int64{}, nil
		}
		if err := json.Unmarshal(raw, &rows); err != nil {
			return nil, err
		}
		out := map[string]int64{}
		for _, r := range rows {
			out[r.Code+"|"+r.App+"|"+r.ScoreSet] = r.Score
		}
		return out, nil
	}
	left, err := decode(old.FactScores)
	if err != nil {
		return scoreCoverage{}, err
	}
	right, err := decode(current.FactScores)
	if err != nil {
		return scoreCoverage{}, err
	}
	out := scoreCoverage{}
	for key, value := range right {
		oldValue, ok := left[key]
		if !ok {
			out.Added = append(out.Added, key+"="+fmt.Sprint(value))
		} else if oldValue != value {
			out.Changed = append(out.Changed, key+"="+fmt.Sprint(oldValue)+"->"+fmt.Sprint(value))
		}
	}
	for key, value := range left {
		if _, ok := right[key]; !ok {
			out.Removed = append(out.Removed, key+"="+fmt.Sprint(value))
		}
	}
	sort.Strings(out.Added)
	sort.Strings(out.Removed)
	sort.Strings(out.Changed)
	return out, nil
}
func check(dir string, artifacts map[string][]byte) error {
	for name, want := range artifacts {
		got, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil || !bytes.Equal(got, want) {
			return fmt.Errorf("%w: %s", errOutdated, name)
		}
	}
	return nil
}
func atomicWrite(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(path), ".trash-")
	if err != nil {
		return err
	}
	name := file.Name()
	defer os.Remove(name)
	if _, err = file.Write(data); err != nil {
		file.Close()
		return err
	}
	if err = file.Chmod(0644); err != nil {
		file.Close()
		return err
	}
	if err = file.Close(); err != nil {
		return err
	}
	return os.Rename(name, path)
}
func validVersion(value string) bool {
	return len(strings.Split(value, ".")) == 3 && !strings.ContainsAny(value, " \t\n")
}
