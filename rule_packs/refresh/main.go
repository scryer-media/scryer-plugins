// Command refresh validates the bounded changes made by the scheduled rule-pack
// refresh workflows. It deliberately has no network client: upstream
// acquisition remains the responsibility of each reviewed pack converter.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
)

// packProfile pins one automated refresh: the rule-pack id the generated
// manifest must carry and the exact committed paths a refresh may rewrite.
type packProfile struct {
	ID      string
	Allowed map[string]struct{}
}

var profiles = map[string]packProfile{
	"trash": {
		ID: "trash-guides-scoring-pack",
		Allowed: pathSet(
			"rule_packs/trash-scoring.json",
			"rule_packs/trash-scoring-coverage.json",
			"rule_packs/trash/snapshot/core-snapshot.json",
			"rule_packs/trash/snapshot/detection-snapshot.json",
			"rule_packs/trash/snapshot/upstream-coverage.json",
			"rule_packs/trash/snapshot/upstream-raw.json",
			"rule_packs/trash/generated/asian.rego",
			"rule_packs/trash/generated/audio.rego",
			"rule_packs/trash/generated/editions-anime.rego",
			"rule_packs/trash/generated/features.rego",
			"rule_packs/trash/generated/french-vf.rego",
			"rule_packs/trash/generated/french-vo.rego",
			"rule_packs/trash/generated/french-vostfr.rego",
			"rule_packs/trash/generated/german.rego",
			"rule_packs/trash/generated/groups.rego",
			"rule_packs/trash/generated/hdr.rego",
			"rule_packs/trash/generated/size.rego",
			"rule_packs/trash/generated/source-video.rego",
			"rule_packs/trash/generated/streaming.rego",
			"rule_packs/trash/generated/unwanted.rego",
		),
	},
	"seadex": {
		ID: "seadex-scoring-pack",
		Allowed: pathSet(
			"rule_packs/seadex-scoring.json",
			"rule_packs/seadex-scoring.rego",
			"rule_packs/seadex-coverage.json",
			"rule_packs/seadex/snapshot.json.gz",
		),
	},
}

func pathSet(paths ...string) map[string]struct{} {
	set := make(map[string]struct{}, len(paths))
	for _, path := range paths {
		set[path] = struct{}{}
	}
	return set
}

func profileFlag(fs *flag.FlagSet) *string {
	return fs.String("profile", "", "refresh profile: trash or seadex")
}

func lookupProfile(name string) (packProfile, error) {
	profile, ok := profiles[name]
	if !ok {
		return packProfile{}, fmt.Errorf("unknown refresh profile %q; expected trash or seadex", name)
	}
	return profile, nil
}

type pack struct {
	ID      string `json:"id"`
	Version string `json:"version"`
	Rules   []rule `json:"rules"`
}

type rule struct {
	ID              string   `json:"id"`
	RegoSource      string   `json:"regoSource"`
	AppliedFacets   []string `json:"appliedFacets,omitempty"`
	EvaluationPhase string   `json:"evaluationPhase"`
	DefaultEnabled  bool     `json:"defaultEnabled"`
	ExclusiveGroup  string   `json:"exclusiveGroup,omitempty"`
	Customizable    bool     `json:"customizable"`
}

type semanticRule struct {
	ID              string   `json:"id"`
	RegoSource      string   `json:"regoSource"`
	AppliedFacets   []string `json:"appliedFacets,omitempty"`
	EvaluationPhase string   `json:"evaluationPhase"`
	DefaultEnabled  bool     `json:"defaultEnabled"`
	ExclusiveGroup  string   `json:"exclusiveGroup,omitempty"`
	Customizable    bool     `json:"customizable"`
}

func main() {
	if len(os.Args) < 2 {
		fatal("usage: refresh fingerprint|next-version|verify-diff --profile trash|seadex")
	}
	var err error
	switch os.Args[1] {
	case "fingerprint":
		err = fingerprintCommand(os.Args[2:])
	case "next-version":
		err = nextVersionCommand(os.Args[2:])
	case "verify-diff":
		err = verifyDiffCommand(os.Args[2:])
	default:
		err = fmt.Errorf("unknown command %q", os.Args[1])
	}
	if err != nil {
		fatal(err.Error())
	}
}

func fatal(message string) { fmt.Fprintln(os.Stderr, message); os.Exit(1) }

func nextPatch(version string) (string, error) {
	parts := strings.Split(version, ".")
	if len(parts) != 3 {
		return "", fmt.Errorf("expected a stable numeric version, got %q", version)
	}
	var numbers [3]uint64
	for index, part := range parts {
		value, err := strconv.ParseUint(part, 10, 64)
		if err != nil || strconv.FormatUint(value, 10) != part {
			return "", fmt.Errorf("invalid stable version %q", version)
		}
		numbers[index] = value
	}
	if numbers[2] == ^uint64(0) {
		return "", errors.New("patch version overflow")
	}
	return fmt.Sprintf("%d.%d.%d", numbers[0], numbers[1], numbers[2]+1), nil
}

func nextVersionCommand(args []string) error {
	fs := flag.NewFlagSet("next-version", flag.ContinueOnError)
	path := fs.String("pack", "", "generated rule-pack JSON")
	profileName := profileFlag(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}
	profile, err := lookupProfile(*profileName)
	if err != nil {
		return err
	}
	data, err := os.ReadFile(*path)
	if err != nil {
		return err
	}
	var current pack
	if err := json.Unmarshal(data, &current); err != nil {
		return err
	}
	if current.ID != profile.ID {
		return fmt.Errorf("unexpected rule-pack id %q, want %q", current.ID, profile.ID)
	}
	version, err := nextPatch(current.Version)
	if err != nil {
		return err
	}
	fmt.Println(version)
	return nil
}

func fingerprintCommand(args []string) error {
	fs := flag.NewFlagSet("fingerprint", flag.ContinueOnError)
	path := fs.String("pack", "", "generated rule-pack JSON")
	profileName := profileFlag(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *path == "" {
		return errors.New("fingerprint requires --pack")
	}
	profile, err := lookupProfile(*profileName)
	if err != nil {
		return err
	}
	fingerprint, err := semanticFingerprintFile(profile, *path)
	if err != nil {
		return err
	}
	fmt.Println(fingerprint)
	return nil
}

func verifyDiffCommand(args []string) error {
	fs := flag.NewFlagSet("verify-diff", flag.ContinueOnError)
	base := fs.String("base", "", "Git revision to compare with HEAD")
	profileName := profileFlag(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *base == "" {
		return errors.New("verify-diff requires --base")
	}
	profile, err := lookupProfile(*profileName)
	if err != nil {
		return err
	}
	changed, err := gitNULPaths("diff", "--name-only", "-z", *base, "--")
	if err != nil {
		return err
	}
	if len(changed) == 0 {
		return errors.New("refresh produced no committed changes")
	}
	for _, path := range changed {
		if _, ok := profile.Allowed[path]; !ok {
			return fmt.Errorf("refresh changed non-generated path %q", path)
		}
	}
	untracked, err := gitNULPaths("status", "--porcelain=v1", "-z")
	if err != nil {
		return err
	}
	for _, entry := range untracked {
		if strings.HasPrefix(entry, "?? ") {
			return fmt.Errorf("refresh produced untracked path %q", strings.TrimPrefix(entry, "?? "))
		}
	}
	return nil
}

func semanticFingerprintFile(profile packProfile, path string) (string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	return semanticFingerprint(profile, data)
}

func semanticFingerprint(profile packProfile, data []byte) (string, error) {
	var decoded pack
	if err := json.Unmarshal(data, &decoded); err != nil {
		return "", err
	}
	if decoded.ID != profile.ID {
		return "", fmt.Errorf("unexpected rule-pack id %q, want %q", decoded.ID, profile.ID)
	}
	rules := make([]semanticRule, 0, len(decoded.Rules))
	for _, item := range decoded.Rules {
		source, err := sourceFingerprintText(item.RegoSource)
		if err != nil {
			return "", fmt.Errorf("rule %s: %w", item.ID, err)
		}
		facets := append([]string(nil), item.AppliedFacets...)
		sort.Strings(facets)
		rules = append(rules, semanticRule{
			ID: item.ID, RegoSource: source, AppliedFacets: facets,
			EvaluationPhase: item.EvaluationPhase, DefaultEnabled: item.DefaultEnabled,
			ExclusiveGroup: item.ExclusiveGroup, Customizable: item.Customizable,
		})
	}
	sort.Slice(rules, func(i, j int) bool { return rules[i].ID < rules[j].ID })
	canonical, err := json.Marshal(rules)
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(canonical)
	return hex.EncodeToString(digest[:]), nil
}

func gitNULPaths(args ...string) ([]string, error) {
	output, err := gitOutput(args...)
	if err != nil {
		return nil, err
	}
	return nulPaths(output), nil
}

func nulPaths(output []byte) []string {
	if len(output) == 0 {
		return nil
	}
	parts := strings.Split(string(output), "\x00")
	return parts[:len(parts)-1]
}

func gitOutput(args ...string) ([]byte, error) {
	command := exec.Command("git", args...)
	output, err := command.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("git %s: %w: %s", strings.Join(args, " "), err, strings.TrimSpace(string(output)))
	}
	return output, nil
}

func rel(path string) string { return filepath.ToSlash(filepath.Clean(path)) }
