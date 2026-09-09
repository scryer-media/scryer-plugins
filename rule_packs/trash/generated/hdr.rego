package scryer.rules.user.template

import rego.v1

native_hdr_persona := lower(object.get(input.profile, "scoring_persona", "balanced"))
native_hdr_quality := upper(value) if { value := object.get(input.release, "quality", null); value != null } else := ""
native_hdr_dv := 50 if { native_hdr_persona in {"balanced", "efficient"} } else := 150 if { native_hdr_persona == "audiophile" } else := -50
native_hdr_hdr10 := 30 if { native_hdr_persona == "balanced" } else := 50 if { native_hdr_persona == "audiophile" } else := 20 if { native_hdr_persona == "efficient" } else := 40
native_hdr_sdr_4k := -150 if { native_hdr_persona == "balanced" } else := -300 if { native_hdr_persona == "audiophile" } else := -80 if { native_hdr_persona == "efficient" } else := 0

score_entry["dolby_vision_bonus"] := native_hdr_dv if {
  input.release.is_dolby_vision == true
  input.profile.dolby_vision_allowed == true
  native_hdr_dv != 0
}
score_entry["hdr_bonus"] := native_hdr_hdr10 if {
  input.release.detected_hdr == true
  input.profile.detected_hdr_allowed == true
  native_hdr_hdr10 != 0
}
score_entry["sdr_at_4k"] := native_hdr_sdr_4k if {
  contains(native_hdr_quality, "2160")
  input.release.detected_hdr != true
  native_hdr_sdr_4k != 0
}

