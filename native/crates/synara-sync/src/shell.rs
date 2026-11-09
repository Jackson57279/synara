//! Sidebar reducer for `orchestration.subscribeShell`.
//!
//! Upserts replace whole rows, so this needs no event semantics: only cursor
//! discipline and tombstones.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use synara_protocol::ids::{ProjectId, SpaceId, ThreadId};
use synara_protocol::orchestration::{
    ProjectShell, ShellSnapshot, ShellStreamItem, SpaceShell, ThreadShell,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellChange {
    pub reset: bool,
    pub threads: BTreeSet<ThreadId>,
    pub projects: bool,
    pub spaces: bool,
}

impl ShellChange {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One project row and its visible top-level threads, in display order.
#[derive(Debug, Clone, PartialEq)]
pub struct SidebarProject<'a> {
    pub project: &'a ProjectShell,
    pub threads: Vec<&'a ThreadShell>,
}

#[derive(Debug, Clone, Default)]
pub struct ShellStore {
    cursor: Option<u64>,
    snapshot_loaded: bool,
    spaces: Vec<SpaceShell>,
    projects: BTreeMap<ProjectId, ProjectShell>,
    threads: BTreeMap<ThreadId, ThreadShell>,
    thread_tombstones: HashMap<ThreadId, u64>,
    project_tombstones: HashMap<ProjectId, u64>,
    space_tombstones: HashMap<SpaceId, u64>,
}

impl ShellStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    pub fn is_loaded(&self) -> bool {
        self.snapshot_loaded
    }

    pub fn spaces(&self) -> &[SpaceShell] {
        &self.spaces
    }

    pub fn project(&self, id: &ProjectId) -> Option<&ProjectShell> {
        self.projects.get(id)
    }

    pub fn thread(&self, id: &ThreadId) -> Option<&ThreadShell> {
        self.threads.get(id)
    }

    pub fn projects(&self) -> impl Iterator<Item = &ProjectShell> {
        self.projects.values()
    }

    pub fn threads(&self) -> impl Iterator<Item = &ThreadShell> {
        self.threads.values()
    }

    /// The server instance changed: everything must come from a new snapshot.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn apply(&mut self, item: ShellStreamItem) -> ShellChange {
        if let Some(sequence) = item.sequence() {
            if self.cursor.is_some_and(|cursor| sequence <= cursor) {
                return ShellChange::default();
            }
        }
        let mut change = ShellChange::default();
        match item {
            ShellStreamItem::Snapshot { snapshot } => return self.apply_snapshot(*snapshot),
            ShellStreamItem::SpaceUpserted { sequence, space } => {
                self.space_tombstones.remove(&space.id);
                match self.spaces.iter_mut().find(|s| s.id == space.id) {
                    Some(existing) => *existing = space,
                    None => self.spaces.push(space),
                }
                self.sort_spaces();
                self.cursor = Some(sequence);
                change.spaces = true;
            }
            ShellStreamItem::SpaceRemoved {
                sequence, space_id, ..
            } => {
                self.spaces.retain(|s| s.id != space_id);
                self.space_tombstones.insert(space_id, sequence);
                self.cursor = Some(sequence);
                change.spaces = true;
            }
            ShellStreamItem::SpaceOrderUpdated {
                sequence,
                ordered_space_ids,
            } => {
                for (index, id) in ordered_space_ids.iter().enumerate() {
                    if let Some(space) = self.spaces.iter_mut().find(|s| &s.id == id) {
                        space.sort_order = index as u64;
                    }
                }
                self.sort_spaces();
                self.cursor = Some(sequence);
                change.spaces = true;
            }
            ShellStreamItem::ProjectUpserted { sequence, project } => {
                self.project_tombstones.remove(&project.id);
                self.projects.insert(project.id.clone(), *project);
                self.cursor = Some(sequence);
                change.projects = true;
            }
            ShellStreamItem::ProjectRemoved {
                sequence,
                project_id,
            } => {
                self.projects.remove(&project_id);
                self.project_tombstones.insert(project_id, sequence);
                self.cursor = Some(sequence);
                change.projects = true;
            }
            ShellStreamItem::ThreadUpserted { sequence, thread } => {
                self.thread_tombstones.remove(&thread.id);
                change.threads.insert(thread.id.clone());
                self.threads.insert(thread.id.clone(), *thread);
                self.cursor = Some(sequence);
            }
            ShellStreamItem::ThreadRemoved {
                sequence,
                thread_id,
            } => {
                self.threads.remove(&thread_id);
                change.threads.insert(thread_id.clone());
                self.thread_tombstones.insert(thread_id, sequence);
                self.cursor = Some(sequence);
            }
            ShellStreamItem::Unknown => {}
        }
        change
    }

    fn apply_snapshot(&mut self, snapshot: ShellSnapshot) -> ShellChange {
        let at = snapshot.snapshot_sequence;
        self.spaces = snapshot.spaces;
        self.projects = snapshot
            .projects
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        self.threads = snapshot
            .threads
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        // A removal newer than the snapshot wins; older ones the snapshot already reflects.
        self.thread_tombstones
            .retain(|_, removed_at| *removed_at > at);
        self.project_tombstones
            .retain(|_, removed_at| *removed_at > at);
        self.space_tombstones
            .retain(|_, removed_at| *removed_at > at);
        for id in self.thread_tombstones.keys() {
            self.threads.remove(id);
        }
        for id in self.project_tombstones.keys() {
            self.projects.remove(id);
        }
        self.spaces
            .retain(|s| !self.space_tombstones.contains_key(&s.id));
        self.sort_spaces();
        self.cursor = Some(at);
        self.snapshot_loaded = true;
        ShellChange {
            reset: true,
            ..ShellChange::default()
        }
    }

    fn sort_spaces(&mut self) {
        self.spaces.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    /// Projects in the given space (`None` = every space) with their visible
    /// top-level threads. Pinned first, then most recent activity.
    pub fn sidebar(&self, space: Option<&SpaceId>) -> Vec<SidebarProject<'_>> {
        let mut rows: Vec<SidebarProject<'_>> = self
            .projects
            .values()
            .filter(|p| space.is_none_or(|space| p.space_id.as_ref() == Some(space)))
            .map(|project| {
                let mut threads: Vec<&ThreadShell> = self
                    .threads
                    .values()
                    .filter(|t| {
                        t.project_id == project.id
                            && !t.is_archived()
                            && t.parent_thread_id.is_none()
                    })
                    .collect();
                threads.sort_by(|a, b| {
                    b.is_pinned
                        .cmp(&a.is_pinned)
                        .then_with(|| activity_key(b).cmp(activity_key(a)))
                        .then_with(|| a.id.cmp(&b.id))
                });
                SidebarProject { project, threads }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.project
                .is_pinned
                .cmp(&a.project.is_pinned)
                .then_with(|| project_activity(b).cmp(project_activity(a)))
                .then_with(|| a.project.title.cmp(&b.project.title))
        });
        rows
    }
}

fn activity_key(thread: &ThreadShell) -> &str {
    thread
        .latest_user_message_at
        .as_deref()
        .filter(|at| *at > thread.created_at.as_str())
        .unwrap_or(&thread.created_at)
}

fn project_activity<'a>(row: &SidebarProject<'a>) -> &'a str {
    row.threads
        .iter()
        .map(|t| activity_key(t))
        .max()
        .unwrap_or(row.project.updated_at.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn thread(id: &str, project: &str, created: &str) -> Value {
        json!({
            "id": id, "projectId": project, "title": id,
            "modelSelection": {"provider": "codex", "instanceId": "codex", "model": "gpt-5.5"},
            "runtimeMode": "full-access", "branch": null, "worktreePath": null, "latestTurn": null,
            "createdAt": created, "updatedAt": created, "session": null
        })
    }

    fn project(id: &str, title: &str) -> Value {
        json!({"id": id, "title": title, "workspaceRoot": format!("/w/{id}"), "defaultModelSelection": null,
               "scripts": [], "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z"})
    }

    fn item(value: Value) -> ShellStreamItem {
        serde_json::from_value(value).unwrap()
    }

    fn snapshot(sequence: u64, threads: Vec<Value>) -> ShellStreamItem {
        item(json!({"kind": "snapshot", "snapshot": {
            "snapshotSequence": sequence, "spaces": [],
            "projects": [project("p-1", "Alpha"), project("p-2", "Beta")],
            "threads": threads, "updatedAt": "x"
        }}))
    }

    #[test]
    fn snapshot_then_upserts_and_removals() {
        let mut store = ShellStore::new();
        store.apply(snapshot(
            5,
            vec![thread("t-1", "p-1", "2026-01-02T00:00:00.000Z")],
        ));
        assert!(store.is_loaded());
        assert_eq!(store.cursor(), Some(5));

        let change = store.apply(item(json!({"kind": "thread-upserted", "sequence": 6, "thread": thread("t-2", "p-1", "2026-01-03T00:00:00.000Z")})));
        assert!(change.threads.contains(&ThreadId::from("t-2")));
        let rows = store.sidebar(None);
        assert_eq!(rows[0].project.title, "Alpha");
        assert_eq!(
            rows[0]
                .threads
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["t-2", "t-1"]
        );

        store.apply(item(
            json!({"kind": "thread-removed", "sequence": 7, "threadId": "t-2"}),
        ));
        assert!(store.thread(&"t-2".into()).is_none());
        // Stale upsert at or below the cursor is dropped.
        assert!(store
            .apply(item(json!({"kind": "thread-upserted", "sequence": 7, "thread": thread("t-2", "p-1", "x")})))
            .is_empty());
    }

    #[test]
    fn tombstone_beats_older_snapshot() {
        let mut store = ShellStore::new();
        store.apply(snapshot(5, vec![thread("t-1", "p-1", "a")]));
        store.apply(item(
            json!({"kind": "thread-removed", "sequence": 9, "threadId": "t-1"}),
        ));
        // A snapshot taken before the removal still lists the thread.
        store.apply(snapshot(8, vec![thread("t-1", "p-1", "a")]));
        assert!(store.thread(&"t-1".into()).is_none());
        // A newer snapshot is authoritative and clears the tombstone.
        store.apply(snapshot(10, vec![thread("t-1", "p-1", "a")]));
        assert!(store.thread(&"t-1".into()).is_some());
    }

    #[test]
    fn archived_and_child_threads_are_hidden() {
        let mut archived = thread("t-a", "p-1", "a");
        archived["archivedAt"] = json!("2026-01-05T00:00:00.000Z");
        let mut child = thread("t-c", "p-1", "a");
        child["parentThreadId"] = json!("t-1");
        let mut store = ShellStore::new();
        store.apply(snapshot(
            1,
            vec![thread("t-1", "p-1", "a"), archived, child],
        ));
        let rows = store.sidebar(None);
        let alpha = rows
            .iter()
            .find(|r| r.project.id.as_str() == "p-1")
            .unwrap();
        assert_eq!(alpha.threads.len(), 1);
    }

    #[test]
    fn space_order_and_reset() {
        let mut store = ShellStore::new();
        store.apply(snapshot(1, vec![]));
        for (seq, id) in [(2, "s-a"), (3, "s-b")] {
            store.apply(item(json!({"kind": "space-upserted", "sequence": seq, "space": {
                "id": id, "name": id, "icon": "folder", "sortOrder": seq, "createdAt": "x", "updatedAt": "x"}})));
        }
        store.apply(item(json!({"kind": "space-order-updated", "sequence": 4, "orderedSpaceIds": ["s-b", "s-a"]})));
        assert_eq!(store.spaces()[0].id.as_str(), "s-b");
        store.reset();
        assert_eq!(store.cursor(), None);
        assert!(!store.is_loaded());
    }
}
