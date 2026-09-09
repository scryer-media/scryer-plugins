package scryer.rules.user.template

import rego.v1

# Runtime-derived continuous size curve. The host owns the extreme-size veto;
# this template deliberately emits only the bounded numeric term.
native_size_persona := lower(object.get(input.profile, "scoring_persona", "balanced"))
native_size_override := object.get(object.get(input.profile, "scoring_overrides", {}), "prefer_compact_encodes", null)
native_size_curve_persona := "efficient" if { native_size_override == true } else := "balanced" if { native_size_persona == "efficient"; native_size_override == false } else := native_size_persona
native_size_category := "anime" if { lower(object.get(input.context, "category", "")) == "anime" } else := "series" if { lower(object.get(input.context, "category", "")) == "series" } else := "movie"

native_size_delta(name) := value if {
  table := {
    "balanced": {"excessive": -1200, "massive": -700, "very_large": -350, "large": -100, "expected": 120, "slightly_small": 0, "small": -125, "very_small": -400, "tiny": -800},
    "audiophile": {"excessive": -150, "massive": 700, "very_large": 500, "large": 350, "expected": 200, "slightly_small": 0, "small": -400, "very_small": -900, "tiny": -2000},
    "efficient": {"excessive": -250, "massive": -200, "very_large": -100, "large": 0, "expected": 200, "slightly_small": 300, "small": 100, "very_small": -200, "tiny": -800},
    "compatible": {"excessive": -250, "massive": 550, "very_large": 380, "large": 240, "expected": 120, "slightly_small": 0, "small": -700, "very_small": -1300, "tiny": -2500},
  }[native_size_curve_persona]
  value := table[name]
}
native_size_movie_2160_bitrate := 32 if { native_size_persona == "balanced" } else := 57
native_size_quality := upper(value) if { value := object.get(input.release, "quality", null); value != null } else := "UNKNOWN"
native_size_bitrate := value if {
  table := {
    "movie": {"4320P": 142.5, "2160P": native_size_movie_2160_bitrate, "1080P": 9.1, "720P": 3.4, "576P": 2.4, "480P": 1.4, "unknown": 6.8},
    "series": {"4320P": 55, "2160P": 22, "1080P": 8.5, "720P": 3.3, "576P": 2.4, "480P": 1.4, "unknown": 5.5},
    "anime": {"4320P": 70, "2160P": 28, "1080P": 8.5, "720P": 3.4, "576P": 2.4, "480P": 1.4, "unknown": 5.7},
  }[native_size_category]
  value := object.get(table, native_size_quality, table["unknown"])
}
native_size_codec_factor := 0.5 if { input.release.video_codec == "AV1" } else := 0.75 if { input.release.video_codec in {"H.265", "VP9"} } else := 1.1 if { input.release.video_codec == "H.264" } else := 1
native_size_remux_factor := 1.45 if { input.profile.prefer_remux == true } else := 1 if { native_size_persona == "balanced" } else := 1.45
native_size_bluray_factor := 1.35 if { input.release.source in {"BluRay", "BR-DISK"} } else := 1
native_size_remux_source_factor := native_size_remux_factor if {
  input.release.is_remux == true
  native_size_category != "anime"
} else := 1
native_size_disk_factor := 1.8 if { input.release.is_bd_disk == true } else := 1
native_size_web_factor := 0.8 if { input.release.source in {"WEB-DL", "WEBRip"} } else := 1
native_size_source_factor := native_size_bluray_factor * native_size_remux_source_factor * native_size_disk_factor * native_size_web_factor
native_size_default_runtime := 120 if { native_size_category == "movie" } else := 45 if { native_size_category == "series" } else := 24
native_size_runtime(field) := value if { candidate := object.get(input.context, field, null); candidate != null; candidate > 0; value := candidate } else := native_size_default_runtime
native_size_expected_gib(field) := max([native_size_bitrate * native_size_codec_factor * native_size_source_factor * (native_size_runtime(field) * 60) / 8 / 1024, 0.5])
native_size_total_ratio := input.release.size_bytes / 1073741824 / native_size_expected_gib("coverage_total_runtime_minutes")
native_size_member_ratio := input.release.size_bytes / 1073741824 / native_size_expected_gib("coverage_member_runtime_minutes")
native_size_member_count := value if { value := object.get(input.context, "coverage_member_count", null); value != null } else := 1

native_size_thresholds := [8, 4, 2.4, 1.8, 1.35, 1, 0.75, 0.55, 0.35, 0.1] if { native_size_category == "movie" } else := [8, 4, 2.4, 1.8, 1.35, 1, 0.75, 0.55, 0.35, 0.04] if { native_size_category == "series" } else := [6, 2.5, 2.1, 1.6, 1.2, 0.85, 0.65, 0.5, 0.3, 0.04]
native_size_upper_multiplier := 1.5 if { input.release.video_codec == "AV1" } else := 1
native_size_threshold(index) := native_size_thresholds[index] * native_size_upper_multiplier if { index <= 5 } else := native_size_thresholds[index]
native_size_anchor_values := [0.1870828693, 0.4387482194, 0.6422616289, 0.8660254038, 1.1618950039, 1.5588457268, 2.0784609691, 3.0983866769, 5.6568542495] if { native_size_category == "movie"; input.release.video_codec != "AV1" }
native_size_anchor_values := [0.1183215957, 0.4387482194, 0.6422616289, 0.8660254038, 1.1618950039, 1.5588457268, 2.0784609691, 3.0983866769, 5.6568542495] if { native_size_category == "series"; input.release.video_codec != "AV1" }
native_size_anchor_values := [0.1095445115, 0.3872983346, 0.5700877125, 0.7433034374, 1.0099504938, 1.3856406461, 1.8330302779, 2.2912878475, 3.8729833462] if { native_size_category == "anime"; input.release.video_codec != "AV1" }
native_size_anchor_values := [0.1870828693, 0.4387482194, 0.6422616289, 1.0606601718, 1.7428425058, 2.3382685902, 3.1176914536, 4.6475800154, 8.4852813742] if { native_size_category == "movie"; input.release.video_codec == "AV1" }
native_size_anchor_values := [0.1183215957, 0.4387482194, 0.6422616289, 1.0606601718, 1.7428425058, 2.3382685902, 3.1176914536, 4.6475800154, 8.4852813742] if { native_size_category == "series"; input.release.video_codec == "AV1" }
native_size_anchor_values := [0.1095445115, 0.3872983346, 0.5700877125, 0.9096702699, 1.5149257408, 2.0784609691, 2.7495454169, 3.4369317712, 5.8094750193] if { native_size_category == "anime"; input.release.video_codec == "AV1" }
native_size_anchor_deltas := [native_size_delta("tiny"), native_size_delta("very_small"), native_size_delta("small"), native_size_delta("slightly_small"), native_size_delta("expected"), native_size_delta("large"), native_size_delta("very_large"), native_size_delta("massive"), native_size_delta("excessive")]
native_size_log_series(x) := 2 * (z + z*z*z/3 + z*z*z*z*z/5 + z*z*z*z*z*z*z/7 + z*z*z*z*z*z*z*z*z/9 + z*z*z*z*z*z*z*z*z*z*z/11 + z*z*z*z*z*z*z*z*z*z*z*z*z/13 + z*z*z*z*z*z*z*z*z*z*z*z*z*z*z/15) if { z := (x - 1) / (x + 1) }
native_size_log(x) := native_size_log_series(x * 4) - 1.3862943611198906 if { x < 0.25 } else := native_size_log_series(x * 2) - 0.6931471805599453 if { x < 0.5 } else := native_size_log_series(x) if { x < 2 } else := native_size_log_series(x / 2) + 0.6931471805599453 if { x < 4 } else := native_size_log_series(x / 4) + 1.3862943611198906 if { x < 8 } else := native_size_log_series(x / 8) + 2.0794415416798357
native_size_curve_delta(ratio) := native_size_anchor_deltas[0] if { ratio <= native_size_anchor_values[0] } else := native_size_anchor_deltas[8] if { ratio >= native_size_anchor_values[8] } else := round(native_size_anchor_deltas[index] + ((native_size_log(ratio) - native_size_log(native_size_anchor_values[index])) / (native_size_log(native_size_anchor_values[index + 1]) - native_size_log(native_size_anchor_values[index]))) * (native_size_anchor_deltas[index + 1] - native_size_anchor_deltas[index])) if { some index; index < 8; ratio > native_size_anchor_values[index]; ratio <= native_size_anchor_values[index + 1] }
native_size_code(ratio) := "size_excessive_for_quality" if { ratio >= native_size_threshold(1) } else := "size_massive_for_quality" if { ratio >= native_size_threshold(2) } else := "size_very_large_for_quality" if { ratio >= native_size_threshold(3) } else := "size_large_for_quality" if { ratio >= native_size_threshold(4) } else := "size_expected_for_quality" if { ratio >= native_size_threshold(5) } else := "size_slightly_small_for_quality" if { ratio >= native_size_threshold(6) } else := "size_small_for_quality" if { ratio >= native_size_threshold(7) } else := "size_very_small_for_quality" if { ratio >= native_size_threshold(8) } else := "size_tiny_for_quality"

native_size_member_basis if {
  native_size_total_ratio < native_size_threshold(8)
  native_size_member_count > 1
  native_size_member_ratio >= native_size_threshold(7)
  native_size_member_ratio < native_size_threshold(2)
}
score_entry[native_size_code(native_size_member_ratio)] := min([native_size_curve_delta(native_size_member_ratio), 0]) if { input.release.size_bytes != null; input.release.size_bytes > 0; native_size_member_basis }
score_entry["size_pack_member_basis"] := 0 if { input.release.size_bytes != null; input.release.size_bytes > 0; native_size_member_basis }
score_entry[native_size_code(native_size_total_ratio)] := native_size_curve_delta(native_size_total_ratio) if { input.release.size_bytes != null; input.release.size_bytes > 0; not native_size_member_basis }

