package main

import (
	"strings"
	"testing"
)

func TestEncodedTemplateCodecRoundTripsReservedMarkers(t *testing.T) {
	keys := []string{
		"show ¦ § 42",
		"show ¦¦§ 42",
		"show §¦ 42",
		"show ` %\n\r 42",
	}
	representations := map[string]bool{}
	for _, key := range keys {
		skeleton, captures := encodedTemplateKey(key)
		restored, err := restoreEncodedTemplate(skeleton, captures)
		if err != nil {
			t.Fatal(err)
		}
		if restored != key {
			t.Fatalf("codec restored %q, want %q", restored, key)
		}
		representation := skeleton + "\x00" + strings.Join(captures, ",")
		if representations[representation] {
			t.Fatalf("codec collision for %q", key)
		}
		representations[representation] = true
	}
	if got, want := blobKey("`%\n\r"), "%60%25%0A%0D"; got != want {
		t.Fatalf("blob key = %q, want %q", got, want)
	}
}

func TestEncodedRoutesKeepCorrelatedTuplesAndScoresSeparate(t *testing.T) {
	values := map[string]int{
		"show s01e02 v1 1080p x264":   400,
		"show s02e01 v2 1080p x264":   400,
		"listed s01e02 v1 1080p x264": 200,
		"listed s02e01 v2 1080p x264": 200,
	}
	table, _, err := encodeTable(values, map[string]int{
		"blocked s01e02 v1 1080p x264": 1,
		"blocked s02e01 v2 1080p x264": 1,
	})
	if err != nil {
		t.Fatal(err)
	}

	strictSkeleton, _ := encodedTemplateKey("show s01e02 v1 1080p x264")
	strictRoute := "B\t" + strictSkeleton
	record := table.routes[bucketIdentifier(strictRoute)][strictRoute].([]any)
	constants := record[0].([]string)
	if got, want := strings.Join(constants, ","), ",,,1080,264"; got != want {
		t.Fatalf("dense constants = %q, want %q", got, want)
	}
	chunks := record[1].([]string)
	joined := strings.Join(chunks, "")
	for _, expected := range []string{"\n01,02,1\n", "\n02,01,2\n"} {
		if !strings.Contains(joined, expected) {
			t.Fatalf("missing correlated tuple %q in %q", expected, joined)
		}
	}
	if strings.Contains(joined, "\n01,01,2\n") {
		t.Fatalf("cross-product tuple was emitted: %q", joined)
	}

	listedSkeleton, _ := encodedTemplateKey("listed s01e02 v1 1080p x264")
	if _, exists := table.routes[bucketIdentifier("L\t"+listedSkeleton)]["L\t"+listedSkeleton]; !exists {
		t.Fatal("listed route missing or merged with best route")
	}
	blockedSkeleton, _ := encodedTemplateKey("blocked s01e02 v1 1080p x264")
	if _, exists := table.routes[bucketIdentifier("X\t"+blockedSkeleton)]["X\t"+blockedSkeleton]; !exists {
		t.Fatal("strict collision route missing")
	}
}

func TestEncodedRendererHasBoundedLinesAndPhysicalStats(t *testing.T) {
	tables := Tables{
		Strict: map[string]int{
			"show s01e02 v1 1080p x264": 400,
			"show s02e01 v2 1080p x264": 400,
			"literal ` % 03":            200,
		},
		StrictCollisions: map[string]bool{"blocked s01e02 v1 1080p x264": true},
		Tolerant: map[string]int{
			"group|show s01e02 v1 1080p x264": 400,
			"group|show s02e01 v2 1080p x264": 400,
		},
		TolerantCollisions: map[string]bool{},
	}
	policy, stats, err := renderEncodedPolicy(tables)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(policy, "__STRICT_DIRECT__") || strings.Contains(policy, "__TOLERANT_ROUTES__") {
		t.Fatal("unrendered policy placeholder")
	}
	if !strings.Contains(policy, "default selected_match := []") {
		t.Fatal("cached selected match rule missing")
	}
	if stats.DirectBuckets.KeyCount != stats.DirectEntries || stats.TemplateBuckets.KeyCount == 0 {
		t.Fatalf("unexpected index statistics: %#v", stats)
	}
	if stats.SourceBytes != len(policy) || stats.SourceLines != strings.Count(policy, "\n")+1 {
		t.Fatalf("source statistics do not describe rendered policy: %#v", stats)
	}
	for _, line := range strings.Split(policy, "\n") {
		if len(line) > 1024 {
			t.Fatalf("rendered line exceeds limit: %d bytes", len(line))
		}
	}
}
