//! Jump to definition: `/api/definitions` and `/api/definitions/refresh`.

use super::*;

/// Waits until the open project's index has finished the passes asked for so far.
fn wait_for_index(site: &Site) {
    let project = site
        .project
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    project
        .as_ref()
        .expect("a project is open")
        .definitions
        .wait();
}

/// `GET /api/definitions?name=<name>&from=<from>` (both already percent-encoded), which must
/// succeed, as JSON.
fn definitions(site: &Site, name: &str, from: &str) -> serde_json::Value {
    get_json(
        site,
        &format!("postil://localhost/api/definitions?name={name}&from={from}"),
    )
}

fn refresh(site: &Site) {
    let (status, body) = post(site, "postil://localhost/api/definitions/refresh", "");
    assert_eq!((status, body.as_str()), (StatusCode::OK, "{}"));
}

#[test]
fn a_definition_follows_an_edit() {
    let repo = TempRepo::new("definitions-edit");
    repo.write("src/a.rs", "fn alpha() {}\n");
    let site = repo.site();
    wait_for_index(&site);

    assert_eq!(
        definitions(&site, "alpha", "src%2Fb.rs"),
        serde_json::json!({
            "state": "ready",
            "definitions": [{ "path": "src/a.rs", "line": 1, "kind": "function" }],
            "total": 1,
        })
    );

    repo.write("src/a.rs", "// moved\nfn alpha() {}\n");
    refresh(&site);
    wait_for_index(&site);

    assert_eq!(definitions(&site, "alpha", "")["definitions"][0]["line"], 2);
}

#[test]
fn a_deleted_file_leaves_no_definitions() {
    let repo = TempRepo::new("definitions-deleted");
    repo.write("src/a.rs", "fn alpha() {}\n");
    let site = repo.site();
    wait_for_index(&site);
    assert_eq!(definitions(&site, "alpha", "")["total"], 1);

    fs::remove_file(repo.root.join("src/a.rs")).unwrap();
    refresh(&site);
    wait_for_index(&site);

    assert_eq!(definitions(&site, "alpha", "")["total"], 0);
}

#[test]
fn a_name_with_no_definition_is_an_empty_list() {
    let repo = TempRepo::new("definitions-none");
    // A JavaScript name with a `$`, sent percent-encoded as the page sends it.
    repo.write("src/a.js", "const $el = () => 1;\n");
    let site = repo.site();
    wait_for_index(&site);

    assert_eq!(
        definitions(&site, "nothing", ""),
        serde_json::json!({ "state": "ready", "definitions": [], "total": 0 })
    );
    assert_eq!(definitions(&site, "%24el", "")["total"], 1);
}

#[test]
fn a_lookup_needs_a_name_and_an_open_project() {
    let repo = TempRepo::new("definitions-refused");

    let site = repo.empty_site();
    let (status, body) = get(&site, "postil://localhost/api/definitions?name=alpha");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = post(&site, "postil://localhost/api/definitions/refresh", "");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let site = repo.site();
    for query in ["", "?name=", "?from=src%2Fa.rs"] {
        let (status, body) = get(&site, &format!("postil://localhost/api/definitions{query}"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert!(body.contains("?name="), "{body}");
    }
}

#[test]
fn a_file_says_whether_its_language_has_definitions() {
    let repo = TempRepo::new("definitions-file");
    repo.write("src/a.rs", "fn alpha() {}\n");
    repo.write("src/a.json", "{}\n");
    let site = repo.site();

    let file = |path| get_json(&site, &format!("postil://localhost/api/file?path={path}"));
    assert_eq!(file("src%2Fa.rs")["definitions"], true);
    assert_eq!(file("src%2Fa.json")["definitions"], false);
}
