//! Projects: `/api/repo`, `/api/open`, `/api/init` and `/api/recent`, and the project
//! opened at start (`startup_repo`), which no request reaches.

use super::*;

#[test]
fn repo_lists_tracked_and_untracked_files_but_not_ignored_ones() {
    let repo = TempRepo::new("list");
    repo.write("src/tracked.rs", "fn a() {}\n");
    repo.git(&["add", "src/tracked.rs"]);
    repo.write("untracked.txt", "x\n");
    repo.write(".gitignore", "ignored.log\n");
    repo.write("ignored.log", "x\n");

    let info = get_json(&repo.site(), "postil://localhost/api/repo");

    assert_eq!(info["branch"], "trunk");
    assert_eq!(
        info["name"],
        repo.root.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(
        info["files"],
        serde_json::json!([".gitignore", "src/tracked.rs", "untracked.txt"])
    );
    assert_eq!(info["bench"], serde_json::Value::Null);
}

#[test]
fn repo_leaves_out_repositories_inside_it_untracked_or_submodules() {
    let repo = TempRepo::new("list-nested");
    repo.write("a.txt", "a\n");
    repo.git(&["init", "--quiet", "nested"]);
    repo.write("nested/n.txt", "n\n");
    // A submodule as the index records it: a gitlink, mode 160000, naming a commit of the
    // other repository (any id will do; nothing here reads it).
    fs::create_dir(repo.root.join("sub")).unwrap();
    let gitlink = format!("160000,{},sub", "1".repeat(40));
    repo.git(&["update-index", "--add", "--cacheinfo", &gitlink]);

    let info = get_json(&repo.site(), "postil://localhost/api/repo");

    assert_eq!(info["files"], serde_json::json!(["a.txt"]));
}

#[test]
fn repo_leaves_out_files_outside_a_sparse_checkout() {
    let repo = TempRepo::new("list-sparse");
    fs::create_dir(repo.root.join("away")).unwrap();
    repo.write("src/kept.rs", "fn kept() {}\n");
    repo.write("away/gone.rs", "fn gone() {}\n");
    repo.git(&["add", "."]);
    let identity = ["-c", "user.name=t", "-c", "user.email=t@example.com"];
    repo.git(&[&identity[..], &["commit", "--quiet", "-m", "both"]].concat());
    // `away/gone.rs` stays in the index but leaves the disk.
    repo.git(&["sparse-checkout", "set", "src"]);

    let info = get_json(&repo.site(), "postil://localhost/api/repo");

    assert_eq!(info["files"], serde_json::json!(["src/kept.rs"]));
}

#[test]
fn repo_keeps_a_skip_worktree_file_that_is_on_disk() {
    // Not in a sparse checkout: there git ignores a skip-worktree bit set by hand.
    let repo = TempRepo::new("list-skip-worktree");
    repo.write("src/local.toml", "key = 1\n");
    repo.git(&["add", "."]);
    let identity = ["-c", "user.name=t", "-c", "user.email=t@example.com"];
    repo.git(&[&identity[..], &["commit", "--quiet", "-m", "one"]].concat());
    // Hidden from `git status`, as people do with a config file they edit locally.
    repo.git(&["update-index", "--skip-worktree", "src/local.toml"]);

    let info = get_json(&repo.site(), "postil://localhost/api/repo");

    assert_eq!(info["files"], serde_json::json!(["src/local.toml"]));
}

#[test]
fn a_repository_whose_path_holds_a_newline_opens() {
    let probe = std::env::temp_dir().join(format!("postil-{}-probe\n", std::process::id()));
    if let Err(error) = fs::create_dir(&probe) {
        eprintln!("skipped: this filesystem refuses a newline in a name ({error})");
        return;
    }
    fs::remove_dir(&probe).unwrap();
    let repo = TempRepo::new("line\nbreak");
    repo.write("src/a.rs", "fn a() {}\n");

    let info = get_json(&repo.site(), "postil://localhost/api/repo");

    assert_eq!(info["files"], serde_json::json!(["src/a.rs"]));
    let name = format!("postil-{}-line\nbreak", std::process::id());
    assert_eq!(info["name"], name);
}

#[test]
fn without_a_project_the_page_gets_null_and_project_routes_say_to_open_one() {
    let repo = TempRepo::new("no-project");
    let site = repo.empty_site();

    let (status, body) = get(&site, "postil://localhost/api/repo");
    assert_eq!((status, body.as_str()), (StatusCode::OK, "null"));
    for uri in ["/api/notes", "/api/file?path=a.rs"] {
        let (status, body) = get(&site, &format!("postil://localhost{uri}"));
        assert_eq!(status, StatusCode::CONFLICT, "{uri}");
        assert!(body.contains("open a folder first"), "{uri}: {body}");
    }
}

#[test]
fn opening_a_folder_makes_its_repository_the_project_and_lists_it_first() {
    let first = TempRepo::new("open-first");
    let second = TempRepo::new("open-second");
    second.write("src/b.rs", "fn b() {}\n");
    let site = first.empty_site();

    for repo in [&first, &second] {
        let (status, body) = post(
            &site,
            "postil://localhost/api/open",
            &open_request(&repo.root),
        );
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let info = get_json(&site, "postil://localhost/api/repo");
    assert_eq!(info["files"], serde_json::json!(["src/b.rs"]));
    assert_eq!(info["warning"], serde_json::Value::Null);

    let recent = get_json(&site, "postil://localhost/api/recent");
    let paths: Vec<&str> = recent
        .as_array()
        .unwrap()
        .iter()
        .map(|project| project["path"].as_str().unwrap())
        .collect();
    let root = |repo: &TempRepo| site_root(&repo.root);
    assert_eq!(paths, [root(&second), root(&first)]);
    assert_eq!(recent[0]["exists"], true);
    assert_eq!(
        recent[0]["name"],
        second.root.file_name().unwrap().to_str().unwrap()
    );
}

#[test]
fn a_plain_folder_is_refused_with_its_own_status_until_it_is_made_a_repository() {
    let repo = TempRepo::new("init-recent");
    let site = repo.empty_site();
    let folder = plain_folder("init");

    let (status, body) = post(&site, "postil://localhost/api/open", &open_request(&folder));
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("is not in a git repository"), "{body}");
    assert_eq!(get(&site, "postil://localhost/api/repo").1, "null");

    let (status, body) = post(&site, "postil://localhost/api/init", &open_request(&folder));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(folder.join(".git").is_dir());
    let info = get_json(&site, "postil://localhost/api/repo");
    assert_eq!(info["root"], site_root(&folder));
    drop(fs::remove_dir_all(&folder));
}

#[test]
fn bad_open_requests_name_the_problem() {
    let repo = TempRepo::new("open-bad");
    let site = repo.empty_site();

    let missing = open_request(Path::new("/no/such/folder"));
    let (status, body) = post(&site, "postil://localhost/api/open", &missing);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "/no/such/folder is not a folder");

    let (status, body) = post(&site, "postil://localhost/api/open", "{\"dir\": \"x\"}");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("unreadable request"), "{body}");

    for route in ["open", "init"] {
        let (status, _) = get(&site, &format!("postil://localhost/api/{route}"));
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{route}");
    }
}

#[test]
fn a_folder_with_a_broken_git_link_gets_git_s_error_and_no_offer_to_init() {
    let repo = TempRepo::new("broken-gitlink");
    let site = repo.empty_site();
    let folder = plain_folder("broken-gitlink");
    // What a linked worktree looks like after its git directory was removed.
    fs::write(folder.join(".git"), "gitdir: /no/such/git/dir\n").unwrap();

    let (status, body) = post(&site, "postil://localhost/api/open", &open_request(&folder));
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.starts_with("git rev-parse"), "{body}");
    let (status, _) = post(&site, "postil://localhost/api/init", &open_request(&folder));
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(folder.join(".git").is_file(), "the link is left as it was");
    drop(fs::remove_dir_all(&folder));
}

#[test]
fn a_recent_list_that_cannot_be_written_warns_and_the_project_still_opens() {
    let repo = TempRepo::new("recent-unwritable");
    // The list's path is a directory, so no file can be renamed onto it.
    fs::create_dir_all(repo.config().join("recent.json/inside")).unwrap();
    let site = repo.empty_site();

    let (status, body) = post(
        &site,
        "postil://localhost/api/open",
        &open_request(&repo.root),
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    // Every load of the project shows it: a request from the page being replaced must
    // not be the one that takes it.
    for _ in 0..2 {
        let info = get_json(&site, "postil://localhost/api/repo");
        let warning = info["warning"].as_str().unwrap();
        assert!(
            warning.contains("cannot update the recent projects"),
            "{warning}"
        );
    }
}

#[test]
fn without_a_configuration_directory_the_recent_list_is_off_and_projects_open() {
    let repo = TempRepo::new("recent-off");
    let site = Site::new(None, None, None, no_agents(), quiet());

    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    let info = get_json(&site, "postil://localhost/api/repo");
    assert_eq!(
        info["warning"],
        "recent projects are off: cannot find the home directory (is HOME set?)"
    );
    assert_eq!(get(&site, "postil://localhost/api/recent").1, "[]");
}

#[test]
fn a_benchmark_leaves_the_recent_list_alone() {
    let repo = TempRepo::new("recent-bench");
    repo.write("a.rs", "fn a() {}\n");
    let recent = repo.config().join("recent.json");
    let site = Site::new(
        Some(&repo.config()),
        None,
        Some("a.rs"),
        no_agents(),
        quiet(),
    );

    site.open(Repo::open(&repo.root).unwrap(), LiveRun::Stop)
        .unwrap();
    assert!(!recent.exists());
    assert_eq!(
        get_json(&site, "postil://localhost/api/repo")["warning"],
        serde_json::Value::Null
    );
}

#[test]
fn init_inside_a_repository_opens_it_and_makes_no_repository_within() {
    let repo = TempRepo::new("init-inside");
    repo.write("src/a.rs", "fn a() {}\n");
    let site = repo.empty_site();

    let src = repo.root.join("src");
    let (status, body) = post(&site, "postil://localhost/api/init", &open_request(&src));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!src.join(".git").exists(), "no nested repository");
    let info = get_json(&site, "postil://localhost/api/repo");
    assert_eq!(info["root"], site_root(&repo.root));
}

#[test]
fn a_recent_project_that_was_moved_is_listed_as_missing() {
    let repo = TempRepo::new("recent-moved");
    let site = repo.empty_site();
    let folder = plain_folder("moved");
    let (status, body) = post(&site, "postil://localhost/api/init", &open_request(&folder));
    assert_eq!(status, StatusCode::OK, "{body}");
    fs::remove_dir_all(&folder).unwrap();

    let recent = get_json(&site, "postil://localhost/api/recent");
    assert_eq!(recent[0]["exists"], false);
}

#[test]
fn repo_is_found_from_a_subdirectory() {
    let repo = TempRepo::new("subdir");
    repo.write("src/a.rs", "fn a() {}\n");

    let site = Site::new(
        Some(&repo.config()),
        None,
        Some("src/a.rs"),
        no_agents(),
        quiet(),
    );
    site.open(Repo::open(&repo.root.join("src")).unwrap(), LiveRun::Stop)
        .unwrap();
    let info = get_json(&site, "postil://localhost/api/repo");

    assert_eq!(info["files"], serde_json::json!(["src/a.rs"]));
    assert_eq!(info["bench"], "src/a.rs");
}

#[test]
fn the_last_project_reopens_only_when_the_settings_ask_and_it_still_opens() {
    let repo = TempRepo::new("settings-reopen");
    let other = TempRepo::new("settings-reopen-other");
    let recent = repo.config().join("recent.json");
    recent::record(&recent, Path::new(&site_root(&other.root))).unwrap();
    let raw: RawSettings =
        serde_json::from_value(serde_json::json!({ "reopenLastProject": true })).unwrap();
    let reopen = Settings::try_from(raw).unwrap();
    let root = |found: Option<Repo>| found.map(|repo| repo.root().to_owned());

    let opened = startup_repo(None, &reopen, Some(&recent)).unwrap();
    assert_eq!(root(opened), Some(PathBuf::from(site_root(&other.root))));
    let off = startup_repo(None, &Settings::default(), Some(&recent)).unwrap();
    assert_eq!(root(off), None, "off by default");
    let given = startup_repo(Some(&repo.root), &reopen, Some(&recent)).unwrap();
    assert_eq!(
        root(given),
        Some(PathBuf::from(site_root(&repo.root))),
        "a folder on the command line wins"
    );
    assert_eq!(root(startup_repo(None, &reopen, None).unwrap()), None);

    let other_root = other.root.clone();
    drop(other);
    assert!(!other_root.exists());
    let gone = startup_repo(None, &reopen, Some(&recent)).unwrap();
    assert_eq!(
        root(gone),
        None,
        "a project that is gone shows the welcome screen"
    );
}
