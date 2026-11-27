//! Tests of the analysis file's format, of checking citations, and of placing references.

use std::fs::File;
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::*;
use crate::testing::{TempDir, make_fifo, skip_as_root};

fn on_lines(path: &str, start: usize, end: usize, snippet: &str) -> CodeRef {
    CodeRef::found(
        path.to_owned(),
        Target::Lines {
            lines: LineRange { start, end },
            snippet: snippet.to_owned(),
        },
    )
}

fn outdated(mut anchor: CodeRef) -> CodeRef {
    if let RefDetail::Verified { placed, .. } = &mut anchor.detail {
        *placed = VerifiedState::Outdated;
    }
    anchor
}

fn node(id: &str, kind: NodeKind, group: Option<&str>, anchors: Vec<CodeRef>) -> Node {
    Node {
        id: NodeId::parse(id).unwrap(),
        kind,
        label: "secret label".to_owned(),
        summary: "secret summary".to_owned(),
        detail: "secret detail".to_owned(),
        group: group.map(|group| NodeId::parse(group).unwrap()),
        anchors,
    }
}

fn edge(from: &str, to: &str, anchor: Option<CodeRef>) -> Edge {
    Edge {
        from: NodeId::parse(from).unwrap(),
        to: NodeId::parse(to).unwrap(),
        label: "secret edge".to_owned(),
        anchor,
    }
}

fn diagram(id: u32, kind: DiagramKind, parent: Option<&str>, nodes: Vec<Node>) -> Diagram {
    Diagram {
        id: DiagramId(id),
        kind,
        title: "secret title".to_owned(),
        parent: parent.map(|parent| NodeId::parse(parent).unwrap()),
        run: RunId(u64::from(id)),
        nodes,
        edges: Vec::new(),
    }
}

/// Every kind, state, target and optional field at least once.
fn every_kind() -> Analysis {
    let mut top = diagram(
        1,
        DiagramKind::Architecture,
        None,
        vec![
            node(
                "D1.1",
                NodeKind::Component,
                Some("D1.2"),
                vec![
                    on_lines("src/a.rs", 2, 3, "b\nc"),
                    CodeRef::found("b.rs".to_owned(), Target::Path(PathKind::File)),
                ],
            ),
            node(
                "D1.2",
                NodeKind::Boundary,
                None,
                vec![CodeRef::unverified(
                    "big.bin".to_owned(),
                    Some("21 MB, over the 20 MB limit".to_owned()),
                )],
            ),
        ],
    );
    top.edges.push(edge(
        "D1.1",
        "D1.2",
        Some(outdated(CodeRef::found(
            "src".to_owned(),
            Target::Path(PathKind::Folder),
        ))),
    ));
    let mut data = diagram(
        2,
        DiagramKind::Data,
        Some("D1.1"),
        vec![
            node(
                "D2.1",
                NodeKind::Store,
                None,
                vec![CodeRef::unverified("x.rs".to_owned(), None)],
            ),
            node("D2.2", NodeKind::External, None, Vec::new()),
        ],
    );
    data.edges.push(edge("D2.1", "D2.2", None));
    Analysis {
        diagrams: vec![top, data, diagram(3, DiagramKind::Flow, None, Vec::new())],
    }
}

const EVERY_KIND_JSON: &str = r#"{
  "version": 1,
  "diagrams": [
    {
      "id": "D1",
      "kind": "architecture",
      "title": "secret title",
      "run": "R1",
      "nodes": [
        {
          "id": "D1.1",
          "kind": "component",
          "label": "secret label",
          "summary": "secret summary",
          "detail": "secret detail",
          "group": "D1.2",
          "anchors": [
            {
              "path": "src/a.rs",
              "target": "lines",
              "lines": {
                "start": 2,
                "end": 3
              },
              "snippet": "b\nc",
              "state": "found"
            },
            {
              "path": "b.rs",
              "target": "file",
              "state": "found"
            }
          ]
        },
        {
          "id": "D1.2",
          "kind": "boundary",
          "label": "secret label",
          "summary": "secret summary",
          "detail": "secret detail",
          "anchors": [
            {
              "path": "big.bin",
              "state": "unverified",
              "reason": "21 MB, over the 20 MB limit"
            }
          ]
        }
      ],
      "edges": [
        {
          "from": "D1.1",
          "to": "D1.2",
          "label": "secret edge",
          "anchor": {
            "path": "src",
            "target": "folder",
            "state": "outdated"
          }
        }
      ]
    },
    {
      "id": "D2",
      "kind": "data",
      "title": "secret title",
      "parent": "D1.1",
      "run": "R2",
      "nodes": [
        {
          "id": "D2.1",
          "kind": "store",
          "label": "secret label",
          "summary": "secret summary",
          "detail": "secret detail",
          "anchors": [
            {
              "path": "x.rs",
              "state": "unverified"
            }
          ]
        },
        {
          "id": "D2.2",
          "kind": "external",
          "label": "secret label",
          "summary": "secret summary",
          "detail": "secret detail",
          "anchors": []
        }
      ],
      "edges": [
        {
          "from": "D2.1",
          "to": "D2.2",
          "label": "secret edge"
        }
      ]
    },
    {
      "id": "D3",
      "kind": "flow",
      "title": "secret title",
      "run": "R3",
      "nodes": [],
      "edges": []
    }
  ]
}
"#;

#[test]
fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
    let dir = TempDir::new("analysis-bytes");
    let file = dir.path().join("postil/analysis.json");
    let _lock = lock(&file).unwrap();

    save(&file, &every_kind()).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), EVERY_KIND_JSON);

    // What is read back writes the same bytes over it: no field is lost or changed on the way.
    let loaded = load(&file).unwrap().unwrap();
    save(&file, &loaded).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), EVERY_KIND_JSON);
}

#[test]
fn an_analysis_that_breaks_the_graph_rules_is_not_saved() {
    let dir = TempDir::new("analysis-save-invalid");
    let file = dir.path().join("analysis.json");
    let _lock = lock(&file).unwrap();
    save(&file, &every_kind()).unwrap();

    let mut looped = every_kind();
    looped.diagrams[0].edges.push(edge("D1.1", "D1.1", None));
    let error = save(&file, &looped).unwrap_err();

    assert!(
        matches!(&error, Error::InvalidAnalysis { path, problem }
            if *path == file && problem == "edge D1.1 -> D1.1 joins a node to itself"),
        "{error:?}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), EVERY_KIND_JSON);
}

#[test]
fn the_lock_reports_a_stuck_lock_and_save_an_unwritable_dir() {
    let dir = TempDir::new("analysis-lock");
    let file = dir.path().join("analysis.json");
    let lock_file = dir.path().join("analysis.lock");
    // A lock on a file of its own, as another process's would be: `flock` locks belong to the
    // open file, so this one conflicts with `lock`'s in the same process.
    let held = File::create(&lock_file).unwrap();
    held.lock().unwrap();
    let error = lock(&file).unwrap_err();
    drop(held);
    assert!(
        matches!(&error, Error::AnalysisLocked { path, seconds: 2 } if *path == lock_file),
        "{error:?}"
    );

    if skip_as_root() {
        return;
    }
    // The lock file is there, but nothing new can be made beside it.
    let _lock = lock(&file).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let error = save(&file, &every_kind()).unwrap_err();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(&error, Error::WriteAnalysis { path, .. } if *path == file),
        "{error:?}"
    );
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "only the lock file: no analysis and no temporary file"
    );
}

fn at(start: usize, end: usize) -> CitedLines {
    CitedLines { start, end }
}

fn cite(path: &str, lines: CitedLines, first: Option<&str>) -> Cited {
    Cited {
        path: path.to_owned(),
        lines: Some(lines),
        first: first.map(str::to_owned),
    }
}

/// A citation of a whole file or folder: no lines, no first line.
fn whole(path: &str) -> Cited {
    Cited {
        path: path.to_owned(),
        lines: None,
        first: None,
    }
}

fn found(path: &str, start: usize, end: usize, snippet: &str) -> serde_json::Value {
    json!({
        "path": path,
        "target": "lines",
        "lines": {"start": start, "end": end},
        "snippet": snippet,
        "state": "found",
    })
}

fn unverified(path: &str) -> serde_json::Value {
    json!({"path": path, "state": "unverified"})
}

#[test]
fn citations_are_checked_against_the_file() {
    let code = "fn a() {\n    one();\n    two();\n}\n";
    let two_more = format!("// x\n// y\n{code}");
    let repeated = "}\nx\n}\n";
    let blank = "a\n\nb\n";
    let indented = found("a.rs", 2, 3, "    one();\n    two();");
    let cases: Vec<(&str, Cited, serde_json::Value)> = vec![
        // Trimmed on both sides, and the snippet keeps the file's indentation.
        (
            code,
            cite("a.rs", at(2, 3), Some("one();  ")),
            indented.clone(),
        ),
        (
            &two_more,
            cite("a.rs", at(2, 3), Some("one();")),
            found("a.rs", 4, 5, "    one();\n    two();"),
        ),
        (code, cite("a.rs", at(3, 4), Some("one();")), indented),
        (
            "a\r\nb\r\nc\r\n",
            cite("a.rs", at(2, 3), Some("b")),
            found("a.rs", 2, 3, "b\nc"),
        ),
        // A repeated line holds where it is stated, and is never searched for.
        (
            repeated,
            cite("a.rs", at(3, 3), Some("}")),
            found("a.rs", 3, 3, "}"),
        ),
        (
            repeated,
            cite("a.rs", at(2, 2), Some("}")),
            unverified("a.rs"),
        ),
        // A blank first line holds only where it is stated, even when it is the only one.
        (
            blank,
            cite("a.rs", at(2, 2), Some("  ")),
            found("a.rs", 2, 2, ""),
        ),
        (blank, cite("a.rs", at(1, 1), Some("")), unverified("a.rs")),
        (code, cite("a.rs", at(2, 3), None), unverified("a.rs")),
        (
            code,
            cite("a.rs", at(0, 1), Some("fn a() {")),
            unverified("a.rs"),
        ),
        (
            code,
            cite("a.rs", at(3, 2), Some("two();")),
            unverified("a.rs"),
        ),
        (code, cite("a.rs", at(4, 5), Some("}")), unverified("a.rs")),
        (
            code,
            cite("a.rs", at(1, 5), Some("fn a() {")),
            unverified("a.rs"),
        ),
        // Moved to the only `}`, the range would end past the file.
        (code, cite("a.rs", at(1, 2), Some("}")), unverified("a.rs")),
        // Past the end of the file, even when `first` is the only such line in it.
        (
            code,
            cite("a.rs", at(usize::MAX, usize::MAX), Some("}")),
            unverified("a.rs"),
        ),
        (
            "a\nb\nc\n",
            cite("a.rs", at(4, 4), Some("c")),
            unverified("a.rs"),
        ),
        (
            "a\nb\nc\n",
            cite("a.rs", at(100, 100), Some("b")),
            unverified("a.rs"),
        ),
        (
            code,
            cite("a.rs", at(1, usize::MAX), Some("fn a() {")),
            unverified("a.rs"),
        ),
        (
            code,
            whole("a.rs"),
            json!({"path": "a.rs", "target": "file", "state": "found"}),
        ),
    ];
    for (text, cited, expected) in cases {
        let debug = format!("{cited:?} in {text:?}");
        let checked = serde_json::to_value(check(cited, text)).unwrap();
        assert_eq!(checked, expected, "{debug}");
    }
}

#[test]
fn citations_are_checked_against_the_repository() {
    let dir = TempDir::new("analysis-check-in");
    let repo = Repo::init(dir.path()).unwrap();
    let root = dir.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "x\ny\n").unwrap();
    make_fifo(&root.join("pipe"));
    let mut at_limit = vec![b'a'; usize::try_from(MAX_READ).unwrap()];
    at_limit[1] = b'\n';
    fs::write(root.join("limit.txt"), &at_limit).unwrap();
    at_limit.push(b'a');
    fs::write(root.join("over.txt"), &at_limit).unwrap();
    fs::write(root.join("binary.bin"), b"a\n\xff\n").unwrap();

    let file = json!({"path": "src/a.rs", "target": "file", "state": "found"});
    let folder = json!({"path": "src", "target": "folder", "state": "found"});
    let cases = [
        (
            cite("src/a.rs", at(2, 2), Some("y")),
            found("src/a.rs", 2, 2, "y"),
        ),
        (whole("src/a.rs"), file),
        (whole("src"), folder),
        (cite("src", at(1, 1), Some("x")), unverified("src")),
        (whole("../a.rs"), unverified("../a.rs")),
        (whole("/etc/hosts"), unverified("/etc/hosts")),
        (whole("./src/a.rs"), unverified("./src/a.rs")),
        (whole("src//a.rs"), unverified("src//a.rs")),
        (whole(""), unverified("")),
        (whole("missing.rs"), unverified("missing.rs")),
        (whole("pipe"), unverified("pipe")),
        (
            cite("limit.txt", at(1, 1), Some("a")),
            found("limit.txt", 1, 1, "a"),
        ),
        (
            cite("over.txt", at(1, 1), Some("a")),
            json!({
                "path": "over.txt",
                "state": "unverified",
                "reason": "21 MB, over the 20 MB limit",
            }),
        ),
        (
            cite("binary.bin", at(1, 1), Some("a")),
            json!({
                "path": "binary.bin",
                "state": "unverified",
                "reason": "the file is not UTF-8 text",
            }),
        ),
    ];
    for (cited, expected) in cases {
        let debug = format!("{cited:?}");
        let checked = serde_json::to_value(check_in(cited, &repo)).unwrap();
        assert_eq!(checked, expected, "{debug}");
    }
}

/// The lines a found reference on lines is at, and its snippet.
fn placed(anchor: &CodeRef) -> Option<(LineRange, &str)> {
    match &anchor.detail {
        RefDetail::Verified {
            target: Target::Lines { lines, snippet },
            placed: VerifiedState::Found,
        } => Some((*lines, snippet.as_str())),
        RefDetail::Verified { .. } | RefDetail::Unverified { .. } => None,
    }
}

#[test]
fn check_finds_exact_and_shifted_citations_and_never_leaves_the_file() {
    // Every right citation of every range of a file of distinct lines is found there, and,
    // after lines are inserted above it, found where it moved.
    for ending in ["\n", "\r\n"] {
        for count in 1..=6 {
            let lines: Vec<String> = (1..=count).map(|n| format!("  line {n}")).collect();
            for start in 1..=count {
                for end in start..=count {
                    for inserted in 0..=2 {
                        let mut file: Vec<String> =
                            (0..inserted).map(|n| format!("new {n}")).collect();
                        file.extend(lines.iter().cloned());
                        let text = file.join(ending) + ending;
                        let first = lines[start - 1].trim();
                        let checked = check(cite("a", at(start, end), Some(first)), &text);
                        let expected = LineRange {
                            start: start + inserted,
                            end: end + inserted,
                        };
                        let snippet = lines[start - 1..end].join("\n");
                        assert_eq!(
                            placed(&checked),
                            Some((expected, snippet.as_str())),
                            "{start}-{end} with {inserted} above, {ending:?}"
                        );
                    }
                }
            }
        }
    }

    // Random files of repeated and blank lines, random citations: a found citation is always
    // inside the file, its first line equals `first`, and its snippet is its lines.
    let alphabet = ["a", " a", "b", "", "  ", "}"];
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |below: u64| {
        // xorshift64 (Marsaglia, 2003): deterministic, so a failure repeats.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % below).unwrap()
    };
    for case in 0..5_000 {
        let length = next(9);
        let file: Vec<&str> = (0..length).map(|_| alphabet[next(6)]).collect();
        let text = file.join("\n");
        let start = match next(12) {
            11 => usize::MAX,
            n => n,
        };
        let end = match next(12) {
            11 => usize::MAX,
            n => n,
        };
        let first = (next(5) > 0).then(|| alphabet[next(6)]);
        let debug = format!("case {case}: {start}-{end} {first:?} in {text:?}");
        let checked = check(cite("a", at(start, end), first), &text);
        let Some((range, snippet)) = placed(&checked) else {
            continue;
        };
        assert!(
            1 <= range.start && range.start <= range.end && range.end <= file.len(),
            "{debug}: {range:?}"
        );
        assert_eq!(range.end - range.start, end - start, "{debug}");
        assert_eq!(
            snippet,
            file[range.start - 1..range.end].join("\n"),
            "{debug}"
        );
        let first = first.expect("a citation without `first` is never found");
        assert_eq!(file[range.start - 1].trim(), first.trim(), "{debug}");
    }
}

#[test]
fn place_finds_outdated_anchors_again_keeps_unverified_ones_and_scales_to_200() {
    let dir = TempDir::new("analysis-place");
    let repo = Repo::init(dir.path()).unwrap();
    let line = |file: usize, number: usize| format!("    file {file} line {number};");
    let write = |file: usize, above: usize| {
        let mut text: String = (0..above).map(|_| "// new\n").collect();
        for number in 1..=1_000 {
            text.push_str(&line(file, number));
            text.push('\n');
        }
        fs::write(dir.path().join(format!("f{file}.rs")), text).unwrap();
    };
    for file in 0..60 {
        write(file, 0);
    }
    let start_of = |n: usize| 1 + n * 7 % 990;

    // 200 anchors of three lines each, spread over the 60 files.
    let mut anchors: Vec<CodeRef> = (0..200)
        .map(|n| {
            let file = n % 60;
            let start = start_of(n);
            let snippet: Vec<String> = (start..start + 3).map(|at| line(file, at)).collect();
            on_lines(
                &format!("f{file}.rs"),
                start,
                start + 2,
                &snippet.join("\n"),
            )
        })
        .collect();
    // An anchor stored as outdated whose snippet is in its file is placed and found again, as
    // a found one is; an unverified one stays unverified.
    anchors.push(outdated(on_lines("f0.rs", 1, 1, &line(0, 1))));
    anchors.push(CodeRef::unverified("f0.rs".to_owned(), None));
    let mut analysis = Analysis {
        diagrams: vec![diagram(
            1,
            DiagramKind::Architecture,
            None,
            vec![node("D1.1", NodeKind::Component, None, anchors)],
        )],
    };
    let before =
        serde_json::to_value(&analysis).unwrap()["diagrams"][0]["nodes"][0]["anchors"].clone();

    // Two lines above file 0's first line: only its anchors move.
    write(0, 2);
    // The docs of `place` give its time on these files. No bound on it is asserted: one that
    // a slow machine always meets would catch nothing.
    place(&mut analysis, &repo);

    let after =
        serde_json::to_value(&analysis).unwrap()["diagrams"][0]["nodes"][0]["anchors"].clone();
    for n in 0..202 {
        let mut expected = before[n].clone();
        if n < 200 && n % 60 == 0 {
            let start = start_of(n) + 2;
            expected["lines"] = json!({"start": start, "end": start + 2});
        }
        if n == 200 {
            expected["lines"] = json!({"start": 3, "end": 3});
            expected["state"] = json!("found");
        }
        assert_eq!(after[n], expected, "anchor {n}");
    }
    assert_eq!(after[201]["state"], "unverified");
}

#[test]
fn debug_output_hides_code_and_prose_text() {
    let debug = format!("{:?}", every_kind());
    for shown in [
        "DiagramId(1)",
        "Architecture",
        "Boundary",
        "\"src/a.rs\"",
        "Lines { lines: LineRange { start: 2, end: 3 }, .. }",
        "Outdated",
        "\"big.bin\", state: Unverified, ..",
    ] {
        assert!(debug.contains(shown), "{shown} in {debug}");
    }
    for hidden in ["secret", "b\\nc", "21 MB"] {
        assert!(!debug.contains(hidden), "{hidden} in {debug}");
    }
    let cited = format!("{:?}", cite("a.rs", at(1, 2), Some("secret first")));
    assert_eq!(
        cited,
        "Cited { path: \"a.rs\", lines: Some(CitedLines { start: 1, end: 2 }), .. }"
    );
}
