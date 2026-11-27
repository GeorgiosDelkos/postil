//! The analysis: read back placed, and a file that cannot be read reported and kept.

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
            "version 0, but this build of Postil reads version 1",
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
    ]
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
    // Version 2 adds `checked`: the version is the reason given, not the field.
    let newer = r#"{"version": 2, "checked": "abc1234", "diagrams": []}"#;
    fs::write(&file, newer).unwrap();

    let (status, reply) = get(&site, ANALYSIS);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        reply.contains("version 2, but this build of Postil reads version 1"),
        "{reply}"
    );
    let error = crate::analysis::save(&file, &analysis).unwrap_err();
    assert!(error.to_string().contains("version 2"), "{error}");
    assert_eq!(fs::read_to_string(&file).unwrap(), newer);
}
