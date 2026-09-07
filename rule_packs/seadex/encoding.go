package main

import (
	"encoding/json"
	"fmt"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

const (
	templateEscape      = "¦"
	templatePlaceholder = "§"
	tupleChunkBytes     = 400
	jsonWrapColumn      = 500
)

var encodedNumericToken = regexp.MustCompile(`[0-9a-f]{8}|[0-9]+`)

// EncodingStats describes the physical policy representation. It excludes
// report-only provenance and ambiguity details, which remain in MatchReport.
type EncodingStats struct {
	DirectBuckets   IndexStats `json:"direct_buckets"`
	DirectEntries   int        `json:"direct_entries"`
	RouteBuckets    int        `json:"route_buckets"`
	SourceBytes     int        `json:"source_bytes"`
	SourceLines     int        `json:"source_lines"`
	TemplateBuckets IndexStats `json:"template_buckets"`
	TemplateGroups  int        `json:"template_groups"`
	TupleChunks     int        `json:"tuple_chunks"`
}

// IndexStats measures the concrete length-indexed maps evaluated by the
// policy. BucketCount is per rendered table, so strict and tolerant buckets
// with the same length remain separate runtime objects.
type IndexStats struct {
	BucketCount int     `json:"bucket_count"`
	KeyCount    int     `json:"key_count"`
	MaxKeys     int     `json:"max_keys"`
	MeanKeys    float64 `json:"mean_keys"`
}

type encodedGroup struct {
	captures [][]string
	code     byte
	skeleton string
	tuples   map[string]bool
}

type encodedTable struct {
	direct map[string]map[string]string
	routes map[string]map[string]any
}

func renderEncodedPolicy(tables Tables) (string, EncodingStats, error) {
	strict, strictStats, err := encodeTable(tables.Strict, collisionScores(tables.StrictCollisions))
	if err != nil {
		return "", EncodingStats{}, err
	}
	tolerant, tolerantStats, err := encodeTable(tables.Tolerant)
	if err != nil {
		return "", EncodingStats{}, err
	}
	strictDirect, err := renderRawBuckets(strict.direct)
	if err != nil {
		return "", EncodingStats{}, err
	}
	strictRoutes, err := renderRouteBuckets(strict.routes)
	if err != nil {
		return "", EncodingStats{}, err
	}
	tolerantDirect, err := renderRawBuckets(tolerant.direct)
	if err != nil {
		return "", EncodingStats{}, err
	}
	tolerantRoutes, err := renderRouteBuckets(tolerant.routes)
	if err != nil {
		return "", EncodingStats{}, err
	}
	policy := strings.NewReplacer(
		"__STRICT_DIRECT__", strictDirect,
		"__STRICT_ROUTES__", strictRoutes,
		"__TOLERANT_DIRECT__", tolerantDirect,
		"__TOLERANT_ROUTES__", tolerantRoutes,
	).Replace(policyTemplate)
	stats := EncodingStats{
		DirectBuckets:   indexStats(strict.direct, tolerant.direct),
		DirectEntries:   strictStats.DirectEntries + tolerantStats.DirectEntries,
		RouteBuckets:    strictStats.RouteBuckets + tolerantStats.RouteBuckets,
		SourceBytes:     len(policy),
		SourceLines:     strings.Count(policy, "\n") + 1,
		TemplateBuckets: indexStats(strict.routes, tolerant.routes),
		TemplateGroups:  strictStats.TemplateGroups + tolerantStats.TemplateGroups,
		TupleChunks:     strictStats.TupleChunks + tolerantStats.TupleChunks,
	}
	return policy, stats, nil
}

func indexStats[V any](tables ...map[string]map[string]V) IndexStats {
	stats := IndexStats{}
	for _, table := range tables {
		for _, bucket := range table {
			stats.BucketCount++
			stats.KeyCount += len(bucket)
			if len(bucket) > stats.MaxKeys {
				stats.MaxKeys = len(bucket)
			}
		}
	}
	if stats.BucketCount > 0 {
		stats.MeanKeys = float64(stats.KeyCount) / float64(stats.BucketCount)
	}
	return stats
}

// encodingStats is intentionally separate from generation callers so coverage
// reporting can record physical policy characteristics without writing output.
func encodingStats(tables Tables) (EncodingStats, error) {
	_, stats, err := renderEncodedPolicy(tables)
	return stats, err
}

func encodeTable(valueMaps ...map[string]int) (encodedTable, EncodingStats, error) {
	groups := map[string]*encodedGroup{}
	for _, values := range valueMaps {
		for key, value := range values {
			code := scoreCode(value)
			skeleton, captures := encodedTemplateKey(key)
			tuple := strings.Join(captures, ",")
			groupKey := string(code) + "\x00" + skeleton
			group := groups[groupKey]
			if group == nil {
				group = &encodedGroup{code: code, skeleton: skeleton, tuples: map[string]bool{}}
				groups[groupKey] = group
			}
			if group.tuples[tuple] {
				return encodedTable{}, EncodingStats{}, fmt.Errorf("duplicate encoded tuple for %q", key)
			}
			group.tuples[tuple] = true
			group.captures = append(group.captures, captures)
		}
	}
	result := encodedTable{direct: map[string]map[string]string{}, routes: map[string]map[string]any{}}
	stats := EncodingStats{}
	groupKeys := sortedMapKeys(groups)
	for _, groupKey := range groupKeys {
		group := groups[groupKey]
		if len(group.tuples) == 1 {
			for tuple := range group.tuples {
				key, err := restoreEncodedTemplate(group.skeleton, tupleCaptures(tuple))
				if err != nil {
					return encodedTable{}, EncodingStats{}, err
				}
				bucket := bucketIdentifier(key)
				if result.direct[bucket] == nil {
					result.direct[bucket] = map[string]string{}
				}
				result.direct[bucket][string(group.code)+key] = string(group.code) + blobKey(key)
				stats.DirectEntries++
			}
			continue
		}
		stats.TemplateGroups++
		constants, chunks, err := encodedGroupData(group)
		if err != nil {
			return encodedTable{}, EncodingStats{}, err
		}
		routeKey := string(group.code) + "\t" + group.skeleton
		bucket := bucketIdentifier(routeKey)
		if result.routes[bucket] == nil {
			result.routes[bucket] = map[string]any{}
		}
		result.routes[bucket][routeKey] = []any{constants, chunks}
		stats.TupleChunks += len(chunks)
	}
	stats.RouteBuckets = len(result.routes)
	return result, stats, nil
}

func scoreCode(value int) byte {
	if value == 400 {
		return 'B'
	}
	if value == 1 {
		return 'X'
	}
	return 'L'
}

func collisionScores(values map[string]bool) map[string]int {
	result := make(map[string]int, len(values))
	for key, value := range values {
		if value {
			result[key] = 1
		}
	}
	return result
}

func encodedTemplateKey(key string) (string, []string) {
	key = strings.ReplaceAll(key, templateEscape, templateEscape+templateEscape)
	key = strings.ReplaceAll(key, templatePlaceholder, templateEscape+templatePlaceholder)
	locations := encodedNumericToken.FindAllStringIndex(key, -1)
	if len(locations) == 0 {
		return key, nil
	}
	var skeleton strings.Builder
	captures := make([]string, 0, len(locations))
	previous := 0
	for _, location := range locations {
		skeleton.WriteString(key[previous:location[0]])
		skeleton.WriteString(templatePlaceholder)
		captures = append(captures, key[location[0]:location[1]])
		previous = location[1]
	}
	skeleton.WriteString(key[previous:])
	return skeleton.String(), captures
}

func restoreEncodedTemplate(skeleton string, captures []string) (string, error) {
	var output strings.Builder
	runes := []rune(skeleton)
	captureIndex := 0
	for index := 0; index < len(runes); index++ {
		current := string(runes[index])
		if current == templateEscape && index+1 < len(runes) {
			next := string(runes[index+1])
			if next == templateEscape || next == templatePlaceholder {
				output.WriteRune(runes[index+1])
				index++
				continue
			}
		}
		if current == templatePlaceholder {
			if captureIndex >= len(captures) {
				return "", fmt.Errorf("template capture count is too short")
			}
			output.WriteString(captures[captureIndex])
			captureIndex++
			continue
		}
		output.WriteRune(runes[index])
	}
	if captureIndex != len(captures) {
		return "", fmt.Errorf("template capture count is too long")
	}
	return output.String(), nil
}

func tupleCaptures(tuple string) []string {
	if tuple == "" {
		return nil
	}
	return strings.Split(tuple, ",")
}

func encodedGroupData(group *encodedGroup) ([]string, []string, error) {
	if len(group.captures) == 0 {
		return nil, nil, fmt.Errorf("template group has no captures")
	}
	width := len(group.captures[0])
	constants := append([]string(nil), group.captures[0]...)
	isConstant := make([]bool, width)
	for index := range isConstant {
		isConstant[index] = true
	}
	for _, captures := range group.captures[1:] {
		if len(captures) != width {
			return nil, nil, fmt.Errorf("template capture widths differ")
		}
		for index, value := range captures {
			if constants[index] != value {
				isConstant[index] = false
			}
		}
	}
	for index := range constants {
		if !isConstant[index] {
			constants[index] = ""
		}
	}
	tuples := make([]string, 0, len(group.captures))
	seen := map[string]bool{}
	for _, captures := range group.captures {
		variables := make([]string, 0, width)
		for index, value := range captures {
			if !isConstant[index] {
				variables = append(variables, value)
			}
		}
		tuple := strings.Join(variables, ",")
		if seen[tuple] {
			return nil, nil, fmt.Errorf("correlated template tuple collision")
		}
		seen[tuple] = true
		tuples = append(tuples, tuple)
	}
	sort.Strings(tuples)
	return constants, sharedTupleChunks(tuples, tupleChunkBytes), nil
}

func sharedTupleChunks(tuples []string, maximum int) []string {
	chunks := make([]string, 0)
	var chunk strings.Builder
	chunk.WriteByte('\n')
	for _, tuple := range tuples {
		addition := tuple + "\n"
		if chunk.Len() > 1 && chunk.Len()+len(addition) > maximum {
			chunks = append(chunks, chunk.String())
			chunk.Reset()
			chunk.WriteByte('\n')
		}
		chunk.WriteString(addition)
	}
	if chunk.Len() > 1 {
		chunks = append(chunks, chunk.String())
	}
	return chunks
}

func blobKey(key string) string {
	key = strings.ReplaceAll(key, "%", "%25")
	key = strings.ReplaceAll(key, "`", "%60")
	key = strings.ReplaceAll(key, "\n", "%0A")
	return strings.ReplaceAll(key, "\r", "%0D")
}

func renderRawBuckets(values map[string]map[string]string) (string, error) {
	keys := sortedMapKeys(values)
	var output strings.Builder
	output.WriteString("{")
	for _, bucket := range keys {
		entries := values[bucket]
		entryKeys := sortedMapKeys(entries)
		var blob strings.Builder
		for _, entry := range entryKeys {
			blob.WriteByte('\n')
			blob.WriteString(entries[entry])
			blob.WriteByte('\n')
		}
		for _, line := range strings.Split(blob.String(), "\n") {
			if len(line) > 1024 {
				return "", fmt.Errorf("raw bucket %s line exceeds 1024 bytes", bucket)
			}
		}
		output.WriteString("\n  ")
		output.WriteString(strconv.Quote(bucket))
		output.WriteString(": `")
		output.WriteString(blob.String())
		output.WriteString("`,")
	}
	if len(keys) > 0 {
		output.WriteByte('\n')
	}
	output.WriteString("}")
	return output.String(), nil
}

func renderRouteBuckets(values map[string]map[string]any) (string, error) {
	keys := sortedMapKeys(values)
	var output strings.Builder
	output.WriteString("{")
	for _, bucket := range keys {
		payload, err := json.Marshal(values[bucket])
		if err != nil {
			return "", err
		}
		wrapped, err := wrapRouteJSON(string(payload), jsonWrapColumn)
		if err != nil {
			return "", fmt.Errorf("route bucket %s: %w", bucket, err)
		}
		encoded := strings.ReplaceAll(wrapped, "`", `\u0060`)
		for _, line := range strings.Split(encoded, "\n") {
			if len(line) > 1024 {
				return "", fmt.Errorf("route bucket %s line exceeds 1024 bytes", bucket)
			}
		}
		output.WriteString("\n  ")
		output.WriteString(strconv.Quote(bucket))
		output.WriteString(": `")
		output.WriteString(encoded)
		output.WriteString("`,")
	}
	if len(keys) > 0 {
		output.WriteByte('\n')
	}
	output.WriteString("}")
	return output.String(), nil
}

func wrapRouteJSON(value string, target int) (string, error) {
	var output strings.Builder
	column := 0
	inString := false
	escaped := false
	for index := 0; index < len(value); index++ {
		character := value[index]
		output.WriteByte(character)
		column++
		if inString {
			if escaped {
				escaped = false
				continue
			}
			if character == '\\' {
				escaped = true
			} else if character == '"' {
				inString = false
			}
			continue
		}
		if character == '"' {
			inString = true
			continue
		}
		if column >= target && strings.ContainsRune(",:[{]}", rune(character)) {
			output.WriteByte('\n')
			column = 0
		}
	}
	if inString || escaped {
		return "", fmt.Errorf("invalid JSON string state")
	}
	return output.String(), nil
}

func sortedMapKeys[V any](values map[string]V) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
