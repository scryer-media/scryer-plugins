// Included by Scryer's offline trash_pack_validation test module.
// Uses its production engine, package rewriting, and canonical fixture helper.
#[test]
#[ignore = "explicit pack review validation; requires SCRYER_TRASH_PACK"]
fn trash_pack_review_regressions() {
    let policies = pack_policies();
    let engine = UserRulesEngine::build(&policies).expect("review pack builds");
    let mut evaluator = engine.evaluator();
    let cases = [
        (
            "groups",
            "anime",
            json!({"release_group":"Arid", "source":"BluRay"}),
            json!({}),
            "group_",
            json!({"group_silver":150}),
        ),
        (
            "groups",
            "anime",
            json!({"release_group":"Arid", "source":"WEB-DL"}),
            json!({}),
            "group_",
            json!({"group_gold":300}),
        ),
        (
            "groups",
            "anime",
            json!({"release_group":"Arid", "source":null}),
            json!({}),
            "group_",
            json!({"group_unknown":-30}),
        ),
        (
            "groups",
            "movie",
            json!({"release_group":"DON", "source":"BR-DISK", "quality":"1080P"}),
            json!({}),
            "group_",
            json!({"group_gold":300}),
        ),
        (
            "groups",
            "movie",
            json!({"release_group":"DON", "source":"BR-DISK", "quality":"2160P"}),
            json!({}),
            "group_",
            json!({"group_gold":300}),
        ),
        (
            "german",
            "anime",
            json!({"release_group":"AO", "source":"BluRay"}),
            json!({"required_audio_languages":["deu"]}),
            "trash_tier_",
            json!({"trash_tier_1":151}),
        ),
        (
            "german",
            "anime",
            json!({"release_group":"AO", "source":"WEB-DL"}),
            json!({"required_audio_languages":["deu"]}),
            "trash_tier_",
            json!({"trash_tier_2":146}),
        ),
        (
            "german",
            "anime",
            json!({"release_group":"AO", "source":null}),
            json!({"required_audio_languages":["deu"]}),
            "trash_tier_",
            json!({}),
        ),
        (
            "german",
            "movie",
            json!({"release_group":"AO", "source":"BluRay"}),
            json!({"required_audio_languages":["deu"]}),
            "trash_tier_",
            json!({}),
        ),
        (
            "french-vo",
            "movie",
            json!({"release_group":"FCK", "source":"BluRay", "quality":"2160P", "is_remux":true}),
            json!({"required_audio_languages":["fra"]}),
            "trash_tier_",
            json!({}),
        ),
        (
            "french-vo",
            "movie",
            json!({"release_group":"FCK", "source":"BluRay", "quality":"2160P", "is_remux":false}),
            json!({"required_audio_languages":["fra"]}),
            "trash_tier_",
            json!({"trash_tier_2":240}),
        ),
        (
            "unwanted",
            "movie",
            json!({"release_group":null}),
            json!({}),
            "trash.no_release_group",
            json!({"trash.no_release_group":-10000}),
        ),
        (
            "unwanted",
            "series",
            json!({"release_group":""}),
            json!({}),
            "trash.no_release_group",
            json!({"trash.no_release_group":-10000}),
        ),
        (
            "unwanted",
            "anime",
            json!({"release_group":null}),
            json!({}),
            "trash.no_release_group",
            json!({}),
        ),
        (
            "unwanted",
            "movie",
            json!({"release_group":"Unknown"}),
            json!({}),
            "trash.no_release_group",
            json!({}),
        ),
        (
            "unwanted",
            "movie",
            json!({"is_ai_enhanced":false, "normalized_tokens":["AI", "UPSCALED"]}),
            json!({}),
            "ai_enhanced_upscaled",
            json!({"ai_enhanced_upscaled":-10000}),
        ),
        (
            "unwanted",
            "movie",
            json!({"is_ai_enhanced":true, "normalized_tokens":["AI", "UPSCALED"]}),
            json!({}),
            "ai_enhanced_upscaled",
            json!({"ai_enhanced_upscaled":-10000}),
        ),
        (
            "unwanted",
            "movie",
            json!({"is_ai_enhanced":true, "normalized_tokens":["AI", "UPSCALED"]}),
            json!({"scoring_overrides":{"block_upscaled":false}}),
            "ai_enhanced_upscaled",
            json!({}),
        ),
        (
            "unwanted",
            "movie",
            json!({"is_ai_enhanced":false}),
            json!({}),
            "ai_enhanced_upscaled",
            json!({}),
        ),
    ];
    for (index, (name, category, release, profile, prefix, expected)) in
        cases.into_iter().enumerate()
    {
        let policy = policies
            .iter()
            .find(|p| p.name == format!("trash-guides-{name}"))
            .unwrap();
        let mut input = fixture("balanced", category);
        input["release"]
            .as_object_mut()
            .unwrap()
            .extend(release.as_object().unwrap().clone());
        input["profile"]
            .as_object_mut()
            .unwrap()
            .extend(profile.as_object().unwrap().clone());
        evaluator.engine.set_input(input.into());
        let value = evaluator
            .engine
            .eval_rule(crate::score_entry_wrapper_rule_path(&policy.id))
            .unwrap();
        let mut actual = serde_json::Map::new();
        if value != regorus::Value::Undefined {
            for (key, delta) in value.as_object().unwrap().iter() {
                let key = key.as_string().unwrap();
                if key.starts_with(prefix) {
                    actual.insert(key.to_string(), json!(delta.as_i64().unwrap()));
                }
            }
        }
        assert_eq!(
            Value::Object(actual),
            expected,
            "case {index}: {name}/{category}"
        );
    }
}
