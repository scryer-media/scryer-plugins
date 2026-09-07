// Command validation exercises a pack with Scryer's existing rule engine offline.
// It copies the rule crate to temporary storage; neither checkout is modified.
package main

import (
	"bytes"
	"crypto/sha256"
	_ "embed"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
)

//go:embed engine_test.rs.txt
var engineTest string

func read(path string) (string, error) { b, err := os.ReadFile(path); return string(b), err }
func write(path, value string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	return os.WriteFile(path, []byte(value), 0644)
}

func copyTree(source, target string) error {
	return filepath.WalkDir(source, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(source, path)
		if err != nil {
			return err
		}
		dest := filepath.Join(target, rel)
		if d.IsDir() {
			return os.MkdirAll(dest, 0755)
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		return os.WriteFile(dest, data, 0644)
	})
}

func prepare(scryer, scratch string, experiment bool) error {
	root, err := read(filepath.Join(scryer, "Cargo.toml"))
	if err != nil {
		return err
	}
	crate := filepath.Join(scryer, "crates/scryer-rules")
	manifest, err := read(filepath.Join(crate, "Cargo.toml"))
	if err != nil {
		return err
	}
	depRE := regexp.MustCompile(`(?m)^([a-zA-Z0-9_-]+)\s*=\s*\{\s*workspace\s*=\s*true\s*\}`)
	section := strings.SplitN(root, "[workspace.dependencies]", 2)
	if len(section) != 2 {
		return fmt.Errorf("missing workspace.dependencies")
	}
	dependencies := strings.SplitN(section[1], "\n[", 2)[0]
	var declarations []string
	for _, match := range depRE.FindAllStringSubmatch(manifest, -1) {
		re := regexp.MustCompile(`(?m)^` + regexp.QuoteMeta(match[1]) + `\s*=.*$`)
		line := re.FindString(dependencies)
		if line == "" {
			return fmt.Errorf("missing single-line workspace dependency %s", match[1])
		}
		declarations = append(declarations, line)
	}
	sort.Strings(declarations)
	edition := regexp.MustCompile(`(?m)^edition\s*=\s*"[0-9]+"`).FindString(root)
	if edition == "" {
		return fmt.Errorf("missing workspace edition")
	}
	minimal := "[workspace]\nmembers = [\"crates/scryer-rules\"]\nresolver = \"2\"\n\n[workspace.package]\n" + edition + "\n\n[workspace.dependencies]\n" + strings.Join(declarations, "\n") + "\n"
	if err := write(filepath.Join(scratch, "Cargo.toml"), minimal); err != nil {
		return err
	}
	lock, err := read(filepath.Join(scryer, "Cargo.lock"))
	if err != nil {
		return err
	}
	if err := write(filepath.Join(scratch, "Cargo.lock"), lock); err != nil {
		return err
	}
	target := filepath.Join(scratch, "crates/scryer-rules")
	if err := copyTree(crate, target); err != nil {
		return err
	}
	validation, err := read(filepath.Join(crate, "src/validation.rs"))
	if err != nil {
		return err
	}
	parts := strings.SplitN(validation, "fn synthetic_test_input()", 2)
	if len(parts) != 2 {
		return fmt.Errorf("Scryer validation fixture changed; update harness")
	}
	sample := "fn synthetic_test_input()" + strings.SplitN(parts[1], "#[cfg(test)]", 2)[0]
	if err := write(filepath.Join(target, "tests/seadex_pack.rs"), strings.ReplaceAll(engineTest, "__SYNTHETIC_INPUT__", sample)); err != nil {
		return err
	}
	if !experiment {
		return nil
	}
	config := `regorus::PolicyLengthConfig { max_file_bytes: std::num::NonZeroUsize::new(16 * 1024 * 1024).unwrap(), max_lines: std::num::NonZeroUsize::new(200_000).unwrap(), ..Default::default() }`
	for _, relative := range []string{"src/lib.rs", "src/validation.rs"} {
		path := filepath.Join(target, relative)
		source, err := read(path)
		if err != nil {
			return err
		}
		source = strings.ReplaceAll(source, "let mut engine = Engine::new();", "let mut engine = Engine::new();\nengine.set_policy_length_config("+config+");")
		source = strings.ReplaceAll(source, "Source::from_contents(policy_path.to_string(), rego_source.to_string())", `Source::from_contents_with_limits(policy_path.to_string(), rego_source.to_string(), std::num::NonZeroUsize::new(16 * 1024 * 1024).unwrap(), std::num::NonZeroUsize::new(200_000).unwrap())`)
		if err := write(path, source); err != nil {
			return err
		}
	}
	return nil
}

// Compare exact package identities after Cargo prunes the copied workspace lock.
func packageIdentities(lock string) map[string]bool {
	result := map[string]bool{}
	field := regexp.MustCompile(`(?m)^(name|version|source|checksum) = .*$`)
	for _, p := range strings.Split(lock, "[[package]]")[1:] {
		result[strings.Join(field.FindAllString(p, -1), "\n")] = true
	}
	return result
}

func treeDigest(root string) (string, error) {
	digest := sha256.New()
	err := filepath.WalkDir(root, func(path string, entry os.DirEntry, err error) error {
		if err != nil || entry.IsDir() {
			return err
		}
		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		fmt.Fprintf(digest, "%s\x00", filepath.ToSlash(rel))
		digest.Write(data)
		digest.Write([]byte{0})
		return nil
	})
	return fmt.Sprintf("%x", digest.Sum(nil)), err
}

func run() error {
	scryer := flag.String("scryer", "", "path to unchanged Scryer checkout")
	jobs := flag.String("jobs", "", "JSON jobs with pack/source and expected cases")
	source := flag.String("source", "", "optional Rego source override for every job; leaves original jobs untouched")
	maxAddedRSS := flag.Uint64("max-added-rss-mib", 0, "require loaded and post-batch RSS growth below this MiB limit (one job only; 0 disables)")
	memoryPhase := flag.String("memory-phase", "installation", "installation includes validation; runtime measures loading already-validated rules")
	report := flag.String("report", "", "output measurements JSON")
	experiment := flag.Bool("experiment-policy-limits", false, "TEMPORARY COPY ONLY: try 16 MiB / 200,000 lines; not unchanged-host compatibility")
	flag.Parse()
	if *memoryPhase != "installation" && *memoryPhase != "runtime" {
		return fmt.Errorf("--memory-phase must be installation or runtime")
	}
	if *scryer == "" || *jobs == "" || *report == "" {
		return fmt.Errorf("--scryer, --jobs, and --report are required")
	}
	var err error
	*scryer, err = filepath.Abs(*scryer)
	if err != nil {
		return err
	}
	*jobs, err = filepath.Abs(*jobs)
	if err != nil {
		return err
	}
	*report, err = filepath.Abs(*report)
	if err != nil {
		return err
	}
	scratch, err := os.MkdirTemp("", "seadex-engine-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(scratch)
	if *source != "" || *maxAddedRSS > 0 {
		data, err := os.ReadFile(*jobs)
		if err != nil {
			return err
		}
		var selected []map[string]any
		if err := json.Unmarshal(data, &selected); err != nil {
			return err
		}
		if *maxAddedRSS > 0 && len(selected) != 1 {
			return fmt.Errorf("RSS acceptance requires exactly one job in a fresh test process")
		}
		if *source != "" {
			absolute, err := filepath.Abs(*source)
			if err != nil {
				return err
			}
			for _, job := range selected {
				delete(job, "pack")
				job["source"] = absolute
			}
			data, err = json.Marshal(selected)
			if err != nil {
				return err
			}
			*jobs = filepath.Join(scratch, "jobs.json")
			if err := os.WriteFile(*jobs, data, 0644); err != nil {
				return err
			}
		}
	}
	git := exec.Command("rtk", "proxy", "git", "rev-parse", "HEAD")
	git.Dir = *scryer
	revision, err := git.Output()
	if err != nil {
		return err
	}
	if err := prepare(*scryer, scratch, *experiment); err != nil {
		return err
	}
	crateDigest, err := treeDigest(filepath.Join(scratch, "crates/scryer-rules"))
	if err != nil {
		return err
	}
	oldLock, err := read(filepath.Join(scratch, "Cargo.lock"))
	if err != nil {
		return err
	}
	cmd := exec.Command("rtk", "proxy", "cargo", "nextest", "run", "--offline", "--no-fail-fast", "--cargo-profile", "release", "-p", "scryer-rules", "--test", "seadex_pack")
	cmd.Dir = scratch
	cmd.Env = append(os.Environ(), "SEADEX_ENGINE_JOBS="+*jobs, "SEADEX_ENGINE_REPORT="+filepath.Join(scratch, "report.json"), "SEADEX_MEMORY_PHASE="+*memoryPhase, "CARGO_TARGET_DIR="+filepath.Join(os.TempDir(), "seadex-engine-target"))
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	testErr := cmd.Run()
	newLock, err := read(filepath.Join(scratch, "Cargo.lock"))
	if err != nil {
		return err
	}
	original := packageIdentities(oldLock)
	for identity := range packageIdentities(newLock) {
		if !original[identity] {
			return fmt.Errorf("validation resolved a package outside the existing lockfile: %s", identity)
		}
	}
	result, err := os.ReadFile(filepath.Join(scratch, "report.json"))
	if err != nil {
		result = []byte("[]")
		if testErr == nil {
			return err
		}
	}
	var memoryErr error
	if *maxAddedRSS > 0 && testErr == nil {
		var measurements []map[string]json.RawMessage
		if err := json.Unmarshal(result, &measurements); err != nil {
			return err
		}
		if len(measurements) != 1 {
			return fmt.Errorf("missing single-job RSS measurement")
		}
		var baseline uint64
		if err := json.Unmarshal(measurements[0]["rss_before_source_kib"], &baseline); err != nil || baseline == 0 {
			return fmt.Errorf("missing RSS baseline")
		}
		for _, phase := range []string{"rss_loaded_engine_kib", "rss_after_batch_kib"} {
			var measured uint64
			if err := json.Unmarshal(measurements[0][phase], &measured); err != nil || measured == 0 {
				return fmt.Errorf("missing %s measurement", phase)
			}
			if measured > baseline && float64(measured-baseline)/1024 >= float64(*maxAddedRSS) {
				memoryErr = fmt.Errorf("%s adds %.2f MiB RSS; required less than %d MiB", phase, float64(measured-baseline)/1024, *maxAddedRSS)
				break
			}
		}
	}
	output, err := json.MarshalIndent(map[string]any{"scryer_revision": strings.TrimSpace(string(revision)), "copied_rule_crate_sha256": crateDigest, "passed": testErr == nil && memoryErr == nil, "correctness_passed": testErr == nil, "max_added_rss_mib": *maxAddedRSS, "memory_phase": *memoryPhase, "experimental_limits": *experiment, "jobs": json.RawMessage(result)}, "", "  ")
	if err != nil {
		return err
	}
	if err := write(*report, string(output)+"\n"); err != nil {
		return err
	}
	_, err = os.Stdout.Write(append(bytes.Clone(output), '\n'))
	if testErr != nil {
		return testErr
	}
	if memoryErr != nil {
		return memoryErr
	}
	return err
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}
