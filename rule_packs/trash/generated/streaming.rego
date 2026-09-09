package scryer.user.trash_guides_streaming
import rego.v1

trash_stream_weight(tier) := value if { weights:={"balanced":{"one":30,"two":20,"three":10,"anime":20},"audiophile":{"one":20,"two":15,"three":5,"anime":15},"efficient":{"one":40,"two":30,"three":20,"anime":25},"compatible":{"one":30,"two":20,"three":15,"anime":20}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),tier,0) }
trash_stream_tier := "one" if { input.release.streaming_service in {"Netflix","Apple TV+","Amazon","Disney+"} }
trash_stream_tier := "two" if { input.release.streaming_service in {"HBO Max","Paramount+","Hulu","Peacock"} }
trash_stream_tier := "anime" if { input.release.streaming_service in {"Crunchyroll","Funimation","HIDIVE"} }
trash_stream_tier := "three" if { input.release.streaming_service != null; input.release.streaming_service != ""; not input.release.streaming_service in {"Netflix","Apple TV+","Amazon","Disney+","HBO Max","Paramount+","Hulu","Peacock","Crunchyroll","Funimation","HIDIVE"} }
score_entry["streaming_service"] := trash_stream_weight(trash_stream_tier) if { trash_stream_tier; trash_stream_weight(trash_stream_tier) != 0 }
