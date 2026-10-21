//! CortexCode memory plugin (`memory@1`): an IronGraph code graph per repository (one embedded
//! IronGraph instance per process, exclusive owner of `<CORTEX_HOME>/irongraph`), a line store
//! outside the graph, persistent work state (tasks, board, agents, rules, style, runs), cited
//! notes and project knowledge (concepts, facts, decisions, hypotheses, questions) in the
//! KNOWLEDGE layer (global, or a project's). The code graph's facts come from the
//! language plugins (`lang@1`), one per language.

pub mod app;
pub mod attributes;
pub mod db;
pub mod engine;
pub mod facts;
pub mod host;
pub mod knowledge;
pub mod langs;
pub mod linestore;
pub mod model;
pub mod notes;
pub mod queries;
pub mod service;
pub mod settings;
pub mod skills;
pub mod summary;
pub mod tree;
pub mod watch;
pub mod work;
