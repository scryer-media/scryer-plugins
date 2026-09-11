package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

const releaseTriggerPrefix = "refs/tags/plugins-v3/release/"

// catalogOnlyTriggerPrefix marks a trigger that republishes the central catalog
// from component releases that already exist. Nothing is rebuilt or re-signed, so
// the build, rule-pack, and provenance jobs are gated off and report "skipped".
const catalogOnlyTriggerPrefix = releaseTriggerPrefix + "catalog-"

type releaseComponent struct {
	ID      string
	Version string
}

type pluginMatrixEntry struct {
	PluginID string `json:"plugin_id"`
	Version  string `json:"version"`
}

type rulePackMatrixEntry struct {
	RulePackID string `json:"rule_pack_id"`
	Version    string `json:"version"`
}

type releasePlan struct {
	Plugins   []releaseComponent
	RulePacks []releaseComponent
}

type rulePackManifest struct {
	ID      string `json:"id"`
	Version string `json:"version"`
}

var componentID = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)
var componentVersion = regexp.MustCompile(`^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$`)

func validComponentVersion(value string) bool {
	parts := componentVersion.FindStringSubmatch(value)
	if parts == nil {
		return false
	}
	for _, core := range parts[1:4] {
		if _, err := strconv.ParseUint(core, 10, 64); err != nil {
			return false
		}
	}
	for _, identifier := range strings.Split(parts[4], ".") {
		if len(identifier) > 1 && identifier[0] == '0' && strings.Trim(identifier, "0123456789") == "" {
			return false
		}
	}
	return true
}

// centralAssets lists only artifacts staged by this catalog preparation.
// Historical releases remain referenced remotely and are not uploaded again.
func centralAssets(directory string) ([]string, error) {
	type artifact struct {
		URL string `json:"url"`
	}
	var artifacts []artifact
	for _, name := range []string{"catalog-v3.redirect.json", "catalog-v3.modern.redirect.json"} {
		content, err := os.ReadFile(filepath.Join(directory, name))
		if err != nil {
			return nil, err
		}
		var redirect struct {
			Artifacts []artifact `json:"artifacts"`
		}
		if err := json.Unmarshal(content, &redirect); err != nil {
			return nil, err
		}
		if len(redirect.Artifacts) == 0 {
			return nil, fmt.Errorf("%s has no catalog artifacts", name)
		}
		artifacts = append(artifacts, redirect.Artifacts...)
	}
	content, err := os.ReadFile(filepath.Join(directory, "prepared-rule-pack-artifacts.json"))
	if err != nil {
		return nil, err
	}
	var packs []artifact
	if err := json.Unmarshal(content, &packs); err != nil {
		return nil, err
	}
	artifacts = append(artifacts, packs...)
	names := map[string]bool{}
	for _, item := range artifacts {
		parsed, err := url.Parse(item.URL)
		if err != nil {
			return nil, err
		}
		name := path.Base(parsed.Path)
		if (parsed.Scheme != "https" && parsed.Scheme != "http") || parsed.Host == "" ||
			strings.ContainsAny(name, "\\\r\n\t") || name == "." || name == ".." ||
			!(strings.HasSuffix(name, ".json.zst") || strings.HasSuffix(name, ".json.br")) {
			return nil, fmt.Errorf("invalid staged artifact URL %q", item.URL)
		}
		info, err := os.Stat(filepath.Join(directory, name))
		if err != nil {
			return nil, fmt.Errorf("missing staged artifact %s: %w", name, err)
		}
		if !info.Mode().IsRegular() {
			return nil, fmt.Errorf("staged artifact %s is not a regular file", name)
		}
		names[name] = true
	}
	result := make([]string, 0, len(names))
	for name := range names {
		result = append(result, name)
	}
	sort.Strings(result)
	return result, nil
}

func readReleasePlan(path string) ([]releaseComponent, error) {
	content, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	plan := []releaseComponent{}
	seen := map[string]bool{}
	for lineNumber, line := range strings.Split(strings.TrimSpace(string(content)), "\n") {
		if line == "" {
			continue
		}
		fields := strings.Split(line, "\t")
		if len(fields) != 2 || !componentID.MatchString(fields[0]) || !validComponentVersion(fields[1]) {
			return nil, fmt.Errorf("%s:%d: expected a safe id and version separated by one tab", path, lineNumber+1)
		}
		if seen[fields[0]] {
			return nil, fmt.Errorf("%s:%d: duplicate component id %q", path, lineNumber+1, fields[0])
		}
		seen[fields[0]] = true
		plan = append(plan, releaseComponent{ID: fields[0], Version: fields[1]})
	}
	return plan, nil
}

func loadReleasePlan(pluginPath, rulePackPath string) (releasePlan, error) {
	plugins, err := readReleasePlan(pluginPath)
	if err != nil {
		return releasePlan{}, fmt.Errorf("read plugin plan: %w", err)
	}
	rulePacks, err := readReleasePlan(rulePackPath)
	if err != nil {
		return releasePlan{}, fmt.Errorf("read rule-pack plan: %w", err)
	}
	return releasePlan{Plugins: plugins, RulePacks: rulePacks}, nil
}

func isReleasePush(eventName, ref string) bool {
	return eventName == "push" && strings.HasPrefix(ref, releaseTriggerPrefix) && len(ref) > len(releaseTriggerPrefix)
}

func successfulOrSkipped(status string) bool {
	return status == "success" || status == "skipped"
}

func isCatalogOnlyRelease(eventName, ref string) bool {
	return isReleasePush(eventName, ref) && strings.HasPrefix(ref, catalogOnlyTriggerPrefix) && len(ref) > len(catalogOnlyTriggerPrefix)
}

func centralPublishAllowed(eventName, ref string, hasPlugins, hasRulePacks bool, pluginBuild, rulePackBuild, provenance string) bool {
	if !isReleasePush(eventName, ref) || (!hasPlugins && !hasRulePacks) {
		return false
	}
	if isCatalogOnlyRelease(eventName, ref) {
		// A catalog-only republication must have rebuilt nothing: any non-skipped
		// build or provenance result means the gating drifted and the run is not
		// the republication it claims to be. Rule packs cannot be republished this
		// way because their assets are staged by the build jobs.
		return hasPlugins && !hasRulePacks &&
			pluginBuild == "skipped" && rulePackBuild == "skipped" && provenance == "skipped"
	}
	if hasPlugins && (pluginBuild != "success" || provenance != "success") {
		return false
	}
	if hasRulePacks && rulePackBuild != "success" {
		return false
	}
	return successfulOrSkipped(pluginBuild) && successfulOrSkipped(rulePackBuild) && successfulOrSkipped(provenance)
}

func validateRulePackTagVersion(path, expectedID, tagVersion string) error {
	content, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	var manifest rulePackManifest
	if err := json.Unmarshal(content, &manifest); err != nil {
		return fmt.Errorf("parse rule-pack manifest: %w", err)
	}
	if manifest.ID != expectedID || !componentID.MatchString(manifest.ID) {
		return fmt.Errorf("rule-pack manifest id %q does not match %q", manifest.ID, expectedID)
	}
	if !validComponentVersion(manifest.Version) || manifest.Version != tagVersion {
		return fmt.Errorf("rule-pack manifest version %q does not match tag version %q", manifest.Version, tagVersion)
	}
	return nil
}

func appendOutput(path string, plan releasePlan) error {
	if path == "" {
		return errors.New("github output path is required")
	}
	plugins := make([]pluginMatrixEntry, 0, len(plan.Plugins))
	for _, component := range plan.Plugins {
		plugins = append(plugins, pluginMatrixEntry{PluginID: component.ID, Version: component.Version})
	}
	rulePacks := make([]rulePackMatrixEntry, 0, len(plan.RulePacks))
	for _, component := range plan.RulePacks {
		rulePacks = append(rulePacks, rulePackMatrixEntry{RulePackID: component.ID, Version: component.Version})
	}
	pluginMatrix, err := json.Marshal(struct {
		Include []pluginMatrixEntry `json:"include"`
	}{Include: plugins})
	if err != nil {
		return err
	}
	rulePackMatrix, err := json.Marshal(struct {
		Include []rulePackMatrixEntry `json:"include"`
	}{Include: rulePacks})
	if err != nil {
		return err
	}
	output := strings.Join([]string{
		"plugin_matrix=" + string(pluginMatrix),
		"rule_pack_matrix=" + string(rulePackMatrix),
		fmt.Sprintf("has_plugins=%t", len(plan.Plugins) > 0),
		fmt.Sprintf("has_rule_packs=%t", len(plan.RulePacks) > 0),
		"",
	}, "\n")
	return appendFile(path, output)
}

func appendFile(path, content string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil && filepath.Dir(path) != "." {
		return err
	}
	file, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer file.Close()
	_, err = file.WriteString(content)
	return err
}

func main() {
	pluginPlan := flag.String("plugin-plan", "", "tab-separated plugin id/version plan")
	rulePackPlan := flag.String("rule-pack-plan", "", "tab-separated rule-pack id/version plan")
	githubOutput := flag.String("github-output", os.Getenv("GITHUB_OUTPUT"), "GitHub Actions output file")
	checkCentralGate := flag.Bool("check-central-gate", false, "exit successfully only when central publishing is allowed")
	eventName := flag.String("event-name", "", "GitHub event name for central publishing")
	ref := flag.String("ref", "", "GitHub ref for central publishing")
	hasPlugins := flag.Bool("has-plugins", false, "whether plugins are selected")
	hasRulePacks := flag.Bool("has-rule-packs", false, "whether rule packs are selected")
	pluginBuild := flag.String("plugin-build", "skipped", "plugin build job result")
	rulePackBuild := flag.String("rule-pack-build", "skipped", "rule-pack build job result")
	provenance := flag.String("provenance", "skipped", "plugin provenance job result")
	checkRulePackVersion := flag.Bool("check-rule-pack-version", false, "validate a component tag version against a rule-pack manifest")
	rulePackID := flag.String("rule-pack-id", "", "rule-pack id for manifest validation")
	tagVersion := flag.String("tag-version", "", "component tag version for manifest validation")
	manifestPath := flag.String("manifest", "", "rule-pack manifest path for version validation")
	centralAssetsDir := flag.String("central-assets-dir", "", "list files staged for central catalog publication")
	flag.Parse()
	if *centralAssetsDir != "" {
		assets, err := centralAssets(*centralAssetsDir)
		if err != nil {
			fmt.Fprintln(os.Stderr, "central assets:", err)
			os.Exit(1)
		}
		fmt.Println(strings.Join(assets, "\n"))
		return
	}
	if *checkCentralGate {
		if !centralPublishAllowed(*eventName, *ref, *hasPlugins, *hasRulePacks, *pluginBuild, *rulePackBuild, *provenance) {
			fmt.Fprintln(os.Stderr, "central publishing is not allowed for this workflow state")
			os.Exit(1)
		}
		return
	}
	if *checkRulePackVersion {
		if *rulePackID == "" || *tagVersion == "" || *manifestPath == "" {
			fmt.Fprintln(os.Stderr, "--rule-pack-id, --tag-version, and --manifest are required")
			os.Exit(2)
		}
		if err := validateRulePackTagVersion(*manifestPath, *rulePackID, *tagVersion); err != nil {
			fmt.Fprintln(os.Stderr, "rule-pack tag validation:", err)
			os.Exit(1)
		}
		return
	}
	if *pluginPlan == "" || *rulePackPlan == "" {
		fmt.Fprintln(os.Stderr, "--plugin-plan and --rule-pack-plan are required")
		os.Exit(2)
	}
	plan, err := loadReleasePlan(*pluginPlan, *rulePackPlan)
	if err == nil {
		err = appendOutput(*githubOutput, plan)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "release workflow plan:", err)
		os.Exit(1)
	}
}
