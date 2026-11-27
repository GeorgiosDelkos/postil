//! Files: `/api/file`, the page's own files, query strings, and `/api/folders`.

use super::*;

#[test]
fn file_comes_back_as_lines_of_tokens() {
    let repo = TempRepo::new("file");
    repo.write("src/my file.rs", "// hi\nfn a() {}\n");

    let view = get_json(
        &repo.site(),
        "postil://localhost/api/file?path=src%2Fmy%20file.rs",
    );

    assert_eq!(view["path"], "src/my file.rs");
    assert_eq!(view["lines"].as_array().unwrap().len(), 2);
    assert_eq!(view["lines"][0], serde_json::json!([["m", "// hi"]]));
    assert_eq!(view["lines"][1][0], serde_json::json!(["k", "fn"]));
}

#[test]
fn a_file_over_the_read_limit_is_refused_with_its_size_and_one_at_it_is_shown() {
    let repo = TempRepo::new("file-too-large");
    let limit = usize::try_from(crate::repo::MAX_READ).expect("20 MB fits a usize");
    repo.write("at.txt", &"x".repeat(limit));
    repo.write("over.txt", &"x".repeat(limit + 1));
    // Valid text with a two-byte character just past the limit, so the bytes read (the
    // limit plus one) end inside it: refused as large, not as "not text".
    repo.write("wide.txt", &format!("{}é", "x".repeat(limit)));
    let site = repo.site();

    let (status, body) = get(&site, "postil://localhost/api/file?path=wide.txt");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("too large to show"), "{body}");

    let (status, body) = get(&site, "postil://localhost/api/file?path=at.txt");
    assert_eq!(status, StatusCode::OK, "at the limit the file is shown");
    assert!(body.starts_with(r#"{"path":"at.txt""#), "{}", &body[..40]);

    let (status, body) = get(&site, "postil://localhost/api/file?path=over.txt");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.contains("over.txt is 21 MB, too large to show"),
        "{body}"
    );
}

#[test]
fn file_outside_the_repository_is_refused() {
    let repo = TempRepo::new("outside");
    let site = repo.site();

    for path in [
        "..%2Fsecret",
        "src%2F..%2F..%2Fsecret",
        "%2Fetc%2Fpasswd",
        "",
    ] {
        let (status, body) = get(&site, &format!("postil://localhost/api/file?path={path}"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
}

#[test]
fn missing_file_and_missing_parameter_are_errors_with_a_message() {
    let repo = TempRepo::new("missing");
    let site = repo.site();

    let (status, body) = get(&site, "postil://localhost/api/file?path=nope.rs");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("nope.rs"), "{body}");

    let (status, _) = get(&site, "postil://localhost/api/file");
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = get(&site, "postil://localhost/nope");
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn page_assets_are_served_with_their_types() {
    let repo = TempRepo::new("assets");
    let site = repo.site();
    let cases = [
        ("/", "text/html"),
        ("/app.css", "text/css"),
        ("/icons.svg", "image/svg+xml"),
        ("/agent-logos.svg", "image/svg+xml"),
        ("/fonts/Geist-Variable.woff2", "font/woff2"),
        ("/fonts/GeistMono-Variable.woff2", "font/woff2"),
    ];

    for (path, content_type) in cases {
        let uri = format!("postil://localhost{path}");
        let request = Request::builder().uri(uri).body(Vec::new()).unwrap();
        let reply = site.respond(&request);
        assert_eq!(reply.status(), StatusCode::OK, "{path}");
        assert_eq!(reply.headers()[CONTENT_TYPE], content_type, "{path}");
        assert!(!reply.body().is_empty(), "{path}");
    }
}

#[test]
fn page_only_links_to_assets_that_are_served() {
    // The page is checked by eye, but a renamed asset would only show as a blank window.
    // The served paths themselves are covered by `page_assets_are_served_with_their_types`
    // and `every_page_module_is_served`.
    for link in ["app.css", "app.js"] {
        assert!(INDEX_HTML.contains(&format!("\"{link}\"")), "{link}");
    }
    // Every module any page module imports is in PAGE.
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    for entry in fs::read_dir(&ui).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "js") {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        for import in source.split("from \"./").skip(1) {
            let name = format!("/{}", import.split('"').next().unwrap());
            assert!(
                PAGE.iter().any(|(served, ..)| *served == name),
                "{} imports {name}",
                path.display()
            );
        }
    }
    for sprite in ["icons.svg", "agent-logos.svg"] {
        assert!(
            DOM_JS.contains(&format!("\"{sprite}\"")),
            "dom.js loads {sprite}"
        );
    }
    for font in [
        "fonts/Geist-Variable.woff2",
        "fonts/GeistMono-Variable.woff2",
    ] {
        assert!(APP_CSS.contains(font), "{font}");
    }
}

#[test]
fn query_value_decodes_what_encode_uri_component_writes() {
    // encodeURIComponent("src/my file+é.rs")
    let query = "path=src%2Fmy%20file%2B%C3%A9.rs";
    assert_eq!(query_value(query, "path").unwrap(), "src/my file+é.rs");
    // Percent-encoding allows lowercase hex too, though the page never writes it.
    assert_eq!(query_value("path=%c3%a9%2f", "path").unwrap(), "é/");
}

#[test]
fn query_value_finds_the_key_among_others() {
    assert_eq!(query_value("a=1&path=x&b=2", "path").unwrap(), "x");
    assert_eq!(query_value("pathology=1&path=x", "path").unwrap(), "x");
    assert_eq!(query_value("path=", "path").unwrap(), "");
}

#[test]
fn query_value_rejects_missing_keys_and_bad_escapes() {
    assert_eq!(query_value("", "path"), None);
    assert_eq!(query_value("other=x", "path"), None);
    assert_eq!(query_value("path=%", "path"), None, "escape cut short");
    assert_eq!(query_value("path=%4", "path"), None, "escape cut short");
    assert_eq!(query_value("path=%zz", "path"), None, "not hex");
    assert_eq!(query_value("path=%ff", "path"), None, "not UTF-8");
}

#[test]
fn open_folders_are_kept_per_working_tree_and_read_back() {
    let repo = TempRepo::new("folders");
    repo.write("src/a.rs", "fn a() {}\n");
    let site = repo.site();
    let folders = "postil://localhost/api/folders";
    assert_eq!(get_json(&site, folders), serde_json::json!({ "open": [] }));

    let (status, body) = put(&site, folders, r#"{"open": ["src", "src/gone"]}"#);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        get_json(&repo.site(), folders),
        serde_json::json!({ "open": ["src", "src/gone"] }),
        "a new process reads them"
    );
    assert!(repo.root.join(".git/postil/folders.json").exists());

    let (status, body) = put(&site, folders, r#"{"open": "src"}"#);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("unreadable request"), "{body}");
    assert_eq!(post(&site, folders, "").0, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        get(&repo.empty_site(), folders).0,
        StatusCode::CONFLICT,
        "no project"
    );
}

#[test]
fn every_page_module_is_served() {
    // The page imports its modules by name; one left out of PAGE stops the page.
    let site = Site::new(None, None, None, no_agents(), quiet());
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    for entry in fs::read_dir(&ui).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let path = Path::new(&name);
        let script = path.extension().is_some_and(|extension| extension == "js");
        let test = path
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().ends_with(".test"));
        if !script || test {
            continue;
        }
        let uri = format!("postil://localhost/{name}");
        let reply = site.respond(&Request::builder().uri(uri).body(Vec::new()).unwrap());
        assert_eq!(reply.status(), StatusCode::OK, "{name}");
        // WebKit refuses to run a module served under another type.
        assert_eq!(reply.headers()[CONTENT_TYPE], "text/javascript", "{name}");
        let source = fs::read_to_string(ui.join(&name)).unwrap();
        assert_eq!(reply.body().as_ref(), source.as_bytes(), "{name}");
    }
}

/// The sha256 of `@dagrejs/dagre` 3.1.3's `dist/dagre.min.js`, the release vendored as
/// `ui/vendor/dagre.min.js` (docs/design.md, "The map's layout").
const DAGRE_SHA256: &str = "54a503e346f93ca0bb10abe7999587e037262935687aca45eb4e4b0f64bbb578";

#[test]
fn every_vendor_file_is_served() {
    // The vendored scripts are loaded by a script element, not imported, so the module tests
    // above do not see them; a missing row would only show when the map first opens.
    let site = Site::new(None, None, None, no_agents(), quiet());
    let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/vendor");
    let mut served = Vec::new();
    for entry in fs::read_dir(&vendor).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "LICENSE")
        {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let uri = format!("postil://localhost/vendor/{name}");
        let reply = site.respond(&Request::builder().uri(uri).body(Vec::new()).unwrap());
        assert_eq!(reply.status(), StatusCode::OK, "{name}");
        assert_eq!(reply.headers()[CONTENT_TYPE], "text/javascript", "{name}");
        assert_eq!(reply.body().as_ref(), fs::read(&path).unwrap(), "{name}");
        served.push(name);
    }
    assert_eq!(served, ["dagre.min.js"]);
    assert!(
        include_str!("../../../ui/diagram-view.js").contains("\"vendor/dagre.min.js\""),
        "the map loads the served path"
    );

    // The bytes are the release vendored, and the license file records their hash, so an
    // edited bundle fails here and not in review.
    let hash = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(vendor.join("dagre.min.js"))
        .output()
        .unwrap();
    let hash = String::from_utf8(hash.stdout).unwrap();
    let hash = hash.split_whitespace().next().unwrap();
    let license = fs::read_to_string(vendor.join("dagre.LICENSE")).unwrap();
    assert_eq!(
        hash, DAGRE_SHA256,
        "ui/vendor/dagre.min.js is not the 3.1.3 release"
    );
    assert!(license.contains(&format!("sha256: {DAGRE_SHA256}")));
}
