//! Settings (`/api/settings` and the agents) and pane widths (`/api/layout`, layout.rs).

use super::*;

/// The settings view once the agent scan has finished.
fn scanned_settings(site: &Site) -> serde_json::Value {
    site.agents.wait();
    get_json(site, SETTINGS)
}

/// Settings that pass every rule and differ from the defaults in every field.
fn custom_settings() -> serde_json::Value {
    serde_json::json!({
        "theme": "dark",
        "codeFontSize": 15,
        "tabWidth": 2,
        "reopenLastProject": true,
        "customAgents": [{ "name": "Local", "command": "local-agent --acp" }],
        "enabledAgents": ["Grok", "Local"],
        "agent": { "name": "Local", "model": null },
        "analyzer": { "name": "Grok", "model": "grok-4.7-fast" },
        "reviewers": [
            { "name": "Grok", "model": "grok-4.7" },
            { "name": "Grok", "model": null },
        ],
    })
}

#[test]
fn settings_start_as_the_defaults_and_saved_ones_survive_a_restart() {
    let repo = TempRepo::new("settings-saved");
    let file = repo.config().join("settings.json");
    let view = get_json(&repo.empty_site(), SETTINGS);
    assert_eq!(view["settings"]["theme"], "system");
    assert_eq!(view["settings"]["codeFontSize"], 13);
    assert_eq!(view["settings"]["tabWidth"], 4);
    assert_eq!(view["problem"], serde_json::Value::Null);
    assert_eq!(view["firstRun"], true);
    assert_eq!(view["file"], file.to_str().unwrap());
    assert_eq!(view["fontSizes"], serde_json::json!([11, 20]));
    assert_eq!(view["tabWidths"], serde_json::json!([2, 4, 8]));
    assert!(!file.exists(), "reading the defaults writes nothing");

    let (status, body) = save(&repo.empty_site(), &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(saved["firstRun"], false);
    assert_eq!(
        saved["settings"],
        custom_settings(),
        "the reply is the new settings"
    );

    let restarted = get_json(&repo.empty_site(), SETTINGS);
    assert_eq!(restarted["firstRun"], false);
    assert_eq!(restarted["settings"], custom_settings());
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 3);
    assert_eq!(stored["settings"], custom_settings());
}

#[test]
fn a_refused_save_names_the_broken_rule_and_nothing_else() {
    let repo = TempRepo::new("settings-refused-message");
    let site = repo.empty_site();
    let mut settings = custom_settings();
    settings["tabWidth"] = 3.into();

    let (status, reply) = save(&site, &settings.to_string());

    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The whole message, as the dialog shows it: no serde position, no "unreadable" prefix.
    assert_eq!(
        reply,
        "cannot save the settings: the tab width must be 2, 4 or 8, not 3"
    );
}

#[test]
fn settings_that_break_a_rule_are_refused_and_nothing_is_written() {
    let repo = TempRepo::new("settings-refused");
    let site = repo.empty_site();
    let file = repo.config().join("settings.json");
    let with = |field: &str, value: serde_json::Value| {
        let mut settings = custom_settings();
        settings[field] = value;
        settings.to_string()
    };
    let agent =
        |name: &str, command: &str| serde_json::json!([{ "name": name, "command": command }]);
    let cases = [
        (
            with("codeFontSize", 10.into()),
            "code font size must be 11 to 20",
        ),
        (
            with("codeFontSize", 21.into()),
            "code font size must be 11 to 20",
        ),
        (with("tabWidth", 3.into()), "tab width must be 2, 4 or 8"),
        (with("theme", "sepia".into()), "unknown variant `sepia`"),
        (
            with("customAgents", agent("claude", "x")),
            "already an agent called \"claude\"",
        ),
        (
            with("customAgents", agent(" Local", "x")),
            "cannot be empty or start or end",
        ),
        (
            with("customAgents", agent("", "x")),
            "cannot be empty or start or end",
        ),
        (
            with("customAgents", agent(&"a".repeat(41), "x")),
            "at most 40 characters",
        ),
        (
            with("customAgents", agent("Local", "  ")),
            "needs a command",
        ),
        (
            with("customAgents", agent("Local", "run \"a b\"")),
            "cannot hold quotes",
        ),
        (
            with("customAgents", agent("Local", "./bin/agent --acp")),
            "must be a name on the search path or an absolute path, not ./bin/agent",
        ),
        (
            with("enabledAgents", serde_json::json!(["Nobody"])),
            "no agent called \"Nobody\"",
        ),
        (
            with("enabledAgents", serde_json::json!(["Grok", "Grok"])),
            "enabled twice",
        ),
        (
            with("agent", serde_json::json!({ "name": "Claude" })),
            "notes cannot go to \"Claude\"",
        ),
        (
            with("reviewers", serde_json::json!([{ "name": "Claude" }])),
            "\"Claude\" cannot review",
        ),
        (with("colour", "red".into()), "unknown field `colour`"),
        ("not json".to_owned(), "unreadable request"),
    ];
    for (body, expected) in cases {
        let (status, reply) = save(&site, &body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
        assert!(reply.starts_with("cannot save the settings: "), "{reply}");
        assert!(reply.contains(expected), "{body}: {reply}");
    }
    let two_locals = serde_json::json!([
        { "name": "Local", "command": "a" },
        { "name": "LOCAL", "command": "b" },
    ]);
    let (status, reply) = save(&site, &with("customAgents", two_locals));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        reply.contains("already an agent called \"LOCAL\""),
        "{reply}"
    );
    assert!(!file.exists());

    // The limits themselves are allowed: a 40 character name, sizes 11 and 20.
    let longest = "a".repeat(40);
    let mut at_limits = custom_settings();
    at_limits["customAgents"] = serde_json::json!([{ "name": longest, "command": "x" }]);
    at_limits["enabledAgents"] = serde_json::json!([longest, "Grok"]);
    at_limits["agent"] = serde_json::json!({ "name": longest, "model": "m".repeat(200) });
    at_limits["reviewers"] = serde_json::json!([]);
    for size in [11, 20] {
        at_limits["codeFontSize"] = size.into();
        let (status, reply) = save(&site, &at_limits.to_string());
        assert_eq!(status, StatusCode::OK, "{reply}");
    }
    // So is a program named by an absolute path.
    let mut absolute = custom_settings();
    absolute["customAgents"][0]["command"] = "/opt/agents/local --acp".into();
    let (status, reply) = save(&site, &absolute.to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");
}

#[test]
fn a_settings_file_that_cannot_be_used_is_kept_until_it_is_reset() {
    let repo = TempRepo::new("settings-broken");
    let site = repo.empty_site();
    let file = repo.config().join("settings.json");
    fs::create_dir_all(repo.config()).unwrap();
    let unusable = [
        (
            "{ \"version\": 1, \"settings\": { \"theme\": ",
            "is not valid settings JSON",
        ),
        (
            "{\"version\": 4, \"settings\": {}}",
            "written by a newer Postil (format version 4)",
        ),
        (
            "{\"version\": 1, \"settings\": {\"agent\": 7}}",
            "is not valid settings JSON: invalid type: integer `7`, expected struct AgentSlot",
        ),
        (
            "{\"version\": 0, \"settings\": {}}",
            "unknown format version 0",
        ),
        (
            "{\"version\": 1, \"settings\": {\"tabWidth\": 5}}",
            "breaks a rule: the tab width must be 2, 4 or 8, not 5",
        ),
    ];
    for (text, expected) in unusable {
        fs::write(&file, text).unwrap();
        let view = get_json(&site, SETTINGS);
        assert_eq!(view["firstRun"], false);
        let problem = view["problem"].as_str().unwrap();
        assert!(problem.contains(expected), "{problem}");
        assert!(problem.contains("saving is off until"), "{problem}");
        assert_eq!(view["settings"]["tabWidth"], 4, "the defaults are in use");

        let (status, reply) = save(&site, &custom_settings().to_string());
        assert_eq!(status, StatusCode::CONFLICT, "{reply}");
        assert!(reply.contains(expected), "{reply}");
        assert_eq!(fs::read_to_string(&file).unwrap(), text, "left as it was");
    }

    let (status, reply) = post(&site, "postil://localhost/api/settings/reset", "");
    assert_eq!(status, StatusCode::OK, "{reply}");
    let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(view["problem"], serde_json::Value::Null);
    assert_eq!(view["firstRun"], false);
    assert_eq!(view["settings"]["tabWidth"], 4);
    let (status, reply) = save(&site, &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "saving works again: {reply}");
}

#[test]
fn a_save_from_before_another_change_is_refused_and_the_other_change_kept() {
    let repo = TempRepo::new("settings-changed");
    let file = repo.config().join("settings.json");
    let (status, reply) = save(&repo.empty_site(), &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");

    // A window reads the settings; then a hand edit (or another window) changes them.
    let site = repo.empty_site();
    let stale = get_json(&site, SETTINGS)["revision"].clone();
    let edited = "{\"version\": 1, \"settings\": {\"theme\": \"light\"}}";
    fs::write(&file, edited).unwrap();

    let body = format!(
        "{{\"revision\": {stale}, \"settings\": {}}}",
        custom_settings()
    );
    let (status, reply) = put(&site, SETTINGS, &body);
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply.contains("the file changed since this window read it"),
        "{reply}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        edited,
        "the other change is kept"
    );

    // Read again, the window saves on top of the edit.
    let fresh = get_json(&site, SETTINGS);
    assert_eq!(fresh["settings"]["theme"], "light");
    assert_ne!(fresh["revision"], stale);
    let (status, reply) = save(&site, &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");
}

#[test]
fn a_settings_file_written_by_hand_may_name_only_what_it_changes() {
    let repo = TempRepo::new("settings-partial");
    fs::create_dir_all(repo.config()).unwrap();
    fs::write(
        repo.config().join("settings.json"),
        "{\"version\": 1, \"settings\": {\"theme\": \"light\"}}",
    )
    .unwrap();
    let view = get_json(&repo.empty_site(), SETTINGS);
    assert_eq!(view["problem"], serde_json::Value::Null);
    assert_eq!(view["settings"]["theme"], "light");
    assert_eq!(view["settings"]["codeFontSize"], 13);
    assert_eq!(view["settings"]["enabledAgents"], serde_json::json!([]));
}

#[test]
fn without_a_configuration_directory_settings_are_the_defaults_and_cannot_be_saved() {
    let site = Site::new(None, None, None, no_agents(), quiet());
    let view = get_json(&site, SETTINGS);
    assert_eq!(
        view["problem"],
        "settings are off: cannot find the home directory (is HOME set?)"
    );
    assert_eq!(view["file"], serde_json::Value::Null);
    assert_eq!(view["firstRun"], false);
    let (status, reply) = save(&site, &custom_settings().to_string());
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply, "cannot find the home directory (is HOME set?)");
}

#[test]
fn agents_are_found_on_the_search_path_and_custom_ones_by_their_command() {
    let repo = TempRepo::new("settings-agents");
    let bin = repo.root.join(".git/test-bin");
    fs::create_dir_all(&bin).unwrap();
    for program in ["grok", "local-agent"] {
        let path = bin.join(program);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The first scan finds `bin`; every later one finds nothing, so a rescan shows.
    let scans = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&scans);
    let first_path = bin.clone().into_os_string();
    let site = Site::new(
        Some(&repo.config()),
        None,
        None,
        Scanner::new(move || {
            let mut scans = counter.lock().unwrap();
            *scans += 1;
            if *scans == 1 {
                first_path.clone()
            } else {
                OsString::new()
            }
        }),
        quiet(),
    );
    let (status, body) = save(&site, &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "{body}");

    let agents = scanned_settings(&site)["agents"].clone();
    let names: Vec<&str> = agents
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Claude", "Codex", "Gemini", "Grok", "OpenCode", "Local"]
    );
    let agent = |name: &str| {
        agents
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(agent("Grok")["found"], bin.join("grok").to_str().unwrap());
    assert_eq!(agent("Grok")["command"], "grok agent stdio");
    assert_eq!(agent("Grok")["builtIn"], true);
    assert_eq!(agent("Claude")["found"], serde_json::Value::Null);
    assert_eq!(
        agent("Local")["found"],
        bin.join("local-agent").to_str().unwrap()
    );
    assert_eq!(agent("Local")["builtIn"], false);

    // A rescan starts over, and its result replaces the first one.
    let (status, body) = post(&site, "postil://localhost/api/agents/rescan", "");
    assert_eq!(status, StatusCode::OK, "{body}");
    let rescanned = scanned_settings(&site)["agents"].clone();
    let found: Vec<&serde_json::Value> = rescanned
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| &agent["found"])
        .collect();
    assert!(found.iter().all(|found| found.is_null()), "{rescanned}");
    assert_eq!(*scans.lock().unwrap(), 2);

    // Reset and rescan change state, so they take POST only.
    for route in ["api/settings/reset", "api/agents/rescan"] {
        let (status, _) = get(&site, &format!("postil://localhost/{route}"));
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{route}");
    }
    assert_eq!(*scans.lock().unwrap(), 2, "a refused rescan does not scan");
    assert_eq!(
        get_json(&site, SETTINGS)["settings"],
        custom_settings(),
        "nor reset"
    );
}

#[test]
fn a_version_1_file_is_read_as_slots_without_models_and_saved_as_version_3() {
    let repo = TempRepo::new("settings-v1");
    let file = repo.config().join("settings.json");
    fs::create_dir_all(repo.config()).unwrap();
    let v1 = serde_json::json!({
        "version": 1,
        "settings": {
            "customAgents": [{ "name": "Local", "command": "local-agent --acp" }],
            "enabledAgents": ["Grok", "Local"],
            "agent": "Local",
            "reviewers": ["Grok", "Local"],
        },
    });
    fs::write(&file, v1.to_string()).unwrap();
    let site = repo.empty_site();

    let view = get_json(&site, SETTINGS);
    assert_eq!(view["problem"], serde_json::Value::Null, "{view}");
    assert_eq!(
        view["settings"]["agent"],
        serde_json::json!({ "name": "Local", "model": null })
    );
    assert_eq!(
        view["settings"]["reviewers"],
        serde_json::json!([
            { "name": "Grok", "model": null },
            { "name": "Local", "model": null },
        ])
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        v1.to_string(),
        "reading writes nothing"
    );

    let mut changed = view["settings"].clone();
    changed["reviewers"][0]["model"] = "grok-4.7".into();
    let (status, reply) = save(&site, &changed.to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 3);
    assert_eq!(stored["settings"]["analyzer"], serde_json::Value::Null);
    assert_eq!(stored["settings"]["agent"], view["settings"]["agent"]);
    assert_eq!(stored["settings"]["reviewers"][0]["model"], "grok-4.7");
}

#[test]
fn a_version_2_settings_file_still_reads() {
    let repo = TempRepo::new("settings-v2");
    let file = repo.config().join("settings.json");
    fs::create_dir_all(repo.config()).unwrap();
    let v2 = serde_json::json!({
        "version": 2,
        "settings": {
            "enabledAgents": ["Grok"],
            "agent": { "name": "Grok", "model": "grok-4.7" },
            "reviewers": [],
        },
    });
    fs::write(&file, v2.to_string()).unwrap();
    let site = repo.empty_site();

    let view = get_json(&site, SETTINGS);
    assert_eq!(view["problem"], serde_json::Value::Null, "{view}");
    assert_eq!(view["settings"]["analyzer"], serde_json::Value::Null);
    assert_eq!(view["settings"]["agent"]["model"], "grok-4.7");
    let analyzer = crate::settings::load(&file).settings.analyzer().unwrap();
    assert_eq!(
        (analyzer.name.as_str(), analyzer.model.as_deref()),
        ("Grok", Some("grok-4.7")),
        "with no analyzer, analyses go to the agent notes are sent to, on its model"
    );
}

#[test]
fn the_analyzer_falls_back_to_the_default_agent() {
    let repo = TempRepo::new("settings-analyzer");
    let site = repo.empty_site();
    let file = repo.config().join("settings.json");
    let mut chosen = custom_settings();
    chosen["enabledAgents"] = serde_json::json!(["Grok", "Local"]);
    chosen["agent"] = serde_json::json!({ "name": "Grok", "model": "grok-4.7" });
    chosen["analyzer"] = serde_json::json!({ "name": "Local", "model": "big" });
    let (status, reply) = save(&site, &chosen.to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");
    let analyzer = crate::settings::load(&file).settings.analyzer().unwrap();
    assert_eq!(
        (analyzer.name.as_str(), analyzer.model.as_deref()),
        ("Local", Some("big")),
        "the analyzer's own slot, model included"
    );
    assert_eq!(analyzer.command, "local-agent --acp");

    // An agent in use as the analyzer cannot be turned off: the save is refused.
    let mut off = chosen.clone();
    off["enabledAgents"] = serde_json::json!(["Grok"]);
    let (status, reply) = save(&site, &off.to_string());
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert!(reply.contains("\"Local\" cannot analyze"), "{reply}");
    // Its model is checked as the other slots' are.
    let mut stray = chosen.clone();
    stray["analyzer"]["model"] = " big".into();
    let (status, reply) = save(&site, &stray.to_string());
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert!(
        reply.contains("the model of \"Local\" must be 1 to 200"),
        "{reply}"
    );

    chosen["analyzer"] = serde_json::Value::Null;
    let (status, reply) = save(&site, &chosen.to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");
    let analyzer = crate::settings::load(&file).settings.analyzer().unwrap();
    assert_eq!(
        (analyzer.name.as_str(), analyzer.model.as_deref()),
        ("Grok", Some("grok-4.7"))
    );

    // With no agent chosen either, there is nothing to analyze with.
    chosen["agent"] = serde_json::Value::Null;
    let (status, reply) = save(&site, &chosen.to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");
    let error = crate::settings::load(&file)
        .settings
        .analyzer()
        .unwrap_err();
    assert!(error.to_string().contains("choose the agent"), "{error}");
}

#[test]
fn a_model_id_with_a_stray_space_or_control_character_is_refused() {
    let repo = TempRepo::new("settings-bad-model");
    let site = repo.empty_site();
    let file = repo.config().join("settings.json");
    for model in ["", " pro ", "pro\n", "pro\u{7}", &"m".repeat(201)] {
        let slot = serde_json::json!({ "name": "Local", "model": model });
        let mut as_agent = custom_settings();
        as_agent["agent"] = slot.clone();
        let mut as_reviewer = custom_settings();
        as_reviewer["reviewers"][1] = slot;
        for settings in [as_agent, as_reviewer] {
            let (status, reply) = save(&site, &settings.to_string());
            assert_eq!(status, StatusCode::BAD_REQUEST, "{model:?}: {reply}");
            assert!(
                reply.contains("the model of \"Local\" must be 1 to 200 characters"),
                "{reply}"
            );
        }
    }
    // Only a file is read in the old shape: the page always sends slots.
    let mut plain_name = custom_settings();
    plain_name["agent"] = "Local".into();
    let (status, reply) = save(&site, &plain_name.to_string());
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert!(reply.contains("expected struct AgentSlot"), "{reply}");
    assert!(!file.exists(), "nothing was written");
}

#[test]
fn gemini_is_found_as_agy_and_no_text_points_at_the_gemini_cli() {
    let repo = TempRepo::new("settings-agy");
    let bin = repo.root.join(".git/test-bin");
    fs::create_dir_all(&bin).unwrap();
    let agy = bin.join("agy");
    fs::write(&agy, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o755)).unwrap();
    let search_path = bin.into_os_string();
    let site = Site::new(
        Some(&repo.config()),
        None,
        None,
        Scanner::new(move || search_path.clone()),
        quiet(),
    );

    let agents = scanned_settings(&site)["agents"].clone();
    let gemini = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["name"] == "Gemini")
        .unwrap()
        .clone();
    assert_eq!(gemini["found"], agy.to_str().unwrap(), "{gemini}");
    assert_eq!(gemini["command"], "agy acp");

    let usage = |site: &Site| {
        let body = get_json(site, "postil://localhost/api/usage?scope=all");
        body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["name"] == "Gemini")
            .unwrap()["quota"]
            .clone()
    };
    assert_eq!(usage(&site)["status"], "loading");
    site.quotas.join_probes();
    let quota = usage(&site);
    assert_eq!(quota["status"], "unavailable", "{quota}");
    let message = quota["message"].as_str().unwrap();
    assert!(message.contains("agy"), "{message}");
    assert!(!message.contains("gemini"), "{message}");

    // Installed, so it is started for its models: as `agy acp`, which here exits at once.
    let body = models_body(&["Gemini"], &[]);
    let (status, reply) = post(&site, MODELS, &body);
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(
        reply.contains(r#""Gemini":{"status":"loading"}"#),
        "{reply}"
    );
    site.models.join_probes();
    let (_, reply) = post(&site, MODELS, &body);
    assert!(reply.contains("the agent exited"), "{reply}");
}

/// The body of `POST /api/agents/models`, as the page sends it.
fn models_body(agents: &[&str], drafts: &[(&str, &str)]) -> String {
    let drafts: Vec<_> = drafts
        .iter()
        .map(|(name, command)| serde_json::json!({ "name": name, "command": command }))
        .collect();
    serde_json::json!({ "agents": agents, "customAgents": drafts }).to_string()
}

const MODELS: &str = "postil://localhost/api/agents/models";

#[test]
fn agents_report_their_models_for_saved_and_unsaved_custom_agents() {
    let repo = TempRepo::new("settings-models");
    let path = std::env::var_os("PATH").expect("PATH is set");
    let site = Site::new(
        Some(&repo.config()),
        None,
        None,
        Scanner::new(move || path.clone()),
        quiet(),
    );
    let settings = serde_json::json!({
        "customAgents": [{ "name": "Saved", "command": fake_agent("plain") }],
        "enabledAgents": ["Saved"],
    });
    assert_eq!(save(&site, &settings.to_string()).0, StatusCode::OK);
    site.agents.wait();
    let body = models_body(
        &["Saved", "Draft", "Broken"],
        &[
            ("Draft", &fake_agent("no-config")),
            ("Broken", "/nonexistent/agent --acp"),
        ],
    );

    let (status, reply) = post(&site, MODELS, &body);
    assert_eq!(status, StatusCode::OK, "{reply}");
    let first: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(first["loading"], true);
    assert_eq!(first["models"]["Saved"]["status"], "loading");

    site.models.join_probes();
    let (status, reply) = post(&site, MODELS, &body);
    assert_eq!(status, StatusCode::OK, "{reply}");
    let done: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(done["loading"], false, "{done}");
    // The boolean that also says "model" is passed over, and the group is flattened.
    assert_eq!(
        done["models"]["Saved"],
        serde_json::json!({
            "status": "ready",
            "current": "m",
            "options": [
                { "value": "m", "name": "Model M" },
                { "value": "pro", "name": "Pro", "description": "The careful one" },
                { "value": "fast", "name": "Fast" },
                { "value": "deep", "name": "Deep" },
                { "value": "long", "name": "OpenCode/Nemotron 3.5 Super VL Free" },
            ],
        })
    );
    assert_eq!(
        done["models"]["Draft"],
        serde_json::json!({ "status": "ready", "current": "m", "options": [] }),
        "an agent without a model select offers no choice"
    );
    assert_eq!(done["models"]["Broken"]["status"], "failed");
    let error = done["models"]["Broken"]["error"].as_str().unwrap();
    assert!(
        error.contains("cannot find the agent's program /nonexistent/agent"),
        "{error}"
    );

    // A rescan forgets what agents reported: an update may have changed their models.
    let (status, reply) = post(&site, "postil://localhost/api/agents/rescan", "");
    assert_eq!(status, StatusCode::OK, "{reply}");
    let (_, reply) = post(&site, MODELS, &body);
    let rescanned: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(rescanned["models"]["Saved"]["status"], "loading", "{reply}");
    site.models.join_probes();

    let (status, reply) = post(&site, MODELS, &models_body(&["Nobody"], &[]));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        reply.contains("there is no agent called \"Nobody\""),
        "{reply}"
    );
    let quoted = models_body(&["Odd"], &[("Odd", "run \"a b\"")]);
    let (status, reply) = post(&site, MODELS, &quoted);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(reply.contains("cannot hold quotes"), "{reply}");
}

#[test]
fn a_built_in_agent_that_is_not_installed_is_not_started_for_its_models() {
    let repo = TempRepo::new("settings-models-missing");
    let site = repo.empty_site();
    site.agents.wait();
    let (status, reply) = post(&site, MODELS, &models_body(&["Claude"], &[]));
    assert_eq!(status, StatusCode::OK, "{reply}");
    let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        view,
        serde_json::json!({
            "loading": false,
            "models": { "Claude": { "status": "failed", "error": "Claude is not installed" } },
        })
    );
    let (status, _) = get(&site, MODELS);
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn external_tools_are_detected_on_the_search_path_for_the_startup_wizard() {
    let repo = TempRepo::new("settings-tools");
    let bin = repo.root.join(".git/test-bin");
    fs::create_dir_all(&bin).unwrap();
    for program in ["git", "npx"] {
        let path = bin.join(program);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let search_path = bin.clone().into_os_string();
    let site = Site::new(
        Some(&repo.config()),
        None,
        None,
        Scanner::new(move || search_path.clone()),
        quiet(),
    );

    let view = scanned_settings(&site);
    assert_eq!(view["tools"]["git"], bin.join("git").to_str().unwrap());
    assert_eq!(view["tools"]["npx"], bin.join("npx").to_str().unwrap());
    assert_eq!(view["tools"]["gh"], serde_json::Value::Null);
}

#[test]
fn settings_that_cannot_be_read_or_written_say_so() {
    let repo = TempRepo::new("settings-io");
    // A directory where the settings file should be cannot be read as one.
    fs::create_dir_all(repo.config().join("settings.json")).unwrap();
    let site = repo.empty_site();
    let problem = get_json(&site, SETTINGS)["problem"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        problem.contains("settings.json cannot be read"),
        "{problem}"
    );
    let (status, reply) = save(&site, &custom_settings().to_string());
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply.ends_with("(fix the file, or reset the settings)"),
        "{reply}"
    );

    if skip_as_root() {
        return;
    }
    // A read-only configuration directory: the file is missing, so saving may go ahead,
    // and then the write fails.
    let blocked = TempRepo::new("settings-unwritable");
    fs::create_dir_all(blocked.config()).unwrap();
    fs::set_permissions(blocked.config(), fs::Permissions::from_mode(0o555)).unwrap();
    let (status, reply) = save(&blocked.empty_site(), &custom_settings().to_string());
    // Writable again, so the repository's drop can remove it.
    fs::set_permissions(blocked.config(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{reply}");
    assert!(reply.contains("the settings were not saved"), "{reply}");
}

#[test]
fn a_linked_settings_file_keeps_its_link_and_its_mode() {
    let repo = TempRepo::new("settings-linked");
    let real = repo.root.join(".git/dotfiles/settings.json");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "{\"version\": 1, \"settings\": {}}\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    // Relative, as `ln -s` in a dotfiles setup often makes it: resolved from the link's
    // directory, not from the process's.
    let link = repo.config().join("settings.json");
    fs::create_dir_all(repo.config()).unwrap();
    std::os::unix::fs::symlink("../dotfiles/settings.json", &link).unwrap();

    let (status, reply) = save(&repo.empty_site(), &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "{reply}");

    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("../dotfiles/settings.json"),
        "the link is still a link, to the same place"
    );
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
    assert_eq!(stored["settings"], custom_settings());
    let mode = fs::metadata(&real).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the file is as private as before");
}

const LAYOUT: &str = "postil://localhost/api/layout";

#[test]
fn pane_widths_are_kept_per_mac_with_their_limits_and_defaults() {
    let repo = TempRepo::new("layout");
    let site = repo.empty_site();
    let defaults = serde_json::json!({ "sidebar": 248, "tray": 320, "map": 400 });
    let fresh = get_json(&site, LAYOUT);
    assert_eq!(
        fresh,
        serde_json::json!({
            "sidebar": 248, "tray": 320, "map": 400,
            "defaults": defaults,
            "limits": { "sidebar": [200, 480], "tray": [280, 720], "map": [320, 960] },
        })
    );

    let (status, body) = put(
        &site,
        LAYOUT,
        r#"{"sidebar": 300, "tray": 460, "map": 520}"#,
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    // Another window, or the next start, reads the same widths: they are the Mac's, not
    // the project's.
    let other = TempRepo::new("layout-other");
    let again = Site::new(Some(&repo.config()), None, None, no_agents(), quiet());
    again
        .open(Repo::open(&other.root).unwrap(), LiveRun::Stop)
        .unwrap();
    let kept = get_json(&again, LAYOUT);
    assert_eq!(
        (
            kept["sidebar"].clone(),
            kept["tray"].clone(),
            kept["map"].clone()
        ),
        (300.into(), 460.into(), 520.into())
    );

    // The whole message, as the page shows it: no serde position, no "unreadable" prefix.
    let (status, reply) = put(
        &site,
        LAYOUT,
        r#"{"sidebar": 248, "tray": 900, "map": 400}"#,
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(reply, "the tray is 900 px wide, outside 280 to 720 px");
    for (body, words) in [
        (
            r#"{"sidebar": 100, "tray": 320, "map": 400}"#,
            "outside 200 to 480",
        ),
        (r#"{"sidebar": 248}"#, "unreadable layout"),
        (
            r#"{"sidebar": 248, "tray": 320, "map": 400, "x": 1}"#,
            "unreadable layout",
        ),
    ] {
        let (status, reply) = put(&site, LAYOUT, body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
        assert!(reply.contains(words), "{body}: {reply}");
    }
    assert_eq!(
        get_json(&site, LAYOUT)["sidebar"],
        300,
        "a refused layout changes nothing"
    );
    assert_eq!(post(&site, LAYOUT, "{}").0, StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn a_hand_edited_layout_with_a_width_out_of_range_reads_as_the_defaults() {
    let repo = TempRepo::new("layout-hand-edit");
    let site = repo.empty_site();
    fs::create_dir_all(repo.config()).unwrap();
    fs::write(
        repo.config().join("layout.json"),
        r#"{"version": 1, "sidebar": 10, "tray": 460}"#,
    )
    .unwrap();

    let layout = get_json(&site, LAYOUT);

    assert_eq!(
        (layout["sidebar"].clone(), layout["tray"].clone()),
        (248.into(), 320.into()),
        "both widths are the defaults, not only the one out of range"
    );
}

#[test]
fn without_a_configuration_directory_widths_are_the_defaults_and_not_kept() {
    let site = Site::new(None, None, None, no_agents(), quiet());
    assert_eq!(get_json(&site, LAYOUT)["tray"], 320);
    let (status, body) = put(
        &site,
        LAYOUT,
        r#"{"sidebar": 300, "tray": 460, "map": 520}"#,
    );
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("HOME"), "{body}");
}
