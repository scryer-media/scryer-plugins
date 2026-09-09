package scryer.rules.user.template

import rego.v1

# Numeric source and video-codec terms. Profile restrictions are enforced by the
# host; the guards below only prevent a disallowed value from earning a bonus.

native_sv_persona := lower(object.get(input.profile, "scoring_persona", "balanced"))

native_sv_source_weight := 150 if { native_sv_persona == "balanced" } else := 250 if { native_sv_persona == "audiophile" } else := 80 if { native_sv_persona == "efficient" } else := 120
native_sv_webdl_weight := 120 if { native_sv_persona == "balanced" } else := 100 if { native_sv_persona == "audiophile" } else := 150
native_sv_webrip_weight := 80 if { native_sv_persona == "balanced" } else := 50 if { native_sv_persona == "audiophile" } else := 120 if { native_sv_persona == "efficient" } else := 100
native_sv_hdtv_weight := 40 if { native_sv_persona == "balanced" } else := 10 if { native_sv_persona == "audiophile" } else := 60 if { native_sv_persona == "compatible" } else := 40
native_sv_high_codec_weight := 60 if { native_sv_persona == "balanced" } else := 150 if { native_sv_persona == "efficient" } else := 40 if { native_sv_persona == "compatible" } else := 60
native_sv_mid_codec_weight := 40 if { native_sv_persona == "balanced" } else := 30 if { native_sv_persona == "efficient" } else := 60 if { native_sv_persona == "compatible" } else := 40


native_sv_source_permitted if {
  source := input.release.source
  source != null
  blocklist := object.get(input.profile, "source_blocklist", [])
  not source in blocklist
  allowlist := object.get(input.profile, "source_allowlist", [])
  count(allowlist) == 0
}
native_sv_source_permitted if {
  source := input.release.source
  source != null
  allowlist := object.get(input.profile, "source_allowlist", [])
  source in allowlist
}

score_entry["source_bluray"] := native_sv_source_weight if {
  native_sv_source_permitted
  input.release.source in {"BluRay", "BR-DISK"}
}
score_entry["source_webdl"] := native_sv_webdl_weight if {
  native_sv_source_permitted
  input.release.source == "WEB-DL"
}
score_entry["source_webrip"] := native_sv_webrip_weight if {
  native_sv_source_permitted
  input.release.source == "WEBRip"
}
score_entry["source_hdtv"] := native_sv_hdtv_weight if {
  native_sv_source_permitted
  input.release.source == "HDTV"
}

score_entry["quality_unknown_allowed"] := 100 if {
  input.release.quality == null
  input.profile.allow_unknown_quality == true
}

native_sv_video_permitted if {
  codec := input.release.video_codec
  codec != null
  blocklist := object.get(input.profile, "video_codec_blocklist", [])
  not codec in blocklist
  allowlist := object.get(input.profile, "video_codec_allowlist", [])
  count(allowlist) == 0
}
native_sv_video_permitted if {
  codec := input.release.video_codec
  codec != null
  allowlist := object.get(input.profile, "video_codec_allowlist", [])
  codec in allowlist
}

score_entry[code] := max([80 - index * 20, 0]) if {
  native_sv_video_permitted
  allowlist := object.get(input.profile, "video_codec_allowlist", [])
  count(allowlist) > 0
  some index
  allowlist[index] == input.release.video_codec
  code := sprintf("video_codec_preferred_%d", [index])
}
score_entry["video_codec_quality_high"] := native_sv_high_codec_weight if {
  native_sv_video_permitted
  count(object.get(input.profile, "video_codec_allowlist", [])) == 0
  input.release.video_codec in {"H.265", "AV1", "VP9"}
}
score_entry["video_codec_quality_mid"] := native_sv_mid_codec_weight if {
  native_sv_video_permitted
  count(object.get(input.profile, "video_codec_allowlist", [])) == 0
  input.release.video_codec == "H.264"
}

