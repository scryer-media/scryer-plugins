package scryer.rules.user.german
import rego.v1

# MANAGED_TRASH_REGISTRY_VERSION=managed-trash-registry-v2
# TRASH_GUIDES_SOURCE_REVISION=31a2716d03a3f554a5a2a6bd76456109d900af05
# TRASH_SCORE_SET=german/german-anime
# Offline community-pack spike exported from Scryer's locale policy generator.
# Scores are normalized TRaSH Guides scores unless marked scryer-native.

has_any_tag(values) if {
    some tag in input.context.tags
    some value in values
    lower(tag) == value
}

default configured_tags := []
locale_intent if { count(configured_tags) == 0 }
locale_intent if { has_any_tag(configured_tags) }
locale_intent if { has_required_audio(["de", "deu", "ger", "german", "de-de"]) }
locale_intent if { has_any_tag(["locale:de", "locale:de-de"]) }

has_required_audio(values) if {
    some language in input.profile.required_audio_languages
    some value in values
    lower(language) == value
}

has_fact(value) if {
    some fact in input.release.guide_facts
    lower(fact) == value
}

score_entry["trash_tier_1"] := 151 if {
    locale_intent
    has_fact("trash.locale.german.group.tier1")
}

score_entry["trash_tier_2"] := 146 if {
    locale_intent
    has_fact("trash.locale.german.group.tier2")
}

score_entry["trash_tier_3"] := 143 if {
    locale_intent
    has_fact("trash.locale.german.group.tier3")
}

score_entry["trash_lq"] := -10000 if {
    locale_intent
    has_fact("trash.locale.german.lq")
}

score_entry["trash_scene"] := 141 if {
    locale_intent
    has_fact("trash.locale.german.scene")
}

score_entry["trash_german_subbed"] := 329 if {
    locale_intent
    has_fact("trash.locale.german.marker.subbed")
}

has_audio_language(value) if {
    some language in input.release.languages_audio
    lower(language) == value
}

has_original_audio_language if {
    some language in input.release.languages_audio
    lower(language) == lower(input.context.inferred_original_audio_language)
}

# not-german-japanese-korean-chinese-or-english (radarr)
trash_lang_not_german_japanese_korean_chinese_or_english if {
    not has_audio_language("deu")
    not has_audio_language("eng")
    not has_audio_language("jpn")
    not has_audio_language("kor")
    not has_audio_language("zho")
}

score_entry["trash_lang_not_german_japanese_korean_chinese_or_english"] := -10000 if {
    locale_intent
    trash_lang_not_german_japanese_korean_chinese_or_english
}

# not-german-japanese-or-english (radarr)
trash_lang_not_german_japanese_or_english if {
    not has_audio_language("deu")
    not has_audio_language("eng")
    not has_audio_language("jpn")
}

score_entry["trash_lang_not_german_japanese_or_english"] := -10000 if {
    locale_intent
    trash_lang_not_german_japanese_or_english
}

# not-german-or-english (radarr)
trash_lang_not_german_or_english if {
    not has_audio_language("deu")
    not has_audio_language("eng")
}

score_entry["trash_lang_not_german_or_english"] := -10000 if {
    locale_intent
    trash_lang_not_german_or_english
}
