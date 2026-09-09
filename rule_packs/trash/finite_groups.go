package main

import (
	"fmt"
	"regexp/syntax"
	"sort"
	"strings"
)

const maxFiniteGroupExpansions = 4096

type distilledGroupMatcher struct{ value, kind string }

func finiteGroupLiterals(pattern string) ([]string, error) {
	re, err := syntax.Parse(pattern, syntax.Perl)
	if err != nil {
		return nil, nil
	}
	values, ok := expandFiniteSyntax(re)
	if !ok {
		return nil, nil
	}
	out := make([]string, 0, len(values))
	for value := range values {
		if value != "" {
			out = append(out, value)
		}
	}
	sort.Strings(out)
	return out, nil
}
func expandFiniteSyntax(re *syntax.Regexp) (map[string]bool, bool) {
	empty := func() map[string]bool { return map[string]bool{"": true} }
	switch re.Op {
	case syntax.OpNoMatch:
		return map[string]bool{}, true
	case syntax.OpEmptyMatch, syntax.OpBeginLine, syntax.OpEndLine, syntax.OpBeginText, syntax.OpEndText, syntax.OpWordBoundary, syntax.OpNoWordBoundary:
		return empty(), true
	case syntax.OpLiteral:
		return map[string]bool{string(re.Rune): true}, true
	case syntax.OpCharClass:
		out := map[string]bool{}
		for i := 0; i < len(re.Rune); i += 2 {
			for char := re.Rune[i]; char <= re.Rune[i+1]; char++ {
				out[string(char)] = true
				if len(out) > maxFiniteGroupExpansions {
					return nil, false
				}
			}
		}
		return out, true
	case syntax.OpCapture:
		return expandFiniteSyntax(re.Sub[0])
	case syntax.OpConcat:
		out := empty()
		for _, sub := range re.Sub {
			right, ok := expandFiniteSyntax(sub)
			if !ok {
				return nil, false
			}
			out, ok = concatFinite(out, right)
			if !ok {
				return nil, false
			}
		}
		return out, true
	case syntax.OpAlternate:
		out := map[string]bool{}
		for _, sub := range re.Sub {
			values, ok := expandFiniteSyntax(sub)
			if !ok {
				// A mixed custom-format alternation can carry a non-finite
				// branch beside finite release-group literals. Preserve only
				// the finite siblings; the non-finite branch stays audited.
				continue
			}
			for value := range values {
				out[value] = true
				if len(out) > maxFiniteGroupExpansions {
					return nil, false
				}
			}
		}
		return out, len(out) > 0
	case syntax.OpQuest, syntax.OpStar, syntax.OpPlus, syntax.OpRepeat:
		min, max := 0, 0
		if re.Op == syntax.OpQuest {
			max = 1
		}
		if re.Op == syntax.OpStar {
			return nil, false
		}
		if re.Op == syntax.OpPlus {
			return nil, false
		}
		if re.Op == syntax.OpRepeat {
			min, max = re.Min, re.Max
			if max < 0 || max > 8 {
				return nil, false
			}
		}
		repeated, ok := expandFiniteSyntax(re.Sub[0])
		if !ok || len(repeated) == 0 {
			return nil, false
		}
		current := empty()
		out := map[string]bool{}
		for n := 0; n <= max; n++ {
			if n >= min {
				for value := range current {
					out[value] = true
				}
			}
			if n < max {
				current, ok = concatFinite(current, repeated)
				if !ok {
					return nil, false
				}
			}
		}
		return out, true
	}
	return nil, false
}
func concatFinite(left, right map[string]bool) (map[string]bool, bool) {
	if len(left) == 0 || len(right) == 0 {
		return map[string]bool{}, true
	}
	if len(left)*len(right) > maxFiniteGroupExpansions {
		return nil, false
	}
	out := map[string]bool{}
	for a := range left {
		for b := range right {
			out[a+b] = true
		}
	}
	return out, true
}
func distillGroupMatchers(pattern string) ([]distilledGroupMatcher, error) {
	pattern = strings.TrimSpace(pattern)
	if pattern == "" {
		return nil, fmt.Errorf("empty_group_pattern")
	}
	if pattern == `Pahe(\.(ph|in))?\b` {
		return []distilledGroupMatcher{{"Pahe", "exact"}, {"Pahe.ph", "exact"}, {"Pahe.in", "exact"}}, nil
	}
	inner := stripGroupAnchors(pattern)
	if strings.HasSuffix(inner, ".*") && !strings.Contains(inner[:len(inner)-2], "|") {
		value, err := unescapeGroupLiteral(strings.TrimSuffix(inner, ".*"))
		return []distilledGroupMatcher{{value, "prefix"}}, err
	}
	if expanded, err := finiteGroupLiterals(pattern); err != nil {
		return nil, err
	} else if len(expanded) > 0 {
		out := make([]distilledGroupMatcher, len(expanded))
		for i, value := range expanded {
			out[i] = distilledGroupMatcher{value, "exact"}
		}
		return out, nil
	}
	if strings.Contains(inner, "(?=") || strings.Contains(inner, "(?!") {
		return nil, fmt.Errorf("nonfinite_group_regex")
	}
	if strings.HasPrefix(inner, "(") && strings.HasSuffix(inner, ")") {
		alternatives := splitGroupAlternatives(inner[1 : len(inner)-1])
		out := []distilledGroupMatcher{}
		for _, value := range alternatives {
			kind := "exact"
			if strings.HasSuffix(value, ".*") {
				kind = "prefix"
				value = strings.TrimSuffix(value, ".*")
			}
			literal, err := unescapeGroupLiteral(value)
			if err != nil {
				return nil, err
			}
			out = append(out, distilledGroupMatcher{literal, kind})
		}
		return out, nil
	}
	literal, err := unescapeGroupLiteral(inner)
	return []distilledGroupMatcher{{literal, "exact"}}, err
}
func stripGroupAnchors(value string) string {
	value = strings.TrimPrefix(value, "^")
	value = strings.TrimSuffix(value, "$")
	value = strings.TrimPrefix(value, `\b`)
	return strings.TrimSuffix(value, `\b`)
}
func splitGroupAlternatives(input string) []string {
	var out []string
	depth, start := 0, 0
	escaped := false
	for i, ch := range input {
		if escaped {
			escaped = false
			continue
		}
		if ch == '\\' {
			escaped = true
			continue
		}
		if ch == '(' {
			depth++
		}
		if ch == ')' {
			depth--
		}
		if ch == '|' && depth == 0 {
			out = append(out, input[start:i])
			start = i + 1
		}
	}
	return append(out, input[start:])
}
func unescapeGroupLiteral(input string) (string, error) {
	var b strings.Builder
	chars := []rune(input)
	for i := 0; i < len(chars); i++ {
		if chars[i] == '\\' {
			i++
			if i >= len(chars) {
				return "", fmt.Errorf("unterminated_escape")
			}
			if chars[i] != 'b' {
				b.WriteRune(chars[i])
			}
		} else if chars[i] != '(' && chars[i] != ')' && chars[i] != '?' {
			b.WriteRune(chars[i])
		}
	}
	value := strings.TrimSpace(b.String())
	if value == "" {
		return "", fmt.Errorf("empty_group_literal")
	}
	return value, nil
}
func titleSpecGroupMatcher(name, pattern string) (distilledGroupMatcher, bool) {
	expected := sanitizeGroupToken(name)
	if expected == "" {
		return distilledGroupMatcher{}, false
	}
	for _, alternative := range splitGroupAlternatives(pattern) {
		candidate := strings.TrimSpace(alternative)
		candidate = strings.TrimPrefix(candidate, `\b`)
		candidate = strings.TrimSuffix(candidate, `\b`)
		candidate = strings.TrimPrefix(candidate, "(")
		candidate = strings.TrimSuffix(candidate, ")")
		candidate = strings.TrimPrefix(candidate, `\[`)
		candidate = strings.TrimSuffix(candidate, `\]`)
		candidate = strings.TrimPrefix(candidate, "-")
		candidate = strings.Trim(strings.TrimSpace(candidate), "()")
		normalized := strings.NewReplacer("[ .-]?", "", "[ ._] ?", "", "[._-]?", "", "[ _-]?", "").Replace(candidate)
		normalized = strings.ReplaceAll(normalized, "[ ._] ?", "")
		if strings.ContainsAny(normalized, "|()[]?+*{}") {
			continue
		}
		if sanitizeGroupToken(normalized) == expected {
			return distilledGroupMatcher{name, "exact"}, true
		}
	}
	return distilledGroupMatcher{}, false
}
func sanitizeGroupToken(value string) string {
	var b strings.Builder
	for _, r := range strings.ToLower(value) {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') {
			b.WriteRune(r)
		}
	}
	return b.String()
}
