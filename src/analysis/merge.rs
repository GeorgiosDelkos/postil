//! Reading an analysis run's reply into the analysis file (docs/review-assist.md, "An analysis
//! run"; docs/review-assist-plan.md, 8.2, "Edge cases").
//!
//! [`merge`] works in two steps, so `analysis.lock` is held for milliseconds. First, with no
//! lock, it reads the reply's last json block, cuts what is over a limit, and checks every
//! citation against the run's worktree ([`check_in`]): that reads up to hundreds of files.
//! Then it takes [`lock`], loads the file as stored (not as placed: placing would rewrite the
//! lines of diagrams the run did not touch), merges, and saves.
//!
//! The agent names its nodes with keys of its own; Postil gives each its id, `D<n>.<m>`, in the
//! order the reply lists them, which is a flow's step order. Whatever cannot be kept is counted
//! on its diagram ([`Dropped`]), never left out silently: a node with no key or a key used
//! twice, one past [`MAX_NODES`], an edge to or from no node or from a node to itself, a group
//! that names no node or loops (the node is kept, with no group), and text past its limit (cut,
//! ending in "…").
//!
//! What replaces what: Analyze replaces the top-level diagrams of the kinds it drew; a deeper
//! analysis the diagram under the same node; a flow adds one; an update changes only the nodes
//! its prompt sent as outdated ([`AnalysisRun::nodes`]), adds nodes and edges, and may delete
//! an outdated node, all within the diagram it was asked for when it was asked for one. A
//! diagram whose parent node goes takes the diagrams below it with it ([`prune_orphans`]); a
//! node replaced in place keeps its id, and so the diagrams below it, but loses its edges out,
//! which its prompt lists for the reply to give back. A diagram left with no node goes too.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use super::{
    Analysis, AnalysisKind, AnalysisRun, Cited, CitedLines, CodeRef, CommitId, Diagram, DiagramId,
    DiagramKind, Dropped, Edge, Node, NodeId, NodeKind, ReadFiles, RefDetail, RunId, check_in,
    has_architecture, load, lock, prune_orphans, save,
};
use crate::error::error_chain;
use crate::repo::Repo;
use crate::report::repo_path;
use crate::review::last_json_block;

/// The most of a reply read, in bytes: 2 MiB. Arbitrary, from the plan: 200 nodes of 4,000
/// characters of detail each fit, with room for their citations. A longer reply is read from
/// its end, where the block is.
const MAX_REPLY: usize = 2 << 20;

/// The most nodes a diagram keeps. Arbitrary, from the plan: the layout is measured at 200
/// (spike S4), and a reader cannot follow more.
const MAX_NODES: usize = 200;

/// The longest label and title kept, in characters: what a node's card shows on one line.
const MAX_LABEL: usize = 60;

/// The longest summary kept, in characters: one line under the label.
const MAX_SUMMARY: usize = 160;

/// The longest detail kept, in characters. Arbitrary: a few paragraphs in the node's dialog.
const MAX_DETAIL: usize = 4_000;

/// What the run says when its reply gave nothing to store.
const EMPTY: &str = "the analysis was empty";

/// Merges the reply `reply` of analysis run `run`, which worked in `worktree` and asked what
/// `asked` says, into `repo`'s analysis file (module docs). Nothing is written unless the reply
/// gives at least one node, or for an update one change. Takes [`lock`] once the citations are
/// checked, and holds it across the load and the save; takes no other lock.
///
/// # Errors
///
/// Why nothing was stored, for the run's dialog: the reply has no block that reads, gave
/// nothing ([`EMPTY`]), deepens a node that is gone, or the file could not be locked, read or
/// written.
pub(crate) fn merge(
    repo: &Repo,
    worktree: &Repo,
    run: &str,
    asked: &AnalysisRun,
    reply: &str,
) -> Result<(), String> {
    let run = RunId::try_from(run.to_owned())?;
    let checked = check_reply(reply, worktree)?;
    let failed = |error| error_chain(&error);
    // The commit the run read: its thread's fork, which a read-only run leaves checked out.
    let fork = CommitId::of_head(worktree).map_err(failed)?;
    let file = repo.analysis_file();
    let _lock = lock(&file).map_err(failed)?;
    let mut analysis = load(&file).map_err(failed)?.unwrap_or(Analysis {
        checked: None,
        diagrams: Vec::new(),
    });
    let had_architecture = has_architecture(&analysis);
    apply(&mut analysis, checked, run, asked)?;
    // `checked` is the commit the Architecture was compared with: a run that stored an
    // Architecture diagram drew it at its fork, when it is Analyze, which draws the whole
    // Architecture again, or when there was none before. Any other run leaves `checked` where
    // it was, which the forced update moves (`check`): a flow, or a deeper diagram beside an
    // Architecture compared at an older commit, says nothing of the rest.
    let stored_architecture = analysis
        .diagrams
        .iter()
        .any(|diagram| diagram.run == run && diagram.kind == DiagramKind::Architecture);
    if stored_architecture && (asked.kind == AnalysisKind::Analyze || !had_architecture) {
        analysis.checked = fork;
    }
    save(&file, &analysis).map_err(failed)
}

/// The reply's diagrams as the agent wrote them. Every field may be missing or null, a list
/// read as empty: what a diagram cannot use is counted, not a reason to drop the reply. A field
/// of the wrong type (a label that is a number) still makes the block unreadable.
#[derive(Deserialize)]
struct RawReply {
    #[serde(default, deserialize_with = "null_as_empty")]
    diagrams: Vec<RawDiagram>,
}

/// A list that may be `null`, read as empty.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDiagram {
    /// An update's: the diagram it changes.
    id: Option<String>,
    kind: Option<String>,
    title: Option<String>,
    #[serde(deserialize_with = "null_as_empty")]
    nodes: Vec<RawNode>,
    #[serde(deserialize_with = "null_as_empty")]
    edges: Vec<RawEdge>,
    /// An update's: the outdated nodes whose code is gone.
    #[serde(deserialize_with = "null_as_empty")]
    deleted: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawNode {
    /// A new node's name in the reply.
    key: Option<String>,
    /// An update's: the outdated node it replaces.
    id: Option<String>,
    kind: Option<String>,
    label: Option<String>,
    summary: Option<String>,
    detail: Option<String>,
    group: Option<String>,
    #[serde(deserialize_with = "null_as_empty")]
    anchors: Vec<RawCite>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEdge {
    from: Option<String>,
    to: Option<String>,
    label: Option<String>,
    anchor: Option<RawCite>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawCite {
    path: String,
    /// Read as any value, so a malformed one makes the citation unverified ([`cited_lines`]),
    /// not the reply unreadable.
    lines: Option<Value>,
    first: Option<String>,
}

/// The lines a citation names: none when it names none (a whole file or folder); two positive
/// integers as they are; anything else as lines 0 to 0, which [`check_in`] never finds. Read
/// as none, a malformed `lines` would verify the whole file.
fn cited_lines(lines: Option<&Value>) -> Option<CitedLines> {
    let lines = lines.filter(|lines| !lines.is_null())?;
    let positive = |value: &Value| value.as_u64().and_then(|n| usize::try_from(n).ok());
    let read = match lines.as_array().map(Vec::as_slice) {
        Some([start, end]) => positive(start).zip(positive(end)),
        _ => None,
    };
    Some(match read {
        Some((start, end)) if start > 0 => CitedLines { start, end },
        _ => CitedLines { start: 0, end: 0 },
    })
}

/// A reply read and checked against the run's worktree, its text cut to the limits, before
/// any id is given.
struct Checked {
    diagrams: Vec<CheckedDiagram>,
}

struct CheckedDiagram {
    id: Option<String>,
    kind: Option<String>,
    title: String,
    nodes: Vec<CheckedNode>,
    edges: Vec<CheckedEdge>,
    deleted: Vec<String>,
    /// What was cut so far: nodes past [`MAX_NODES`], text past its limit.
    dropped: Dropped,
}

struct CheckedNode {
    key: Option<String>,
    id: Option<String>,
    group: Option<String>,
    body: NodeBody,
}

/// What a node says, which an update's replacement takes whole.
struct NodeBody {
    kind: NodeKind,
    label: String,
    summary: String,
    detail: String,
    anchors: Vec<CodeRef>,
}

struct CheckedEdge {
    from: String,
    to: String,
    label: String,
    anchor: Option<CodeRef>,
}

/// Reads the last json block of `reply`, or of its last [`MAX_REPLY`] bytes, and checks each
/// citation against `worktree`. Reads every cited file.
fn check_reply(reply: &str, worktree: &Repo) -> Result<Checked, String> {
    let ReplyTail { text, cut } = tail(reply);
    // A map first: serde reads a struct from an array too, by position.
    let object: Map<String, Value> = last_json_block(text).map_err(|problem| {
        if cut {
            format!(
                "{problem}; the reply is over the {} MiB limit, and only its end was read",
                MAX_REPLY >> 20
            )
        } else {
            problem
        }
    })?;
    let raw: RawReply = serde_json::from_value(Value::Object(object))
        .map_err(|error| format!("the reply's last json block holds no diagrams: {error}"))?;
    let mut files = CitedFiles {
        worktree,
        read: ReadFiles::default(),
    };
    let diagrams = raw
        .diagrams
        .into_iter()
        .map(|diagram| checked_diagram(diagram, &mut files, cut))
        .collect();
    Ok(Checked { diagrams })
}

/// The part of a reply that is read.
struct ReplyTail<'a> {
    text: &'a str,
    /// Whether the reply was longer than [`MAX_REPLY`], so `text` is its end.
    cut: bool,
}

/// `reply`, or its last [`MAX_REPLY`] bytes from a character's start.
fn tail(reply: &str) -> ReplyTail<'_> {
    if reply.len() <= MAX_REPLY {
        return ReplyTail {
            text: reply,
            cut: false,
        };
    }
    let mut start = reply.len() - MAX_REPLY;
    while !reply.is_char_boundary(start) {
        start += 1;
    }
    ReplyTail {
        text: &reply[start..],
        cut: true,
    }
}

/// The run's worktree, where citations are checked, and the files read so far.
struct CitedFiles<'a> {
    worktree: &'a Repo,
    read: ReadFiles,
}

fn checked_diagram(raw: RawDiagram, files: &mut CitedFiles<'_>, reply_cut: bool) -> CheckedDiagram {
    let mut dropped = Dropped {
        reply_cut,
        ..Dropped::default()
    };
    let title = shorten(raw.title.unwrap_or_default(), MAX_LABEL, &mut dropped);
    // Counted before the cut, which keeps the citations of nodes past it from being read.
    dropped.nodes += raw.nodes.len().saturating_sub(MAX_NODES);
    let nodes = raw
        .nodes
        .into_iter()
        .take(MAX_NODES)
        .map(|node| CheckedNode {
            key: node.key.filter(|key| !key.is_empty()),
            id: node.id.filter(|id| !id.is_empty()),
            group: node.group,
            body: NodeBody {
                kind: node_kind(node.kind.as_deref()),
                label: shorten(node.label.unwrap_or_default(), MAX_LABEL, &mut dropped),
                summary: shorten(node.summary.unwrap_or_default(), MAX_SUMMARY, &mut dropped),
                detail: shorten(node.detail.unwrap_or_default(), MAX_DETAIL, &mut dropped),
                anchors: node
                    .anchors
                    .into_iter()
                    .map(|cite| checked_cite(cite, files))
                    .collect(),
            },
        })
        .collect();
    let edges = raw
        .edges
        .into_iter()
        .map(|edge| CheckedEdge {
            from: edge.from.unwrap_or_default(),
            to: edge.to.unwrap_or_default(),
            label: shorten(edge.label.unwrap_or_default(), MAX_LABEL, &mut dropped),
            anchor: edge.anchor.map(|cite| checked_cite(cite, files)),
        })
        .collect();
    CheckedDiagram {
        id: raw.id,
        kind: raw.kind,
        title,
        nodes,
        edges,
        deleted: raw.deleted,
        dropped,
    }
}

/// A node's kind as the agent named it; one Postil does not know draws as a component.
fn node_kind(kind: Option<&str>) -> NodeKind {
    match kind {
        Some("store") => NodeKind::Store,
        Some("external") => NodeKind::External,
        Some("boundary") => NodeKind::Boundary,
        Some(_) | None => NodeKind::Component,
    }
}

/// `text`, cut to `max` characters, the last one "…", and counted on `dropped` when it was.
fn shorten(text: String, max: usize, dropped: &mut Dropped) -> String {
    if text.chars().count() <= max {
        return text;
    }
    dropped.shortened += 1;
    let mut kept: String = text.chars().take(max - 1).collect();
    kept.push('…');
    kept
}

/// `cite` checked in `worktree`, its path made relative to the worktree as the run log makes
/// tool paths relative. An absolute path through a symbolic link (macOS's `/var` for
/// `/private/var`) is resolved first. A path that stays outside is checked as written, and is
/// unverified.
fn checked_cite(cite: RawCite, files: &mut CitedFiles<'_>) -> CodeRef {
    let root = files.worktree.root();
    let path = repo_path(&cite.path, root)
        .or_else(|| {
            // Only an absolute path: a relative one would be resolved from Postil's own working
            // directory.
            if !Path::new(&cite.path).is_absolute() {
                return None;
            }
            let real = fs::canonicalize(&cite.path).ok()?;
            repo_path(real.to_str()?, root)
        })
        .unwrap_or(cite.path);
    let lines = cited_lines(cite.lines.as_ref());
    check_in(
        Cited {
            path,
            lines,
            first: cite.first,
        },
        files.worktree,
        &mut files.read,
    )
}

/// Merges `checked`, the reply of run `run` that asked what `asked` says, into `analysis` (module
/// docs, "What replaces what"), then removes the diagrams left with no parent.
fn apply(
    analysis: &mut Analysis,
    checked: Checked,
    run: RunId,
    asked: &AnalysisRun,
) -> Result<(), String> {
    // Ids go on from the highest in the file, so a replaced diagram's id is not given again
    // to its replacement: a page that still shows the old one would take one for the other.
    let next = analysis
        .diagrams
        .iter()
        .map(|diagram| diagram.id.0)
        .max()
        .unwrap_or(0)
        + 1;
    match asked.kind {
        AnalysisKind::Analyze => {
            let mut made = Vec::new();
            for diagram in checked.diagrams {
                let kind = match diagram.kind.as_deref() {
                    Some("architecture") => DiagramKind::Architecture,
                    Some("user_flow" | "userflow" | "data") => DiagramKind::UserFlow,
                    // The prompt asks for these two kinds only.
                    _ => continue,
                };
                let id = DiagramId(next + id_offset(made.len()));
                made.extend(build(id, kind, None, run, diagram));
            }
            if made.is_empty() {
                return Err(EMPTY.to_owned());
            }
            let kinds: Vec<DiagramKind> = made.iter().map(|diagram| diagram.kind).collect();
            analysis
                .diagrams
                .retain(|diagram| diagram.parent.is_some() || !kinds.contains(&diagram.kind));
            analysis.diagrams.extend(made);
        }
        AnalysisKind::Deeper => {
            let parent = deeper_parent(analysis, asked.of.as_deref())?;
            let made = first_built(
                checked,
                DiagramId(next),
                DiagramKind::Architecture,
                parent,
                run,
            )?;
            if let Some(parent) = parent {
                analysis
                    .diagrams
                    .retain(|diagram| diagram.parent != Some(parent));
            }
            analysis.diagrams.push(made);
        }
        AnalysisKind::Flow => {
            let made = first_built(checked, DiagramId(next), DiagramKind::Flow, None, run)?;
            analysis.diagrams.push(made);
        }
        AnalysisKind::Update => update(analysis, checked, asked)?,
    }
    // An update may delete every node of a diagram: one with none is no diagram.
    analysis
        .diagrams
        .retain(|diagram| !diagram.nodes.is_empty());
    prune_orphans(&mut analysis.diagrams);
    Ok(())
}

/// `count` as a diagram id offset. At most one per diagram of a reply, which is under 2 MiB.
fn id_offset(count: usize) -> u32 {
    u32::try_from(count).expect("a reply of at most 2 MiB holds fewer than 2^32 diagrams")
}

/// The first diagram of `checked` that gives a node, as diagram `id` of `kind` under `parent`.
fn first_built(
    checked: Checked,
    id: DiagramId,
    kind: DiagramKind,
    parent: Option<NodeId>,
    run: RunId,
) -> Result<Diagram, String> {
    checked
        .diagrams
        .into_iter()
        .find_map(|diagram| build(id, kind, parent, run, diagram))
        .ok_or_else(|| EMPTY.to_owned())
}

/// The node a deeper analysis of `of` goes under: the node `of` names, or for a path the node
/// of an Architecture diagram with the verified reference that is the path or its nearest
/// folder above it; `None` when no node covers the path.
///
/// # Errors
///
/// When `of` names a node the analysis no longer holds: it was deleted or replaced while the
/// run ran.
fn deeper_parent(analysis: &Analysis, of: Option<&str>) -> Result<Option<NodeId>, String> {
    let Some(of) = of else {
        return Ok(None);
    };
    let nodes = || analysis.diagrams.iter().flat_map(|diagram| &diagram.nodes);
    if let Some(id) = NodeId::parse(of) {
        return if nodes().any(|node| node.id == id) {
            Ok(Some(id))
        } else {
            Err(format!(
                "the node {of} it analyzes is gone from the analysis, so nothing was stored"
            ))
        };
    }
    let covering = analysis
        .diagrams
        .iter()
        .filter(|diagram| diagram.kind == DiagramKind::Architecture)
        .flat_map(|diagram| &diagram.nodes)
        .filter_map(|node| {
            node.anchors
                .iter()
                .filter(|anchor| matches!(anchor.detail, RefDetail::Verified { .. }))
                .filter(|anchor| covers(&anchor.path, of))
                .map(|anchor| anchor.path.len())
                .max()
                .map(|length| Covering {
                    length,
                    node: node.id,
                })
        })
        // The longest path is the nearest; of two equal ones, the first node listed.
        .reduce(|best, next| {
            if next.length > best.length {
                next
            } else {
                best
            }
        });
    Ok(covering.map(|covering| covering.node))
}

/// A node whose reference covers a path, and how long that reference's path is.
struct Covering {
    length: usize,
    node: NodeId,
}

/// Whether a reference to `path` covers `of`: the same path, or a folder above it.
fn covers(path: &str, of: &str) -> bool {
    of == path
        || of
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `raw` as diagram `id`, made by run `run`: its nodes numbered in order, its groups and edges
/// joined by key, and what cannot be kept counted. `None` when no node can be kept.
fn build(
    id: DiagramId,
    kind: DiagramKind,
    parent: Option<NodeId>,
    run: RunId,
    raw: CheckedDiagram,
) -> Option<Diagram> {
    let mut dropped = raw.dropped;
    let mut by_key: HashMap<String, NodeId> = HashMap::new();
    let mut nodes = Vec::new();
    let mut groups = Vec::new();
    for node in raw.nodes {
        let Some(key) = node.key.filter(|key| !by_key.contains_key(key)) else {
            dropped.nodes += 1;
            continue;
        };
        let number = u32::try_from(nodes.len() + 1).expect("at most MAX_NODES nodes are kept");
        let node_id = NodeId {
            diagram: id,
            node: number,
        };
        by_key.insert(key, node_id);
        groups.push(node.group);
        nodes.push(node_of(node_id, node.body));
    }
    if nodes.is_empty() {
        return None;
    }
    for (node, group) in nodes.iter_mut().zip(groups) {
        let Some(group) = group else { continue };
        match by_key.get(&group) {
            // A node grouped in itself is a loop, which `break_group_loops` clears and counts.
            Some(&named) => node.group = Some(named),
            _ => dropped.groups += 1,
        }
    }
    dropped.groups += break_group_loops(&mut nodes);
    let mut edges = Vec::new();
    for edge in raw.edges {
        match (by_key.get(&edge.from), by_key.get(&edge.to)) {
            (Some(&from), Some(&to)) if from != to => edges.push(Edge {
                from,
                to,
                label: edge.label,
                anchor: edge.anchor,
            }),
            _ => dropped.edges += 1,
        }
    }
    Some(Diagram {
        id,
        kind,
        title: raw.title,
        parent,
        run,
        nodes,
        edges,
        dropped,
    })
}

/// Node `id` saying what `body` says, with no group yet.
fn node_of(id: NodeId, body: NodeBody) -> Node {
    let NodeBody {
        kind,
        label,
        summary,
        detail,
        anchors,
    } = body;
    Node {
        id,
        kind,
        label,
        summary,
        detail,
        group: None,
        anchors,
    }
}

/// Clears the group of every node that its groups lead back to, so following groups ends; a
/// node whose groups lead into a loop it is not on keeps its group, which ends once the loop
/// is cleared. Every group names a node of `nodes` by now. Returns how many were cleared.
fn break_group_loops(nodes: &mut [Node]) -> usize {
    let mut cleared = 0;
    let mut group_of: HashMap<NodeId, Option<NodeId>> =
        nodes.iter().map(|node| (node.id, node.group)).collect();
    for index in 0..nodes.len() {
        let start = nodes[index].id;
        let mut at = nodes[index].group;
        let mut on_loop = false;
        for _ in 0..nodes.len() {
            match at {
                None => break,
                Some(group) if group == start => {
                    on_loop = true;
                    break;
                }
                Some(group) => at = group_of.get(&group).copied().flatten(),
            }
        }
        if on_loop {
            nodes[index].group = None;
            group_of.insert(start, None);
            cleared += 1;
        }
    }
    cleared
}

/// Merges an update's reply: each diagram it names by id takes its changes ([`update_one`]).
/// A diagram it names that the analysis does not hold, or that is outside the diagram the
/// update was asked for ([`AnalysisRun::of`]), is passed over.
///
/// # Errors
///
/// [`EMPTY`] when no diagram changed.
fn update(analysis: &mut Analysis, checked: Checked, asked: &AnalysisRun) -> Result<(), String> {
    let scope = asked.of.as_deref().and_then(DiagramId::parse);
    let mut changed = false;
    for raw in checked.diagrams {
        let Some(id) = raw.id.as_deref().and_then(DiagramId::parse) else {
            continue;
        };
        if scope.is_some_and(|scope| scope != id) {
            continue;
        }
        let Some(diagram) = analysis
            .diagrams
            .iter_mut()
            .find(|diagram| diagram.id == id)
        else {
            continue;
        };
        changed |= update_one(diagram, raw, &asked.nodes);
    }
    if !changed {
        return Err(EMPTY.to_owned());
    }
    Ok(())
}

/// Applies an update's changes to `diagram`: deletes and replaces only the nodes in
/// `outdated`, adds nodes and edges, and counts what it passed over. Returns whether it changed
/// anything.
///
/// The diagram's `run` stays the run that made it (main session, 2026-11-27): "Not on the map"
/// counts the files added since that run's commit (`analysis::off_the_map`), so a file an
/// update did not place stays listed until one places it, where moving `run` to the update's
/// would drop every file the update saw from the list for good. The downside accepted: the
/// update's own log is kept only while a run names it, so it goes at the next delete of a note
/// (`analysis::runs_named`).
fn update_one(diagram: &mut Diagram, raw: CheckedDiagram, outdated: &[NodeId]) -> bool {
    let mut dropped = raw.dropped;
    // Before the deletions: a deleted node's id is never given again, or a diagram below it and
    // an edge the reply names it in would land on the new node.
    let mut next = diagram
        .nodes
        .iter()
        .map(|node| node.id.node)
        .max()
        .unwrap_or(0)
        + 1;
    let mut changed = delete_nodes(diagram, &raw.deleted, outdated, &mut dropped);

    let mut by_key: HashMap<String, NodeId> = HashMap::new();
    let mut replaced: HashSet<NodeId> = HashSet::new();
    let mut groups: Vec<PendingGroup> = Vec::new();
    for node in raw.nodes {
        let id = if let Some(name) = &node.id {
            // Replaced once: a second replacement of the same node is passed over.
            let Some(existing) = NodeId::parse(name)
                .filter(|id| outdated.contains(id) && !replaced.contains(id))
                .and_then(|id| diagram.nodes.iter_mut().find(|node| node.id == id))
            else {
                dropped.nodes += 1;
                continue;
            };
            let id = existing.id;
            *existing = node_of(id, node.body);
            replaced.insert(id);
            // Its edges out go with it: the prompt lists them, and the reply gives back those
            // that still hold. Kept, an edge whose citation broke would stay outdated, and
            // force an update on every later commit to its file.
            diagram.edges.retain(|edge| edge.from != id);
            id
        } else {
            let Some(key) = node.key.filter(|key| !by_key.contains_key(key)) else {
                dropped.nodes += 1;
                continue;
            };
            if diagram.nodes.len() >= MAX_NODES {
                dropped.nodes += 1;
                continue;
            }
            let id = NodeId {
                diagram: diagram.id,
                node: next,
            };
            next += 1;
            by_key.insert(key, id);
            diagram.nodes.push(node_of(id, node.body));
            id
        };
        if let Some(group) = node.group {
            groups.push(PendingGroup { node: id, group });
        }
        changed = true;
    }
    changed |= join(diagram, &by_key, groups, raw.edges, &mut dropped);
    diagram.dropped.add(dropped);
    changed
}

/// Deletes from `diagram` each node `deleted` names that is in `outdated`, with its edges, and
/// the groups that named it; counts each other name on `dropped`. Returns whether any went.
fn delete_nodes(
    diagram: &mut Diagram,
    deleted: &[String],
    outdated: &[NodeId],
    dropped: &mut Dropped,
) -> bool {
    let mut changed = false;
    for name in deleted {
        let Some(id) = NodeId::parse(name).filter(|id| outdated.contains(id)) else {
            dropped.nodes += 1;
            continue;
        };
        let count = diagram.nodes.len();
        diagram.nodes.retain(|node| node.id != id);
        if diagram.nodes.len() == count {
            dropped.nodes += 1;
            continue;
        }
        diagram
            .edges
            .retain(|edge| edge.from != id && edge.to != id);
        for node in &mut diagram.nodes {
            if node.group == Some(id) {
                node.group = None;
            }
        }
        changed = true;
    }
    changed
}

/// A group an update's node names, given once every node of the update is in its diagram.
struct PendingGroup {
    node: NodeId,
    /// A new node's key, or a node's id.
    group: String,
}

/// Gives the nodes of an update their `groups` and adds its `edges`, each end named by a new
/// node's key in `by_key` or by the id of a node of `diagram`; counts on `dropped` what names
/// no node, or would close a loop. Returns whether an edge was added.
fn join(
    diagram: &mut Diagram,
    by_key: &HashMap<String, NodeId>,
    groups: Vec<PendingGroup>,
    edges: Vec<CheckedEdge>,
    dropped: &mut Dropped,
) -> bool {
    let resolve = |name: &str, nodes: &[Node]| {
        by_key
            .get(name)
            .copied()
            .or_else(|| NodeId::parse(name).filter(|id| nodes.iter().any(|node| node.id == *id)))
    };
    for PendingGroup { node: id, group } in groups {
        // A group that would close a loop is not given, so the groups of the nodes the update
        // did not change stay as they are; breaking loops afterwards could clear one of theirs.
        let named =
            resolve(&group, &diagram.nodes).filter(|named| !leads_to(&diagram.nodes, *named, id));
        let node = diagram
            .nodes
            .iter_mut()
            .find(|node| node.id == id)
            .expect("the node was kept or added by the update");
        match named {
            Some(named) => node.group = Some(named),
            None => dropped.groups += 1,
        }
    }
    let mut added = false;
    for edge in edges {
        let from = resolve(&edge.from, &diagram.nodes);
        let to = resolve(&edge.to, &diagram.nodes);
        match (from, to) {
            (Some(from), Some(to)) if from != to => {
                diagram.edges.push(Edge {
                    from,
                    to,
                    label: edge.label,
                    anchor: edge.anchor,
                });
                added = true;
            }
            _ => dropped.edges += 1,
        }
    }
    added
}

/// Whether following groups from `start`, itself included, reaches `target`. The groups of
/// `nodes` never loop when this is asked, so the walk ends within as many steps as there are
/// nodes.
fn leads_to(nodes: &[Node], start: NodeId, target: NodeId) -> bool {
    let mut at = Some(start);
    for _ in 0..=nodes.len() {
        match at {
            Some(id) if id == target => return true,
            Some(id) => {
                at = nodes
                    .iter()
                    .find(|node| node.id == id)
                    .and_then(|node| node.group);
            }
            None => return false,
        }
    }
    false
}

/// Reads `reply` as [`merge`] does, against `worktree`, and merges it into `analysis` as run
/// `run`, with no file: for the unit tests of the merge's rules.
#[cfg(test)]
pub(super) fn merge_into(
    analysis: &mut Analysis,
    reply: &str,
    worktree: &Repo,
    run: u64,
    asked: &AnalysisRun,
) -> Result<(), String> {
    let checked = check_reply(reply, worktree)?;
    apply(analysis, checked, RunId(run), asked)
}
