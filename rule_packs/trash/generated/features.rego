package scryer.rules.user.template

import rego.v1

native_feature_persona := lower(object.get(input.profile, "scoring_persona", "balanced"))
native_feature_remux_bonus := 250 if { native_feature_persona == "balanced" } else := 400 if { native_feature_persona == "audiophile" } else := 0
native_feature_remux_missing := -75 if { native_feature_persona == "balanced" } else := -80 if { native_feature_persona == "audiophile" } else := 0
native_feature_remux_unpreferred := -400 if { native_feature_persona == "balanced" } else := 0
native_feature_revision := 30 if { native_feature_persona != "audiophile" } else := 50

score_entry["prefer_remux_match"] := native_feature_remux_bonus if {
  input.release.is_remux == true
  input.profile.prefer_remux == true
  native_feature_remux_bonus != 0
}
score_entry["remux_not_preferred"] := native_feature_remux_unpreferred if {
  input.release.is_remux == true
  input.profile.prefer_remux != true
  native_feature_remux_unpreferred != 0
}
score_entry["prefer_remux_missing"] := native_feature_remux_missing if {
  input.release.is_remux != true
  input.profile.prefer_remux == true
  native_feature_remux_missing != 0
}

score_entry["managed_dual_audio_preferred"] := 200 if {
  input.release.is_dual_audio == true
  input.profile.prefer_dual_audio == true
}
score_entry["proper_upload"] := native_feature_revision if { input.release.is_proper_upload == true }
score_entry["repack_upload"] := native_feature_revision if { input.release.is_repack == true }

