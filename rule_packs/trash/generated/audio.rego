package scryer.rules.user.template

import rego.v1

native_audio_persona := lower(object.get(input.profile, "scoring_persona", "balanced"))
native_audio_overrides := object.get(input.profile, "scoring_overrides", {})
native_audio_lossless_default := native_audio_persona == "audiophile"
native_audio_lossless_override := object.get(native_audio_overrides, "prefer_lossless_audio", null)
native_audio_lossless_effective := true if { native_audio_lossless_override == true }
native_audio_lossless_effective := false if { native_audio_lossless_override == false }
native_audio_lossless_effective := native_audio_lossless_default if { native_audio_lossless_override == null }
native_audio_lossless_changed if { native_audio_lossless_effective != native_audio_lossless_default }
native_audio_persona_map := {"balanced": {"TRUEHD": 60, "DTSX": 60, "DTSMA": 40, "FLAC": 60, "DDP": 40, "EAC3": 40, "DTSHD": 40, "DTS": 40, "AC3": 20, "AAC": 20, "MP3": 0, "OPUS": 0}, "audiophile": {"TRUEHD": 300, "DTSX": 360, "DTSMA": 260, "FLAC": 240, "DDP": 80, "EAC3": 80, "DTSHD": 70, "DTS": 50, "AC3": 15, "AAC": 10, "MP3": -50, "OPUS": 5}, "efficient": {"TRUEHD": 80, "DTSX": 90, "DTSMA": 70, "FLAC": 60, "DDP": 100, "EAC3": 100, "DTSHD": 50, "DTS": 50, "AC3": 30, "AAC": 40, "MP3": -10, "OPUS": 35}, "compatible": {"TRUEHD": 60, "DTSX": 70, "DTSMA": 50, "FLAC": 40, "DDP": 120, "EAC3": 120, "DTSHD": 40, "DTS": 50, "AC3": 60, "AAC": 80, "MP3": 10, "OPUS": 30}}[native_audio_persona]
native_audio_persona_weight(codec) := 400 if { native_audio_persona == "audiophile"; codec == "TRUEHD"; input.release.is_atmos == true } else := 100 if { native_audio_persona == "efficient"; codec == "TRUEHD"; input.release.is_atmos == true } else := 80 if { native_audio_persona == "compatible"; codec == "TRUEHD"; input.release.is_atmos == true } else := 60 if { native_audio_persona == "balanced"; codec == "TRUEHD"; input.release.is_atmos == true } else := 150 if { native_audio_persona == "audiophile"; codec in {"DDP", "EAC3"}; input.release.is_atmos == true } else := 110 if { native_audio_persona == "efficient"; codec in {"DDP", "EAC3"}; input.release.is_atmos == true } else := 130 if { native_audio_persona == "compatible"; codec in {"DDP", "EAC3"}; input.release.is_atmos == true } else := object.get(native_audio_persona_map, codec, 0)
native_audio_lossless_weight(codec) := value if { table := {"TRUEHD": 400, "DTSX": 360, "DTSMA": 260, "FLAC": 240}; native_audio_lossless_effective == true; value := table[codec] } else := value if { table := {"TRUEHD": 60, "DTSX": 60, "DTSMA": 40, "FLAC": 60}; native_audio_lossless_effective == false; value := table[codec] }
native_audio_weight(codec) := native_audio_lossless_weight(codec) if { native_audio_lossless_changed; codec in {"TRUEHD", "DTSX", "DTSMA", "FLAC"} } else := native_audio_persona_weight(codec)
native_audio_all_blocklisted(codecs, blocklist) if { every candidate in codecs { candidate in blocklist } }
native_audio_permitted if { codecs := input.release.audio_codecs; count(codecs) > 0; allowlist := object.get(input.profile, "audio_codec_allowlist", []); count(allowlist) == 0; not native_audio_all_blocklisted(codecs, object.get(input.profile, "audio_codec_blocklist", [])) }
native_audio_permitted if { codecs := input.release.audio_codecs; allowlist := object.get(input.profile, "audio_codec_allowlist", []); some codec in codecs; codec in allowlist }
native_audio_prior_match(allowlist, codecs, index) if { some earlier; earlier < index; allowlist[earlier] in codecs }
native_audio_best := max([native_audio_weight(codec) | some codec in input.release.audio_codecs])
score_entry[code] := max([60 - index * 15, 0]) if { native_audio_permitted; allowlist := object.get(input.profile, "audio_codec_allowlist", []); count(allowlist) > 0; some index; allowlist[index] in input.release.audio_codecs; not native_audio_prior_match(allowlist, input.release.audio_codecs, index); code := sprintf("audio_codec_preferred_%d", [index]) }
score_entry["audio_codec_lossless"] := native_audio_best if { native_audio_permitted; count(object.get(input.profile, "audio_codec_allowlist", [])) == 0; native_audio_best >= 60 }
score_entry["audio_codec_high"] := native_audio_best if { native_audio_permitted; count(object.get(input.profile, "audio_codec_allowlist", [])) == 0; native_audio_best >= 40; native_audio_best < 60 }
score_entry["audio_codec_standard"] := native_audio_best if { native_audio_permitted; count(object.get(input.profile, "audio_codec_allowlist", [])) == 0; native_audio_best > 0; native_audio_best < 40 }
native_audio_channel_key := "71" if { input.release.audio_channels == "7.1" } else := "51" if { input.release.audio_channels in {"5.1", "6.1"} } else := "20" if { input.release.audio_channels in {"2.0", "2.1"} } else := "10" if { input.release.audio_channels == "1.0" } else := "other"
native_audio_channels_weight := object.get(native_audio_channel_weights, native_audio_channel_key, 0)
native_audio_channel_weights := {
  "balanced": {"71": 30, "51": 15, "20": 0, "10": -15},
  "audiophile": {"71": 60, "51": 25, "20": -10, "10": -40},
  "efficient": {"71": -20, "51": 20, "20": 10, "10": -10},
  "compatible": {"71": 10, "51": 30, "20": 20, "10": 0},
}[native_audio_persona]
score_entry["audio_channels"] := native_audio_channels_weight if { input.release.audio_channels != null; native_audio_channels_weight != 0 }
native_audio_atmos_bonus := 150 if { native_audio_persona == "audiophile" } else := 0
native_audio_atmos_missing := -30 if { native_audio_persona == "audiophile"; lower(object.get(input.context, "category", "")) != "anime" } else := 0
score_entry["atmos_preferred_match"] := native_audio_atmos_bonus if { input.release.is_atmos == true; native_audio_atmos_bonus != 0 }
score_entry["atmos_preferred_missing"] := native_audio_atmos_missing if { input.release.is_atmos != true; native_audio_atmos_missing != 0 }

