//! Settings (`/api/settings` and the agents) and pane widths (`/api/layout`, layout.rs).

use super::*;

/// The settings view once the agent scan has finished, waiting at most five seconds.
fn scanned_settings(site: &Site) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let view = get_json(site, SETTINGS);
        if !view["agents"].is_null() {
            return view;
        }
        assert!(Instant::now() < deadline, "the agent scan did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
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
        "agent": "Local",
        "reviewers": ["Grok", "Grok"],
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
    assert_eq!(view["file"], file.to_str().unwrap());
    assert_eq!(view["fontSizes"], serde_json::json!([11, 20]));
    assert_eq!(view["tabWidths"], serde_json::json!([2, 4, 8]));
    assert!(!file.exists(), "reading the defaults writes nothing");

    let (status, body) = save(&repo.empty_site(), &custom_settings().to_string());
    assert_eq!(status, StatusCode::OK, "{body}");
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        saved["settings"],
        custom_settings(),
        "the reply is the new settings"
    );

    let restarted = get_json(&repo.empty_site(), SETTINGS);
    assert_eq!(restarted["settings"], custom_settings());
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 1);
    assert_eq!(stored["settings"], custom_settings());
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
            with("agent", "Claude".into()),
            "notes cannot go to \"Claude\"",
        ),
        (
            with("reviewers", serde_json::json!(["Claude"])),
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
    at_limits["enabledAgents"] = serde_json::json!([longest]);
    at_limits["agent"] = longest.into();
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
            "{\"version\": 2, \"settings\": {}}",
            "written by a newer Postil (format version 2)",
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
    let site = Site::new(None, None, no_agents(), quiet());
    let view = get_json(&site, SETTINGS);
    assert_eq!(
        view["problem"],
        "settings are off: cannot find the home directory (is HOME set?)"
    );
    assert_eq!(view["file"], serde_json::Value::Null);
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
    let defaults = serde_json::json!({ "sidebar": 248, "tray": 320 });
    let fresh = get_json(&site, LAYOUT);
    assert_eq!(
        fresh,
        serde_json::json!({
            "sidebar": 248, "tray": 320,
            "defaults": defaults,
            "limits": { "sidebar": [200, 480], "tray": [280, 720] },
        })
    );

    let (status, body) = put(&site, LAYOUT, r#"{"sidebar": 300, "tray": 460}"#);
    assert_eq!(status, StatusCode::OK, "{body}");
    // Another window, or the next start, reads the same widths: they are the Mac's, not
    // the project's.
    let other = TempRepo::new("layout-other");
    let again = Site::new(Some(&repo.config()), None, no_agents(), quiet());
    again
        .open(Repo::open(&other.root).unwrap(), LiveRun::Stop)
        .unwrap();
    let kept = get_json(&again, LAYOUT);
    assert_eq!(
        (kept["sidebar"].clone(), kept["tray"].clone()),
        (300.into(), 460.into())
    );

    for (body, words) in [
        (r#"{"sidebar": 100, "tray": 320}"#, "outside 200 to 480"),
        (r#"{"sidebar": 248}"#, "unreadable layout"),
        (
            r#"{"sidebar": 248, "tray": 320, "x": 1}"#,
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
fn without_a_configuration_directory_widths_are_the_defaults_and_not_kept() {
    let site = Site::new(None, None, no_agents(), quiet());
    assert_eq!(get_json(&site, LAYOUT)["tray"], 320);
    let (status, body) = put(&site, LAYOUT, r#"{"sidebar": 300, "tray": 460}"#);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("HOME"), "{body}");
}
