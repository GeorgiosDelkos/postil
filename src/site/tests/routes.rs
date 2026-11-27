//! Which methods each route takes, and what it answers to the others.

use super::*;

/// Every route, the methods it takes, and its reply to any other method (405).
const ROUTES: &[(&str, &[Method], &str)] = &[
    ("/api/repo", &[Method::GET], "GET"),
    ("/api/recent", &[Method::GET], "GET"),
    ("/api/open", &[Method::POST], "POST {\"path\": ...}"),
    ("/api/init", &[Method::POST], "POST {\"path\": ...}"),
    (
        "/api/notes",
        &[Method::GET, Method::POST, Method::PATCH, Method::DELETE],
        "GET the notes, POST a new one, PATCH ?id=N1 its text, or DELETE ?id=N1",
    ),
    (
        "/api/notes/resolve",
        &[Method::POST],
        "POST ?id=<note id> with no body",
    ),
    (
        "/api/notes/reopen",
        &[Method::POST],
        "POST ?id=<note id> with no body",
    ),
    (
        "/api/folders",
        &[Method::GET, Method::PUT],
        "GET the open folders or PUT them",
    ),
    (
        "/api/settings",
        &[Method::GET, Method::PUT],
        "GET the settings or PUT new ones",
    ),
    ("/api/settings/reset", &[Method::POST], "POST with no body"),
    ("/api/agents/rescan", &[Method::POST], "POST with no body"),
    (
        "/api/agents/models",
        &[Method::POST],
        "POST {\"agents\": [...], \"customAgents\": [...]}",
    ),
    ("/api/memory", &[Method::GET], "GET ?id=mem_..."),
    (
        "/api/memory/recall",
        &[Method::GET],
        "GET ?path=<path>&node=<node label>, ?run=R1, or ?text=<query>",
    ),
    (
        "/api/memory/draft",
        &[Method::POST],
        "POST {\"note\": \"N1\"}",
    ),
    (
        "/api/memory/remember",
        &[Method::POST],
        "POST {\"note\": \"N1\", \"noteType\": ..., \"summary\": ..., \"body\": ..., \
         \"scope\": \"repo\" or \"global\"}",
    ),
    ("/api/memory/refresh", &[Method::POST], "POST with no body"),
    ("/api/memory/check", &[Method::POST], "POST with no body"),
    ("/api/memory/setup", &[Method::POST], "POST with no body"),
    ("/api/usage", &[Method::GET], "GET"),
    ("/api/usage/refresh", &[Method::POST], "POST"),
    ("/api/runs", &[Method::GET], "GET"),
    (
        "/api/layout",
        &[Method::GET, Method::PUT],
        "GET the pane widths or PUT new ones",
    ),
    ("/api/file", &[Method::GET], "GET"),
    (
        "/api/analysis",
        &[Method::GET, Method::POST, Method::DELETE],
        "GET, POST {\"kind\": ...}, or DELETE ?diagram=<diagram id>",
    ),
    (
        "/api/analysis/update",
        &[Method::POST],
        "POST {\"diagram\": ...}, or with no body for every diagram",
    ),
    (
        "/api/definitions",
        &[Method::GET],
        "GET ?name=<name>&from=<path>",
    ),
    (
        "/api/definitions/refresh",
        &[Method::POST],
        "POST with no body",
    ),
    (
        "/api/run",
        &[Method::GET, Method::POST],
        "GET the latest run (?after=<entries seen>) or one run (?id=R1), or POST a new one",
    ),
    ("/api/run/preview", &[Method::GET], "GET ?notes=N1,N2"),
    ("/api/run/changes", &[Method::GET], "GET"),
    ("/api/run/cancel", &[Method::POST], "POST"),
    ("/api/run/permission", &[Method::POST], "POST"),
    ("/api/run/question", &[Method::POST], "POST"),
    ("/api/run/review", &[Method::POST], "POST"),
    ("/api/run/follow", &[Method::POST], "POST"),
    ("/api/run/ask", &[Method::POST], "POST {\"note\": {...}}"),
    (
        "/api/run/uncommitted",
        &[Method::GET],
        "GET, with ?file=<path> for a file",
    ),
];

const METHODS: [Method; 5] = [
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
];

fn send(site: &Site, method: &Method, path: &str) -> (StatusCode, String) {
    call(
        site,
        method.clone(),
        &format!("postil://localhost{path}"),
        "",
    )
}

#[test]
fn every_route_takes_its_methods_and_refuses_the_others_with_what_it_takes() {
    let repo = TempRepo::new("routes");
    // No project is open, so no request that a route takes changes anything but the
    // configuration directory; its status only has to show that the route took it.
    let site = repo.empty_site();
    for &(path, allowed, refusal) in ROUTES {
        for method in &METHODS {
            let (status, body) = send(&site, method, path);
            if allowed.contains(method) {
                assert!(
                    status != StatusCode::METHOD_NOT_ALLOWED && status != StatusCode::NOT_FOUND,
                    "{method} {path}: {status} {body}"
                );
            } else {
                assert_eq!(
                    (status, body.as_str()),
                    (StatusCode::METHOD_NOT_ALLOWED, refusal),
                    "{method} {path}"
                );
            }
        }
    }
}

#[test]
fn a_path_that_is_no_route_is_not_found_whatever_the_method() {
    let repo = TempRepo::new("routes-none");
    let site = repo.empty_site();
    for path in [
        "/api",
        "/api/nothing",
        "/api/run/nothing",
        "/api/runs/R1",
        "/api/analysisx",
        "/api/analysis/nothing",
        "/api/memoryx",
        "/api/memory/nothing",
    ] {
        for method in &METHODS {
            assert_eq!(
                send(&site, method, path),
                (StatusCode::NOT_FOUND, "not found".to_owned()),
                "{method} {path}"
            );
        }
    }
}
