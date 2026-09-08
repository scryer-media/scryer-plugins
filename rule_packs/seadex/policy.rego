import rego.v1

# This template is rendered by the exact numeric/CRC encoder. Direct records
# use framed raw strings; multi-record templates retain each correlated tuple.
strict_direct_buckets := __STRICT_DIRECT__
strict_template_routes := __STRICT_ROUTES__
tolerant_direct_buckets := __TOLERANT_DIRECT__
tolerant_template_routes := __TOLERANT_ROUTES__

default selected_match := []

selected_match := ["strict", score] if {
    anime_context
    strict_key := strict_signature(input.release.raw_title)
    score := strict_recommendation(strict_key)
} else := [] if {
    anime_context
    strict_key := strict_signature(input.release.raw_title)
    strict_collision(strict_key)
} else := ["tolerant", score] if {
    anime_context
    raw_title := input.release.raw_title
    release_group := input.release.release_group
    release_group != null
    group_key := group_signature(release_group)
    group_key != ""
    has_title_evidence(raw_title, release_group)
    ascii_tolerant_evidence(raw_title, release_group)
    has_edge_group(raw_title, release_group)
    key := sprintf("%s|%s", [group_key, tolerant_signature(raw_title, release_group)])
    score := tolerant_recommendation(key)
}

score_entry["seadex_strict_best"] := 400 if {
    selected_match == ["strict", 400]
}

score_entry["seadex_strict_listed"] := 200 if {
    selected_match == ["strict", 200]
}

score_entry["seadex_tolerant_best"] := 400 if {
    selected_match == ["tolerant", 400]
}

score_entry["seadex_tolerant_listed"] := 200 if {
    selected_match == ["tolerant", 200]
}

strict_recommendation(key) := 400 if {
    strict_match(key, "B")
} else := 200 if {
    strict_match(key, "L")
}

strict_collision(key) if {
    strict_match(key, "X")
}

tolerant_recommendation(key) := 400 if {
    tolerant_match(key, "B")
} else := 200 if {
    tolerant_match(key, "L")
}

strict_match(key, code) if {
    direct_match(strict_direct_buckets, key, code)
}

strict_match(key, code) if {
    template_match(strict_template_routes, key, code)
}

tolerant_match(key, code) if {
    direct_match(tolerant_direct_buckets, key, code)
}

tolerant_match(key, code) if {
    template_match(tolerant_template_routes, key, code)
}

bucket_length(key) := length if {
    length := sprintf("%d", [count(key)])
}

blob_key(key) := encoded if {
    percent := replace(key, "%", "%25")
    tick := replace(percent, "`", "%60")
    newline := replace(tick, "\n", "%0A")
    encoded := replace(newline, "\r", "%0D")
}

direct_match(buckets, key, code) if {
    blob := buckets[bucket_length(key)]
    contains(blob, sprintf("\n%s%s\n", [code, blob_key(key)]))
}

template_escaped(key) := escaped if {
    markers := replace(key, "¦", "¦¦")
    escaped := replace(markers, "§", "¦§")
}

template_match(routes, key, code) if {
    escaped := template_escaped(key)
    captures := regex.find_n("[0-9a-f]{8}|[0-9]+", escaped, -1)
    skeleton := regex.replace(escaped, "[0-9a-f]{8}|[0-9]+", "§")
    route_key := sprintf("%s\t%s", [code, skeleton])
    encoded := routes[bucket_length(route_key)]
    routes_for_length := json.unmarshal(encoded)
    record := routes_for_length[route_key]
    constants_match(captures, record[0])
    tuple := variable_tuple(captures, record[0])
    some chunk in record[1]
    contains(chunk, sprintf("\n%s\n", [tuple]))
}

constants_match(captures, constants) if {
    count(captures) == count(constants)
    not constant_mismatch(captures, constants)
}

constant_mismatch(captures, constants) if {
    some index
    constants[index] != ""
    captures[index] != constants[index]
}

variable_tuple(captures, constants) := tuple if {
    values := [capture |
        some index
        capture := captures[index]
        constants[index] == ""
    ]
    tuple := concat(",", values)
}

anime_context if {
    input.context.is_anime == true
}

release_basename(raw_title) := basename if {
    normalized_path := replace(raw_title, "\\", "/")
    parts := split(normalized_path, "/")
    basename := parts[count(parts) - 1]
}

release_stem(raw_title) := stem if {
    lowercase_basename := lower(release_basename(raw_title))
    stem := regex.replace(lowercase_basename, "\\.(avi|m2ts|mkv|mp4|ts|webm)$", "")
}

strict_signature(raw_title) := signature if {
    normalized_whitespace := regex.replace(release_stem(raw_title), "[\\t\\r\\n ]+", " ")
    signature := regex.replace(normalized_whitespace, "^[\\t\\r\\n ]+|[\\t\\r\\n ]+$", "")
}

group_signature(value) := signature if {
    signature := regex.replace(lower(value), "[^a-z0-9]+", "")
}

strip_edge_group(value, release_group) := remainder if {
    trimmed := trim_space(value)
    group_signature(release_group) != ""
    startswith(trimmed, "[")
    pieces := split(trimmed, "]")
    count(pieces) > 1
    group_signature(pieces[0]) == group_signature(release_group)
    remainder := trim_space(concat("]", array.slice(pieces, 1, count(pieces))))
} else := remainder if {
    trimmed := trim_space(value)
    prefix := sprintf("%s -", [trim_space(lower(release_group))])
    startswith(trimmed, prefix)
    remainder := trim_space(trim_prefix(trimmed, prefix))
} else := remainder if {
    trimmed := trim_space(value)
    suffix := sprintf(" - %s", [trim_space(lower(release_group))])
    endswith(trimmed, suffix)
    remainder := trim_space(trim_suffix(trimmed, suffix))
} else := remainder if {
    trimmed := trim_space(value)
    suffix := sprintf("-%s", [trim_space(lower(release_group))])
    endswith(trimmed, suffix)
    remainder := trim_space(trim_suffix(trimmed, suffix))
} else := remainder if {
    trimmed := trim_space(value)
    suffix := sprintf("[%s]", [trim_space(lower(release_group))])
    endswith(trimmed, suffix)
    remainder := trim_space(trim_suffix(trimmed, suffix))
} else := remainder if {
    remainder := trim_space(value)
}

has_edge_group(raw_title, release_group) if {
    stem := trim_space(release_stem(raw_title))
    group_signature(release_group) != ""
    strip_edge_group(stem, release_group) != stem
}

tolerant_signature(raw_title, release_group) := signature if {
    stem := release_stem(raw_title)
    without_group := strip_edge_group(stem, release_group)
    without_crc := regex.replace(without_group, "\\[[ \\t\\r\\n]*[0-9a-f]{8}[ \\t\\r\\n]*\\]", "")
    normalized := regex.replace(without_crc, "[^a-z0-9]+", " ")
    signature := trim_space(normalized)
}

ascii_tolerant_evidence(raw_title, release_group) if {
    regex.match("^[\\x00-\\x7f]*$", release_stem(raw_title))
    regex.match("^[\\x00-\\x7f]*$", release_group)
}

technical_tokens := {
    "aac", "atmos", "av1", "bd", "bdrip", "bluray", "ddp", "dts", "flac",
    "h264", "h265", "hevc", "opus", "remux", "truehd", "web", "webdl", "webrip",
    "x264", "x265",
}

title_token(token) if {
    token != ""
    not regex.match("^[0-9]+$", token)
    not regex.match("^(v[0-9]+|s[0-9]+e[0-9]+|e[0-9]+)$", token)
    not regex.match("^[0-9]{3,4}p$", token)
    not technical_tokens[token]
}

has_title_evidence(raw_title, release_group) if {
    stem := strip_edge_group(release_stem(raw_title), release_group)
    without_crc := regex.replace(stem, "\\[[ \\t\\r\\n]*[0-9a-f]{8}[ \\t\\r\\n]*\\]", "")
    normalized := regex.replace(without_crc, "[^a-z0-9]+", " ")
    some token in split(normalized, " ")
    title_token(token)
}
