//! Build the review model: symbols at both revisions, statuses, call edges,
//! entry points and the DFS review tree.

use crate::syntax::{self, ParsedFile};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Removed,
    BodyChanged,
    Moved,
    Unchanged,
}

impl Status {
    pub fn glyph(&self) -> &'static str {
        match self {
            Status::Added => "+",
            Status::Removed => "-",
            Status::BodyChanged => "~",
            Status::Moved => ">",
            Status::Unchanged => " ",
        }
    }

    pub fn is_change(&self) -> bool {
        !matches!(self, Status::Unchanged)
    }
}

/// A node in the review graph. Head-node based; removed functions are kept
/// as their own nodes (with base info) so their callers/callees are visible.
#[derive(Debug, Clone)]
pub struct Node {
    pub file: String,
    pub name: String,
    pub status: Status,
    /// head file (or base file for Removed nodes)
    pub src: String,
    pub start_row: usize,
    pub end_row: usize,
    /// resolved head-graph caller keys
    pub callers: Vec<String>,
    /// resolved head-graph callee keys
    pub callees: Vec<String>,
    /// for Removed nodes: base callees; for others: unresolved callee names
    pub lost_callees: Vec<String>,
    /// any subtree reachable child changed?
    pub relevant: bool,
}

pub fn node_key(file: &str, name: &str) -> String {
    format!("{file}::{name}")
}

pub struct Analysis {
    pub nodes: HashMap<String, Node>,
    pub entry_points: Vec<String>,
    /// (+added, -removed, ~changed, >moved) across symbols
    pub summary: (usize, usize, usize, usize),
}

/// Inputs: changed file paths plus full sources at base/head.
pub struct Inputs {
    /// (path, base_src_or_None, head_src_or_None)
    pub files: Vec<(String, Option<String>, Option<String>)>,
    /// unchanged files at head, parsed ONLY for caller edges into changed
    /// symbols (they never become nodes themselves)
    pub extra_head: Vec<(String, String)>,
}

pub fn analyze(inputs: &Inputs) -> Analysis {
    // --- parse both revisions -------------------------------------------
    let mut base_map: HashMap<String, (usize, String, ParsedFile)> = HashMap::new(); // key -> (idx, file, parsed)
    let mut head_map: HashMap<String, (usize, String, ParsedFile)> = HashMap::new();
    let mut head_srcs: HashMap<String, String> = HashMap::new();
    let mut extra_parsed: Vec<(String, ParsedFile)> = Vec::new();

    for (path, base_src, head_src) in &inputs.files {
        if let Some(src) = base_src {
            if let Some(pf) = syntax::parse_file(path, src) {
                for (i, f) in pf.fns.iter().enumerate() {
                    base_map.insert(node_key(path, &f.name), (i, path.clone(), pf.clone()));
                }
            }
        }
        if let Some(src) = head_src {
            if let Some(pf) = syntax::parse_file(path, src) {
                for (i, f) in pf.fns.iter().enumerate() {
                    head_map.insert(node_key(path, &f.name), (i, path.clone(), pf.clone()));
                }
                head_srcs.insert(path.clone(), src.clone());
            }
        }
    }
    for (path, src) in &inputs.extra_head {
        if let Some(pf) = syntax::parse_file(path, src) {
            extra_parsed.push((path.clone(), pf));
        }
    }

    // simple name -> keys, across changed + unchanged head files
    let mut name_index: HashMap<String, Vec<String>> = HashMap::new();
    for key in head_map.keys() {
        if let Some(name) = key.rsplit("::").next() {
            name_index
                .entry(name.to_string())
                .or_default()
                .push(key.clone());
        }
    }
    for (path, pf) in &extra_parsed {
        for f in &pf.fns {
            name_index
                .entry(f.name.clone())
                .or_default()
                .push(node_key(path, &f.name));
        }
    }

    // --- statuses ---------------------------------------------------------
    let mut nodes: HashMap<String, Node> = HashMap::new();

    // rename/move detection: removed body hash == added body hash
    let mut removed_by_hash: HashMap<u64, Vec<String>> = HashMap::new();
    for (key, (idx, file, pf)) in &base_map {
        if !head_map.contains_key(key) {
            let f = &pf.fns[*idx];
            removed_by_hash
                .entry(f.body_hash)
                .or_default()
                .push(key.clone());
        }
        let _ = file;
    }
    let mut moved_pairs: HashMap<String, String> = HashMap::new(); // head key -> base key
    let mut consumed_removed: HashSet<String> = HashSet::new();
    for key in head_map.keys() {
        if !base_map.contains_key(key) {
            let (idx, _file, pf) = &head_map[key];
            let h = pf.fns[*idx].body_hash;
            if let Some(cands) = removed_by_hash.get(&h) {
                if let Some(base_key) = cands.iter().find(|k| !consumed_removed.contains(*k)) {
                    moved_pairs.insert(key.clone(), base_key.clone());
                    consumed_removed.insert(base_key.clone());
                }
            }
        }
    }

    // head nodes
    for (key, (idx, file, pf)) in &head_map {
        let f = &pf.fns[*idx];
        let status = if let Some(bkey) = moved_pairs.get(key) {
            let (_bi, _bf, bpf) = &base_map[bkey];
            let _ = bpf;
            Status::Moved
        } else if let Some((bidx, _bf, bpf)) = base_map.get(key) {
            if bpf.fns[*bidx].body_hash == f.body_hash {
                Status::Unchanged
            } else {
                Status::BodyChanged
            }
        } else {
            Status::Added
        };
        nodes.insert(
            key.clone(),
            Node {
                file: file.clone(),
                name: f.name.clone(),
                status,
                src: head_srcs.get(file).cloned().unwrap_or_default(),
                start_row: f.start_row,
                end_row: f.end_row,
                callers: vec![],
                callees: vec![],
                lost_callees: vec![],
                relevant: status.is_change(),
            },
        );
    }

    // removed nodes (skip consumed renames)
    for (key, (idx, file, pf)) in &base_map {
        if head_map.contains_key(key) || consumed_removed.contains(key) {
            continue;
        }
        let f = &pf.fns[*idx];
        let mut node = Node {
            file: file.clone(),
            name: f.name.clone(),
            status: Status::Removed,
            src: String::new(), // base source patched in below
            start_row: f.start_row,
            end_row: f.end_row,
            callers: vec![],
            callees: vec![],
            lost_callees: vec![],
            relevant: true,
        };
        // callees from the base revision
        let mut callees = vec![];
        for c in &pf.calls {
            if c.caller != *idx {
                continue;
            }
            let ck = resolve_call(&c.name, file, &base_map, &head_map, None);
            match ck {
                Some(k) => callees.push(k),
                None => node.lost_callees.push(c.name.clone()),
            }
        }
        node.callees = callees;
        nodes.insert(key.clone(), node);
    }

    // base sources for removed nodes
    for (path, base_src, _) in &inputs.files {
        if let Some(src) = base_src {
            for (key, node) in nodes.iter_mut() {
                if node.status == Status::Removed && &node.file == path {
                    let _ = key;
                    node.src = src.clone();
                }
            }
        }
    }

    // --- edges (head graph) ----------------------------------------------
    for (key, (idx, file, pf)) in &head_map {
        let mut callees = vec![];
        let mut unresolved = vec![];
        for c in &pf.calls {
            if c.caller != *idx {
                continue;
            }
            match resolve_call(&c.name, file, &base_map, &head_map, Some(&name_index)) {
                Some(k) => callees.push(k),
                None => unresolved.push(c.name.clone()),
            }
        }
        if let Some(n) = nodes.get_mut(key) {
            n.callees = callees;
            n.lost_callees = unresolved;
        }
    }

    // callers from UNCHANGED files: they don't become nodes but they do
    // disqualify their callees from entry-point status
    for (path, pf) in &extra_parsed {
        for (i, f) in pf.fns.iter().enumerate() {
            for c in &pf.calls {
                if c.caller != i {
                    continue;
                }
                if let Some(k) =
                    resolve_call(&c.name, path, &base_map, &head_map, Some(&name_index))
                {
                    if let Some(target) = nodes.get_mut(&k) {
                        target.callers.push(node_key(path, &f.name));
                    }
                }
            }
        }
    }
    let caller_keys: Vec<(String, Vec<String>)> = nodes
        .iter()
        .filter(|(_, n)| n.status != Status::Removed)
        .map(|(k, n)| (k.clone(), n.callees.clone()))
        .collect();
    for (k, callees) in &caller_keys {
        for c in callees {
            if let Some(target) = nodes.get_mut(c) {
                target.callers.push(k.clone());
            }
        }
    }

    // --- entry points: no resolved callers within the changed set ---------
    // (callers that live in unchanged files are "external": the function is
    // still a review root, the diff is where comprehension must start)
    let mut entry_points: Vec<String> = nodes
        .iter()
        .filter(|(_, n)| n.callers.iter().all(|c| !nodes.contains_key(c)))
        .map(|(k, _)| k.clone())
        .collect();
    sort_keys(&mut entry_points, &nodes);

    // --- relevance: does any descendant (or self) carry a change? ---------
    let mut memo: HashMap<String, bool> = HashMap::new();
    for key in nodes.keys() {
        compute_relevant(key, &nodes, &mut memo, &mut HashSet::new());
    }
    for (k, n) in nodes.iter_mut() {
        n.relevant = *memo.get(k).unwrap_or(&n.status.is_change());
    }

    // --- summary ----------------------------------------------------------
    let mut summary = (0usize, 0usize, 0usize, 0usize);
    for n in nodes.values() {
        match n.status {
            Status::Added => summary.0 += 1,
            Status::Removed => summary.1 += 1,
            Status::BodyChanged => summary.2 += 1,
            Status::Moved => summary.3 += 1,
            Status::Unchanged => {}
        }
    }

    Analysis {
        nodes,
        entry_points,
        summary,
    }
}

fn compute_relevant(
    key: &str,
    nodes: &HashMap<String, Node>,
    memo: &mut HashMap<String, bool>,
    stack: &mut HashSet<String>,
) -> bool {
    if let Some(v) = memo.get(key) {
        return *v;
    }
    if !stack.insert(key.to_string()) {
        return false; // cycle
    }
    let mut rel = nodes
        .get(key)
        .map(|n| n.status.is_change())
        .unwrap_or(false);
    if let Some(n) = nodes.get(key) {
        for c in &n.callees {
            if compute_relevant(c, nodes, memo, stack) {
                rel = true;
                break;
            }
        }
    }
    stack.remove(key);
    memo.insert(key.to_string(), rel);
    rel
}

/// Resolve a simple callee name to a node key.
/// Preference: same-file exact name, then a unique match repo-wide.
/// `name_index` covers changed + unchanged head files; without it only
/// changed files are searched.
fn resolve_call(
    name: &str,
    caller_file: &str,
    base_map: &HashMap<String, (usize, String, ParsedFile)>,
    head_map: &HashMap<String, (usize, String, ParsedFile)>,
    name_index: Option<&HashMap<String, Vec<String>>>,
) -> Option<String> {
    let same = node_key(caller_file, name);
    if head_map.contains_key(&same) {
        return Some(same);
    }
    let mut hits: Vec<String> = match name_index {
        Some(idx) => idx.get(name).cloned().unwrap_or_default(),
        None => head_map
            .keys()
            .filter(|k| k.ends_with(&format!("::{name}")))
            .cloned()
            .collect(),
    };
    hits.retain(|k| head_map.contains_key(k)); // only changed nodes are real targets
                                               // for removed callees fall back to the base map
    if hits.is_empty() {
        hits = base_map
            .keys()
            .filter(|k| k.ends_with(&format!("::{name}")))
            .cloned()
            .collect();
    }
    if hits.len() == 1 {
        Some(hits.pop().unwrap())
    } else {
        None
    }
}

fn sort_keys(keys: &mut Vec<String>, nodes: &HashMap<String, Node>) {
    keys.sort_by(|a, b| {
        let na = nodes.get(a);
        let nb = nodes.get(b);
        let ra = na.map(|n| status_rank(n.status)).unwrap_or(9);
        let rb = nb.map(|n| status_rank(n.status)).unwrap_or(9);
        ra.cmp(&rb).then_with(|| a.cmp(b))
    });
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::BodyChanged => 0,
        Status::Added => 1,
        Status::Moved => 2,
        Status::Removed => 3,
        Status::Unchanged => 4,
    }
}

/// One visible row of the review tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub key: String,
    pub depth: usize,
    pub has_children: bool,
}

/// DFS rows starting from entry points (or a single forced root).
/// `focus` hides unchanged subtrees that contain no change anywhere below.
pub fn build_rows(
    a: &Analysis,
    roots: &[String],
    expanded: &HashSet<String>,
    focus: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut visited_global: HashSet<String> = HashSet::new();

    for root in roots {
        if !a.nodes.contains_key(root) {
            continue;
        }
        dfs(a, root, 0, expanded, focus, &mut rows, &mut visited_global);
    }
    rows
}

fn dfs(
    a: &Analysis,
    key: &str,
    depth: usize,
    expanded: &HashSet<String>,
    focus: bool,
    rows: &mut Vec<TreeRow>,
    visited_global: &mut HashSet<String>,
) {
    let Some(node) = a.nodes.get(key) else { return };
    if focus && !node.relevant {
        return;
    }
    let first_visit = visited_global.insert(key.to_string());
    let children = children_of(a, key, focus);
    let has_children = !children.is_empty();
    rows.push(TreeRow {
        key: key.to_string(),
        depth,
        has_children,
    });
    if !expanded.contains(key) || !has_children {
        return;
    }
    for child in children {
        if first_visit {
            dfs(a, &child, depth + 1, expanded, focus, rows, visited_global);
        } else {
            // already rendered elsewhere in this pass: reference, don't recurse
            rows.push(TreeRow {
                key: child.clone(),
                depth: depth + 1,
                has_children: false,
            });
        }
    }
}

fn children_of(a: &Analysis, key: &str, focus: bool) -> Vec<String> {
    let Some(n) = a.nodes.get(key) else {
        return vec![];
    };
    let mut kids: Vec<String> = n
        .callees
        .iter()
        .filter(|c| a.nodes.contains_key(*c))
        .filter(|c| !focus || a.nodes.get(*c).map(|x| x.relevant).unwrap_or(true))
        .cloned()
        .collect();
    kids.sort_by(|x, y| {
        let nx = &a.nodes[x];
        let ny = &a.nodes[y];
        status_rank(nx.status)
            .cmp(&status_rank(ny.status))
            .then_with(|| nx.relevant.cmp(&ny.relevant).reverse())
            .then_with(|| x.cmp(y))
    });
    kids.dedup();
    kids
}

/// Breadth-first caller impact for the detail pane.
pub fn impacted_callers(a: &Analysis, key: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut q: VecDeque<String> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    q.push_back(key.to_string());
    seen.insert(key.to_string());
    while let Some(k) = q.pop_front() {
        if let Some(n) = a.nodes.get(&k) {
            for c in &n.callers {
                if seen.insert(c.clone()) {
                    out.push(c.clone());
                    if out.len() >= max {
                        return out;
                    }
                    q.push_back(c.clone());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_pair(file: &str, base: Option<&str>, head: Option<&str>) -> Analysis {
        let inputs = Inputs {
            files: vec![(
                file.to_string(),
                base.map(|s| s.to_string()),
                head.map(|s| s.to_string()),
            )],
            extra_head: vec![],
        };
        analyze(&inputs)
    }

    #[test]
    fn caller_in_unchanged_file_blocks_entry_point() {
        let base = "fn a() { b(); }\nfn b() {}\n";
        let head = "fn a() { b(); }\nfn b() { println!(\"changed\"); }\n";
        let inputs = Inputs {
            files: vec![(
                "m.rs".to_string(),
                Some(base.to_string()),
                Some(head.to_string()),
            )],
            // unchanged file m2.rs calls a()
            extra_head: vec![("m2.rs".to_string(), "fn top() { a(); }\n".to_string())],
        };
        let a = analyze(&inputs);
        // b changed; a unchanged but reaches a change
        assert_eq!(a.nodes[&node_key("m.rs", "b")].status, Status::BodyChanged);
        // a is called from top() in m2.rs (external) => a IS the review root
        assert!(a.nodes[&node_key("m.rs", "a")]
            .callers
            .contains(&node_key("m2.rs", "top")));
        // b's only caller is a (in scope) => b is NOT an entry point
        assert!(a.entry_points.contains(&node_key("m.rs", "a")));
        assert!(!a.entry_points.contains(&node_key("m.rs", "b")));
    }

    #[test]
    fn statuses_and_entry_points() {
        let base = "fn a() { b(); }\nfn b() { c(); }\nfn c() { println!(\"x\"); }\n";
        let head = "fn a() { b(); }\nfn b() { c(); }\nfn c() { println!(\"changed\"); }\nfn fresh() { c(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 4);
        assert_eq!(a.nodes[&node_key("m.rs", "c")].status, Status::BodyChanged);
        assert_eq!(a.nodes[&node_key("m.rs", "fresh")].status, Status::Added);
        // entry points: a (no callers) and fresh (no callers)
        let eps: HashSet<&str> = a
            .entry_points
            .iter()
            .map(|k| k.rsplit("::").next().unwrap())
            .collect();
        assert!(eps.contains("a") && eps.contains("fresh"), "{eps:?}");
        // b changed? no. But relevance: a -> b -> c(changed) => a relevant
        assert!(a.nodes[&node_key("m.rs", "a")].relevant);
        // focus rows from a: only relevant nodes visible
        let rows = build_rows(
            &a,
            &[node_key("m.rs", "a")],
            &HashSet::from([node_key("m.rs", "a"), node_key("m.rs", "b")]),
            true,
        );
        let names: Vec<&str> = rows
            .iter()
            .map(|r| r.key.rsplit("::").next().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn removed_and_added() {
        let base = "fn gone() { helper(); extra(); }\nfn helper() {}\nfn extra() {}\n";
        let head = "fn new_thing() { helper(); }\nfn helper() {}\nfn extra() {}\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes[&node_key("m.rs", "gone")].status, Status::Removed);
        assert_eq!(
            a.nodes[&node_key("m.rs", "new_thing")].status,
            Status::Added
        );
        assert_eq!(
            a.nodes[&node_key("m.rs", "helper")].status,
            Status::Unchanged
        );
        // removed node's caller list is empty but its callees resolve
        let gone = &a.nodes[&node_key("m.rs", "gone")];
        assert!(gone.callees.contains(&node_key("m.rs", "helper")));
        let eps: HashSet<&str> = a
            .entry_points
            .iter()
            .map(|k| k.rsplit("::").next().unwrap())
            .collect();
        assert!(eps.contains("gone") && eps.contains("new_thing"));
    }

    #[test]
    fn move_detected() {
        let base = "fn alpha() { println!(\"same\"); }\n";
        let head = "fn beta() { println!(\"same\"); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes[&node_key("m.rs", "beta")].status, Status::Moved);
    }
}
