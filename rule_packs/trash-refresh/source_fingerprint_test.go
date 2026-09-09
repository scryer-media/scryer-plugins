package main

import "testing"

func TestSourceFingerprintIgnoresOnlyNonExecutableText(t *testing.T) {
	first, err := sourceFingerprintText("# revision abc\nscore_entry[\"name #1\"] := 10 # trailing\n")
	if err != nil {
		t.Fatal(err)
	}
	second, err := sourceFingerprintText("# revision def\nscore_entry [ \"name #1\" ]:=10\n")
	if err != nil {
		t.Fatal(err)
	}
	if first != second {
		t.Fatal("comment/whitespace caused an update")
	}
	for _, source := range []string{"score_entry[\"name #2\"] := 10", "score_entry[\"name #1\"] := 11", "score_entry[`name #1`] := 10"} {
		changed, err := sourceFingerprintText(source)
		if err != nil {
			t.Fatal(err)
		}
		if first == changed {
			t.Fatalf("executable change was ignored: %s", source)
		}
	}
}

func TestSourceFingerprintHandlesEscapedQuotesAndRejectsUnterminatedStrings(t *testing.T) {
	if _, err := sourceFingerprintText("x := \"a\\\"#b\" # comment"); err != nil {
		t.Fatal(err)
	}
	for _, source := range []string{"x := \"oops", "x := `oops"} {
		if _, err := sourceFingerprintText(source); err == nil {
			t.Fatalf("accepted %q", source)
		}
	}
}

func TestNextPatchRequiresStableVersionAndChecksOverflow(t *testing.T) {
	got, err := nextPatch("1.2.9")
	if err != nil || got != "1.2.10" {
		t.Fatalf("got %q, %v", got, err)
	}
	for _, version := range []string{"1.2", "1.2.3-beta", "1.02.3", "-1.2.3", "1.2.18446744073709551615"} {
		if _, err := nextPatch(version); err == nil {
			t.Fatalf("accepted %q", version)
		}
	}
}
