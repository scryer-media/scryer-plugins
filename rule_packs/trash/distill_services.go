package main

import (
	"path"
	"sort"
	"strings"
)

type serviceSpec struct {
	service   string
	web       bool
	excluded  map[string]bool
	overrides map[string]bool
}
type serviceAliasRow struct {
	Token                string `json:"token"`
	Service              string `json:"service"`
	RequiresWebAdjacency bool   `json:"requires_web_adjacency"`
}

var serviceSpecs = map[string]serviceSpec{
	"amzn": {"Amazon", false, nil, nil}, "atvp": {"Apple TV+", false, nil, nil}, "cr": {"Crunchyroll", false, nil, nil}, "dsnp": {"Disney+", false, nil, nil}, "funi": {"Funimation", false, nil, nil}, "hbo": {"HBO Max", false, nil, nil}, "hidive": {"HIDIVE", false, nil, nil}, "hmax": {"HBO Max", false, nil, nil}, "hulu": {"Hulu", false, nil, nil}, "max": {"HBO Max", false, nil, nil}, "nf": {"Netflix", false, nil, nil}, "pcok": {"Peacock", false, nil, nil}, "pmtp": {"Paramount+", false, nil, nil}, "stan": {"Stan", false, nil, nil}, "4od": {"Channel 4", false, nil, nil}, "abema": {"ABEMA", false, nil, nil}, "all4": {"Channel 4", false, nil, nil}, "atv": {"ATV", false, nil, nil}, "aubc": {"ABC iview", false, nil, nil}, "bcore": {"BCORE", false, map[string]bool{"CORE": true}, nil}, "bglobal": {"B-Global", false, nil, nil}, "bilibili": {"Bilibili", false, nil, nil}, "cbc": {"CBC Gem", false, nil, nil}, "cnlp": {"CANAL+", true, nil, nil}, "cpng": {"Coupang Play", false, nil, nil}, "crav": {"Crave", true, nil, nil}, "dcu": {"DC Universe", false, map[string]bool{"DC": true}, nil}, "dmm-tv": {"DMM TV", false, nil, nil}, "dscp": {"Discovery+", true, map[string]bool{"DISC": true, "DCP": true}, nil}, "fod": {"FOD", false, nil, nil}, "french-adn": {"ADN", false, nil, nil}, "french-salto": {"Salto", false, nil, nil}, "french-wkn": {"Wakanim", false, nil, nil}, "friday": {"friDay Video", true, nil, nil}, "hami": {"Hami Video", false, nil, nil}, "htsr": {"Disney+ Hotstar", false, map[string]bool{"HS": true}, nil}, "ip": {"BBC iPlayer", true, nil, map[string]bool{"IPLAYER": false}}, "iqiy": {"iQIYI", true, map[string]bool{"IQ": true}, nil}, "it": {"iTunes", true, map[string]bool{"ITUNES": true}, nil}, "itvx": {"ITVX", true, nil, nil}, "kcw": {"KOCOWA", false, nil, nil}, "kktv": {"KKTV", false, nil, nil}, "linetv": {"LINE TV", false, nil, nil}, "my5": {"My5", false, nil, nil}, "mytvsuper": {"myTV SUPER", false, nil, nil}, "nlz": {"NLZiet", false, nil, nil}, "now": {"NOW", true, nil, nil}, "ovid": {"OVID.tv", false, nil, nil}, "pathe": {"Pathé Thuis", false, nil, nil}, "play": {"PLAY", true, nil, nil}, "qibi": {"Quibi", false, nil, nil}, "red": {"YouTube Premium", true, map[string]bool{"YOUTUBE": true}, nil}, "roku": {"The Roku Channel", false, nil, nil}, "sho": {"Showtime", true, nil, map[string]bool{"SHOWTIME": false}}, "strp": {"Star+", false, nil, nil}, "syfy": {"SYFY", false, nil, nil}, "tver": {"TVer", false, nil, nil}, "tving": {"TVING", true, nil, nil}, "vdl": {"Videoland", false, nil, nil}, "viki": {"Viki", false, nil, nil}, "viu": {"Viu", true, nil, nil}, "vrv": {"VRV", false, nil, nil}, "wavve": {"Wavve", false, nil, nil}, "wetv": {"WeTV", false, nil, nil}, "youku": {"Youku", false, nil, nil},
}

func distillServiceAliases(raw rawUpstreamSnapshot) ([]serviceAliasRow, []string) {
	seen := map[string]serviceAliasRow{}
	var ignored []string
	for _, file := range raw.Files {
		stem := strings.TrimSuffix(path.Base(file.Path), ".json")
		spec, ok := serviceSpecs[stem]
		if !ok {
			continue
		}
		for _, record := range file.Records {
			for _, rule := range record.Specifications {
				if rule.Implementation != "ReleaseTitleSpecification" || isTrue(rule.Negate) {
					ignored = append(ignored, file.Path+":"+rule.Name+":requires_positive_title")
					continue
				}
				pattern, err := groupPattern(rule.Fields)
				if err != nil {
					ignored = append(ignored, file.Path+":"+rule.Name+":invalid")
					continue
				}
				if strings.Contains(pattern, "(?!") || strings.Contains(pattern, "(?<!") {
					ignored = append(ignored, file.Path+":"+rule.Name+":negative_lookaround")
					continue
				}
				values, _ := finiteGroupLiterals(pattern)
				if len(values) == 0 {
					ignored = append(ignored, file.Path+":"+rule.Name+":nonlossless")
					continue
				}
				for _, value := range values {
					token := aliasToken(value)
					if token == "" || spec.excluded[token] {
						continue
					}
					web := spec.web
					if override, ok := spec.overrides[token]; ok {
						web = override
					}
					row := serviceAliasRow{token, spec.service, web}
					seen[token+"|"+row.Service+"|"+string(rune(boolInt(web)))] = row
				}
			}
		}
	}
	// Curated supplements retained by the upstream Rust table where the source
	// expression is intentionally not finite enough for literal expansion.
	for _, row := range []serviceAliasRow{{"BBCI", "BBC iPlayer", false}, {"BBC", "BBC iPlayer", false}, {"CNLP", "CANAL+", true}, {"DNSP", "Disney+", false}, {"FRIDAY", "friDay Video", true}, {"HBOMAX", "HBO Max", false}, {"HBOM", "HBO Max", false}, {"HOTSTAR", "Hotstar", false}, {"IQIYI", "iQIYI", true}, {"IQIY", "iQIYI", true}, {"ITUNES", "iTunes", false}, {"IT", "iTunes", true}, {"YOUTUBE", "YouTube", false}} {
		seen[row.Token+"|"+row.Service+"|"+string(rune(boolInt(row.RequiresWebAdjacency)))] = row
	}
	out := make([]serviceAliasRow, 0, len(seen))
	for _, row := range seen {
		out = append(out, row)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Token+out[i].Service < out[j].Token+out[j].Service })
	return out, ignored
}
func aliasToken(value string) string {
	first := strings.FieldsFunc(value, func(r rune) bool {
		return !((r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9'))
	})
	if len(first) == 0 {
		return ""
	}
	token := strings.ToUpper(first[0])
	if len(token) < 2 {
		return ""
	}
	has := false
	for _, r := range token {
		if r >= 'A' && r <= 'Z' {
			has = true
		}
	}
	if !has {
		return ""
	}
	return token
}
func boolInt(v bool) int {
	if v {
		return 1
	}
	return 0
}
