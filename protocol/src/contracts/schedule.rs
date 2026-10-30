//! `schedule@1`: routines, recurring and timed work (docs/SCHEDULES-PLAN.md). A routine wakes a
//! session, hands a brief to an agent, or runs a command, on a clock. The scheduler plugin keeps
//! the clock, fires what is due, and ends routines that no longer serve anything; memory keeps the
//! records ([`crate::contracts::memory::Routine`]).

use crate::contracts::memory::{Routine, RoutineRun};
use serde::{Deserialize, Serialize};

pub mod methods {
    /// [`super::AddParams`] -> [`super::RoutineAnswer`]: a routine, started at once. A duplicate of
    /// an active one (same scope, kind, session and brief or command) returns that one.
    pub const ADD: &str = "schedule.add";
    /// [`super::EditParams`] -> [`super::RoutineAnswer`].
    pub const EDIT: &str = "schedule.edit";
    /// [`super::IdParams`] -> [`super::RoutineAnswer`].
    pub const PAUSE: &str = "schedule.pause";
    pub const RESUME: &str = "schedule.resume";
    pub const REMOVE: &str = "schedule.remove";
    /// [`super::IdParams`] -> [`super::RoutineAnswer`]: fire once now; the schedule is unchanged.
    pub const RUN_NOW: &str = "schedule.run_now";
    /// [`super::ListParams`] -> [`super::ListAnswer`].
    pub const LIST: &str = "schedule.list";
    /// [`super::IdParams`] -> [`super::RoutineAnswer`] with its last runs.
    pub const GET: &str = "schedule.get";
    /// `{id, limit?}` -> `{runs}`.
    pub const RUNS: &str = "schedule.runs";
}

pub mod topics {
    /// `{routine, change}` after any change (`added`, `edited`, `paused`, `resumed`, `ended`,
    /// `removed`, `fired`).
    pub const CHANGED: &str = "schedule.changed";
    /// A [`super::Fired`]: a wake routine is due. The chat flow tells the session and, when it is
    /// idle, wakes it.
    pub const FIRED: &str = "schedule.fired";
    /// `{routine, run}` when a run ended (done, failed, skipped).
    pub const RAN: &str = "schedule.ran";
}

/// Published on [`topics::FIRED`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fired {
    /// `<routine id>:<due ms>`: whichever chat flow instance hears it first tells the session, once.
    pub id: String,
    pub routine_id: String,
    pub session_id: String,
    pub project_id: String,
    pub title: String,
    /// The schedule as written.
    pub when: String,
    /// What to do now (the routine's brief, or a failed command's output).
    pub text: String,
    /// An agent check-in: the session decides in MANAGE.
    #[serde(default)]
    pub manage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AddParams {
    /// A project id or `global` (absent: the session's project).
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// wake (default) | handoff | command
    #[serde(default)]
    pub kind: Option<String>,
    /// When it runs, in words: `every 5m`, `every mon at 09:00`, `once at 2026-11-02 09:00`.
    pub when: String,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    /// The session that asked: a wake goes to it; the routine's mode is its mode.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The model run that asked (culling at the run's end).
    #[serde(default)]
    pub run_id: Option<String>,
    /// Task ids the routine exists for.
    #[serde(default)]
    pub watches: Vec<String>,
    /// `done` (end when the watched work is finished) or a date (`2026-12-01`, `2026-12-01 18:00`).
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub max_runs: Option<u32>,
    #[serde(default)]
    pub manage: bool,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub flow: Option<String>,
    /// model (default) | user | agents
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct EditParams {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    /// `done`, a date, or empty to clear.
    #[serde(default)]
    pub until: Option<String>,
    /// 0 clears.
    #[serde(default)]
    pub max_runs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ListParams {
    /// A project id or `global`; absent: every scope.
    #[serde(default)]
    pub scope: Option<String>,
    /// With `scope` a project: also the global routines.
    #[serde(default)]
    pub with_global: bool,
}

/// A routine and the line the model reads about it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutineAnswer {
    pub routine: Routine,
    /// The next times it fires (ms), up to three.
    #[serde(default)]
    pub next: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<RoutineRun>,
    /// `add`: an active routine for the same thing was there already.
    #[serde(default)]
    pub existing: bool,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ListAnswer {
    pub routines: Vec<Routine>,
    pub text: String,
}
