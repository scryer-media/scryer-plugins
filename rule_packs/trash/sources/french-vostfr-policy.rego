package scryer.rules.user.french_vostfr
import rego.v1

# MANAGED_TRASH_REGISTRY_VERSION=managed-trash-registry-v2
# TRASH_GUIDES_SOURCE_REVISION=31a2716d03a3f554a5a2a6bd76456109d900af05
# TRASH_SCORE_SET=french-vostfr/french-anime-vostfr
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
locale_intent if { has_required_audio(["fr", "fra", "fre", "french", "fr-fr", "fr-ca"]) }
locale_intent if { has_any_tag(["locale:fr", "locale:fr-fr", "locale:fr-ca"]) }

has_required_audio(values) if {
    some language in input.profile.required_audio_languages
    some value in values
    lower(language) == value
}

fr_fr_intent if {
    has_required_audio(["fr-fr"])
}

fr_fr_intent if {
    has_any_tag(["locale:fr-fr"])
}

fr_ca_intent if {
    has_required_audio(["fr-ca"])
}

fr_ca_intent if {
    has_any_tag(["locale:fr-ca"])
}

score_entry["trash_french_vostfr"] := 181 if {
    locale_intent
    has_fact("trash.locale.french.marker.vostfr")
}

has_fact(value) if {
    some fact in input.release.guide_facts
    lower(fact) == value
}

score_entry["trash_tier_1"] := 0 if {
    locale_intent
    has_fact("trash.locale.french.group.tier1")
}

score_entry["trash_tier_2"] := 0 if {
    locale_intent
    has_fact("trash.locale.french.group.tier2")
}

score_entry["trash_tier_3"] := 0 if {
    locale_intent
    has_fact("trash.locale.french.group.tier3")
}

score_entry["trash_lq"] := -10000 if {
    locale_intent
    has_fact("trash.locale.french.lq")
}

score_entry["trash_scene"] := 0 if {
    locale_intent
    has_fact("trash.locale.french.scene")
}

regional_reference if {
    has_fact("trash.locale.french.marker.vff")
}

regional_reference if {
    has_fact("trash.locale.french.marker.vfi")
}

regional_reference if {
    has_fact("trash.locale.french.marker.vof")
}

regional_quebec if {
    has_fact("trash.locale.french.marker.vfq")
}

regional_quebec if {
    has_fact("trash.locale.french.marker.vq")
}

regional_quebec if {
    has_fact("trash.locale.french.marker.voq")
}

# scryer-native score
score_entry["trash_french_fr_fr_reference"] := 40 if {
    fr_fr_intent
    regional_reference
}

# scryer-native score
score_entry["trash_french_fr_fr_quebec"] := -20 if {
    fr_fr_intent
    regional_quebec
}

# scryer-native score
score_entry["trash_french_fr_ca_reference"] := -20 if {
    fr_ca_intent
    regional_reference
}

# scryer-native score
score_entry["trash_french_fr_ca_quebec"] := 40 if {
    fr_ca_intent
    regional_quebec
}

has_audio_language(value) if {
    some language in input.release.languages_audio
    lower(language) == value
}

has_original_audio_language if {
    some language in input.release.languages_audio
    lower(language) == lower(input.context.inferred_original_audio_language)
}

# language-not-original (radarr)
trash_lang_not_original if {
    not has_original_audio_language
}

score_entry["trash_lang_not_original"] := -10000 if {
    locale_intent
    trash_lang_not_original
}
