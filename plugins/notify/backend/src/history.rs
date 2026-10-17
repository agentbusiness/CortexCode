//! The recent notifications with their delivery state, and dedup by key.
//!
//! A notification whose key matches an item posted or updated within the dedup window updates that
//! item (same id, new content, `updates` + 1) instead of adding one. The list keeps the most recent
//! [`CAPACITY`] items; it is persisted in the plugin's data directory and handed to a successor
//! version on hot-swap.

use cc_protocol::contracts::notify::{Notification, Posted, delivery};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// How many recent notifications are kept (a recent list for the panel, not an archive).
pub const CAPACITY: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub enum Recorded {
    /// A new item.
    New(Posted),
    /// An existing item within the dedup window was updated. `previous` is its state before.
    Updated { posted: Posted, previous: Box<Posted> },
}

impl Recorded {
    pub fn posted(&self) -> &Posted {
        match self {
            Recorded::New(posted) | Recorded::Updated { posted, .. } => posted,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct History {
    /// Oldest first.
    items: VecDeque<Posted>,
}

/// Whether two notifications would look different to the user.
pub fn same_content(a: &Notification, b: &Notification) -> bool {
    a.kind == b.kind && a.title == b.title && a.subtitle == b.subtitle && a.body == b.body && a.actions == b.actions
}

impl History {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Record a notification: update the item with the same key within `window_ms`, or add one.
    pub fn record(&mut self, notification: Notification, source: &str, now_ms: u64, window_ms: u64) -> Recorded {
        if let Some(key) = notification.key.as_deref()
            && let Some(item) = self
                .items
                .iter_mut()
                .rev()
                .find(|item| item.notification.key.as_deref() == Some(key) && now_ms.saturating_sub(last_seen(item)) <= window_ms)
        {
            let previous = Box::new(item.clone());
            item.notification = notification;
            item.updates += 1;
            item.updated_ms = now_ms;
            if !source.is_empty() {
                item.source = source.to_owned();
            }
            return Recorded::Updated { posted: item.clone(), previous };
        }
        let posted = Posted {
            id: uuid::Uuid::new_v4().simple().to_string(),
            notification,
            ts_ms: now_ms,
            delivery: delivery::PENDING.into(),
            detail: String::new(),
            source: source.to_owned(),
            method: String::new(),
            updates: 0,
            updated_ms: now_ms,
            activated_ms: None,
            activated_action: None,
        };
        self.items.push_back(posted.clone());
        while self.items.len() > CAPACITY {
            self.items.pop_front();
        }
        Recorded::New(posted)
    }

    pub fn get(&self, id: &str) -> Option<&Posted> {
        self.items.iter().find(|item| item.id == id)
    }

    /// Set an item's delivery state.
    pub fn set_delivery(&mut self, id: &str, state: &str, method: &str, detail: &str) -> Option<Posted> {
        let item = self.items.iter_mut().find(|item| item.id == id)?;
        item.delivery = state.to_owned();
        item.method = method.to_owned();
        item.detail = detail.to_owned();
        Some(item.clone())
    }

    /// Mark an item activated by the user.
    pub fn activate(&mut self, id: &str, action: Option<&str>, now_ms: u64) -> Option<Posted> {
        let item = self.items.iter_mut().find(|item| item.id == id)?;
        item.activated_ms = Some(now_ms);
        item.activated_action = action.map(str::to_owned);
        Some(item.clone())
    }

    /// Shown in the CortexCode window instead of macOS: delivery `ui` by `method`, not seen yet
    /// (a changed repeat marks the session again).
    pub fn mark_in_app(&mut self, id: &str, method: &str, detail: &str) -> Option<Posted> {
        let item = self.items.iter_mut().find(|item| item.id == id)?;
        item.delivery = delivery::UI.to_owned();
        item.method = method.to_owned();
        item.detail = detail.to_owned();
        item.activated_ms = None;
        item.activated_action = None;
        Some(item.clone())
    }

    /// The user opened `session`: its items that mark it in the session list ([`marks_session`])
    /// are seen now. Returns the items that changed.
    pub fn viewed(&mut self, session: &str, method: &str, now_ms: u64) -> Vec<Posted> {
        let mut changed = Vec::new();
        for item in self.items.iter_mut() {
            if marks_session(item, method) && item.notification.session_id.as_deref() == Some(session) {
                item.activated_ms = Some(now_ms);
                item.activated_action = Some("viewed".into());
                changed.push(item.clone());
            }
        }
        changed
    }

    /// Newest first, at most `limit`.
    pub fn list(&self, limit: usize) -> Vec<Posted> {
        self.items.iter().rev().take(limit).cloned().collect()
    }

    /// Merge items from a predecessor or the persisted file (ids already present are kept as is).
    pub fn merge(&mut self, other: History) {
        for item in other.items {
            if self.get(&item.id).is_none() {
                self.items.push_back(item);
            }
        }
        self.items.make_contiguous().sort_by_key(|item| item.ts_ms);
        while self.items.len() > CAPACITY {
            self.items.pop_front();
        }
    }
}

/// Whether `item` marks its session in the session list: it has a session, was not seen yet, and
/// reached the user only inside CortexCode — shown in the window (`ui` by `in_app_method`) because
/// the window was in front, or not shown by macOS at all (`failed`). The session list's rule is the
/// same (plugins/sidebar).
pub fn marks_session(item: &Posted, in_app_method: &str) -> bool {
    item.notification.session_id.is_some()
        && item.activated_ms.is_none()
        && ((item.delivery == delivery::UI && item.method == in_app_method) || item.delivery == delivery::FAILED)
}

fn last_seen(item: &Posted) -> u64 {
    item.updated_ms.max(item.ts_ms)
}
