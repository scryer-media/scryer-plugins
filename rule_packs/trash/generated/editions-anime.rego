package scryer.user.trash_guides_editions_anime
import rego.v1

trash_ea_weight(name) := value if { weights:={"balanced":{"imax":80,"extended":40,"hybrid":30,"criterion":20,"remaster":20,"v2":20,"bit":40,"uncensored":30,"dubs":-100},"audiophile":{"imax":120,"extended":60,"hybrid":50,"criterion":40,"remaster":30,"v2":25,"bit":50,"uncensored":40,"dubs":-150},"efficient":{"imax":40,"extended":20,"hybrid":20,"criterion":10,"remaster":10,"v2":20,"bit":60,"uncensored":20,"dubs":-60},"compatible":{"imax":60,"extended":30,"hybrid":25,"criterion":15,"remaster":15,"v2":15,"bit":20,"uncensored":20,"dubs":-80}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),name,0) }
trash_edition := "imax" if { input.release.edition in {"IMAX","IMAX Enhanced"} }
trash_edition := "extended" if { input.release.edition in {"Extended","Unrated","Director's Cut"} }
trash_edition := "hybrid" if { input.release.edition == "Hybrid" }
trash_edition := "criterion" if { input.release.edition == "Criterion" }
trash_edition := "remaster" if { input.release.edition == "Remaster" }
score_entry["edition_bonus"] := trash_ea_weight(trash_edition) if { trash_edition }
score_entry["anime_version_bonus"] := trash_ea_weight("v2") if { lower(object.get(input.context,"category","")) == "anime"; input.release.anime_version >= 2 }
score_entry["anime_10bit_bonus"] := trash_ea_weight("bit") if { lower(object.get(input.context,"category","")) == "anime"; input.release.is_10bit == true }
score_entry["anime_uncensored_bonus"] := trash_ea_weight("uncensored") if { lower(object.get(input.context,"category","")) == "anime"; input.release.is_uncensored == true }
score_entry["anime_dubs_only"] := trash_ea_weight("dubs") if { lower(object.get(input.context,"category","")) == "anime"; input.release.is_dubs_only == true }
