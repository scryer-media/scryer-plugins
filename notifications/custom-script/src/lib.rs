use std::collections::BTreeMap;

use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "custom-script",
        "Custom Script",
        env!("CARGO_PKG_VERSION"),
        "customscript",
        vec![NotificationDeliveryMode::CustomScript],
        vec![NotificationPayloadFormat::ScriptEnvironment],
        config_fields(),
        false,
        false,
    );
    if let ProviderDescriptor::Notification(notification) = &mut descriptor.provider {
        notification.provider_aliases = vec!["custom-script".to_string()];
        notification.capabilities.requires_host_process = true;
    }
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "path",
            "Path",
            ConfigFieldType::String,
            true,
            None,
            Some("Absolute path to the script executable."),
        ),
        field(
            "working_directory",
            "Working Directory",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "application_url",
            "Application URL",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "timeout_ms",
            "Timeout",
            ConfigFieldType::Number,
            false,
            Some("20000"),
            Some("Maximum runtime in milliseconds. The host caps this at 30000."),
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let mut env = script_environment(&req);
    add_sonarr_environment(&req, &mut env);

    let result = process_exec(ProcessExecRequest {
        command: required_config("path")?,
        args: Vec::new(),
        env,
        working_directory: config_value("working_directory"),
        stdin_base64: None,
        timeout_ms: config_value("timeout_ms").and_then(|value| value.parse().ok()),
    });

    let response = match result {
        Ok(output) if output.timed_out => error_response("custom script timed out", None),
        Ok(output) if output.status_code.unwrap_or(1) != 0 => error_response(
            format!(
                "custom script exited with code {}",
                output
                    .status_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            None,
        ),
        Ok(_) => ok_response(),
        Err(error) => error_response(format!("custom script failed: {}", error.message), None),
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn add_sonarr_environment(req: &PluginNotificationRequest, env: &mut BTreeMap<String, String>) {
    env.insert("Sonarr_EventType".to_string(), sonarr_event_type(req));
    env.insert("Sonarr_InstanceName".to_string(), req.app.name.clone());
    env.insert(
        "Sonarr_ApplicationUrl".to_string(),
        config_value("application_url").unwrap_or_default(),
    );
    let episodes = if req.episodes.is_empty() {
        req.episode.iter().collect::<Vec<_>>()
    } else {
        req.episodes.iter().collect::<Vec<_>>()
    };

    if let Some(title) = &req.title {
        insert_opt(env, "Sonarr_Series_Id", title.id.as_deref());
        env.insert("Sonarr_Series_Title".to_string(), title.name.clone());
        insert_opt(env, "Sonarr_Series_TitleSlug", title.slug.as_deref());
        insert_opt(env, "Sonarr_Series_Path", title.path.as_deref());
        insert_opt(
            env,
            "Sonarr_Series_TvdbId",
            title.external_ids.tvdb_id.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Series_TvMazeId",
            title.external_ids.tvmaze_id.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Series_TmdbId",
            title.external_ids.tmdb_id.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Series_ImdbId",
            title.external_ids.imdb_id.as_deref(),
        );
        env.insert("Sonarr_Series_Type".to_string(), title.facet.clone());
        if let Some(year) = title.year {
            env.insert("Sonarr_Series_Year".to_string(), year.to_string());
        }
        insert_opt(
            env,
            "Sonarr_Series_OriginalCountry",
            title.original_country.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Series_OriginalLanguage",
            title.original_language.as_deref(),
        );
        env.insert("Sonarr_Series_Genres".to_string(), title.genres.join("|"));
        env.insert("Sonarr_Series_Tags".to_string(), title.tags.join("|"));
    }

    if let Some(episode) = episodes.first().copied() {
        insert_opt(env, "Sonarr_EpisodeFile_Id", episode.id.as_deref());
        env.insert(
            "Sonarr_EpisodeFile_EpisodeCount".to_string(),
            episodes.len().to_string(),
        );
        env.insert(
            "Sonarr_EpisodeFile_EpisodeIds".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.id.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        insert_opt(
            env,
            "Sonarr_EpisodeFile_SeasonNumber",
            episode.season_number.as_deref(),
        );
        env.insert(
            "Sonarr_EpisodeFile_EpisodeNumbers".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.episode_number.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_EpisodeFile_EpisodeTitles".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.title.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_EpisodeAirDates".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.air_date.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_EpisodeFile_EpisodeAirDatesUtc".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.air_date_utc.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_EpisodeFile_AbsoluteEpisodeNumbers".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.absolute_number.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_EpisodeFile_EpisodeOverviews".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.overview.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_FinaleTypes".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.finale_type.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
    }

    if !episodes.is_empty() {
        env.insert(
            "Sonarr_Release_EpisodeCount".to_string(),
            episodes.len().to_string(),
        );
        insert_or_empty(
            env,
            "Sonarr_Release_SeasonNumber",
            episodes
                .first()
                .and_then(|episode| episode.season_number.as_deref()),
        );
        env.insert(
            "Sonarr_Release_EpisodeNumbers".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.episode_number.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_Release_AbsoluteEpisodeNumbers".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.absolute_number.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_Release_EpisodeAirDates".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.air_date.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_Release_EpisodeAirDatesUtc".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.air_date_utc.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        env.insert(
            "Sonarr_Release_EpisodeTitles".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.title.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_Release_EpisodeOverviews".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.overview.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_Release_FinaleTypes".to_string(),
            episodes
                .iter()
                .filter_map(|episode| episode.finale_type.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
    }

    if let Some(release) = &req.release {
        insert_or_empty(env, "Sonarr_Release_Title", release.source_title.as_deref());
        insert_or_empty(env, "Sonarr_Release_Indexer", release.indexer.as_deref());
        insert_or_empty(env, "Sonarr_Release_Quality", release.quality.as_deref());
        insert_or_empty(env, "Sonarr_Release_QualityVersion", None);
        insert_or_empty(
            env,
            "Sonarr_Release_ReleaseGroup",
            release.release_group.as_deref(),
        );
        insert_or_empty(env, "Sonarr_Release_IndexerFlags", None);
        insert_or_empty(env, "Sonarr_Release_CustomFormat", None);
        insert_or_empty(env, "Sonarr_Release_CustomFormatScore", None);
        insert_or_empty(
            env,
            "Sonarr_Release_ReleaseType",
            release.protocol.as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_Release_Group",
            release.release_group.as_deref(),
        );
        env.insert(
            "Sonarr_Release_Size".to_string(),
            req.download
                .as_ref()
                .and_then(|download| download.size_bytes)
                .map(|size| size.to_string())
                .unwrap_or_default(),
        );
    }
    if !req.media_files.is_empty() {
        env.entry("Sonarr_Release_Quality".to_string())
            .or_insert_with(|| {
                req.media_files
                    .first()
                    .and_then(|file| file.quality.clone())
                    .unwrap_or_default()
            });
        env.entry("Sonarr_Release_QualityVersion".to_string())
            .or_default();
        env.entry("Sonarr_Release_Group".to_string())
            .or_insert_with(|| {
                req.media_files
                    .first()
                    .and_then(|file| file.release_group.clone())
                    .unwrap_or_default()
            });
    }

    if let Some(download) = &req.download {
        insert_opt(
            env,
            "Sonarr_Download_Client",
            download.client_name.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Download_Client_Type",
            download.client_type.as_deref(),
        );
        insert_opt(env, "Sonarr_Download_Id", download.download_id.as_deref());
        if let Some(size) = download.size_bytes {
            env.insert("Sonarr_Download_Size".to_string(), size.to_string());
        }
        insert_opt(env, "Sonarr_Download_Title", download.title.as_deref());
    }

    if let Some(import) = &req.import {
        env.insert("Sonarr_IsUpgrade".to_string(), import.upgrade.to_string());
        insert_opt(
            env,
            "Sonarr_EpisodeFile_SourcePath",
            import.source_path.as_deref(),
        );
        insert_opt(env, "Sonarr_SourcePath", import.source_path.as_deref());
        insert_opt(env, "Sonarr_DestinationPath", import.dest_path.as_deref());
        insert_or_empty(
            env,
            "Sonarr_SourceFolder",
            import
                .source_path
                .as_deref()
                .and_then(parent_path)
                .as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_DestinationFolder",
            import.dest_path.as_deref().and_then(parent_path).as_deref(),
        );
        if !import.deleted_paths.is_empty() {
            env.insert(
                "Sonarr_DeletedPaths".to_string(),
                import.deleted_paths.join("|"),
            );
            env.insert(
                "Sonarr_DeletedRelativePaths".to_string(),
                import
                    .deleted_paths
                    .iter()
                    .map(|path| {
                        relative_path(
                            path,
                            req.title.as_ref().and_then(|title| title.path.as_deref()),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
    }

    if let Some(file) = &req.file {
        insert_opt(env, "Sonarr_EpisodeFile_Path", file.primary_path.as_deref());
    }
    let series_path = req.title.as_ref().and_then(|title| title.path.as_deref());
    if let Some(media_file) = req.media_files.first() {
        insert_opt(env, "Sonarr_EpisodeFile_Id", media_file.id.as_deref());
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_Path",
            Some(media_file.path.as_str()),
        );
        env.insert(
            "Sonarr_EpisodeFile_RelativePath".to_string(),
            relative_path(&media_file.path, series_path),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_Quality",
            media_file.quality.as_deref(),
        );
        insert_or_empty(env, "Sonarr_EpisodeFile_QualityVersion", None);
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_ReleaseGroup",
            media_file.release_group.as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_SceneName",
            media_file.scene_name.as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_MediaInfo_AudioChannels",
            media_file.audio_channels.as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_MediaInfo_AudioCodec",
            media_file.audio_codec.as_deref(),
        );
        env.insert(
            "Sonarr_EpisodeFile_MediaInfo_AudioLanguages".to_string(),
            media_file.audio_languages.join(" / "),
        );
        env.insert(
            "Sonarr_EpisodeFile_MediaInfo_Languages".to_string(),
            media_file.audio_languages.join(" / "),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_MediaInfo_Height",
            media_file
                .video_height
                .map(|height| height.to_string())
                .as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_MediaInfo_Width",
            media_file
                .video_width
                .map(|width| width.to_string())
                .as_deref(),
        );
        env.insert(
            "Sonarr_EpisodeFile_MediaInfo_Subtitles".to_string(),
            media_file.subtitle_languages.join(" / "),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_MediaInfo_VideoCodec",
            media_file.video_codec.as_deref(),
        );
        insert_or_empty(
            env,
            "Sonarr_EpisodeFile_MediaInfo_VideoDynamicRangeType",
            media_file.video_hdr_format.as_deref(),
        );
    }
    if !req.media_files.is_empty() {
        env.insert(
            "Sonarr_EpisodeFile_Ids".to_string(),
            req.media_files
                .iter()
                .filter_map(|file| file.id.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_Count".to_string(),
            req.media_files.len().to_string(),
        );
        env.insert(
            "Sonarr_EpisodeFile_RelativePaths".to_string(),
            req.media_files
                .iter()
                .map(|file| relative_path(&file.path, series_path))
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_Paths".to_string(),
            req.media_files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_Qualities".to_string(),
            req.media_files
                .iter()
                .map(|file| file.quality.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_QualityVersions".to_string(),
            req.media_files
                .iter()
                .map(|_| String::new())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_ReleaseGroups".to_string(),
            req.media_files
                .iter()
                .map(|file| file.release_group.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|"),
        );
        env.insert(
            "Sonarr_EpisodeFile_SceneNames".to_string(),
            req.media_files
                .iter()
                .map(|file| file.scene_name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|"),
        );
        let previous_paths = req
            .media_files
            .iter()
            .map(|file| file.previous_path.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        if previous_paths.iter().any(|path| !path.is_empty()) {
            env.insert(
                "Sonarr_EpisodeFile_PreviousPaths".to_string(),
                previous_paths.join("|"),
            );
            env.insert(
                "Sonarr_EpisodeFile_PreviousRelativePaths".to_string(),
                req.media_files
                    .iter()
                    .map(|file| {
                        file.previous_path
                            .as_deref()
                            .map(|path| relative_path(path, series_path))
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        let recycle_bin_paths = req
            .media_files
            .iter()
            .map(|file| file.recycle_bin_path.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        if recycle_bin_paths.iter().any(|path| !path.is_empty()) {
            env.insert(
                "Sonarr_DeletedRecycleBinPaths".to_string(),
                recycle_bin_paths.join("|"),
            );
        }
    }

    if let Some(health) = &req.health {
        insert_opt(env, "Sonarr_Health_Issue_Level", health.severity.as_deref());
        insert_opt(
            env,
            "Sonarr_Health_Issue_Message",
            health.message.as_deref(),
        );
        insert_opt(env, "Sonarr_Health_Issue_Type", health.code.as_deref());
        insert_opt(env, "Sonarr_Health_Issue_Wiki", health.details.as_deref());
        insert_opt(
            env,
            "Sonarr_Health_Restored_Level",
            health.severity.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Health_Restored_Message",
            health.message.as_deref(),
        );
        insert_opt(env, "Sonarr_Health_Restored_Type", health.code.as_deref());
        insert_opt(
            env,
            "Sonarr_Health_Restored_Wiki",
            health.details.as_deref(),
        );
    }

    if let Some(update) = &req.application_update {
        insert_opt(env, "Sonarr_Update_Message", update.summary.as_deref());
        insert_opt(
            env,
            "Sonarr_Update_NewVersion",
            update.target_version.as_deref(),
        );
        insert_opt(
            env,
            "Sonarr_Update_PreviousVersion",
            update.current_version.as_deref(),
        );
    }
}

fn sonarr_event_type(req: &PluginNotificationRequest) -> String {
    match req.event_type.as_str() {
        "grab" => "Grab",
        "download" | "upgrade" | "import_complete" => "Download",
        "rename" => "Rename",
        "title_added" => "SeriesAdd",
        "title_deleted" => "SeriesDelete",
        "file_deleted" | "file_deleted_for_upgrade" => "EpisodeFileDelete",
        "health_issue" => "HealthIssue",
        "health_restored" => "HealthRestored",
        "application_update" => "ApplicationUpdate",
        "manual_interaction_required" => "ManualInteractionRequired",
        "test" => "Test",
        other => other,
    }
    .to_string()
}

fn insert_opt(env: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        env.insert(key.to_string(), value.to_string());
    }
}

fn insert_or_empty(env: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
    env.insert(key.to_string(), value.unwrap_or_default().to_string());
}

fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let index = trimmed.rfind(['/', '\\'])?;
    if index == 0 {
        Some(trimmed[..=index].to_string())
    } else {
        Some(trimmed[..index].to_string())
    }
}

fn relative_path(path: &str, base: Option<&str>) -> String {
    let Some(base) = base.map(|value| value.trim_end_matches(['/', '\\'])) else {
        return path.to_string();
    };
    if base.is_empty() {
        return path.to_string();
    }
    if path == base {
        return String::new();
    }
    for separator in ['/', '\\'] {
        let prefix = format!("{base}{separator}");
        if let Some(relative) = path.strip_prefix(&prefix) {
            return relative.to_string();
        }
    }
    path.to_string()
}
