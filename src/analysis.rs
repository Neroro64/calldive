//! Build the review model: symbols at both revisions, statuses, call edges,
//! entry points and the DFS review tree. Symbols retain revision-specific spans;
//! call resolution considers the complete same-language revision universe.

use crate::syntax::{self, FnDef, Lang};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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

    pub fn label(&self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Removed => "removed",
            Status::BodyChanged => "changed",
            Status::Moved => "moved",
            Status::Unchanged => "unchanged",
        }
    }
}

/// A function location in one revision. Rows are zero-based, end exclusive;
/// `file` is an identity path in the corresponding Analysis source map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub file: String,
    pub start_row: usize,
    pub end_row: usize,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub key: String,
    pub file: String,
    pub name: String,
    pub status: Status,
    pub head: Option<Span>,
    pub base: Option<Span>,
    pub moved_from: Option<String>,
    pub callers: Vec<String>,
    pub external_callers: Vec<String>,
    pub callees: Vec<String>,
    pub lost_callees: Vec<String>,
    pub relevant: bool,
}

pub fn node_key(file: &str, name: &str) -> String {
    format!("{file}::{name}")
}

pub struct Analysis {
    pub nodes: HashMap<String, Node>,
    pub entry_points: Vec<String>,
    /// (+added, -removed, ~changed, >moved) across symbols.
    pub summary: (usize, usize, usize, usize),
    pub head_srcs: HashMap<String, String>,
    pub base_srcs: HashMap<String, String>,
    pub file_labels: HashMap<String, String>,
}

impl Analysis {
    pub fn head_body(&self, n: &Node) -> Option<String> {
        revision_body(&self.head_srcs, n.head.as_ref()?)
    }

    pub fn base_body(&self, n: &Node) -> Option<String> {
        revision_body(&self.base_srcs, n.base.as_ref()?)
    }

    pub fn label<'a>(&'a self, path: &'a str) -> &'a str {
        self.file_labels
            .get(path)
            .map(String::as_str)
            .unwrap_or(path)
    }
}

fn revision_body(srcs: &HashMap<String, String>, span: &Span) -> Option<String> {
    let src = srcs.get(&span.file)?;
    let mut body = String::new();
    for (i, line) in src
        .lines()
        .skip(span.start_row)
        .take(span.end_row.saturating_sub(span.start_row))
        .enumerate()
    {
        if i > 0 {
            body.push('\n');
        }
        for part in line.split('\r') {
            body.push_str(part);
        }
    }
    Some(body)
}

pub struct Inputs {
    /// (identity path, base source, head source) for changed files.
    pub files: Vec<(String, Option<String>, Option<String>)>,
    /// Unchanged files supply resolution candidates and external callers,
    /// but never become nodes or disqualify an entry point.
    pub extra_head: Vec<(String, String)>,
    pub include_tests: bool,
}

/// Parsed exactly once; calls and definitions are indexed once per file.
struct FileData {
    path: String,
    family: Lang,
    fns: Vec<FnDef>,
    display: Vec<Option<String>>,
    keys: Vec<Option<String>>,
    calls: Vec<Vec<String>>,
    by_name: HashMap<String, Vec<usize>>,
}

impl FileData {
    fn parse(path: &str, src: &str, include_tests: bool) -> Option<Self> {
        let family = syntax::lang_for_path(path)?.family();
        let parsed = syntax::parse_file(path, src)?;
        let mut qual_counts: HashMap<&str, usize> = HashMap::new();
        for f in &parsed.fns {
            if include_tests || !f.is_test {
                *qual_counts.entry(&f.qual).or_default() += 1;
            }
        }
        let mut display: Vec<Option<String>> = parsed
            .fns
            .iter()
            .map(|f| {
                if f.is_test && !include_tests {
                    None
                } else if qual_counts[f.qual.as_str()] > 1 {
                    Some(format!("{}{}", f.qual, f.sig))
                } else {
                    Some(f.qual.clone())
                }
            })
            .collect();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for name in display.iter().flatten() {
            *counts.entry(name.clone()).or_default() += 1;
        }
        // Reserve all unsuffixed identities so a generated suffix cannot
        // collide with another definition's literal display name.
        let mut used: HashSet<String> = counts.keys().cloned().collect();
        let mut ordinals: HashMap<String, usize> = HashMap::new();
        for name in display.iter_mut().flatten() {
            if counts[name.as_str()] > 1 {
                let ordinal = ordinals.entry(name.clone()).or_default();
                loop {
                    *ordinal += 1;
                    let candidate = format!("{name}#{ordinal}");
                    if used.insert(candidate.clone()) {
                        *name = candidate;
                        break;
                    }
                }
            }
        }
        let keys = display
            .iter()
            .map(|n| n.as_ref().map(|n| node_key(path, n)))
            .collect();
        let mut calls = vec![Vec::new(); parsed.fns.len()];
        for call in parsed.calls {
            if display.get(call.caller).is_some_and(Option::is_some) {
                calls[call.caller].push(call.name);
            }
        }
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, f) in parsed.fns.iter().enumerate() {
            if display[i].is_some() {
                by_name.entry(f.name.clone()).or_default().push(i);
            }
        }
        Some(Self {
            path: path.to_string(),
            family,
            fns: parsed.fns,
            display,
            keys,
            calls,
            by_name,
        })
    }

    fn span(&self, function: usize) -> Span {
        let f = &self.fns[function];
        Span {
            file: self.path.clone(),
            start_row: f.start_row,
            end_row: f.end_row,
        }
    }
}

#[derive(Clone, Copy)]
struct DefId {
    file: usize,
    function: usize,
}

/// Pair within qualified-name groups, retaining appearance order for hashes.
fn pair_functions(base: &FileData, head: &FileData) -> Vec<(usize, usize)> {
    let mut groups: BTreeMap<&str, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, f) in base.fns.iter().enumerate() {
        if base.display[i].is_some() {
            groups.entry(&f.qual).or_default().0.push(i);
        }
    }
    for (i, f) in head.fns.iter().enumerate() {
        if head.display[i].is_some() {
            groups.entry(&f.qual).or_default().1.push(i);
        }
    }
    let mut paired_base = vec![false; base.fns.len()];
    let mut paired_head = vec![false; head.fns.len()];
    let mut pairs = Vec::new();
    for (bs, hs) in groups.values() {
        if bs.len() == 1 && hs.len() == 1 {
            pairs.push((bs[0], hs[0]));
            continue;
        }
        let mut base_sigs: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut head_counts: HashMap<&str, usize> = HashMap::new();
        for &b in bs {
            base_sigs.entry(&base.fns[b].sig).or_default().push(b);
        }
        for &h in hs {
            *head_counts.entry(&head.fns[h].sig).or_default() += 1;
        }
        for &h in hs {
            let sig = head.fns[h].sig.as_str();
            if head_counts[sig] == 1 {
                if let Some(candidates) = base_sigs.get(sig) {
                    if candidates.len() == 1 {
                        let b = candidates[0];
                        paired_base[b] = true;
                        paired_head[h] = true;
                        pairs.push((b, h));
                    }
                }
            }
        }
        let mut hashes: HashMap<u64, VecDeque<usize>> = HashMap::new();
        for &b in bs {
            if !paired_base[b] {
                hashes
                    .entry(base.fns[b].body_hash)
                    .or_default()
                    .push_back(b);
            }
        }
        for &h in hs {
            if !paired_head[h] {
                if let Some(b) = hashes
                    .get_mut(&head.fns[h].body_hash)
                    .and_then(VecDeque::pop_front)
                {
                    pairs.push((b, h));
                }
            }
        }
    }
    pairs
}

pub fn analyze(inputs: &Inputs) -> Analysis {
    let mut base_files = Vec::new();
    let mut head_files = Vec::new();
    let mut extras = Vec::new();
    let mut file_pairs = Vec::new();
    let mut base_srcs = HashMap::new();
    let mut head_srcs = HashMap::new();
    let mut ordered_files: Vec<_> = inputs.files.iter().collect();
    ordered_files.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, base, head) in ordered_files {
        let b = base.as_ref().and_then(|src| {
            base_srcs.insert(path.clone(), src.clone());
            FileData::parse(path, src, inputs.include_tests).map(|file| {
                let i = base_files.len();
                base_files.push(file);
                i
            })
        });
        let h = head.as_ref().and_then(|src| {
            head_srcs.insert(path.clone(), src.clone());
            FileData::parse(path, src, inputs.include_tests).map(|file| {
                let i = head_files.len();
                head_files.push(file);
                i
            })
        });
        file_pairs.push((b, h));
    }
    let mut ordered_extras: Vec<_> = inputs.extra_head.iter().collect();
    ordered_extras.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, src) in ordered_extras {
        if let Some(mut file) = FileData::parse(path, src, inputs.include_tests) {
            // Extra definitions participate in uniqueness, but have no nodes.
            file.keys.fill(None);
            extras.push(file);
        }
    }

    let mut origins: Vec<Vec<Option<DefId>>> =
        head_files.iter().map(|f| vec![None; f.fns.len()]).collect();
    let mut targets: Vec<Vec<Option<DefId>>> =
        base_files.iter().map(|f| vec![None; f.fns.len()]).collect();
    let mut moved: Vec<Vec<bool>> = head_files
        .iter()
        .map(|f| vec![false; f.fns.len()])
        .collect();
    for (base, head) in file_pairs {
        if let (Some(b), Some(h)) = (base, head) {
            for (bi, hi) in pair_functions(&base_files[b], &head_files[h]) {
                origins[h][hi] = Some(DefId {
                    file: b,
                    function: bi,
                });
                targets[b][bi] = Some(DefId {
                    file: h,
                    function: hi,
                });
            }
        }
    }

    // Each removed definition appears in two ordered queues. Lazy consumption
    // makes same-file preference deterministic without quadratic rescanning.
    {
        let mut removed = Vec::new();
        for (file, data) in base_files.iter().enumerate() {
            for (function, key) in data.keys.iter().enumerate() {
                if let Some(key) = key {
                    if targets[file][function].is_none() {
                        removed.push((key.as_str(), DefId { file, function }));
                    }
                }
            }
        }
        removed.sort_by(|a, b| a.0.cmp(b.0));
        let mut by_hash: HashMap<(Lang, u64), VecDeque<DefId>> = HashMap::new();
        let mut by_file: HashMap<(Lang, u64, &str), VecDeque<DefId>> = HashMap::new();
        for (_, id) in removed {
            let data = &base_files[id.file];
            let hash = data.fns[id.function].body_hash;
            by_hash
                .entry((data.family, hash))
                .or_default()
                .push_back(id);
            by_file
                .entry((data.family, hash, &data.path))
                .or_default()
                .push_back(id);
        }
        let mut added = Vec::new();
        for (file, data) in head_files.iter().enumerate() {
            for (function, key) in data.keys.iter().enumerate() {
                if let Some(key) = key {
                    if origins[file][function].is_none() {
                        added.push((key.as_str(), DefId { file, function }));
                    }
                }
            }
        }
        added.sort_by(|a, b| a.0.cmp(b.0));
        for (_, head) in added {
            let data = &head_files[head.file];
            let hash = data.fns[head.function].body_hash;
            let local = by_file
                .get_mut(&(data.family, hash, data.path.as_str()))
                .and_then(|queue| take_unconsumed(queue, &targets));
            let origin = local.or_else(|| {
                by_hash
                    .get_mut(&(data.family, hash))
                    .and_then(|queue| take_unconsumed(queue, &targets))
            });
            if let Some(base) = origin {
                origins[head.file][head.function] = Some(base);
                targets[base.file][base.function] = Some(head);
                moved[head.file][head.function] = true;
            }
        }
    }

    let mut nodes = HashMap::new();
    let mut label_paths = HashSet::new();
    for (file, data) in head_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            let Some(key) = key else { continue };
            let f = &data.fns[function];
            let origin = origins[file][function];
            let status = match origin {
                Some(_) if moved[file][function] => Status::Moved,
                Some(b) if base_files[b.file].fns[b.function].body_hash == f.body_hash => {
                    Status::Unchanged
                }
                Some(_) => Status::BodyChanged,
                None => Status::Added,
            };
            let base = origin.map(|b| base_files[b.file].span(b.function));
            if let Some(span) = &base {
                label_paths.insert(span.file.clone());
            }
            label_paths.insert(data.path.clone());
            nodes.insert(
                key.clone(),
                Node {
                    key: key.clone(),
                    file: data.path.clone(),
                    name: data.display[function].as_ref().unwrap().clone(),
                    status,
                    head: Some(data.span(function)),
                    base,
                    moved_from: origin.filter(|_| moved[file][function]).map(|b| {
                        base_files[b.file].display[b.function]
                            .as_ref()
                            .unwrap()
                            .clone()
                    }),
                    callers: Vec::new(),
                    external_callers: Vec::new(),
                    callees: Vec::new(),
                    lost_callees: Vec::new(),
                    relevant: status.is_change(),
                },
            );
        }
    }
    for (file, data) in base_files.iter_mut().enumerate() {
        for (function, target) in targets[file].iter().enumerate() {
            if data.display[function].is_none() {
                continue;
            }
            if let Some(head) = target {
                data.keys[function] = head_files[head.file].keys[head.function].clone();
                continue;
            }
            let original = data.display[function].as_ref().unwrap();
            let mut name = original.clone();
            let mut key = data.keys[function].as_ref().unwrap().clone();
            let mut ordinal = 0;
            while nodes.contains_key(&key) {
                ordinal += 1;
                name = format!("{original}#{ordinal}");
                key = node_key(&data.path, &name);
            }
            data.keys[function] = Some(key.clone());
            label_paths.insert(data.path.clone());
            nodes.insert(
                key.clone(),
                Node {
                    key,
                    file: data.path.clone(),
                    name,
                    status: Status::Removed,
                    head: None,
                    base: Some(data.span(function)),
                    moved_from: None,
                    callers: Vec::new(),
                    external_callers: Vec::new(),
                    callees: Vec::new(),
                    lost_callees: Vec::new(),
                    relevant: true,
                },
            );
        }
    }

    let mut removed_names: HashMap<(Lang, &str), Vec<&str>> = HashMap::new();
    for (file, data) in base_files.iter().enumerate() {
        for (i, key) in data.keys.iter().enumerate() {
            if targets[file][i].is_none() {
                if let Some(key) = key {
                    removed_names
                        .entry((data.family, &data.fns[i].name))
                        .or_default()
                        .push(key);
                }
            }
        }
    }
    let head_universe = Universe::new(head_files.iter().chain(extras.iter()).collect());
    let base_universe = Universe::new(base_files.iter().chain(extras.iter()).collect());
    for (file, data) in head_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            if let Some(key) = key {
                let (callees, lost) =
                    head_universe.resolve_calls(file, function, Some(&removed_names));
                let node = nodes.get_mut(key).unwrap();
                node.callees = callees;
                node.lost_callees = lost;
            }
        }
    }
    for (file, data) in base_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            if targets[file][function].is_none() {
                if let Some(key) = key {
                    let (callees, lost) = base_universe.resolve_calls(file, function, None);
                    let node = nodes.get_mut(key).unwrap();
                    node.callees = callees;
                    node.lost_callees = lost;
                }
            }
        }
    }
    for (file, data) in extras.iter().enumerate() {
        for (function, display) in data.display.iter().enumerate() {
            if display.is_none() {
                continue;
            }
            let (callees, _) = head_universe.resolve_calls(
                head_files.len() + file,
                function,
                Some(&removed_names),
            );
            if !callees.is_empty() {
                label_paths.insert(data.path.clone());
                let caller = node_key(&data.path, &data.fns[function].qual);
                for callee in callees {
                    nodes
                        .get_mut(&callee)
                        .unwrap()
                        .external_callers
                        .push(caller.clone());
                }
            }
        }
    }
    let mut incoming: HashMap<String, Vec<String>> = HashMap::new();
    for (key, node) in &nodes {
        for callee in &node.callees {
            incoming
                .entry(callee.clone())
                .or_default()
                .push(key.clone());
        }
    }
    for (key, node) in &mut nodes {
        node.callers = incoming.remove(key).unwrap_or_default();
        sort_dedup(&mut node.callers);
        sort_dedup(&mut node.external_callers);
    }

    let entry_points = root_components(&nodes);
    mark_relevant(&mut nodes);
    let mut summary = (0, 0, 0, 0);
    for node in nodes.values() {
        match node.status {
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
        head_srcs,
        base_srcs,
        file_labels: file_labels(label_paths),
    }
}

fn take_unconsumed(queue: &mut VecDeque<DefId>, targets: &[Vec<Option<DefId>>]) -> Option<DefId> {
    while let Some(id) = queue.pop_front() {
        if targets[id.file][id.function].is_none() {
            return Some(id);
        }
    }
    None
}

struct Universe<'a> {
    files: Vec<&'a FileData>,
    by_name: HashMap<(Lang, &'a str), Vec<DefId>>,
}

impl<'a> Universe<'a> {
    fn new(files: Vec<&'a FileData>) -> Self {
        let mut by_name: HashMap<(Lang, &str), Vec<DefId>> = HashMap::new();
        for (file, data) in files.iter().enumerate() {
            for (function, display) in data.display.iter().enumerate() {
                if display.is_some() {
                    by_name
                        .entry((data.family, &data.fns[function].name))
                        .or_default()
                        .push(DefId { file, function });
                }
            }
        }
        Self { files, by_name }
    }

    fn resolve_calls(
        &self,
        file: usize,
        function: usize,
        removed: Option<&HashMap<(Lang, &str), Vec<&str>>>,
    ) -> (Vec<String>, Vec<String>) {
        let caller = self.files[file];
        let mut callees = Vec::new();
        let mut lost = Vec::new();
        for name in &caller.calls[function] {
            let mut unresolved = false;
            if let Some(local) = caller.by_name.get(name) {
                // Local overload sets shadow every other file, including
                // when none of their definitions belongs to the review graph.
                for &target in local {
                    if let Some(key) = &caller.keys[target] {
                        callees.push(key.clone());
                    } else {
                        unresolved = true;
                    }
                }
            } else {
                match self
                    .by_name
                    .get(&(caller.family, name.as_str()))
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    [target] => {
                        if let Some(key) = &self.files[target.file].keys[target.function] {
                            callees.push(key.clone());
                        } else {
                            unresolved = true;
                        }
                    }
                    [] => {
                        let fallback =
                            removed.and_then(|index| index.get(&(caller.family, name.as_str())));
                        match fallback.map(Vec::as_slice).unwrap_or(&[]) {
                            [key] => callees.push((*key).to_string()),
                            _ => unresolved = true,
                        }
                    }
                    _ => unresolved = true,
                }
            }
            if unresolved {
                lost.push(name.clone());
            }
        }
        sort_dedup(&mut callees);
        sort_dedup(&mut lost);
        (callees, lost)
    }
}

fn sort_dedup(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

/// Iterative Kosaraju: finish-order DFS followed by reverse-graph components.
/// External callers deliberately do not participate in the node graph.
fn root_components(nodes: &HashMap<String, Node>) -> Vec<String> {
    let mut keys: Vec<_> = nodes.keys().collect();
    keys.sort();
    let indices: HashMap<&str, usize> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let edges: Vec<Vec<usize>> = keys
        .iter()
        .map(|key| {
            nodes[*key]
                .callees
                .iter()
                .filter_map(|callee| indices.get(callee.as_str()).copied())
                .collect()
        })
        .collect();
    let reverse: Vec<Vec<usize>> = keys
        .iter()
        .map(|key| {
            nodes[*key]
                .callers
                .iter()
                .filter_map(|caller| indices.get(caller.as_str()).copied())
                .collect()
        })
        .collect();
    let mut seen = vec![false; keys.len()];
    let mut finished = Vec::with_capacity(keys.len());
    let mut stack = Vec::new();
    for start in 0..keys.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push((start, 0));
        while let Some((node, next)) = stack.last_mut() {
            if *next < edges[*node].len() {
                let child = edges[*node][*next];
                *next += 1;
                if !seen[child] {
                    seen[child] = true;
                    stack.push((child, 0));
                }
            } else {
                finished.push(*node);
                stack.pop();
            }
        }
    }
    let mut component = vec![usize::MAX; keys.len()];
    let mut representatives = Vec::new();
    let mut pending = Vec::new();
    for start in finished.into_iter().rev() {
        if component[start] != usize::MAX {
            continue;
        }
        let id = representatives.len();
        let mut best = start;
        component[start] = id;
        pending.push(start);
        while let Some(node) = pending.pop() {
            if (status_rank(nodes[keys[node]].status), keys[node])
                < (status_rank(nodes[keys[best]].status), keys[best])
            {
                best = node;
            }
            for &parent in &reverse[node] {
                if component[parent] == usize::MAX {
                    component[parent] = id;
                    pending.push(parent);
                }
            }
        }
        representatives.push(best);
    }
    let mut has_incoming = vec![false; representatives.len()];
    for (node, children) in edges.iter().enumerate() {
        for &child in children {
            if component[node] != component[child] {
                has_incoming[component[child]] = true;
            }
        }
    }
    let mut roots: Vec<String> = representatives
        .iter()
        .enumerate()
        .filter(|(id, _)| !has_incoming[*id])
        .map(|(_, &node)| keys[node].clone())
        .collect();
    sort_keys(&mut roots, nodes);
    roots
}

fn mark_relevant(nodes: &mut HashMap<String, Node>) {
    let mut changed: Vec<String> = nodes
        .iter()
        .filter(|(_, n)| n.status.is_change())
        .map(|(k, _)| k.clone())
        .collect();
    changed.sort();
    let mut queue: VecDeque<String> = changed.into();
    while let Some(key) = queue.pop_front() {
        // Discovery sets relevance immediately, so cycles enqueue once only.
        let callers = std::mem::take(&mut nodes.get_mut(&key).unwrap().callers);
        for caller in &callers {
            let node = nodes.get_mut(caller).unwrap();
            if !node.relevant {
                node.relevant = true;
                queue.push_back(caller.clone());
            }
        }
        nodes.get_mut(&key).unwrap().callers = callers;
    }
}

fn file_labels(paths: HashSet<String>) -> HashMap<String, String> {
    let mut suffix_counts: HashMap<String, usize> = HashMap::new();
    let parts: Vec<_> = paths
        .iter()
        .map(|path| {
            (
                path,
                path.split(['/', '\\'])
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    for (_, segments) in &parts {
        for start in 0..segments.len() {
            *suffix_counts
                .entry(segments[start..].join("/"))
                .or_default() += 1;
        }
    }
    let mut labels = HashMap::new();
    for (path, segments) in parts {
        let mut label = segments.join("/");
        for start in (0..segments.len()).rev() {
            let suffix = segments[start..].join("/");
            if suffix_counts[&suffix] == 1 || start == 0 {
                label = suffix;
                break;
            }
        }
        labels.insert(path.clone(), label);
    }
    labels
}

fn sort_keys(keys: &mut [String], nodes: &HashMap<String, Node>) {
    keys.sort_by(|a, b| {
        let ra = nodes.get(a).map(|n| status_rank(n.status)).unwrap_or(9);
        let rb = nodes.get(b).map(|n| status_rank(n.status)).unwrap_or(9);
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

pub fn children_of(a: &Analysis, key: &str, focus: bool) -> Vec<String> {
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
            include_tests: false,
        };
        analyze(&inputs)
    }

    #[test]
    fn caller_in_unchanged_file_is_external() {
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
            include_tests: false,
        };
        let a = analyze(&inputs);
        // b changed; a unchanged but reaches a change
        assert_eq!(a.nodes[&node_key("m.rs", "b")].status, Status::BodyChanged);
        // a is called from top() in m2.rs (external) => a IS the review root
        assert!(a.nodes[&node_key("m.rs", "a")]
            .external_callers
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
    fn test_fns_filtered_unless_included() {
        let base = "fn a() { b(); }\nfn b() {}\n";
        let head = "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\n\n#[cfg(test)]\nmod tests {\n    fn test_b() { super::b(); }\n}\n";
        let mk = |include_tests: bool| Inputs {
            files: vec![(
                "m.rs".to_string(),
                Some(base.to_string()),
                Some(head.to_string()),
            )],
            extra_head: vec![],
            include_tests,
        };
        let a = analyze(&mk(false));
        assert!(a.nodes.contains_key(&node_key("m.rs", "c")));
        assert!(!a.nodes.contains_key(&node_key("m.rs", "tests::test_b")));
        let a = analyze(&mk(true));
        assert!(a.nodes.contains_key(&node_key("m.rs", "tests::test_b")));
    }

    #[test]
    fn move_detected() {
        let base = "fn alpha() { println!(\"same\"); }\n";
        let head = "fn beta() { println!(\"same\"); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes[&node_key("m.rs", "beta")].status, Status::Moved);
        let beta = &a.nodes[&node_key("m.rs", "beta")];
        assert_eq!(beta.moved_from.as_deref(), Some("alpha"));
        assert!(a.base_body(beta).unwrap().contains("fn alpha()"));
        assert!(a.head_body(beta).unwrap().contains("fn beta()"));
        assert_eq!(a.summary, (0, 0, 0, 1));
    }

    #[test]
    fn rust_container_identities_are_distinct() {
        let base =
            "struct A; struct B; impl A { fn new() { old(); } } impl B { fn new() { stable(); } }";
        let head = "struct A; struct B; impl A { fn new() { edited(); } } impl B { fn new() { stable(); } }";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 2);
        assert_eq!(
            a.nodes[&node_key("m.rs", "A::new")].status,
            Status::BodyChanged
        );
        assert_eq!(
            a.nodes[&node_key("m.rs", "B::new")].status,
            Status::Unchanged
        );
        assert_eq!(a.summary, (0, 0, 1, 0));
    }

    #[test]
    fn csharp_overloads_pair_by_signature() {
        let base =
            "class Worker { void Log(int x) { Before(); } void Log(string s) { Stable(); } }";
        let head = "class Worker { void Log(int x) { After(); } void Log(string s) { Stable(); } }";
        let a = analyze_pair("Worker.cs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 2);
        assert_eq!(
            a.nodes[&node_key("Worker.cs", "Worker::Log(int x)")].status,
            Status::BodyChanged
        );
        assert_eq!(
            a.nodes[&node_key("Worker.cs", "Worker::Log(string s)")].status,
            Status::Unchanged
        );
        assert_eq!(a.summary, (0, 0, 1, 0));
    }

    #[test]
    fn adding_overload_keeps_existing_identity_paired() {
        let base = "class Worker { void Log(int x) { Existing(); } }";
        let head =
            "class Worker { void Log(int x) { Existing(); } void Log(string s) { NewBody(); } }";
        let a = analyze_pair("Worker.cs", Some(base), Some(head));
        let existing = &a.nodes[&node_key("Worker.cs", "Worker::Log(int x)")];
        assert_eq!(existing.status, Status::Unchanged);
        assert!(a.base_body(existing).unwrap().contains("Existing();"));
        assert_eq!(
            a.nodes[&node_key("Worker.cs", "Worker::Log(string s)")].status,
            Status::Added
        );
        assert_eq!(a.summary, (1, 0, 0, 0));
    }

    #[test]
    fn duplicate_signatures_and_removed_collisions_keep_every_node() {
        let base = "fn dup() { one(); }\nfn dup() { two(); }\n";
        let head = "fn dup() { three(); }\nfn dup() { four(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 4);
        assert_eq!(a.summary, (2, 2, 0, 0));
        for (key, node) in &a.nodes {
            assert_eq!(key, &node.key);
            assert_eq!(key, &node_key(&node.file, &node.name));
        }
        let again = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(
            a.nodes.keys().collect::<HashSet<_>>(),
            again.nodes.keys().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn repeated_signatures_pair_equal_bodies_by_appearance() {
        let base = "fn dup() { one(); }\nfn dup() { two(); }\n";
        let head = "fn dup() { two(); }\nfn dup() { one(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 2);
        assert_eq!(a.summary, (0, 0, 0, 0));
        for node in a.nodes.values() {
            assert_eq!(node.status, Status::Unchanged);
            let base = a.base_body(node).unwrap();
            let head = a.head_body(node).unwrap();
            assert_eq!(base, head);
            assert_ne!(
                node.base.as_ref().unwrap().start_row,
                node.head.as_ref().unwrap().start_row
            );
        }
    }

    #[test]
    fn self_recursion_has_an_entry_point_and_keeps_edges() {
        let base = "fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }";
        let head = "fn fact(n: u64) -> u64 { if n == 0 { 2 } else { n * fact(n - 1) } }";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        let key = node_key("m.rs", "fact");
        assert_eq!(a.entry_points, vec![key.clone()]);
        assert_eq!(a.nodes[&key].callers, vec![key.clone()]);
        assert_eq!(a.nodes[&key].callees, vec![key]);
    }

    #[test]
    fn mutual_recursion_chooses_changed_component_member() {
        let base = "fn a() { b(); }\nfn b() { a(); }\n";
        let head = "fn a() { b(); }\nfn b() { a(); edited(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.entry_points, vec![node_key("m.rs", "b")]);
        assert!(a.nodes[&node_key("m.rs", "a")].relevant);
    }

    #[test]
    fn cyclic_relevance_is_deterministic() {
        let base = "fn e() { b(); }\nfn b() { a(); }\nfn a() { b(); c(); }\nfn c() { before(); }\n";
        let head = "fn e() { b(); }\nfn b() { a(); }\nfn a() { b(); c(); }\nfn c() { after(); }\n";
        for _ in 0..25 {
            let a = analyze_pair("m.rs", Some(base), Some(head));
            assert_eq!(a.entry_points, vec![node_key("m.rs", "e")]);
            assert!(a.nodes.values().all(|node| node.relevant));
            assert_eq!(
                build_rows(&a, &a.entry_points, &HashSet::new(), true).len(),
                1
            );
        }
    }

    #[test]
    fn unchanged_candidates_prevent_false_unique_resolution() {
        let a = analyze(&Inputs {
            files: vec![
                (
                    "x.rs".into(),
                    None,
                    Some("fn caller() { helper(); }".into()),
                ),
                ("y.rs".into(), None, Some("fn helper() {}".into())),
            ],
            extra_head: vec![("u.rs".into(), "fn helper() {}".into())],
            include_tests: false,
        });
        let caller = &a.nodes[&node_key("x.rs", "caller")];
        assert!(caller.callees.is_empty());
        assert_eq!(caller.lost_callees, vec!["helper"]);
    }

    #[test]
    fn unchanged_local_helpers_shadow_changed_definitions() {
        let a = analyze(&Inputs {
            files: vec![("changed.rs".into(), None, Some("fn init() {}".into()))],
            extra_head: vec![("u.rs".into(), "fn top() { init(); }\nfn init() {}".into())],
            include_tests: false,
        });
        assert!(a.nodes[&node_key("changed.rs", "init")]
            .external_callers
            .is_empty());
        assert!(!a.file_labels.contains_key("u.rs"));
    }

    #[test]
    fn resolution_respects_language_families() {
        let a = analyze(&Inputs {
            files: vec![
                (
                    "x.py".into(),
                    None,
                    Some("def caller():\n    helper()\n".into()),
                ),
                ("y.rs".into(), None, Some("fn helper() {}".into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        let caller = &a.nodes[&node_key("x.py", "caller")];
        assert!(caller.callees.is_empty());
        assert_eq!(caller.lost_callees, vec!["helper"]);
        let a = analyze(&Inputs {
            files: vec![(
                "helper.ts".into(),
                None,
                Some("function helper() {}".into()),
            )],
            extra_head: vec![("caller.js".into(), "function caller() { helper(); }".into())],
            include_tests: false,
        });
        assert_eq!(
            a.nodes[&node_key("helper.ts", "helper")].external_callers,
            vec!["caller.js::caller"]
        );
    }

    #[test]
    fn live_unchanged_definition_does_not_fall_back_to_removed_node() {
        let a = analyze(&Inputs {
            files: vec![
                (
                    "caller.rs".into(),
                    None,
                    Some("fn caller() { helper(); }".into()),
                ),
                ("deleted.rs".into(), Some("fn helper() {}".into()), None),
            ],
            extra_head: vec![("u.rs".into(), "fn helper() {}".into())],
            include_tests: false,
        });
        let caller = &a.nodes[&node_key("caller.rs", "caller")];
        assert!(caller.callees.is_empty());
        assert_eq!(caller.lost_callees, vec!["helper"]);
    }

    #[test]
    fn absent_head_definition_falls_back_to_unique_removed_node() {
        let base = "fn caller() { helper(); }\nfn helper() {}\n";
        let head = "fn caller() { helper(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(
            a.nodes[&node_key("m.rs", "caller")].callees,
            vec![node_key("m.rs", "helper")]
        );
        assert_eq!(a.nodes[&node_key("m.rs", "helper")].status, Status::Removed);
    }

    #[test]
    fn local_overloads_resolve_as_a_set_and_edges_are_deduplicated() {
        let head = "class Worker { void Caller() { Log(1); Log(2); } void Log(int x) {} void Log(string s) {} }";
        let a = analyze_pair("Worker.cs", None, Some(head));
        let caller = &a.nodes[&node_key("Worker.cs", "Worker::Caller")];
        assert_eq!(
            caller.callees,
            vec![
                node_key("Worker.cs", "Worker::Log(int x)"),
                node_key("Worker.cs", "Worker::Log(string s)"),
            ]
        );
        for callee in &caller.callees {
            assert_eq!(a.nodes[callee].callers, vec![caller.key.clone()]);
        }
    }

    #[test]
    fn spans_and_bodies_use_their_own_revision() {
        let base = "fn sample() {\r\n    old_line();\r\n}\r\n";
        let head = "// inserted\n// inserted\n// inserted\nfn sample() {\n    new_line();\n}\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        let sample = &a.nodes[&node_key("m.rs", "sample")];
        assert_eq!(sample.status, Status::BodyChanged);
        assert_eq!(sample.base.as_ref().unwrap().start_row, 0);
        assert_eq!(sample.head.as_ref().unwrap().start_row, 3);
        assert_eq!(
            sample.head.as_ref().unwrap().end_row - sample.base.as_ref().unwrap().end_row,
            3
        );
        assert_eq!(
            a.base_body(sample).as_deref(),
            Some("fn sample() {\n    old_line();\n}")
        );
        assert_eq!(
            a.head_body(sample).as_deref(),
            Some("fn sample() {\n    new_line();\n}")
        );
        assert_eq!(a.base_srcs.len(), 1);
        assert_eq!(a.head_srcs.len(), 1);
    }

    #[test]
    fn move_preference_and_origin_spans_are_deterministic() {
        let inputs = Inputs {
            files: vec![
                (
                    "z.rs".into(),
                    Some("fn local() { same(); }".into()),
                    Some("fn beta() { same(); }".into()),
                ),
                ("a.rs".into(), Some("fn other() { same(); }".into()), None),
                ("b.rs".into(), None, Some("fn alpha() { same(); }".into())),
            ],
            extra_head: vec![],
            include_tests: false,
        };
        for _ in 0..25 {
            let a = analyze(&inputs);
            let alpha = &a.nodes[&node_key("b.rs", "alpha")];
            let beta = &a.nodes[&node_key("z.rs", "beta")];
            assert_eq!(alpha.moved_from.as_deref(), Some("other"));
            assert_eq!(alpha.base.as_ref().unwrap().file, "a.rs");
            assert_eq!(beta.moved_from.as_deref(), Some("local"));
            assert_eq!(beta.base.as_ref().unwrap().file, "z.rs");
            assert_eq!(a.summary, (0, 0, 0, 2));
        }
    }

    #[test]
    fn move_prefers_same_file_over_a_lower_key() {
        let a = analyze(&Inputs {
            files: vec![
                ("a.rs".into(), Some("fn other() { same(); }".into()), None),
                (
                    "z.rs".into(),
                    Some("fn local() { same(); }".into()),
                    Some("fn beta() { same(); }".into()),
                ),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        let beta = &a.nodes[&node_key("z.rs", "beta")];
        assert_eq!(beta.base.as_ref().unwrap().file, "z.rs");
        assert_eq!(beta.moved_from.as_deref(), Some("local"));
        assert_eq!(a.nodes[&node_key("a.rs", "other")].status, Status::Removed);
    }

    #[test]
    fn moves_never_cross_language_families() {
        let base = "class Worker { void alpha() {} };";
        let head = "class Worker { void beta() {} }";
        let base_defs = syntax::parse_file("base.cpp", base).unwrap();
        let head_defs = syntax::parse_file("head.cs", head).unwrap();
        assert_eq!(base_defs.fns[0].body_hash, head_defs.fns[0].body_hash);
        let a = analyze(&Inputs {
            files: vec![
                ("base.cpp".into(), Some(base.into()), None),
                ("head.cs".into(), None, Some(head.into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        assert_eq!(a.summary, (1, 1, 0, 0));
        assert_eq!(
            a.nodes[&node_key("base.cpp", "Worker::alpha")].status,
            Status::Removed
        );
        assert_eq!(
            a.nodes[&node_key("head.cs", "Worker::beta")].status,
            Status::Added
        );
    }

    #[test]
    fn removed_calls_resolve_base_definitions_to_moved_nodes() {
        let base = "fn gone() { helper(); extra(); }\nfn helper() { stable(); }\n";
        let head = "fn renamed() { stable(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        let renamed = &a.nodes[&node_key("m.rs", "renamed")];
        assert_eq!(renamed.status, Status::Moved);
        assert_eq!(renamed.moved_from.as_deref(), Some("helper"));
        let gone = &a.nodes[&node_key("m.rs", "gone")];
        assert_eq!(gone.callees, vec![renamed.key.clone()]);
        assert_eq!(gone.lost_callees, vec!["extra"]);
        assert_eq!(renamed.callers, vec![gone.key.clone()]);
    }

    #[test]
    fn labels_count_distinct_paths_not_functions() {
        let a = analyze_pair("lib.rs", None, Some("fn a() {}\nfn b() {}\n"));
        assert_eq!(a.label("lib.rs"), "lib.rs");
        let a = analyze(&Inputs {
            files: vec![
                (
                    "crates/a/lib.rs".into(),
                    None,
                    Some("fn a() {}\nfn b() {}".into()),
                ),
                ("crates/b/lib.rs".into(), None, Some("fn c() {}".into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        assert_eq!(a.label("crates/a/lib.rs"), "a/lib.rs");
        assert_eq!(a.label("crates/b/lib.rs"), "b/lib.rs");
        assert_eq!(a.label("unknown.rs"), "unknown.rs");
        let labels = file_labels(HashSet::from([
            "lib.rs".into(),
            "a/lib.rs".into(),
            "x/a/lib.rs".into(),
        ]));
        assert_eq!(labels["lib.rs"], "lib.rs");
        assert_eq!(labels["a/lib.rs"], "a/lib.rs");
        assert_eq!(labels["x/a/lib.rs"], "x/a/lib.rs");
    }

    #[test]
    fn test_definitions_are_filtered_from_both_universes() {
        let inputs = Inputs {
            files: vec![(
                "m.rs".into(),
                Some("#[cfg(test)] mod tests { fn deleted() {} }".into()),
                Some("fn caller() { helper(); }\nfn helper() {}".into()),
            )],
            extra_head: vec![(
                "u.rs".into(),
                "#[cfg(test)] mod tests { fn helper() {} }".into(),
            )],
            include_tests: false,
        };
        let a = analyze(&inputs);
        assert_eq!(a.nodes.len(), 2);
        assert_eq!(a.summary.1, 0);
        assert_eq!(
            a.nodes[&node_key("m.rs", "caller")].callees,
            vec![node_key("m.rs", "helper")]
        );
    }
}
