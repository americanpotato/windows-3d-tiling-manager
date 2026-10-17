//! Workflows: named snapshots of the arrangement (card places, sizes and button states, and the
//! template slots), saved as JSON files in a folder tree under `%APPDATA%\Window3D\workflows`.
//! Loading one puts every card back in its saved place and starts the apps that are missing.
//! Several can be open at once; the open ones are listed in `layout.json`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Model;
use crate::store::{self, Entry, Kind, OpenWorkflow, Slot};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Save {
    All,
    Windows,
    Templates,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WfCard {
    pub kind: Kind,
    pub exe: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    pub pos: [f32; 3],
    pub yaw: f32,
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub sound: bool,
    #[serde(default = "crate::store::yes")]
    pub snap: bool,
    #[serde(default)]
    pub toolbar: Option<[f32; 2]>,
}

fn one() -> f32 {
    1.0
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Workflow {
    #[serde(default)]
    pub cards: Vec<WfCard>,
    #[serde(default)]
    pub slots: Vec<Slot>,
}

pub enum Node {
    Folder(String, Vec<Node>),
    File(String, PathBuf),
}

pub fn dir() -> PathBuf {
    store::app_dir().join("workflows")
}

/// Turns a typed name like `work\mail` into a file path inside the workflows folder (folders
/// are created). Characters Windows does not allow in names are replaced.
pub fn path_for(name: &str) -> Option<PathBuf> {
    let mut p = dir();
    let parts: Vec<String> = name
        .split(['\\', '/'])
        .map(|s| {
            s.trim()
                .chars()
                .map(|c| {
                    if ":*?\"<>|".contains(c) || c < ' ' {
                        '_'
                    } else {
                        c
                    }
                })
                .collect::<String>()
        })
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .collect();
    let (file, folders) = parts.split_last()?;
    for f in folders {
        p.push(f);
    }
    p.push(format!("{file}.json"));
    Some(p)
}

/// The workflow name shown for a file: its path inside the workflows folder, without `.json`.
pub fn name_of(path: &Path) -> String {
    let rel = path.strip_prefix(dir()).unwrap_or(path);
    rel.with_extension("").to_string_lossy().into_owned()
}

/// Snapshot of the current arrangement: the cards of open windows and the shortcuts (unless
/// `Templates`), and the template slots (unless `Windows`).
pub fn snapshot(model: &Model, what: Save) -> Workflow {
    let bound: HashSet<u64> = model.bindings.values().copied().collect();
    let cards = if what == Save::Templates {
        Vec::new()
    } else {
        model
            .layout
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Shortcut || bound.contains(&e.id))
            .map(|e| WfCard {
                kind: e.kind,
                exe: e.exe.clone(),
                class: e.class.clone(),
                title: e.title.clone(),
                pos: e.pos,
                yaw: e.yaw,
                scale: e.scale,
                live: e.live,
                sound: e.sound,
                snap: e.snap,
                toolbar: e.toolbar,
            })
            .collect()
    };
    let slots = if what == Save::Windows {
        Vec::new()
    } else {
        model
            .layout
            .slots
            .iter()
            .map(|s| Slot {
                wf: String::new(),
                ..s.clone()
            })
            .collect()
    };
    Workflow { cards, slots }
}

pub fn save(model: &Model, path: &Path, what: Save) -> Result<(), String> {
    let wf = snapshot(model, what);
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let s = serde_json::to_string_pretty(&wf).map_err(|e| e.to_string())?;
    fs::write(path, s).map_err(|e| e.to_string())
}

/// The saved workflows, folders first, both sorted by name.
pub fn tree() -> Vec<Node> {
    fn walk(d: &Path) -> Vec<Node> {
        let Ok(rd) = fs::read_dir(d) else {
            return Vec::new();
        };
        let (mut folders, mut files) = (Vec::new(), Vec::new());
        for e in rd.flatten() {
            let p = e.path();
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if p.is_dir() {
                folders.push(Node::Folder(name, walk(&p)));
            } else if p
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
            {
                files.push(Node::File(name, p));
            }
        }
        let key = |n: &Node| match n {
            Node::Folder(s, _) | Node::File(s, _) => s.to_lowercase(),
        };
        folders.sort_by_key(key);
        files.sort_by_key(key);
        folders.extend(files);
        folders
    }
    walk(&dir())
}

/// Opens workflow `path`: moves the matching cards to their saved places (creating ghost cards
/// for apps that are not running), adds its template slots and lists it as open. Returns the
/// programs to start.
pub fn load(model: &mut Model, path: &Path) -> Result<Vec<String>, String> {
    let s = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let wf: Workflow = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    let name = name_of(path);
    close(model, &name);
    let bound: HashSet<u64> = model.bindings.values().copied().collect();
    let mut used = HashSet::new();
    let mut ids = Vec::new();
    let mut launch = Vec::new();
    let low = |s: &str| s.to_lowercase();
    for c in &wf.cards {
        let fits = |e: &Entry| {
            e.kind == c.kind
                && !used.contains(&e.id)
                && low(&e.exe) == low(&c.exe)
                && (c.kind == Kind::Shortcut || e.class == c.class)
        };
        let es = &model.layout.entries;
        // Prefer a running window with the same title, then any running one, then a ghost.
        let found = es
            .iter()
            .find(|e| fits(e) && bound.contains(&e.id) && e.title == c.title)
            .or_else(|| es.iter().find(|e| fits(e) && bound.contains(&e.id)))
            .or_else(|| es.iter().find(|e| fits(e) && e.title == c.title))
            .or_else(|| es.iter().find(|e| fits(e)))
            .map(|e| e.id);
        let id = match found {
            Some(id) => id,
            None if c.kind == Kind::Shortcut => continue,
            None => {
                let id = model.layout.next_id;
                model.layout.next_id += 1;
                model.layout.entries.push(Entry {
                    id,
                    kind: c.kind,
                    exe: c.exe.clone(),
                    class: c.class.clone(),
                    title: c.title.clone(),
                    pos: c.pos,
                    yaw: c.yaw,
                    last_seen: store::now_secs(),
                    live: false,
                    sound: false,
                    scale: 1.0,
                    snap: true,
                    toolbar: None,
                });
                id
            }
        };
        used.insert(id);
        ids.push(id);
        if c.kind == Kind::Window && !bound.contains(&id) && !launch.contains(&c.exe) {
            launch.push(c.exe.clone());
        }
        if let Some(e) = model.get_mut(id) {
            e.pos = c.pos;
            e.yaw = c.yaw;
            e.scale = c.scale;
            e.live = c.live;
            e.sound = c.sound;
            e.snap = c.snap;
            e.toolbar = c.toolbar;
            // A fresh ghost must win when the app's window shows up.
            e.last_seen = e.last_seen.max(store::now_secs());
        }
    }
    model
        .layout
        .slots
        .extend(wf.slots.into_iter().map(|s| Slot {
            wf: name.clone(),
            ..s
        }));
    model.layout.workflows.push(OpenWorkflow { name, ids });
    Ok(launch)
}

/// Closes open workflow `name`: removes its template slots and forgets the grouping. Cards
/// and windows stay where they are.
pub fn close(model: &mut Model, name: &str) {
    model.layout.slots.retain(|s| s.wf != name);
    model.layout.workflows.retain(|w| w.name != name);
}

/// Saves a workflow, moves a card away, loads the workflow and checks the card came back.
/// Leaves the layout as it was (the test file stays in the test data folder).
pub fn selftest(model: &mut Model) -> String {
    let Some(path) = path_for("selftest/demo") else {
        return "workflow test: bad name".into();
    };
    let bound: HashSet<u64> = model.bindings.values().copied().collect();
    let Some(id) = model
        .layout
        .entries
        .iter()
        .find(|e| bound.contains(&e.id))
        .map(|e| e.id)
    else {
        return "workflow test: no open windows".into();
    };
    let Some(before) = model.get(id).map(|e| e.pos) else {
        return "workflow test: no entry".into();
    };
    let saved = save(model, &path, Save::All);
    if let Some(e) = model.get_mut(id) {
        e.pos[0] += 3.0;
    }
    let listed = tree()
        .iter()
        .any(|n| matches!(n, Node::Folder(f, kids) if f == "selftest" && kids.len() == 1));
    let (n_slots, n_entries) = (model.layout.slots.len(), model.layout.entries.len());
    let res = load(model, &path);
    let back = model.get(id).is_some_and(|e| e.pos == before);
    let open = model.layout.workflows.len();
    close(model, &name_of(&path));
    let closed = model.layout.workflows.is_empty() && model.layout.slots.len() == n_slots;
    format!(
        "workflow test: saved={} listed={listed} loaded={} restored={back} open={open} closed={closed} new-entries={}",
        saved.is_ok(),
        res.map_or_else(|e| e, |l| format!("ok (launch {})", l.len())),
        model.layout.entries.len() - n_entries
    )
}
