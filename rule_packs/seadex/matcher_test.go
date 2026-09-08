package main

import (
	"encoding/json"
	"testing"
)

func matcherRelease(id, group string, best bool, size int64, names ...string) Release {
	files := make([]File, 0, len(names))
	for _, name := range names {
		files = append(files, File{Name: name, Size: size})
	}
	return Release{ID: id, Group: group, Best: best, UpdatedAt: "2026-01-01T00:00:00Z", Files: files}
}

func matcherSnapshot(releases ...Release) Snapshot {
	return Snapshot{SchemaVersion: 1, Entries: []Entry{{
		ID: "entry-1", AniListID: 42, UpdatedAt: "2026-01-01T00:00:00Z",
		Notes: "reference", Releases: releases,
	}}}
}

func TestStrictAndTolerantNormalisation(t *testing.T) {
	value := `folder\[Good-Group] Show.Name - 01v2 [1080p] [A1B2C3D4].mkv`
	if got, want := strictSignature(value), "[good-group] show.name - 01v2 [1080p] [a1b2c3d4]"; got != want {
		t.Fatalf("strict signature = %q, want %q", got, want)
	}
	if got, want := tolerantSignature(value, "good group"), "show name 01v2 1080p"; got != want {
		t.Fatalf("tolerant signature = %q, want %q", got, want)
	}
	if !hasEdgeGroup(value, "good group") {
		t.Fatal("expected bracketed prefix group")
	}
	if hasEdgeGroup("Show Good Group 01.mkv", "good group") {
		t.Fatal("middle title text must not count as a group")
	}
}

func TestStrictWhitespaceMatchesTheRegoCharacterSet(t *testing.T) {
	value := "\u00a0Show\t\tName.mkv"
	if got, want := strictSignature(value), "\u00a0show name"; got != want {
		t.Fatalf("strict whitespace signature = %q, want %q", got, want)
	}
}

func TestGroupSuffixesAndGenericEvidence(t *testing.T) {
	value := "Show.S01E01.1080p.x265-koala.mkv"
	if got, want := tolerantSignature(value, "koala"), "show s01e01 1080p x265"; got != want {
		t.Fatalf("suffix signature = %q, want %q", got, want)
	}
	if !hasEdgeGroup(value, "koala") || !hasEdgeGroup("Show - [Koala].mkv", "koala") {
		t.Fatal("expected suffix group forms")
	}
	if hasTitleEvidence("[Koala] 01.mkv", "koala") {
		t.Fatal("numeric-only names are not title evidence")
	}
	if !hasTitleEvidence("[Koala] C3 - 01.mkv", "koala") {
		t.Fatal("C3 title should be evidence")
	}
}

func TestReleaseFileFiltering(t *testing.T) {
	if !isVideoReleaseFile("Show - 01.mkv") {
		t.Fatal("video file rejected")
	}
	for _, name := range []string{"Extras/Show - 01.mkv", "Show - sample.mkv", "Show - 01.srt"} {
		if isVideoReleaseFile(name) {
			t.Fatalf("non-release file accepted: %s", name)
		}
	}
}

func TestCompilerScoresAndAliases(t *testing.T) {
	tables, report, err := compileSnapshot(matcherSnapshot(
		matcherRelease("best", "Good", true, 1, "[Good] Show - 01.mkv"),
		matcherRelease("listed", "Other", false, 1, "[Other] Show - 02.mkv"),
	), Overrides{SchemaVersion: 1, Releases: map[string]Override{"best": {Aliases: []string{"Show Complete"}}}})
	if err != nil {
		t.Fatal(err)
	}
	if got := tables.Strict["[good] show - 01"]; got != 400 {
		t.Fatalf("best score = %d", got)
	}
	if got := tables.Strict["[other] show - 02"]; got != 200 {
		t.Fatalf("listed score = %d", got)
	}
	if got := tables.Strict["show complete"]; got != 400 {
		t.Fatalf("alias score = %d", got)
	}
	if len(report.Ambiguous) != 0 {
		t.Fatalf("unexpected ambiguity: %#v", report.Ambiguous)
	}
}

func TestCompilerSuppressesConflictingStrictKey(t *testing.T) {
	snapshot := matcherSnapshot(matcherRelease("a", "Good", true, 1, "[Good] Show - 01.mkv"))
	snapshot.Entries = append(snapshot.Entries, Entry{
		ID: "entry-2", AniListID: 99, UpdatedAt: "2026-01-01T00:00:00Z", Notes: "reference",
		Releases: []Release{matcherRelease("b", "Good", true, 1, "[Good] Show - 01.mkv")},
	})
	tables, report, err := compileSnapshot(snapshot, Overrides{Releases: map[string]Override{}})
	if err != nil {
		t.Fatal(err)
	}
	key := "[good] show - 01"
	if _, exists := tables.Strict[key]; exists {
		t.Fatalf("conflicting key emitted: %s", key)
	}
	if !tables.StrictCollisions[key] || len(report.Ambiguous) == 0 || report.Ambiguous[0].Kind != "strict" {
		t.Fatalf("strict conflict was not reported: %#v", report)
	}
}

func TestCompilerTreatsCRCAndSizeVariantsAsAmbiguous(t *testing.T) {
	tables, _, err := compileSnapshot(matcherSnapshot(
		matcherRelease("a", "Good", true, 100, "[Good] Show - 01 [A1B2C3D4].mkv"),
		matcherRelease("b", "Good", true, 101, "[Good] Show - 01 [E5F6A7B8].mkv"),
	), Overrides{Releases: map[string]Override{}})
	if err != nil {
		t.Fatal(err)
	}
	if !tables.TolerantCollisions["good|show 01"] {
		t.Fatalf("CRC variants should not merge: %#v", tables.TolerantCollisions)
	}

	tables, _, err = compileSnapshot(matcherSnapshot(
		matcherRelease("a", "Good", true, 100, "[Good] Same - 01.mkv"),
		matcherRelease("b", "Good", true, 101, "[Good] Same - 01.mkv"),
	), Overrides{Releases: map[string]Override{}})
	if err != nil {
		t.Fatal(err)
	}
	if !tables.StrictCollisions["[good] same - 01"] {
		t.Fatal("same name at different sizes must remain ambiguous")
	}
}

func TestTolerantGroupKeyDoesNotMergeDistinctRawGroups(t *testing.T) {
	tables, report, err := compileSnapshot(matcherSnapshot(
		matcherRelease("a", "A-B", true, 100, "[A-B] Show - 01.mkv"),
		matcherRelease("b", "AB", true, 100, "[AB] Show - 01.mkv"),
	), Overrides{Releases: map[string]Override{}})
	if err != nil {
		t.Fatal(err)
	}
	if !tables.TolerantCollisions["ab|show 01"] {
		t.Fatalf("normalized group ambiguity was not suppressed: %#v", tables.TolerantCollisions)
	}
	if len(report.Ambiguous) == 0 || report.Ambiguous[len(report.Ambiguous)-1].Kind != "tolerant" {
		t.Fatalf("normalized group ambiguity was not reported: %#v", report.Ambiguous)
	}
}

func TestCompilerKeepsUnicodeStrictOnlyAndRequiresSourceGroupMarker(t *testing.T) {
	tables, report, err := compileSnapshot(matcherSnapshot(
		matcherRelease("unicode", "Good", true, 1, "[Good] 日本語 - 01.mkv"),
		matcherRelease("unmarked", "Good", true, 1, "Show - 01.mkv"),
	), Overrides{Releases: map[string]Override{}})
	if err != nil {
		t.Fatal(err)
	}
	if got := tables.Strict["[good] 日本語 - 01"]; got != 400 {
		t.Fatalf("unicode strict score = %d", got)
	}
	if len(tables.Tolerant) != 0 {
		t.Fatalf("unexpected tolerant data: %#v", tables.Tolerant)
	}
	reasons := map[string]bool{}
	for _, skipped := range report.Skipped {
		reasons[skipped.Reason] = true
	}
	if !reasons["non_ascii_tolerant_evidence"] || !reasons["source_missing_edge_group_for_tolerant_match"] {
		t.Fatalf("missing skip reasons: %#v", reasons)
	}
}

func TestCompilerExcludesUnsupportedUnicodeCaseMappingsBeforeScoring(t *testing.T) {
	tables, report, err := compileSnapshot(matcherSnapshot(
		matcherRelease("dotted-i", "Good", true, 1, "[Good] İstanbul - 01.mkv"),
		matcherRelease("ordinary", "Good", true, 1, "[Good] 日本語 - 01.mkv"),
		Release{ID: "sigma-alias", Group: "Good", UpdatedAt: "2026-01-01T00:00:00Z"},
	), Overrides{Releases: map[string]Override{
		"sigma-alias": {Aliases: []string{"[Good] Σigma - 01"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(tables.Strict) != 1 || tables.Strict["[good] 日本語 - 01"] != 400 {
		t.Fatalf("unsupported source entered strict data: %#v", tables.Strict)
	}
	if len(tables.Tolerant) != 0 {
		t.Fatalf("unsupported source entered tolerant data: %#v", tables.Tolerant)
	}
	skipped := map[string]bool{}
	for _, item := range report.Skipped {
		if item.Reason == "unsupported_unicode_case_mapping" {
			skipped[item.SourceName] = true
		}
	}
	for _, source := range []string{"[Good] İstanbul - 01.mkv", "[Good] Σigma - 01"} {
		if !skipped[source] {
			t.Fatalf("unsupported source was not reported: %q in %#v", source, report.Skipped)
		}
	}
}

func TestGenericNamesAndStaleOverridesAreReportedDeterministically(t *testing.T) {
	snapshot := matcherSnapshot(matcherRelease("a", "Good", true, 1, "[Good] 01.mkv"))
	overrides := Overrides{SchemaVersion: 1, Releases: map[string]Override{"gone": {Exclude: true}}}
	firstTables, firstReport, err := compileSnapshot(snapshot, overrides)
	if err != nil {
		t.Fatal(err)
	}
	secondTables, secondReport, err := compileSnapshot(snapshot, overrides)
	if err != nil {
		t.Fatal(err)
	}
	first, _ := json.Marshal(struct {
		Tables Tables
		Report MatchReport
	}{firstTables, firstReport})
	second, _ := json.Marshal(struct {
		Tables Tables
		Report MatchReport
	}{secondTables, secondReport})
	if string(first) != string(second) {
		t.Fatal("compilation is not deterministic")
	}
	if len(firstTables.Strict) != 0 || len(firstReport.StaleOverrides) != 1 || firstReport.StaleOverrides[0] != "gone" {
		t.Fatalf("unexpected generic/stale result: %#v %#v", firstTables, firstReport)
	}
}

func TestRenderPolicyDoesNotReprocessPlaceholderData(t *testing.T) {
	policy, err := renderPolicy(Tables{Strict: map[string]int{"__TOLERANT_RECOMMENDATIONS__": 400}, StrictCollisions: map[string]bool{}, Tolerant: map[string]int{}, TolerantCollisions: map[string]bool{}})
	if err != nil {
		t.Fatal(err)
	}
	if policy == "" {
		t.Fatal("rendered policy is empty")
	}
	if !containsDirectRecord(policy, "B__TOLERANT_RECOMMENDATIONS__") {
		t.Fatal("placeholder text in a release name was reprocessed")
	}
}

func TestEncodedPolicyRenderingIsDeterministic(t *testing.T) {
	tables := Tables{Strict: map[string]int{"ordinary 01": 200}, StrictCollisions: map[string]bool{}, Tolerant: map[string]int{}, TolerantCollisions: map[string]bool{}}
	first, err := renderPolicy(tables)
	if err != nil {
		t.Fatal(err)
	}
	second, err := renderPolicy(tables)
	if err != nil || first != second {
		t.Fatal("encoded policy rendering is not deterministic")
	}
}

func containsDirectRecord(policy, record string) bool {
	return len(record) > 0 && len(policy) > len(record) &&
		policy != "" && stringContains(policy, "\n"+record+"\n")
}

func stringContains(value, part string) bool {
	for index := 0; index+len(part) <= len(value); index++ {
		if value[index:index+len(part)] == part {
			return true
		}
	}
	return false
}

func TestFullSnapshotLengthBucketDistribution(t *testing.T) {
	snapshot, err := loadSnapshot("snapshot.json.gz")
	if err != nil {
		t.Fatal(err)
	}
	tables, _, err := compileSnapshot(snapshot, Overrides{SchemaVersion: 1, Releases: map[string]Override{}})
	if err != nil {
		t.Fatal(err)
	}
	strictBuckets, strictKeys, strictMaximum, strictMean := bucketStatistics(tables.Strict)
	tolerantBuckets, tolerantKeys, tolerantMaximum, tolerantMean := bucketStatistics(tables.Tolerant)
	for _, table := range []struct {
		name    string
		buckets int
		keys    int
		maximum int
		mean    float64
	}{
		{"strict", strictBuckets, strictKeys, strictMaximum, strictMean},
		{"tolerant", tolerantBuckets, tolerantKeys, tolerantMaximum, tolerantMean},
	} {
		t.Logf("%s length buckets=%d keys=%d max=%d mean=%.2f", table.name, table.buckets, table.keys, table.maximum, table.mean)
	}
}
