package main

import (
	"encoding/json"
	"fmt"
	"unicode"
)

// Preserve source tokens while ignoring comments and whitespace. This is a
// change detector, not a Rego validator: the host engine validates the artifact.
// In particular a provenance comment changing does not create a new version.
func sourceFingerprintText(source string) (string, error) {
	characters := []rune(source)
	var tokens []string
	for index := 0; index < len(characters); {
		character := characters[index]
		if unicode.IsSpace(character) {
			index++
			continue
		}
		if character == '#' {
			for index < len(characters) && characters[index] != '\n' {
				index++
			}
			continue
		}
		start := index
		index++
		if character == '"' || character == '`' {
			closed := false
			for index < len(characters) {
				current := characters[index]
				index++
				if character == '"' && current == '\\' {
					if index == len(characters) {
						break
					}
					index++
					continue
				}
				if current == character {
					closed = true
					break
				}
			}
			if !closed {
				return "", fmt.Errorf("unterminated string at character %d", start)
			}
		} else if unicode.IsLetter(character) || unicode.IsDigit(character) || character == '_' {
			for index < len(characters) {
				next := characters[index]
				if !unicode.IsLetter(next) && !unicode.IsDigit(next) && next != '_' {
					break
				}
				index++
			}
		}
		tokens = append(tokens, string(characters[start:index]))
	}
	encoded, err := json.Marshal(tokens)
	return string(encoded), err
}
