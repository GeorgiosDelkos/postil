//! The analysis: read back placed, a file that cannot be read reported and kept, and the
//! analysis runs that write it.

use super::*;

const ANALYSIS: &str = "postil://localhost/api/analysis";

/// An analysis written by hand, as an agent or a script may write one: a node with a line, a
/// file and an unverified reference, a node on a folder, and an edge on lines of another file.
const BY_HAND: &str = r#"{"version": 1, "diagrams": [{
    "id": "D1", "kind": "architecture", "title": "Top", "run": "R1",
    "nodes": [
        {"id": "D1.1", "kind": "component", "label": "A", "summary": "s", "detail": "d",
         "anchors": [
            {"path": "src/a.rs", "target": "lines", "lines": {"start": 2, "end": 2},
             "snippet": "    one();"},
            {"path": "src/b.rs", "target": "file", "state": "found"},
            {"path": "gone.rs", "state": "unverified", "reason": "the file is not UTF-8 text"}
         ]},
        {"id": "D1.2", "kind": "boundary", "label": "B", "summary": "s", "detail": "d",
         "group": null, "anchors": [{"path": "src/sub", "target": "folder"}]}
    ],
    "edges": [{"from": "D1.1", "to": "D1.2", "label": "uses",
               "anchor": {"path": "src/b.rs", "target": "lines",
                          "lines": {"start": 2, "end": 2}, "snippet": "y"}}]
}]}"#;

/// Each reference of the analysis as `path state start-end`, nodes' first, then edges'.
fn anchors(analysis: &serde_json::Value) -> Vec<String> {
    let diagram = &analysis["diagrams"][0];
    let nodes = diagram["nodes"].as_array().unwrap().iter();
    let of_nodes = nodes.flat_map(|node| node["anchors"].as_array().unwrap().clone());
    let of_edges = diagram["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["anchor"].clone());
    of_nodes
        .chain(of_edges)
        .map(|anchor| {
            let lines = &anchor["lines"];
            let place = if lines.is_null() {
                String::new()
            } else {
                format!(" {}-{}", lines["start"], lines["end"])
            };
            let path = anchor["path"].as_str().unwrap();
            let state = anchor["state"].as_str().unwrap();
            format!("{path} {state}{place}")
        })
        .collect()
}

#[test]
fn an_analysis_reads_back_placed() {
    let repo = TempRepo::new("analysis-placed");
    let a = "fn a() {\n    one();\n}\n";
    repo.write("src/a.rs", a);
    repo.write("src/b.rs", "x\ny\nz\n");
    fs::create_dir(repo.root.join("src/sub")).unwrap();
    let site = repo.site();
    assert_eq!(get(&site, ANALYSIS), (StatusCode::OK, "null".to_owned()));

    let file = site.repo().unwrap().analysis_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, BY_HAND).unwrap();
    let as_written = [
        "src/a.rs found 2-2",
        "src/b.rs found",
        "gone.rs unverified",
        "src/sub found",
        "src/b.rs found 2-2",
    ];
    assert_eq!(anchors(&get_json(&site, ANALYSIS)), as_written);

    // Lines above both cited files: each reference moves with its own file.
    repo.write("src/a.rs", &format!("// one\n// two\n{a}"));
    repo.write("src/b.rs", "w\nx\ny\nz\n");
    let read = get_json(&site, ANALYSIS);
    assert_eq!(
        anchors(&read),
        [
            "src/a.rs found 4-4",
            "src/b.rs found",
            "gone.rs unverified",
            "src/sub found",
            "src/b.rs found 3-3",
        ]
    );
    let unverified = &read["diagrams"][0]["nodes"][0]["anchors"][2];
    assert_eq!(unverified["reason"], "the file is not UTF-8 text");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        BY_HAND,
        "a read never writes"
    );

    // The cited line is gone, and the folder is a file now: both outdated, where they were.
    repo.write("src/a.rs", "// one\n// two\nfn a() {\n}\n");
    fs::remove_dir(repo.root.join("src/sub")).unwrap();
    repo.write("src/sub", "now a file\n");
    let read = get_json(&site, ANALYSIS);
    assert_eq!(
        anchors(&read),
        [
            "src/a.rs outdated 2-2",
            "src/b.rs found",
            "gone.rs unverified",
            "src/sub outdated",
            "src/b.rs found 3-3",
        ]
    );
    let outdated = &read["diagrams"][0]["nodes"][0]["anchors"][0];
    assert_eq!(outdated["snippet"], "    one();");

    // Put back, they are found again: the file on disk still says found.
    repo.write("src/a.rs", a);
    fs::remove_file(repo.root.join("src/sub")).unwrap();
    fs::create_dir(repo.root.join("src/sub")).unwrap();
    repo.write("src/b.rs", "x\ny\nz\n");
    assert_eq!(anchors(&get_json(&site, ANALYSIS)), as_written);
}

#[test]
fn a_reference_stored_as_outdated_reads_found_while_its_code_is_there() {
    let repo = TempRepo::new("analysis-outdated");
    repo.write("src/a.rs", "x\ny\n");
    let site = repo.site();
    let file = site.repo().unwrap().analysis_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let stored = with(
        r#"{"path": "src/a.rs", "target": "lines", "lines": {"start": 2, "end": 2},
            "snippet": "y", "state": "outdated"},
           {"path": "src", "target": "folder", "state": "outdated"}"#,
        "",
    );
    fs::write(&file, &stored).unwrap();

    let read = get_json(&site, ANALYSIS);
    assert_eq!(anchors(&read), ["src/a.rs found 2-2", "src found"]);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        stored,
        "a read never writes"
    );
}

/// A version 1 analysis with one diagram of two nodes, `D1.1` and `D1.2`, whose first node
/// has `anchor` and whose diagram has `edges`.
fn with(anchor: &str, edges: &str) -> String {
    format!(
        r#"{{"version": 1, "diagrams": [{{"id": "D1", "kind": "flow", "title": "t", "run": "R1",
        "nodes": [
          {{"id": "D1.1", "kind": "component", "label": "l", "summary": "s", "detail": "d",
            "anchors": [{anchor}]}},
          {{"id": "D1.2", "kind": "component", "label": "l", "summary": "s", "detail": "d",
            "anchors": []}}
        ], "edges": [{edges}]}}]}}"#
    )
}

/// A diagram `D1` with no nodes.
const EMPTY: &str =
    r#"{"id": "D1", "kind": "flow", "title": "t", "run": "R1", "nodes": [], "edges": []}"#;

/// A whole diagram whose nodes are `nodes`, in an analysis of version 1.
fn nodes(nodes: &str) -> String {
    format!(
        r#"{{"version": 1, "diagrams": [{{"id": "D1", "kind": "data", "title": "t", "run": "R1",
        "nodes": [{nodes}], "edges": []}}]}}"#
    )
}

/// A node `id` of kind `store`, in group `group` (`null` for none).
fn node(id: &str, group: &str) -> String {
    format!(
        r#"{{"id": "{id}", "kind": "store", "label": "l", "summary": "s", "detail": "d",
            "group": {group}, "anchors": []}}"#
    )
}

/// Analysis files that do not parse as the format, each with what the error says about it:
/// one for each field and each rule of a reference.
fn broken_shapes() -> Vec<(String, &'static str)> {
    let lines = r#""target": "lines", "lines": {"start": 1, "end": 1}, "snippet": "x""#;
    vec![
        ("{ not json".to_owned(), "cannot parse"),
        (r#"{"diagrams": []}"#.to_owned(), "missing field `version`"),
        (
            r#"{"version": 0, "diagrams": []}"#.to_owned(),
            "version 0, but this build of Postil reads versions 1 to 2",
        ),
        (
            r#"{"version": 1, "diagrams": [], "checked": "abc"}"#.to_owned(),
            "unknown field `checked`",
        ),
        (
            with("", "").replace(r#""run": "R1""#, r#""run": "R1", "x": 1"#),
            "unknown field `x`",
        ),
        (
            with("", "").replace(r#""detail": "d""#, r#""detail": "d", "x": 1"#),
            "unknown field `x`",
        ),
        (
            with(
                "",
                r#"{"from": "D1.1", "to": "D1.2", "label": "l", "x": 1}"#,
            ),
            "unknown field `x`",
        ),
        (
            with(r#"{"path": "a", "target": "file", "x": 1}"#, ""),
            "unknown field `x`",
        ),
        (
            with("", "").replace(r#""kind": "flow""#, r#""kind": "flows""#),
            "unknown variant `flows`",
        ),
        (
            with("", "").replace(r#""id": "D1","#, r#""id": "D01","#),
            "diagram id \"D01\"",
        ),
        (
            with("", "").replace(r#""id": "D1.2""#, r#""id": "D1.0""#),
            "node id \"D1.0\"",
        ),
        (
            with("", "").replace(r#""run": "R1""#, r#""run": "1""#),
            "run id \"1\"",
        ),
        (
            with(
                r#"{"path": "a", "target": "file", "state": "unverified"}"#,
                "",
            ),
            "reference to a: an unverified reference has no `target`, `lines` or `snippet`",
        ),
        (
            with(r#"{"path": "a", "state": "found"}"#, ""),
            "reference to a: a found or outdated reference needs a `target`",
        ),
        (
            with(r#"{"path": "a", "target": "file", "reason": "r"}"#, ""),
            "reference to a: only an unverified reference has a `reason`",
        ),
        (
            with(r#"{"path": "a", "target": "lines"}"#, ""),
            "reference to a: a reference on lines needs `lines` and `snippet`",
        ),
        (
            with(
                &format!(
                    r#"{{"path": "a", {}}}"#,
                    lines.replacen("lines\"", "folder\"", 1)
                ),
                "",
            ),
            "reference to a: a reference on a file or folder has no `lines` or `snippet`",
        ),
        (
            with(
                &format!(
                    r#"{{"path": "a", {}}}"#,
                    lines.replace("\"start\": 1", "\"start\": 0")
                ),
                "",
            ),
            "lines are 1-based",
        ),
    ]
}

/// Analysis files whose ids, edges or groups break a rule of the graph, each with what the
/// error says about it.
fn broken_graphs() -> Vec<(String, &'static str)> {
    let edge =
        |from: &str, to: &str| format!(r#"{{"from": "{from}", "to": "{to}", "label": "l"}}"#);
    vec![
        (
            format!(r#"{{"version": 1, "diagrams": [{EMPTY}, {EMPTY}]}}"#),
            "diagram id D1 is used twice",
        ),
        (nodes(&node("D2.1", "null")), "node D2.1 is in diagram D1"),
        (
            nodes(&format!(
                "{}, {}",
                node("D1.1", "null"),
                node("D1.1", "null")
            )),
            "node id D1.1 is used twice",
        ),
        (
            with("", &edge("D1.1", "D1.3")),
            "edge D1.1 -> D1.3: D1.3 is no node of D1",
        ),
        (
            with("", &edge("D1.1", "D1.1")),
            "edge D1.1 -> D1.1 joins a node to itself",
        ),
        (
            nodes(&node("D1.1", "\"D1.7\"")),
            "node D1.1: its group D1.7 is no node of D1",
        ),
        (
            nodes(&node("D1.1", "\"D1.1\"")),
            "node D1.1: its groups loop",
        ),
        (
            nodes(&format!(
                "{}, {}, {}",
                node("D1.1", "\"D1.2\""),
                node("D1.2", "\"D1.3\""),
                node("D1.3", "\"D1.1\"")
            )),
            "node D1.1: its groups loop",
        ),
        (
            parented(&[("D1", Some("D1.1")), ("D2", None)]),
            "diagram D1: its parent D1.1 is one of its own nodes",
        ),
        (
            parented(&[("D1", None), ("D2", Some("D1.9"))]),
            "diagram D2: its parent D1.9 is no node of the analysis",
        ),
        (
            parented(&[
                ("D1", Some("D3.1")),
                ("D2", Some("D1.1")),
                ("D3", Some("D2.1")),
            ]),
            "diagram D1: its parents loop",
        ),
    ]
}

/// An analysis of diagrams with one node each, `D<n>.1`, each under the parent given.
fn parented(diagrams: &[(&str, Option<&str>)]) -> String {
    let diagrams: Vec<String> = diagrams
        .iter()
        .map(|(id, parent)| {
            let parent = parent.map_or("null".to_owned(), |parent| format!("\"{parent}\""));
            format!(
                r#"{{"id": "{id}", "kind": "architecture", "title": "T", "run": "R1",
                    "parent": {parent}, "edges": [],
                    "nodes": [{{"id": "{id}.1", "kind": "component", "label": "l",
                                "summary": "s", "detail": "d", "anchors": []}}]}}"#
            )
        })
        .collect();
    format!(r#"{{"version": 2, "diagrams": [{}]}}"#, diagrams.join(", "))
}

#[test]
fn an_unreadable_analysis_is_reported_and_kept() {
    let repo = TempRepo::new("analysis-broken");
    let site = repo.site();
    let file = site.repo().unwrap().analysis_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    // What `save` would write over the broken file, read from a good one.
    let good = repo.root.join(".git/good.json");
    fs::write(&good, with("", "")).unwrap();
    let analysis = crate::analysis::load(&good).unwrap().unwrap();
    let _lock = crate::analysis::lock(&file).unwrap();

    for (contents, expected) in broken_shapes().into_iter().chain(broken_graphs()) {
        fs::write(&file, &contents).unwrap();

        let (status, reply) = get(&site, ANALYSIS);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{contents}");
        assert!(reply.contains(expected), "{contents}: {reply}");
        assert!(reply.contains(&*file.to_string_lossy()), "{reply}");
        assert!(reply.contains("move it aside to start over"), "{reply}");

        let error = crate::analysis::save(&file, &analysis).unwrap_err();
        assert!(error.to_string().contains("start over"), "{error}");
        assert_eq!(fs::read_to_string(&file).unwrap(), contents);
    }

    // A folder where the file should be: it exists and cannot be read, and the cause shows.
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    let (status, reply) = get(&site, ANALYSIS);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.starts_with("cannot read"), "{reply}");
    assert!(
        reply.contains("os error"),
        "the cause reaches the page: {reply}"
    );
    assert!(crate::analysis::save(&file, &analysis).is_err());
    assert!(file.is_dir());
}

#[test]
fn a_newer_analysis_is_refused() {
    let repo = TempRepo::new("analysis-newer");
    let site = repo.site();
    let file = site.repo().unwrap().analysis_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let good = repo.root.join(".git/good.json");
    fs::write(&good, with("", "")).unwrap();
    let analysis = crate::analysis::load(&good).unwrap().unwrap();
    let _lock = crate::analysis::lock(&file).unwrap();
    // Version 3 adds `checked`: the version is the reason given, not the field.
    let newer = r#"{"version": 3, "checked": "abc1234", "diagrams": []}"#;
    fs::write(&file, newer).unwrap();

    let (status, reply) = get(&site, ANALYSIS);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        reply.contains("version 3, but this build of Postil reads versions 1 to 2"),
        "{reply}"
    );
    let error = crate::analysis::save(&file, &analysis).unwrap_err();
    assert!(error.to_string().contains("version 3"), "{error}");
    assert_eq!(fs::read_to_string(&file).unwrap(), newer);
}

const START_OVER: &str = "postil://localhost/api/analysis/start-over";

#[test]
fn start_over_moves_an_unreadable_analysis_to_the_trash() {
    let repo = TempRepo::new("analysis-start-over");
    let site = repo.site();
    let file = site.repo().unwrap().analysis_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let newer = r#"{"version": 3, "checked": "abc1234", "diagrams": []}"#;
    fs::write(&file, newer).unwrap();
    assert_eq!(get(&site, ANALYSIS).0, StatusCode::INTERNAL_SERVER_ERROR);

    let (status, reply) = post(&site, START_OVER, "");
    assert_eq!(status, StatusCode::OK, "{reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let trashed = PathBuf::from(reply["trashed"].as_str().expect("where the file went"));
    // The file is in the Trash, whole, and can be put back. Only a file that is both the one
    // written here and in a Trash is deleted again: anything else is left for the engineer.
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let in_trash = trashed.starts_with(home.join(".Trash"))
        || trashed
            .components()
            .any(|part| part.as_os_str() == ".Trashes");
    assert!(in_trash, "{} is in no Trash", trashed.display());
    assert_eq!(fs::read_to_string(&trashed).unwrap(), newer);
    fs::remove_file(&trashed).unwrap();
    assert!(!file.exists());
    assert_eq!(get_json(&site, ANALYSIS), serde_json::Value::Null);

    let (status, reply) = post(&site, START_OVER, "");
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply, r#"{"trashed":null}"#, "no file: nothing to move");
}

#[test]
fn start_over_is_refused_while_an_analysis_runs() {
    let repo = analyzed_repo("analysis-start-over-live");
    let (site, nudges) = run_site(&repo, "hang");
    let file = site.repo().unwrap().analysis_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "{ not json").unwrap();
    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    under_way(&site, &nudges, &id);

    let (status, reply) = post(&site, START_OVER, "");

    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply.contains("an analysis of this window is under way"),
        "{reply}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "{ not json",
        "the file stays"
    );
    // The hanging agent ignores the first Stop; the second, once its wait is over, forces it.
    stop_run(&site, &id);
    site.runner().unwrap().backdate_stop();
    stop_run(&site, &id);
    analyzed(&site, &nudges, &id);
}
// Analysis runs (docs/review-assist.md, "An analysis run"), against the fake agent's
// `analysis` scenarios in `tests/fake-agent.mjs`.

const UPDATE: &str = "postil://localhost/api/analysis/update";

/// A repository with what the fake Analyzer cites: `fn a() {` on line 1 of `src/a.rs` and
/// `    one();` on line 2, `fn two() {` on line 3 of `src/b.rs`.
fn analyzed_repo(name: &str) -> TempRepo {
    let repo = TempRepo::new(name);
    repo.write("src/a.rs", "fn a() {\n    one();\n}\n");
    repo.write("src/b.rs", "// b\n\nfn two() {\n}\n");
    repo.write("a.txt", "a\n");
    repo
}

/// Starts an analysis as `body` asks, which must be accepted, and returns its run's id.
fn analyze(site: &Site, body: &serde_json::Value) -> String {
    let (status, reply) = post(site, ANALYSIS, &body.to_string());
    assert_eq!(status, StatusCode::OK, "{body}: {reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    reply["run"].as_str().unwrap().to_owned()
}

/// Waits until analysis run `id` has ended and its thread with it, and returns the run.
fn analyzed(site: &Site, nudges: &mpsc::Receiver<()>, id: &str) -> serde_json::Value {
    loop {
        let run = get_json(site, &format!("{RUN}?id={id}"));
        let over = ["ended", "failed"].contains(&run["status"].as_str().unwrap_or(""));
        let thread = super::threads::thread(site, id);
        if over && thread["state"] != "open" {
            return run;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no change while waiting for {id} to end: {run} {thread}"));
    }
}

/// Waits until run `id` is under way, its agent started.
fn under_way(site: &Site, nudges: &mpsc::Receiver<()>, id: &str) {
    loop {
        let run = get_json(site, &format!("{RUN}?id={id}"));
        if ["running", "waiting"].contains(&run["status"].as_str().unwrap_or("")) {
            return;
        }
        nudges
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("no change while waiting for {id} to run: {run}"));
    }
}

/// Stops run `id`, as its Stop does.
fn stop_run(site: &Site, id: &str) -> (StatusCode, String) {
    let body = serde_json::json!({ "run": id }).to_string();
    post(site, &format!("{RUN}/cancel"), &body)
}

fn diagram_ids(analysis: &serde_json::Value) -> Vec<String> {
    analysis["diagrams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagram| diagram["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The diagram `id` of `analysis`.
fn diagram<'a>(analysis: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    analysis["diagrams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagram| diagram["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {analysis}"))
}

/// Each node of `diagram` as `id label`.
fn labels(diagram: &serde_json::Value) -> Vec<String> {
    diagram["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            format!(
                "{} {}",
                node["id"].as_str().unwrap(),
                node["label"].as_str().unwrap()
            )
        })
        .collect()
}

/// Each edge of `diagram` as `from -> to`.
fn edge_ends(diagram: &serde_json::Value) -> Vec<String> {
    diagram["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            format!(
                "{} -> {}",
                edge["from"].as_str().unwrap(),
                edge["to"].as_str().unwrap()
            )
        })
        .collect()
}

/// The analysis file's text: what an analysis that stores nothing leaves as it was.
fn stored(repo: &TempRepo) -> Option<String> {
    fs::read_to_string(repo.root.join(".git/postil/analysis.json")).ok()
}

#[test]
fn analyze_stores_checked_diagrams() {
    let repo = analyzed_repo("analysis-run");
    let (site, nudges) = run_site(&repo, "analysis");

    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    let worktree = PathBuf::from(super::threads::thread(&site, &id)["path"].as_str().unwrap());
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["status"], "ended", "{run}");
    assert_eq!(run["problem"], serde_json::Value::Null, "{run}");
    assert_eq!(run["readOnly"], true);
    assert_eq!(run["analysis"], serde_json::json!({ "kind": "analyze" }));
    assert_eq!(run["notes"], serde_json::json!([]));
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram_ids(&analysis), ["D1", "D2"]);
    let top = diagram(&analysis, "D1");
    assert_eq!(
        (&top["kind"], &top["run"]),
        (&"architecture".into(), &id.clone().into())
    );
    let long = format!("{}…", "W".repeat(59));
    assert_eq!(
        labels(top),
        [
            "D1.1 Page",
            &format!("D1.2 {long}"),
            "D1.3 Store",
            "D1.4 Process"
        ]
    );
    // Right, shifted (cited by the worktree's absolute path), wrong, a folder; then the edge.
    assert_eq!(
        anchors(&analysis),
        [
            "src/a.rs found 2-2",
            "src/b.rs found 3-4",
            "src/a.rs unverified",
            "src found",
            "src/a.rs found 2-2",
        ]
    );
    assert_eq!(top["nodes"][0]["group"], "D1.4");
    assert_eq!(top["nodes"][1]["group"], serde_json::Value::Null);
    assert_eq!(edge_ends(top), ["D1.1 -> D1.2"]);
    assert_eq!(
        top["dropped"],
        serde_json::json!({
            "nodes": 1, "edges": 2, "groups": 1, "shortened": 1, "replyCut": false,
        })
    );
    let data = diagram(&analysis, "D2");
    assert_eq!(labels(data), ["D2.1 Note"]);
    assert_eq!(data["dropped"], serde_json::Value::Null, "nothing dropped");

    // The thread ended with the run: its worktree and branch are gone, its record stays.
    assert_eq!(super::threads::thread(&site, &id)["state"], "discarded");
    assert!(!worktree.exists(), "freed");
    assert_eq!(git_out(&repo.root, &["branch", "--list", "postil/*"]), "");
    assert!(
        !thread_lock(&repo, &id).exists(),
        "every count of live runs would try it"
    );
    // Not a run of notes: the Run tab's latest is none.
    assert_eq!(get(&site, RUN).1, "null");
    // Deleting notes keeps the analysis's log, which its diagrams name.
    post_note(&site, "a.txt", 1, 1, "a note");
    let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        repo.root
            .join(format!(".git/postil/runs/{id}.jsonl"))
            .exists()
    );
    // An analysis takes no follow-up: its thread is gone.
    let follow = serde_json::json!({ "of": id, "text": "more" }).to_string();
    let (status, body) = post(&site, &format!("{RUN}/follow"), &follow);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("is an analysis"), "{body}");
}

#[test]
fn the_analyzer_runs_the_analysis_and_the_default_agent_does_not() {
    let repo = analyzed_repo("analysis-analyzer");
    let (site, nudges) = run_site(&repo, "plain");
    set_agents(
        &site,
        &[
            ("Fake", &fake_agent("plain")),
            ("Analyst", &fake_agent("analysis")),
        ],
        "Fake",
    );
    let mut settings = get_json(&site, SETTINGS)["settings"].clone();
    settings["analyzer"] = serde_json::json!({ "name": "Analyst", "model": null });
    let (status, body) = save(&site, &settings.to_string());
    assert_eq!(status, StatusCode::OK, "{body}");

    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["agent"], "Analyst", "{run}");
    assert_eq!(diagram_ids(&get_json(&site, ANALYSIS)), ["D1", "D2"]);
}

#[test]
fn an_analysis_that_edits_stores_nothing() {
    let repo = analyzed_repo("analysis-edits");
    let (site, nudges) = run_site(&repo, "analysis-edits");

    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["status"], "ended", "{run}");
    let problem = run["problem"].as_str().unwrap();
    assert!(problem.contains("changed files"), "{problem}");
    assert_eq!(stored(&repo), None, "nothing stored");
    assert_eq!(fs::read_to_string(repo.root.join("a.txt")).unwrap(), "a\n");
}

#[test]
fn a_stopped_analysis_keeps_the_old_one() {
    // Stopped once, the agent ends its turn cancelled; one that ignores it is stopped by force.
    for scenario in ["cancel", "hang"] {
        let repo = analyzed_repo(&format!("analysis-stopped-{scenario}"));
        let (site, nudges) = run_site(&repo, "analysis");
        let first = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
        analyzed(&site, &nudges, &first);
        let before = stored(&repo).expect("the first analysis is stored");
        set_agents(&site, &[("Fake", &fake_agent(scenario))], "Fake");

        let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
        under_way(&site, &nudges, &id);
        let (status, body) = stop_run(&site, &id);
        assert_eq!(status, StatusCode::OK, "{scenario}: {body}");
        if scenario == "hang" {
            site.runner().unwrap().backdate_stop();
            stop_run(&site, &id);
        }
        analyzed(&site, &nudges, &id);

        assert_eq!(
            stored(&repo).as_deref(),
            Some(before.as_str()),
            "{scenario}"
        );
        assert_eq!(
            super::threads::thread(&site, &id)["state"],
            "discarded",
            "{scenario}"
        );
    }
}

#[test]
fn an_empty_analysis_stores_nothing() {
    let repo = analyzed_repo("analysis-empty");
    let (site, nudges) = run_site(&repo, "analysis-empty");

    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["status"], "ended", "{run}");
    assert_eq!(run["problem"], "the analysis was empty", "{run}");
    assert_eq!(stored(&repo), None);
    assert_eq!(get(&site, ANALYSIS).1, "null");
    // The log says so, so a window opened later does too.
    let read = get_json(&repo.site(), &format!("{RUN}?id={id}"));
    assert_eq!(read["problem"], "the analysis was empty", "{read}");
}

#[test]
fn analyze_deeper_replaces_only_its_branch() {
    let repo = analyzed_repo("analysis-deeper");
    let (site, nudges) = run_site(&repo, "analysis");
    let run = |body: serde_json::Value| {
        let id = analyze(&site, &body);
        analyzed(&site, &nudges, &id)
    };
    let deeper = |of: &str| serde_json::json!({ "kind": "deeper", "of": of });
    run(serde_json::json!({ "kind": "analyze" }));
    run(deeper("D1.1"));
    run(deeper("D1.2"));
    run(deeper("D3.1"));
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram_ids(&analysis), ["D1", "D2", "D3", "D4", "D5"]);
    assert_eq!(diagram(&analysis, "D5")["parent"], "D3.1");

    // Again on D1.1: its diagram D3 is replaced, and D5 below D3's node goes with it. D4,
    // under D1.2, and the top level stay.
    let ended = run(deeper("D1.1"));
    assert_eq!(ended["problem"], serde_json::Value::Null, "{ended}");
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram_ids(&analysis), ["D1", "D2", "D4", "D6"]);
    let inside = diagram(&analysis, "D6");
    assert_eq!(
        (&inside["kind"], &inside["parent"]),
        (&"architecture".into(), &"D1.1".into())
    );
    assert_eq!(labels(inside), ["D6.1 Inner", "D6.2 Other"]);
    assert_eq!(edge_ends(inside), ["D6.1 -> D6.2"]);

    // A folder goes under the node that cites it, a flow in its steps' order at the top.
    run(deeper("src"));
    run(serde_json::json!({ "kind": "flow", "question": "How is a note sent?" }));
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram(&analysis, "D7")["parent"], "D1.4");
    let flow = diagram(&analysis, "D8");
    assert_eq!(flow["parent"], serde_json::Value::Null);
    assert_eq!(labels(flow), ["D8.1 first", "D8.2 second", "D8.3 third"]);

    // A file goes under the Architecture node with the nearest reference to it: D1.1, D4.1, D6.1
    // and D7.1 cite src/a.rs itself, D1.4 its folder, the flow's nodes (no Architecture) the
    // file too. Of the nearest, the first listed; its diagram D6 is replaced.
    run(deeper("src/a.rs"));
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram(&analysis, "D9")["parent"], "D1.1");
    assert_eq!(diagram_ids(&analysis), ["D1", "D2", "D4", "D7", "D8", "D9"]);

    // Analyze again replaces the top level, which takes every diagram below it; the flow stays.
    run(serde_json::json!({ "kind": "analyze" }));
    assert_eq!(
        diagram_ids(&get_json(&site, ANALYSIS)),
        ["D8", "D10", "D11"]
    );

    // What cannot be asked is refused before a run.
    for (body, expected) in [
        (deeper("D99.1"), "no node D99.1"),
        (deeper("nowhere.rs"), "names nothing"),
        (serde_json::json!({ "kind": "deeper" }), "needs `of`"),
        (
            serde_json::json!({ "kind": "analyze", "of": "src" }),
            "takes no `of`",
        ),
        (
            serde_json::json!({ "kind": "flow", "question": " " }),
            "1 to 300 characters",
        ),
        (
            serde_json::json!({ "kind": "flow", "question": "q".repeat(301) }),
            "not 301",
        ),
        (
            serde_json::json!({ "kind": "update" }),
            "/api/analysis/update",
        ),
        (serde_json::json!({ "kind": "other" }), "unknown variant"),
        (
            serde_json::json!({ "kind": "analyze", "question": "Why?" }),
            "only a flow takes a question",
        ),
    ] {
        let (status, reply) = post(&site, ANALYSIS, &body.to_string());
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
        assert!(reply.contains(expected), "{body}: {reply}");
    }
}

#[test]
fn deleting_a_diagram_takes_its_children() {
    let repo = analyzed_repo("analysis-delete");
    let (site, nudges) = run_site(&repo, "analysis");
    for body in [
        serde_json::json!({ "kind": "analyze" }),
        serde_json::json!({ "kind": "deeper", "of": "D1.1" }),
        serde_json::json!({ "kind": "deeper", "of": "D3.1" }),
    ] {
        let id = analyze(&site, &body);
        analyzed(&site, &nudges, &id);
    }

    let (status, reply) = delete(&site, &format!("{ANALYSIS}?diagram=D3"));

    assert_eq!(status, StatusCode::OK, "{reply}");
    let left: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(diagram_ids(&left), ["D1", "D2"]);
    assert_eq!(diagram_ids(&get_json(&site, ANALYSIS)), ["D1", "D2"]);
    for (query, status) in [
        ("?diagram=D3", StatusCode::NOT_FOUND),
        ("?diagram=3", StatusCode::BAD_REQUEST),
        ("", StatusCode::BAD_REQUEST),
    ] {
        let (got, reply) = delete(&site, &format!("{ANALYSIS}{query}"));
        assert_eq!(got, status, "{query}: {reply}");
    }
}

#[test]
fn update_replaces_only_outdated_nodes() {
    let repo = analyzed_repo("analysis-update");
    let (site, nudges) = run_site(&repo, "analysis");
    let first = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    analyzed(&site, &nudges, &first);
    // D3 deepens D1.2, the node the update deletes.
    let deeper = analyze(
        &site,
        &serde_json::json!({ "kind": "deeper", "of": "D1.2" }),
    );
    analyzed(&site, &nudges, &deeper);

    let (status, body) = post(&site, UPDATE, "");
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("nothing to update"), "{body}");
    assert_eq!(
        get_json(&site, RUNS)["runs"],
        serde_json::json!(["R2", "R1"]),
        "no run"
    );

    // Page's line and the worker's are gone, and two files are added, one outside `src`, which
    // a node cites whole, and one inside it: committed, so the Analyzer's worktree has them.
    repo.write("src/a.rs", "fn a() {\n}\n");
    repo.write("src/b.rs", "// b\n");
    fs::create_dir(repo.root.join("tools")).unwrap();
    repo.write("tools/c.rs", "fn c() {}\n");
    repo.write("src/d.rs", "fn d() {}\n");
    repo.commit();
    let id = analyze_update(&site, "{\"diagram\": \"D1\"}");
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["problem"], serde_json::Value::Null, "{run}");
    assert_eq!(
        run["analysis"],
        serde_json::json!({ "kind": "update", "of": "D1", "nodes": ["D1.1", "D1.2"] })
    );
    let prompt = prompt_of(&run);
    assert!(prompt.contains("Outdated node D1.1"), "{prompt}");
    // With its old snippet, indented, never fenced.
    assert!(
        prompt.contains("It cited src/a.rs lines 2 to 2, which read:\n        one();\n"),
        "{prompt}"
    );
    assert!(prompt.contains("- tools/c.rs"), "the new file: {prompt}");
    // A new file goes in a new node: a healthy node's anchors are not the update's to change.
    assert!(
        prompt.contains("where it belongs, as a new node:\n"),
        "{prompt}"
    );
    assert!(!prompt.contains("node's anchors"), "{prompt}");
    assert!(
        !prompt.contains("src/d.rs"),
        "under a folder a node cites: {prompt}"
    );
    assert!(!prompt.contains("Outdated node D1.3"), "{prompt}");
    let analysis = get_json(&site, ANALYSIS);
    // D1.1 replaced; D1.2 deleted, its diagram D3 with it; D1.3, healthy, neither replaced
    // nor deleted though the reply tried both; one node added, numbered after the highest.
    assert_eq!(diagram_ids(&analysis), ["D1", "D2"]);
    let top = diagram(&analysis, "D1");
    assert_eq!(
        labels(top),
        ["D1.1 Renewed", "D1.3 Store", "D1.4 Process", "D1.5 Added"]
    );
    assert_eq!(edge_ends(top), ["D1.5 -> D1.1"]);
    assert_eq!(top["run"], id);
    // The first run's counts, and the update's added to them: two nodes it may not change, an
    // edge to no node, a group that names none, a summary over the limit.
    assert_eq!(
        top["dropped"],
        serde_json::json!({
            "nodes": 1 + 2, "edges": 2 + 1, "groups": 1 + 1, "shortened": 1 + 1,
            "replyCut": false,
        })
    );
    assert_eq!(top["nodes"][0]["anchors"][0]["state"], "found");

    // Brought up to date: nothing left to update.
    let (status, body) = post(&site, UPDATE, "");
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post(&site, UPDATE, "{\"diagram\": \"D9\"}");
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // An outdated node alone, with no file added since, is enough to update.
    repo.write("src/a.rs", "fn renamed() {\n}\n");
    repo.commit();
    let again = analyze_update(&site, "");
    let run = analyzed(&site, &nudges, &again);
    assert_eq!(
        run["analysis"]["nodes"],
        serde_json::json!(["D1.1"]),
        "{run}"
    );

    // So are files added alone: the prompt holds the top-level Architecture, which places them,
    // and not the Data diagram, which has nothing to update. It names 200 of the 201, sorted.
    for n in 0..201 {
        repo.write(&format!("tools/e{n:03}.rs"), "fn e() {}\n");
    }
    repo.commit();
    let placing = analyze_update(&site, "");
    let run = analyzed(&site, &nudges, &placing);
    assert_eq!(
        run["analysis"],
        serde_json::json!({ "kind": "update" }),
        "{run}"
    );
    let prompt = prompt_of(&run);
    assert!(
        prompt.contains("- tools/e199.rs\nand 1 more, not listed here."),
        "{prompt}"
    );
    assert!(!prompt.contains("tools/e200.rs"), "{prompt}");
    assert!(prompt.contains("\nDiagram D1 "), "{prompt}");
    assert!(!prompt.contains("Diagram D2"), "{prompt}");
}

/// What the fake agent of `run` says it got: its prompt.
fn prompt_of(run: &serde_json::Value) -> &str {
    run["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["text"].as_str())
        .find(|text| text.starts_with("got "))
        .unwrap_or_else(|| panic!("no prompt in {run}"))
}

/// Starts an update with `body`, which must be accepted, and returns its run's id.
fn analyze_update(site: &Site, body: &str) -> String {
    let (status, reply) = post(site, UPDATE, body);
    assert_eq!(status, StatusCode::OK, "{body}: {reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    reply["run"].as_str().unwrap().to_owned()
}

#[test]
fn concurrent_and_refused_analysis_runs() {
    let repo = analyzed_repo("analysis-concurrent");
    let (site, nudges) = run_site(&repo, "hang");
    // A run of notes under way does not hold an analysis back.
    post_note(&site, "a.txt", 1, 1, "work");
    send(&site, &["N1"]);
    wait_for(&site, &nudges, "running");
    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));

    // A second in this window waits for the first, of whatever kind, until it ends.
    let (status, body) = post(&site, ANALYSIS, r#"{"kind": "analyze"}"#);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("under way"), "{body}");
    under_way(&site, &nudges, &id);
    let (status, body) = post(&site, ANALYSIS, r#"{"kind": "deeper", "of": "src"}"#);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    stop_run(&site, &id);
    stop(&site);
    site.runner().unwrap().backdate_stop();
    stop_run(&site, &id);
    stop(&site);
    analyzed(&site, &nudges, &id);
    wait_for(&site, &nudges, "failed");

    // Two windows analyze at once: both runs go, and the later merge replaces the earlier.
    let (other, other_nudges) = run_site(&repo, "analysis");
    set_agents(&site, &[("Fake", &fake_agent("analysis"))], "Fake");
    let one = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    let two = analyze(&other, &serde_json::json!({ "kind": "analyze" }));
    analyzed(&site, &nudges, &one);
    analyzed(&other, &other_nudges, &two);
    let analysis = get_json(&site, ANALYSIS);
    let mut ids = diagram_ids(&analysis);
    ids.sort();
    assert_eq!(ids, ["D3", "D4"], "{analysis}");

    // An analyzer that is not installed is refused before a thread is made.
    set_agents(&site, &[("Fake", "/nonexistent/agent-bin")], "Fake");
    let runs = get_json(&site, RUNS)["runs"].clone();
    let (status, body) = post(&site, ANALYSIS, r#"{"kind": "analyze"}"#);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("Fake is not installed"), "{body}");
    assert_eq!(get_json(&site, RUNS)["runs"], runs, "no run");
}

/// The lock file of thread `id`, which a run holds while it is live.
fn thread_lock(repo: &TempRepo, id: &str) -> PathBuf {
    repo.root.join(format!(".git/postil/threads/{id}.lock"))
}

#[test]
fn an_update_never_gives_a_deleted_node_s_id_again() {
    let repo = analyzed_repo("analysis-update-ids");
    let (site, nudges) = run_site(&repo, "analysis");
    let first = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    analyzed(&site, &nudges, &first);
    // D3 deepens D1.4, the highest-numbered node, which the update deletes.
    let deeper = analyze(
        &site,
        &serde_json::json!({ "kind": "deeper", "of": "D1.4" }),
    );
    analyzed(&site, &nudges, &deeper);

    // Every verified reference of D1 is in src, which goes: D1.1, D1.2 and D1.4 are outdated.
    git_out(&repo.root, &["mv", "src", "lib"]);
    repo.commit();
    let id = analyze_update(&site, "{\"diagram\": \"D1\"}");
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["problem"], serde_json::Value::Null, "{run}");
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram_ids(&analysis), ["D1", "D2"], "D3 went with D1.4");
    // The fake Analyzer replaces D1.1, deletes D1.2 and D1.4, and adds a node: D1.5, not D1.4,
    // which D3 and the reply's edge to the old D1.4 would have found.
    assert_eq!(
        labels(diagram(&analysis, "D1")),
        ["D1.1 Renewed", "D1.3 Store", "D1.5 Added"]
    );
}

#[test]
fn an_analysis_stopped_or_closed_while_it_merges_ends_once_the_merge_is_done() {
    for how in ["forced stop", "close"] {
        let repo = analyzed_repo(&format!("analysis-merging-{}", how.replace(' ', "-")));
        let (site, nudges) = run_site(&repo, "analysis");
        let runner = site.runner().unwrap();
        let (release, reached) = runner.hold_merge();
        let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
        reached.recv_timeout(WAIT).expect("the merge is held");

        if how == "close" {
            runner.close();
        } else {
            stop_run(&site, &id);
            runner.backdate_stop();
            let (status, body) = stop_run(&site, &id);
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        // The merge holds the run's end: the run is not failed, and its thread not ended.
        let held = get_json(&site, &format!("{RUN}?id={id}"));
        assert_ne!(held["status"], "failed", "{how}: {held}");
        assert_eq!(super::threads::thread(&site, &id)["state"], "open", "{how}");
        release.send(()).unwrap();
        let run = analyzed(&site, &nudges, &id);

        assert_eq!(run["status"], "ended", "{how}: {run}");
        assert_eq!(
            diagram_ids(&get_json(&site, ANALYSIS)),
            ["D1", "D2"],
            "{how}"
        );
        assert_eq!(
            super::threads::thread(&site, &id)["state"],
            "discarded",
            "{how}"
        );
    }
}

#[test]
fn closing_the_project_during_an_analysis_stores_nothing_and_ends_its_thread() {
    let repo = analyzed_repo("analysis-close");
    let (site, nudges) = run_site(&repo, "hang");
    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    under_way(&site, &nudges, &id);
    let worktree = PathBuf::from(super::threads::thread(&site, &id)["path"].as_str().unwrap());

    site.runner().unwrap().close();

    let run = get_json(&site, &format!("{RUN}?id={id}"));
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(stored(&repo), None);
    assert_eq!(super::threads::thread(&site, &id)["state"], "discarded");
    assert!(!worktree.exists());
}

#[test]
fn an_agent_that_exits_mid_analysis_stores_nothing_and_ends_its_thread() {
    let repo = analyzed_repo("analysis-exits");
    let (site, nudges) = run_site(&repo, "exit-mid-turn");

    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(stored(&repo), None);
    assert_eq!(super::threads::thread(&site, &id)["state"], "discarded");
    assert!(!thread_lock(&repo, &id).exists());
}

#[test]
fn a_flow_from_a_file_starts_there() {
    let repo = analyzed_repo("analysis-flow-of");
    let (site, nudges) = run_site(&repo, "analysis");

    let body = serde_json::json!({ "kind": "flow", "of": "src/a.rs", "question": "How?" });
    let id = analyze(&site, &body);
    let run = analyzed(&site, &nudges, &id);

    assert_eq!(
        run["analysis"],
        serde_json::json!({ "kind": "flow", "of": "src/a.rs" })
    );
    assert!(
        prompt_of(&run).contains("How?\n\nStart from the file src/a.rs"),
        "{run}"
    );
    let analysis = get_json(&site, ANALYSIS);
    assert_eq!(diagram(&analysis, "D1")["kind"], "flow");
}

#[test]
fn a_note_deleted_in_another_window_keeps_an_analysis_under_way() {
    let repo = analyzed_repo("analysis-other-window-delete");
    let (analyzing, nudges) = run_site(&repo, "analysis");
    // Held where no diagram names the run yet, and the run is live.
    let (release, reached) = analyzing.runner().unwrap().hold_merge();
    let id = analyze(&analyzing, &serde_json::json!({ "kind": "analyze" }));
    reached.recv_timeout(WAIT).expect("the merge is held");
    let log = repo.root.join(format!(".git/postil/runs/{id}.jsonl"));

    let other = repo.site();
    post_note(&other, "a.txt", 1, 1, "a note");
    let (status, body) = delete(&other, &format!("{NOTES}?id=N1"));

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(log.exists(), "the run is under way in the other window");
    release.send(()).unwrap();
    let run = analyzed(&analyzing, &nudges, &id);
    assert_eq!(run["status"], "ended", "{run}");
    assert_eq!(diagram_ids(&get_json(&other, ANALYSIS)), ["D1", "D2"]);
    // Ended, it is named by its diagrams: a later delete keeps it too.
    let notes = post_note(&other, "a.txt", 1, 1, "another");
    let note = notes["notes"][0]["id"].as_str().unwrap().to_owned();
    let (status, body) = delete(&other, &format!("{NOTES}?id={note}"));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(log.exists());
}

#[test]
fn an_analysis_that_stored_nothing_goes_with_the_next_delete_of_a_note() {
    let repo = analyzed_repo("analysis-forgotten");
    let (site, nudges) = run_site(&repo, "analysis-empty");
    let id = analyze(&site, &serde_json::json!({ "kind": "analyze" }));
    analyzed(&site, &nudges, &id);
    let log = repo.root.join(format!(".git/postil/runs/{id}.jsonl"));
    assert!(log.exists(), "kept until a delete");
    assert!(!thread_lock(&repo, &id).exists());

    post_note(&site, "a.txt", 1, 1, "a note");
    let (status, body) = delete(&site, &format!("{NOTES}?id=N1"));

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!log.exists(), "no diagram names it");
    let (status, thread) = get(&site, &format!("postil://localhost/api/thread?id={id}"));
    assert_eq!((status, thread.as_str()), (StatusCode::OK, "null"));
}
