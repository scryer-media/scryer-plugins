package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestLoadReleasePlanBuildsIndependentComponentMatrices(t *testing.T) {
	directory := t.TempDir()
	pluginPath := filepath.Join(directory, "plugins.tsv")
	rulePackPath := filepath.Join(directory, "packs.tsv")
	if err := os.WriteFile(pluginPath, []byte("email\t1.2.3\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(rulePackPath, []byte("seadex-scoring-pack\t1.0.0\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	plan, err := loadReleasePlan(pluginPath, rulePackPath)
	if err != nil {
		t.Fatal(err)
	}
	if got := plan.Plugins; len(got) != 1 || got[0] != (releaseComponent{ID: "email", Version: "1.2.3"}) {
		t.Fatalf("plugin matrix = %#v", got)
	}
	if got := plan.RulePacks; len(got) != 1 || got[0] != (releaseComponent{ID: "seadex-scoring-pack", Version: "1.0.0"}) {
		t.Fatalf("rule-pack matrix = %#v", got)
	}
}

func TestCentralPublishGateRequiresPushAndEverySelectedPath(t *testing.T) {
	trigger := "refs/tags/plugins-v3/release/20260907-abcdef0"
	if centralPublishAllowed("workflow_dispatch", trigger, false, true, "skipped", "success", "skipped") {
		t.Fatal("manual runs must never publish")
	}
	if !centralPublishAllowed("push", trigger, false, true, "skipped", "success", "skipped") {
		t.Fatal("pack-only release should publish after its required job succeeds")
	}
	if centralPublishAllowed("push", trigger, true, false, "failure", "skipped", "success") {
		t.Fatal("a failed plugin job must block central publication")
	}
	if centralPublishAllowed("push", trigger, true, true, "success", "failure", "success") {
		t.Fatal("a failed rule-pack job must block mixed central publication")
	}
	if !centralPublishAllowed("push", trigger, true, true, "success", "success", "success") {
		t.Fatal("mixed release should publish after all required jobs succeed")
	}
}

func TestReleasePlanRejectsMalformedRows(t *testing.T) {
	for name, content := range map[string]string{
		"missing-version": "missing-version\n",
		"unsafe-id":       "../pack\t1.0.0\n",
		"unsafe-version":  "pack\t1.0.0/other\n",
		"duplicate":       "pack\t1.0.0\npack\t1.0.1\n",
	} {
		path := filepath.Join(t.TempDir(), name+".tsv")
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
		if _, err := readReleasePlan(path); err == nil {
			t.Fatalf("%s plan row was accepted", name)
		}
	}
}

func TestRulePackTagVersionMustMatchManifest(t *testing.T) {
	manifest := filepath.Join(t.TempDir(), "pack.json")
	if err := os.WriteFile(manifest, []byte(`{"id":"seadex-scoring-pack","version":"1.0.0"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := validateRulePackTagVersion(manifest, "seadex-scoring-pack", "1.0.0"); err != nil {
		t.Fatalf("matching tag was rejected: %v", err)
	}
	for _, candidate := range []struct {
		id      string
		version string
	}{
		{id: "other-pack", version: "1.0.0"},
		{id: "seadex-scoring-pack", version: "1.0.1"},
	} {
		if err := validateRulePackTagVersion(manifest, candidate.id, candidate.version); err == nil {
			t.Fatalf("mismatched manifest was accepted: %#v", candidate)
		}
	}
}

func TestVersionsRejectInvalidOrExecutableText(t *testing.T) {
	for _, version := range []string{"1.0.0", "1.10.0-rc.1+build.02", "0.0.0"} {
		if !validComponentVersion(version) {
			t.Errorf("rejected %q", version)
		}
	}
	for _, version := range []string{"1.0", "01.0.0", "1.0.0-01", "1.0.0;echo", "$(id)", "18446744073709551616.0.0", "1.0.0\n"} {
		if validComponentVersion(version) {
			t.Errorf("accepted %q", version)
		}
	}
}

func TestCentralGateRejectsSkippedRequiredJobsAndCancellation(t *testing.T) {
	ref := releaseTriggerPrefix + "test"
	for _, state := range []string{"skipped", "failure", "cancelled", "", "pending"} {
		if centralPublishAllowed("push", ref, false, true, "skipped", state, "skipped") {
			t.Errorf("accepted pack state %q", state)
		}
		if centralPublishAllowed("push", ref, true, false, "success", "skipped", state) {
			t.Errorf("accepted provenance state %q", state)
		}
	}
	if centralPublishAllowed("push", "refs/heads/main", false, true, "skipped", "success", "skipped") {
		t.Fatal("branch push accepted")
	}
	if centralPublishAllowed("push", ref, false, false, "skipped", "skipped", "skipped") {
		t.Fatal("empty release accepted")
	}
}

func TestCentralAssetsIncludeOnlyStagedVersionsAndFailMissingNewFiles(t *testing.T) {
	dir := t.TempDir()
	write := func(name string, value any) {
		t.Helper()
		content, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, name), content, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	artifact := func(name string) map[string]string {
		return map[string]string{"url": "https://example.test/v1.0.0/" + name}
	}
	write("catalog-v3.redirect.json", map[string]any{"artifacts": []any{artifact("catalog.a.json.zst")}})
	write("catalog-v3.modern.redirect.json", map[string]any{"artifacts": []any{artifact("catalog.a.json.zst")}})
	write("prepared-rule-pack-artifacts.json", []any{artifact("pack.new.min.json.br"), artifact("pack.new.min.json.zst")})
	// The full catalog's historical asset must never be used as an upload list.
	write("catalog-v3.json", map[string]any{"rule_packs": []any{artifact("pack.old.min.json.zst")}})
	want := []string{"catalog.a.json.zst", "pack.new.min.json.br", "pack.new.min.json.zst"}
	for _, name := range want {
		write(name, "fixture")
	}
	got, err := centralAssets(dir)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
	if err := os.Remove(filepath.Join(dir, want[1])); err != nil {
		t.Fatal(err)
	}
	if _, err := centralAssets(dir); err == nil {
		t.Fatal("missing new pack artifact accepted")
	}
	write("prepared-rule-pack-artifacts.json", []any{})
	got, err = centralAssets(dir)
	if err != nil || !reflect.DeepEqual(got, want[:1]) {
		t.Fatalf("plugin-only assets: %v, %v", got, err)
	}
	write("prepared-rule-pack-artifacts.json", []any{artifact("evil%0afile.min.json.zst")})
	if _, err := centralAssets(dir); err == nil {
		t.Fatal("newline filename accepted")
	}
}

func TestCatalogOnlyGateAllowsOnlyUnbuiltPluginRepublication(t *testing.T) {
	ref := catalogOnlyTriggerPrefix + "1788221093-b379212825fe-from-b379212825fe"
	if !isCatalogOnlyRelease("push", ref) {
		t.Fatal("catalog-only trigger not detected")
	}
	if isCatalogOnlyRelease("push", releaseTriggerPrefix+"1788221093-b379212825fe") {
		t.Fatal("ordinary release trigger treated as catalog-only")
	}
	if isCatalogOnlyRelease("workflow_dispatch", ref) {
		t.Fatal("dispatch treated as catalog-only")
	}
	if !centralPublishAllowed("push", ref, true, false, "skipped", "skipped", "skipped") {
		t.Fatal("catalog-only republication rejected")
	}
	for _, state := range []string{"success", "failure", "cancelled", ""} {
		if centralPublishAllowed("push", ref, true, false, state, "skipped", "skipped") {
			t.Errorf("accepted plugin build state %q under a catalog-only trigger", state)
		}
		if centralPublishAllowed("push", ref, true, false, "skipped", "skipped", state) {
			t.Errorf("accepted provenance state %q under a catalog-only trigger", state)
		}
	}
	if centralPublishAllowed("push", ref, true, true, "skipped", "skipped", "skipped") {
		t.Fatal("catalog-only trigger accepted rule packs")
	}
	if centralPublishAllowed("push", ref, false, false, "skipped", "skipped", "skipped") {
		t.Fatal("catalog-only trigger accepted an empty plan")
	}
}
