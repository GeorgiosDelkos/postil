//! What an analysis run asks (docs/review-assist.md, "The four kinds", "Outdated nodes and
//! Update"): the page's request checked against the repository and the analysis, the record
//! its run log keeps ([`AnalysisRun`]), and the prompt.
//!
//! The prompts describe the reply in words and never show an example block: the agent's reply
//! can quote its prompt, and the reply's last json block is the one read ([`super::merge`]).

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{
    Analysis, Diagram, DiagramId, DiagramKind, NodeId, RefDetail, VerifiedState, current, load,
};
use crate::anchor::Target;
use crate::error::{Error, Result};
use crate::notes::ASK_RULES;
use crate::repo::{PathKind, Repo};

/// The longest question a flow takes, in characters (docs/review-assist-plan.md, 8.5).
/// Arbitrary: a question is a sentence, and the thread's branch is named from it.
const MAX_QUESTION: usize = 300;

/// The most paths not on the map an update names. Arbitrary, from the plan (8.5): a
/// repository of generated files can add thousands, which no agent reads one by one.
const MAX_NEW_FILES: usize = 200;

/// What kind of analysis a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AnalysisKind {
    /// The Architecture's top level and the Data diagram of the whole repository.
    Analyze,
    /// One more Architecture diagram under a node, or for a file or folder.
    Deeper,
    /// One behavior as ordered steps.
    Flow,
    /// The outdated nodes again, and the files not on the map.
    Update,
}

/// An analysis run as its log's header records it (run log version 8): what was asked, which
/// is what its reply may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AnalysisRun {
    pub(crate) kind: AnalysisKind,
    /// What the run is about: the node or path deepened, a flow's starting node or path, the
    /// diagram updated. `None` for Analyze, a flow from nowhere in particular, and an update
    /// of every diagram.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) of: Option<String>,
    /// For an update, the outdated nodes its prompt sent: the only nodes its reply may replace
    /// or delete, so an update cannot rewrite the whole diagram. Empty for the other kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) nodes: Vec<NodeId>,
}

/// `POST /api/analysis`, as the page sends it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnalyzeRequest {
    kind: AnalysisKind,
    /// A node id (`D1.3`) or a path relative to the repository root.
    #[serde(default)]
    of: Option<String>,
    /// A flow's question.
    #[serde(default)]
    question: Option<String>,
}

/// `POST /api/analysis/update`, as the page sends it: the diagram to update, or every one.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateRequest {
    #[serde(default)]
    diagram: Option<String>,
}

/// What the page asked an analysis run to do.
#[derive(Debug)]
pub(crate) enum AnalysisJob {
    New(AnalyzeRequest),
    Update(UpdateRequest),
}

/// An analysis run ready to launch: its header's record, its thread's title, its prompt.
#[derive(Debug)]
pub(crate) struct Planned {
    pub(crate) run: AnalysisRun,
    /// What the thread's branch is named from.
    pub(crate) title: String,
    pub(crate) prompt: String,
}

/// Checks `job` against `repo` and its analysis, and plans its run. Reads the analysis, and for
/// an update places it, lists the files, and reads each diagram's thread for the commit its run
/// started from. Writes nothing.
///
/// # Errors
///
/// [`Error::BadRequest`] for a request that does not fit its kind (Analyze with a subject, a
/// deeper analysis of nothing there, a flow with no question or one over 300 characters, an
/// update with nothing to update or no analysis, a kind sent to the wrong route);
/// [`Error::NoSuchDiagram`] for an update of a diagram there is not; and as [`load`],
/// [`current`], [`Repo::files`] and [`Repo::kind`].
pub(crate) fn plan(repo: &Repo, job: AnalysisJob) -> Result<Planned> {
    match job {
        AnalysisJob::New(request) => plan_new(repo, request),
        AnalysisJob::Update(request) => plan_update(repo, request),
    }
}

fn plan_new(repo: &Repo, request: AnalyzeRequest) -> Result<Planned> {
    let AnalyzeRequest { kind, of, question } = request;
    let bad = |problem: &str| Err(Error::BadRequest(problem.to_owned()));
    if question.is_some() && kind != AnalysisKind::Flow {
        return bad("only a flow takes a question");
    }
    match kind {
        AnalysisKind::Analyze => {
            if of.is_some() {
                return bad("Analyze reads the whole repository: it takes no `of`");
            }
            Ok(Planned {
                run: record(kind, None),
                title: "analysis".to_owned(),
                prompt: analyze_prompt(),
            })
        }
        AnalysisKind::Deeper => {
            let Some(of) = of else {
                return bad("Analyze deeper needs `of`: a node id or a path");
            };
            let subject = subject(repo, &of)?;
            Ok(Planned {
                title: format!("deeper {of}"),
                prompt: deeper_prompt(&subject),
                run: record(kind, Some(of)),
            })
        }
        AnalysisKind::Flow => {
            let question = question.as_deref().map_or_default(str::trim);
            let length = question.chars().count();
            if length == 0 || length > MAX_QUESTION {
                return Err(Error::BadRequest(format!(
                    "a flow's question is 1 to {MAX_QUESTION} characters, not {length}"
                )));
            }
            let from = of.as_deref().map(|of| subject(repo, of)).transpose()?;
            Ok(Planned {
                title: format!("flow {question}"),
                prompt: flow_prompt(question, from.as_deref()),
                run: record(kind, of),
            })
        }
        AnalysisKind::Update => bad("an update goes to POST /api/analysis/update"),
    }
}

fn record(kind: AnalysisKind, of: Option<String>) -> AnalysisRun {
    AnalysisRun {
        kind,
        of,
        nodes: Vec::new(),
    }
}

/// What `of`, a node id or a path, names, in words for a prompt.
///
/// # Errors
///
/// [`Error::BadRequest`] when `of` is a node id the analysis does not hold, or a path that
/// names nothing in the working tree; as [`load`] and [`Repo::kind`].
fn subject(repo: &Repo, of: &str) -> Result<String> {
    if let Some(id) = NodeId::parse(of) {
        let analysis = load(&repo.analysis_file())?;
        let node = analysis
            .iter()
            .flat_map(|analysis| &analysis.diagrams)
            .flat_map(|diagram| &diagram.nodes)
            .find(|node| node.id == id)
            .ok_or_else(|| Error::BadRequest(format!("the analysis has no node {of}")))?;
        let mut text = format!(
            "the node {} of the analysis, \"{}\": {}\n{}\nIt cites",
            node.id, node.label, node.summary, node.detail
        );
        for anchor in &node.anchors {
            write!(text, " {}", anchor.path).expect("writing to a String cannot fail");
            if let RefDetail::Verified {
                target: Target::Lines { lines, .. },
                ..
            } = &anchor.detail
            {
                write!(text, " (lines {} to {})", lines.start, lines.end)
                    .expect("writing to a String cannot fail");
            }
        }
        text.push('.');
        return Ok(text);
    }
    match repo.kind(of)? {
        Some(PathKind::Folder) => Ok(format!("the folder {of}")),
        Some(PathKind::File) => Ok(format!("the file {of}")),
        None => Err(Error::BadRequest(format!(
            "{of} is no node id and names nothing in the working tree"
        ))),
    }
}

fn plan_update(repo: &Repo, request: UpdateRequest) -> Result<Planned> {
    let UpdateRequest { diagram } = request;
    let scope = diagram
        .as_deref()
        .map(|id| {
            DiagramId::parse(id).ok_or_else(|| {
                Error::BadRequest(format!("diagram id {id:?}: expected D and a number"))
            })
        })
        .transpose()?;
    let Some(analysis) = current(repo)? else {
        return Err(Error::BadRequest(
            "there is no analysis to update: analyze first".to_owned(),
        ));
    };
    if let Some(id) = scope
        && !analysis.diagrams.iter().any(|diagram| diagram.id == id)
    {
        return Err(Error::NoSuchDiagram { id: id.to_string() });
    }
    let in_scope = |diagram: &&Diagram| scope.is_none_or(|id| diagram.id == id);
    let outdated: Vec<NodeId> = analysis
        .diagrams
        .iter()
        .filter(in_scope)
        .flat_map(|diagram| &diagram.nodes)
        .filter(|node| {
            node.anchors.iter().any(|anchor| {
                matches!(
                    anchor.detail,
                    RefDetail::Verified {
                        placed: VerifiedState::Outdated,
                        ..
                    }
                )
            })
        })
        .map(|node| node.id)
        .collect();
    let new_files = super::off_the_map(repo, &analysis, scope)?;
    if outdated.is_empty() && new_files.is_empty() {
        return Err(Error::BadRequest(
            "nothing is outdated and no file was added since the analysis: there is nothing to \
             update"
                .to_owned(),
        ));
    }
    let prompt = update_prompt(&analysis, scope, &outdated, &new_files);
    Ok(Planned {
        run: AnalysisRun {
            kind: AnalysisKind::Update,
            of: scope.map(|id| id.to_string()),
            nodes: outdated,
        },
        title: "update analysis".to_owned(),
        prompt,
    })
}

/// How a reply's diagrams are written, for Analyze, a deeper analysis and a flow.
const DIAGRAMS: &str = "End your reply with one JSON object in a code block fenced as json, the \
     last thing in your reply. Its field diagrams is a list of diagrams. Each diagram is an \
     object with kind (architecture, data or flow), title (at most 60 characters), nodes and \
     edges. Each node is an object with key (a short name of your own, unique in its \
     diagram), kind (component for something that runs or a module or symbol below one, store \
     for something that holds data, external for something the repository talks to and does \
     not contain, boundary for a process, a machine or a network drawn around other nodes), \
     label (at most 60 characters), summary (one line, at most 160 characters), detail \
     (Markdown, at most 4000 characters), group (the key of the boundary node drawn around \
     it, or null) and anchors, a list of citations of the code it stands for. A citation is \
     an object with path (relative to the repository root), lines (the first and last line, \
     as an array of two numbers) and first (the first cited line, exactly as the file has \
     it); for a whole file or folder give only path. Every node cites code: a component \
     where it starts or is defined, a store the code that reads and writes it, an external \
     system the code that calls it. Each edge is an object with from and to (node keys), \
     label (the mechanism, such as JSON-RPC over stdio, or appends) and anchor, one citation \
     of the call, send or write that makes it, or null. At most 200 nodes per diagram; \
     Postil checks every citation against the files, and keeps what it cannot find as not \
     found in code.\n";

fn analyze_prompt() -> String {
    format!(
        "Analyze this repository for an engineer who reviews changes to it. Draw two \
         diagrams of 7 to 25 nodes each.\n\nThe first, of kind architecture, is the system as \
         it runs, not its folder tree: the components (a process, a thread, a service, a \
         page), the stores (a file, a database, a queue) and the external systems (a \
         subprocess, a third-party API), with boundary nodes for processes or machines, and \
         an edge for each way two of them talk, labeled with the mechanism.\n\nThe second, of \
         kind data, is the main types: what each holds, and how they relate.\n\n{DIAGRAMS}\n\
         {ASK_RULES}"
    )
}

fn deeper_prompt(subject: &str) -> String {
    format!(
        "Analyze one part of this repository in more depth, for an engineer who reviews \
         changes to it: {subject}\n\nDraw one diagram of kind architecture of 7 to 25 nodes: \
         the modules that make it, and below them the symbols that matter, with an edge for \
         each call, message or shared value between them.\n\n{DIAGRAMS}\n{ASK_RULES}"
    )
}

fn flow_prompt(question: &str, from: Option<&str>) -> String {
    let from = from.map_or_else(String::new, |from| format!("\n\nStart from {from}"));
    format!(
        "Trace one behavior of this repository through its code, for an engineer who reviews \
         changes to it:\n\n{question}{from}\n\nDraw one diagram of kind flow whose nodes are the \
         steps in the order they happen, each citing the code that does it, with an edge from \
         each step to the next.\n\n{DIAGRAMS}\n{ASK_RULES}"
    )
}

fn update_prompt(
    analysis: &Analysis,
    scope: Option<DiagramId>,
    outdated: &[NodeId],
    new_files: &[String],
) -> String {
    let mut text = String::from(
        "The code changed since you analyzed this repository. Bring the diagrams below up to \
         date. Only the nodes marked outdated may change: their code is no longer where they \
         cite it.\n",
    );
    let line = "writing to a String cannot fail";
    for diagram in analysis
        .diagrams
        .iter()
        .filter(|diagram| scope.is_none_or(|id| diagram.id == id))
    {
        let stale: Vec<_> = diagram
            .nodes
            .iter()
            .filter(|node| outdated.contains(&node.id))
            .collect();
        let maps_files =
            matches!(diagram.kind, DiagramKind::Architecture) && diagram.parent.is_none();
        let lists_files = maps_files && !new_files.is_empty();
        if stale.is_empty() && !lists_files {
            continue;
        }
        write!(
            text,
            "\nDiagram {} ({:?}), \"{}\". Its nodes:\n",
            diagram.id, diagram.kind, diagram.title
        )
        .expect(line);
        for node in &diagram.nodes {
            let mark = if outdated.contains(&node.id) {
                " (outdated)"
            } else {
                ""
            };
            writeln!(text, "- {} \"{}\"{mark}", node.id, node.label).expect(line);
        }
        for node in stale {
            write!(
                text,
                "\nOutdated node {}, \"{}\": {}\n",
                node.id, node.label, node.summary
            )
            .expect(line);
            for anchor in &node.anchors {
                match &anchor.detail {
                    RefDetail::Verified {
                        target: Target::Lines { lines, snippet },
                        placed: VerifiedState::Outdated,
                    } => {
                        write!(
                            text,
                            "It cited {} lines {} to {}, which read:\n{}\n",
                            anchor.path,
                            lines.start,
                            lines.end,
                            indented(snippet)
                        )
                        .expect(line);
                    }
                    RefDetail::Verified {
                        placed: VerifiedState::Outdated,
                        ..
                    } => writeln!(text, "It cited {}, which is gone.", anchor.path).expect(line),
                    RefDetail::Verified { .. } | RefDetail::Unverified { .. } => {}
                }
            }
        }
    }
    if !new_files.is_empty() {
        text.push_str(
            "\nThese files were added since and no node covers them; place each in the \
             Architecture where it belongs, as a new node:\n",
        );
        for file in new_files.iter().take(MAX_NEW_FILES) {
            writeln!(text, "- {file}").expect(line);
        }
        if new_files.len() > MAX_NEW_FILES {
            writeln!(
                text,
                "and {} more, not listed here.",
                new_files.len() - MAX_NEW_FILES
            )
            .expect(line);
        }
    }
    text.push_str(
        "\nEnd your reply with one JSON object in a code block fenced as json, the last thing \
         in your reply. Its field diagrams is a list, one object per diagram you change, with \
         id (the diagram's, such as D1), nodes, edges and deleted. In nodes, an object with id \
         (an outdated node's) replaces that node whole, and an object with key (a short name \
         of your own) adds a node; either has kind, label, summary, detail, group (a node's id \
         or a new node's key, or null) and anchors, as the analysis wrote them. deleted lists \
         the ids of outdated nodes whose code is gone. Each edge in edges is added, with from \
         and to (node ids or new keys), label and anchor. A citation is an object with path \
         (relative to the repository root), lines (the first and last line, as an array of \
         two numbers) and first (the first cited line, exactly as the file has it); for a \
         whole file or folder give only path.\n\n",
    );
    text.push_str(ASK_RULES);
    text
}

/// `snippet` with each line indented by four spaces, so a prompt quotes code without a fence.
fn indented(snippet: &str) -> String {
    snippet
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
