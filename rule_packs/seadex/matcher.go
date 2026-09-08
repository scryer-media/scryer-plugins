package main

import (
	_ "embed"
	"fmt"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

//go:embed policy.rego
var policyTemplate string

var (
	crcBracket       = regexp.MustCompile(`\[[ \t\r\n]*[0-9a-fA-F]{8}[ \t\r\n]*\]`)
	strictWhitespace = regexp.MustCompile(`[ \t\r\n]+`)
	nonASCIIAlphaNum = regexp.MustCompile(`[^a-z0-9]+`)
	nonReleaseToken  = regexp.MustCompile(`(?i)(^|[^a-z0-9])(bonus|bonuses|extra|extras|featurette|featurettes|sample|samples|trailer|trailers)($|[^a-z0-9])`)
	numericToken     = regexp.MustCompile(`^[0-9]+$`)
	versionToken     = regexp.MustCompile(`^(v[0-9]+|s[0-9]+e[0-9]+|e[0-9]+)$`)
	resolutionToken  = regexp.MustCompile(`^[0-9]{3,4}p$`)
)

var videoExtensions = []string{".m2ts", ".webm", ".mkv", ".mp4", ".avi", ".ts"}

var directoryMarkers = map[string]bool{
	"bonus": true, "bonuses": true, "extra": true, "extras": true,
	"featurette": true, "featurettes": true, "sample": true, "samples": true,
	"trailer": true, "trailers": true,
}

var technicalTokens = map[string]bool{
	"aac": true, "atmos": true, "av1": true, "bd": true, "bdrip": true,
	"bluray": true, "ddp": true, "dts": true, "flac": true, "h264": true,
	"h265": true, "hevc": true, "opus": true, "remux": true, "truehd": true,
	"web": true, "webdl": true, "webrip": true, "x264": true, "x265": true,
}

// Tables are the compact runtime objects embedded in the rendered Rego policy.
// All source provenance remains in MatchReport, rather than inflating policies.
type Tables struct {
	Strict             map[string]int  `json:"strict"`
	StrictCollisions   map[string]bool `json:"strict_collisions"`
	Tolerant           map[string]int  `json:"tolerant"`
	TolerantCollisions map[string]bool `json:"tolerant_collisions"`
}

type MatchProvenance struct {
	AniListID  int64  `json:"anilist_id"`
	EntryID    string `json:"entry_id"`
	ReleaseID  string `json:"release_id"`
	Source     string `json:"source"`
	SourceName string `json:"source_name"`
}

type AmbiguousMatch struct {
	Candidates []MatchProvenance `json:"candidates"`
	Key        string            `json:"key"`
	Kind       string            `json:"kind"`
	Reason     string            `json:"reason"`
}

type IncludedRelease struct {
	AliasSources int    `json:"alias_sources"`
	EntryID      string `json:"entry_id"`
	FileSources  int    `json:"file_sources"`
	ReleaseID    string `json:"release_id"`
	StrictKeys   int    `json:"strict_keys"`
	TolerantKeys int    `json:"tolerant_keys"`
}

type SkippedMatch struct {
	EntryID    string `json:"entry_id"`
	Reason     string `json:"reason"`
	ReleaseID  string `json:"release_id"`
	SourceName string `json:"source_name,omitempty"`
}

type MatchReport struct {
	Ambiguous      []AmbiguousMatch  `json:"ambiguous"`
	Included       []IncludedRelease `json:"included"`
	Provenance     MatchTotals       `json:"provenance"`
	Skipped        []SkippedMatch    `json:"skipped"`
	StaleOverrides []string          `json:"stale_overrides"`
}

type MatchTotals struct {
	StrictKeys   int `json:"strict_keys"`
	TolerantKeys int `json:"tolerant_keys"`
}

type sourceCandidate struct {
	aniListID         int64
	entryID           string
	fileSize          int64
	groupSignature    string
	groupIdentity     string
	hasFileSize       bool
	releaseID         string
	score             int
	source            string
	sourceName        string
	strictSignature   string
	tolerantSignature string
}

type strictIdentity struct {
	aniListID       int64
	fileSize        int64
	group           string
	hasFileSize     bool
	score           int
	strictSignature string
}

type tolerantIdentity struct {
	aniListID         int64
	fileSize          int64
	group             string
	hasFileSize       bool
	score             int
	strictSignature   string
	tolerantSignature string
}

func releaseBasename(value string) string {
	value = strings.ReplaceAll(value, "\\", "/")
	if index := strings.LastIndex(value, "/"); index >= 0 {
		return value[index+1:]
	}
	return value
}

// Regorus applies full Unicode lowercasing while Go's strings.ToLower uses
// simple mappings for these source characters. Exclude them before any
// normalization so a source key cannot identify a different candidate title.
func unsupportedUnicodeCaseMapping(value string) bool {
	basename := releaseBasename(value)
	return strings.Contains(basename, "İ") || strings.Contains(basename, "Σ")
}

func releaseStem(value string) string {
	basename := strings.ToLower(releaseBasename(value))
	for _, extension := range videoExtensions {
		if strings.HasSuffix(basename, extension) {
			return strings.TrimSuffix(basename, extension)
		}
	}
	return basename
}

func strictSignature(value string) string {
	return strings.Trim(strictWhitespace.ReplaceAllString(releaseStem(value), " "), " \t\r\n")
}

func groupSignature(value string) string {
	return nonASCIIAlphaNum.ReplaceAllString(strings.ToLower(value), "")
}

func stripEdgeGroup(stem, group string) string {
	trimmed := strings.TrimSpace(stem)
	expected := groupSignature(group)
	if expected == "" {
		return trimmed
	}
	if strings.HasPrefix(trimmed, "[") {
		if end := strings.Index(trimmed, "]"); end > 0 && groupSignature(trimmed[:end]) == expected {
			return strings.TrimSpace(trimmed[end+1:])
		}
	}
	lowerGroup := strings.TrimSpace(strings.ToLower(group))
	if prefix := lowerGroup + " -"; strings.HasPrefix(trimmed, prefix) {
		return strings.TrimSpace(strings.TrimPrefix(trimmed, prefix))
	}
	if suffix := " - " + lowerGroup; strings.HasSuffix(trimmed, suffix) {
		return strings.TrimSpace(strings.TrimSuffix(trimmed, suffix))
	}
	if suffix := "-" + lowerGroup; strings.HasSuffix(trimmed, suffix) {
		return strings.TrimSpace(strings.TrimSuffix(trimmed, suffix))
	}
	if suffix := "[" + lowerGroup + "]"; strings.HasSuffix(trimmed, suffix) {
		return strings.TrimSpace(strings.TrimSuffix(trimmed, suffix))
	}
	return trimmed
}

func hasEdgeGroup(value, group string) bool {
	stem := strings.TrimSpace(releaseStem(value))
	return groupSignature(group) != "" && stripEdgeGroup(stem, group) != stem
}

func tolerantSignature(value, group string) string {
	stem := stripEdgeGroup(releaseStem(value), group)
	withoutCRC := crcBracket.ReplaceAllString(stem, "")
	return strings.Join(strings.Fields(nonASCIIAlphaNum.ReplaceAllString(strings.ToLower(withoutCRC), " ")), " ")
}

func hasTitleEvidence(value, group string) bool {
	stem := crcBracket.ReplaceAllString(stripEdgeGroup(releaseStem(value), group), "")
	for _, token := range strings.FieldsFunc(strings.ToLower(stem), func(r rune) bool {
		return !unicode.IsLetter(r) && !unicode.IsNumber(r)
	}) {
		if !technicalTokens[token] && !numericToken.MatchString(token) &&
			!versionToken.MatchString(token) && !resolutionToken.MatchString(token) {
			return true
		}
	}
	return false
}

func isASCIITolerantEvidence(value, group string) bool {
	return isASCII(releaseStem(value)) && isASCII(group)
}

func isASCII(value string) bool {
	for _, character := range value {
		if character > 127 {
			return false
		}
	}
	return true
}

func isVideoReleaseFile(value string) bool {
	path := strings.ReplaceAll(value, "\\", "/")
	parts := strings.Split(path, "/")
	if len(parts) == 0 {
		return false
	}
	for _, part := range parts[:len(parts)-1] {
		if directoryMarkers[strings.ToLower(part)] {
			return false
		}
	}
	basename := parts[len(parts)-1]
	if !isVideoExtension(filepath.Ext(strings.ToLower(basename))) {
		return false
	}
	return !nonReleaseToken.MatchString(releaseStem(basename))
}

func isVideoExtension(extension string) bool {
	for _, videoExtension := range videoExtensions {
		if extension == videoExtension {
			return true
		}
	}
	return false
}

func compileSnapshot(snapshot Snapshot, overrides Overrides) (Tables, MatchReport, error) {
	strictBuckets := map[string][]sourceCandidate{}
	tolerantBuckets := map[string][]sourceCandidate{}
	skipped := []SkippedMatch{}
	seenReleaseIDs := map[string]bool{}

	appendSkip := func(entryID, releaseID, reason, sourceName string) {
		skipped = append(skipped, SkippedMatch{EntryID: entryID, ReleaseID: releaseID, Reason: reason, SourceName: sourceName})
	}
	for _, entry := range snapshot.Entries {
		for _, release := range entry.Releases {
			seenReleaseIDs[release.ID] = true
			override, hasOverride := overrides.Releases[release.ID]
			if hasOverride && override.Exclude {
				appendSkip(entry.ID, release.ID, "excluded_by_override", "")
				continue
			}
			groupKey := groupSignature(release.Group)
			score := 200
			if release.Best {
				score = 400
			}
			candidates := make([]struct {
				name        string
				source      string
				size        int64
				hasFileSize bool
			}, 0, len(release.Files)+len(override.Aliases))
			for _, file := range release.Files {
				if !isVideoReleaseFile(file.Name) {
					appendSkip(entry.ID, release.ID, "non_release_file", file.Name)
					continue
				}
				candidates = append(candidates, struct {
					name        string
					source      string
					size        int64
					hasFileSize bool
				}{file.Name, "file", file.Size, true})
			}
			for _, alias := range override.Aliases {
				if strings.TrimSpace(alias) == "" {
					appendSkip(entry.ID, release.ID, "malformed_alias", "")
					continue
				}
				candidates = append(candidates, struct {
					name        string
					source      string
					size        int64
					hasFileSize bool
				}{alias, "alias", 0, false})
			}
			for _, source := range candidates {
				if unsupportedUnicodeCaseMapping(source.name) {
					appendSkip(entry.ID, release.ID, "unsupported_unicode_case_mapping", source.name)
					continue
				}
				strict := strictSignature(source.name)
				if strict == "" {
					appendSkip(entry.ID, release.ID, "empty_signature", source.name)
					continue
				}
				if !hasTitleEvidence(source.name, release.Group) {
					appendSkip(entry.ID, release.ID, "generic_or_numeric_only_title", source.name)
					continue
				}
				candidate := sourceCandidate{
					aniListID: entry.AniListID, entryID: entry.ID, fileSize: source.size,
					groupSignature: groupKey, groupIdentity: strings.TrimSpace(strings.ToLower(release.Group)), hasFileSize: source.hasFileSize, releaseID: release.ID,
					score: score, source: source.source, sourceName: source.name, strictSignature: strict,
					tolerantSignature: tolerantSignature(source.name, release.Group),
				}
				strictBuckets[strict] = append(strictBuckets[strict], candidate)
				if groupKey == "" {
					appendSkip(entry.ID, release.ID, "missing_release_group_for_tolerant_match", source.name)
				} else if !hasEdgeGroup(source.name, release.Group) {
					appendSkip(entry.ID, release.ID, "source_missing_edge_group_for_tolerant_match", source.name)
				} else if !isASCIITolerantEvidence(source.name, release.Group) {
					appendSkip(entry.ID, release.ID, "non_ascii_tolerant_evidence", source.name)
				} else {
					key := groupKey + "|" + candidate.tolerantSignature
					tolerantBuckets[key] = append(tolerantBuckets[key], candidate)
				}
			}
		}
	}

	strict, strictCollisions, strictAmbiguous := collapseStrict(strictBuckets)
	tolerant, tolerantCollisions, tolerantAmbiguous := collapseTolerant(tolerantBuckets)
	tables := Tables{Strict: strict, StrictCollisions: strictCollisions, Tolerant: tolerant, TolerantCollisions: tolerantCollisions}
	ambiguous := make([]AmbiguousMatch, 0, len(strictAmbiguous)+len(tolerantAmbiguous))
	ambiguous = append(ambiguous, strictAmbiguous...)
	ambiguous = append(ambiguous, tolerantAmbiguous...)
	report := MatchReport{
		Ambiguous:      ambiguous,
		Included:       includedReleases(strict, tolerant, strictBuckets, tolerantBuckets),
		Provenance:     MatchTotals{StrictKeys: len(strict), TolerantKeys: len(tolerant)},
		Skipped:        skipped,
		StaleOverrides: []string{},
	}
	for releaseID := range overrides.Releases {
		if !seenReleaseIDs[releaseID] {
			report.StaleOverrides = append(report.StaleOverrides, releaseID)
		}
	}
	sort.Slice(report.Ambiguous, func(i, j int) bool {
		return report.Ambiguous[i].Kind+"\x00"+report.Ambiguous[i].Key < report.Ambiguous[j].Kind+"\x00"+report.Ambiguous[j].Key
	})
	sort.Slice(report.Skipped, func(i, j int) bool {
		left, right := report.Skipped[i], report.Skipped[j]
		return left.EntryID+"\x00"+left.ReleaseID+"\x00"+left.Reason+"\x00"+left.SourceName < right.EntryID+"\x00"+right.ReleaseID+"\x00"+right.Reason+"\x00"+right.SourceName
	})
	sort.Strings(report.StaleOverrides)
	return tables, report, nil
}

func collapseStrict(buckets map[string][]sourceCandidate) (map[string]int, map[string]bool, []AmbiguousMatch) {
	accepted := map[string]int{}
	collisions := map[string]bool{}
	ambiguous := []AmbiguousMatch{}
	for _, key := range sortedCandidateKeys(buckets) {
		candidates := sortedCandidates(buckets[key])
		identities := map[strictIdentity]bool{}
		for _, candidate := range candidates {
			identities[strictIdentity{candidate.aniListID, candidate.fileSize, candidate.groupIdentity, candidate.hasFileSize, candidate.score, candidate.strictSignature}] = true
		}
		if len(identities) == 1 {
			accepted[key] = candidates[0].score
			continue
		}
		collisions[key] = true
		ambiguous = append(ambiguous, ambiguousMatch(key, "strict", "conflicting_identity_or_recommendation_level", candidates))
	}
	return accepted, collisions, ambiguous
}

func collapseTolerant(buckets map[string][]sourceCandidate) (map[string]int, map[string]bool, []AmbiguousMatch) {
	accepted := map[string]int{}
	collisions := map[string]bool{}
	ambiguous := []AmbiguousMatch{}
	for _, key := range sortedCandidateKeys(buckets) {
		candidates := sortedCandidates(buckets[key])
		identities := map[tolerantIdentity]bool{}
		strictSignatures := map[string]bool{}
		for _, candidate := range candidates {
			identities[tolerantIdentity{candidate.aniListID, candidate.fileSize, candidate.groupIdentity, candidate.hasFileSize, candidate.score, candidate.strictSignature, candidate.tolerantSignature}] = true
			strictSignatures[candidate.strictSignature] = true
		}
		if len(identities) == 1 {
			accepted[key] = candidates[0].score
			continue
		}
		collisions[key] = true
		reason := "conflicting_identity_or_recommendation_level"
		if len(strictSignatures) > 1 {
			reason = "tolerant_key_merges_distinct_strict_signatures"
		}
		ambiguous = append(ambiguous, ambiguousMatch(key, "tolerant", reason, candidates))
	}
	return accepted, collisions, ambiguous
}

func sortedCandidateKeys(buckets map[string][]sourceCandidate) []string {
	keys := make([]string, 0, len(buckets))
	for key := range buckets {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}

func sortedCandidates(candidates []sourceCandidate) []sourceCandidate {
	result := append([]sourceCandidate(nil), candidates...)
	sort.Slice(result, func(i, j int) bool {
		left, right := result[i], result[j]
		return left.strictSignature+"\x00"+left.entryID+"\x00"+left.releaseID+"\x00"+left.sourceName+"\x00"+left.source < right.strictSignature+"\x00"+right.entryID+"\x00"+right.releaseID+"\x00"+right.sourceName+"\x00"+right.source
	})
	return result
}

func ambiguousMatch(key, kind, reason string, candidates []sourceCandidate) AmbiguousMatch {
	provenance := make([]MatchProvenance, 0, len(candidates))
	for _, candidate := range candidates {
		provenance = append(provenance, MatchProvenance{candidate.aniListID, candidate.entryID, candidate.releaseID, candidate.source, candidate.sourceName})
	}
	return AmbiguousMatch{Candidates: provenance, Key: key, Kind: kind, Reason: reason}
}

func includedReleases(strict, tolerant map[string]int, strictBuckets, tolerantBuckets map[string][]sourceCandidate) []IncludedRelease {
	type accumulator struct {
		entryID, releaseID string
		aliases, files     map[string]bool
		strict, tolerant   map[string]bool
	}
	items := map[string]*accumulator{}
	record := func(kind, key string, candidates []sourceCandidate) {
		for _, candidate := range candidates {
			id := candidate.entryID + "\x00" + candidate.releaseID
			item := items[id]
			if item == nil {
				item = &accumulator{entryID: candidate.entryID, releaseID: candidate.releaseID, aliases: map[string]bool{}, files: map[string]bool{}, strict: map[string]bool{}, tolerant: map[string]bool{}}
				items[id] = item
			}
			if kind == "strict" {
				item.strict[key] = true
			} else {
				item.tolerant[key] = true
			}
			sourceID := fmt.Sprintf("%s\x00%d\x00%t", candidate.sourceName, candidate.fileSize, candidate.hasFileSize)
			if candidate.source == "alias" {
				item.aliases[sourceID] = true
			} else {
				item.files[sourceID] = true
			}
		}
	}
	for key := range strict {
		record("strict", key, strictBuckets[key])
	}
	for key := range tolerant {
		record("tolerant", key, tolerantBuckets[key])
	}
	keys := make([]string, 0, len(items))
	for key := range items {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	result := make([]IncludedRelease, 0, len(keys))
	for _, key := range keys {
		item := items[key]
		result = append(result, IncludedRelease{len(item.aliases), item.entryID, len(item.files), item.releaseID, len(item.strict), len(item.tolerant)})
	}
	return result
}

func renderPolicy(tables Tables) (string, error) {
	policy, _, err := renderEncodedPolicy(tables)
	return policy, err
}

func bucketIdentifier(key string) string {
	return strconv.Itoa(utf8.RuneCountInString(key))
}

func bucketStatistics[T int | bool](values map[string]T) (bucketCount, keyCount, maximum int, mean float64) {
	counts := map[string]int{}
	for key := range values {
		counts[bucketIdentifier(key)]++
		keyCount++
	}
	for _, count := range counts {
		if count > maximum {
			maximum = count
		}
	}
	if len(counts) > 0 {
		mean = float64(keyCount) / float64(len(counts))
	}
	return len(counts), keyCount, maximum, mean
}
