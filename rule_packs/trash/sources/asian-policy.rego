package scryer.rules.user.asian
import rego.v1

# MANAGED_TRASH_REGISTRY_VERSION=managed-trash-registry-v2
# TRASH_GUIDES_SOURCE_REVISION=31a2716d03a3f554a5a2a6bd76456109d900af05
# TRASH_SCORE_SET=default/default
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
locale_intent if { has_any_tag(["locale:asian"]) }



has_fact(value) if {
    some fact in input.release.guide_facts
    lower(fact) == value
}

score_entry["trash_tier_1"] := 240 if {
    locale_intent
    has_fact("trash.locale.asian.group.tier1")
}

score_entry["trash_tier_2"] := 236 if {
    locale_intent
    has_fact("trash.locale.asian.group.tier2")
}

score_entry["trash_tier_3"] := 100 if {
    locale_intent
    has_fact("trash.locale.asian.group.tier3")
}

score_entry["trash_lq"] := -10000 if {
    locale_intent
    has_fact("trash.locale.asian.lq")
}





