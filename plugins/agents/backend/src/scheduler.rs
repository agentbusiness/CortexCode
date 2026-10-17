//! The agent queue: who runs and who waits. The number allowed in flight (`limit`) is set by the
//! control loop ([`crate::control`]); this queue never exceeds it: an agent starts only when fewer
//! than `limit` are running.

use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentKey {
    pub project_id: String,
    pub name: String,
}

impl AgentKey {
    pub fn label(&self) -> String {
        self.name.clone()
    }
}

#[derive(Debug, Default)]
pub struct Scheduler {
    pub limit: u32,
    pub running: Vec<AgentKey>,
    pub queued: VecDeque<AgentKey>,
    /// Highest number of agents ever running at once (for verification and reporting).
    pub peak: u32,
}

impl Scheduler {
    pub fn new(limit: u32) -> Self {
        Self { limit: limit.max(1), ..Self::default() }
    }

    /// Queue an agent. Returns the agents to start now (in order).
    pub fn enqueue(&mut self, key: AgentKey) -> Vec<AgentKey> {
        if !self.running.contains(&key) && !self.queued.contains(&key) {
            self.queued.push_back(key);
        }
        self.fill()
    }

    /// An agent finished (or stopped). Returns the agents to start now.
    pub fn finished(&mut self, key: &AgentKey) -> Vec<AgentKey> {
        self.running.retain(|running| running != key);
        self.queued.retain(|queued| queued != key);
        self.fill()
    }

    /// Remove a queued agent that has not started.
    pub fn dequeue(&mut self, key: &AgentKey) -> bool {
        let before = self.queued.len();
        self.queued.retain(|queued| queued != key);
        before != self.queued.len()
    }

    /// A new limit from the control loop. Returns the agents to start now (a lower limit stops
    /// nobody: running agents finish, and no new one starts until fewer than the limit run).
    pub fn set_limit(&mut self, limit: u32) -> Vec<AgentKey> {
        self.limit = limit.max(1);
        self.fill()
    }

    fn fill(&mut self) -> Vec<AgentKey> {
        let limit = self.limit.max(1) as usize;
        let mut started = Vec::new();
        while self.running.len() < limit {
            let Some(next) = self.queued.pop_front() else { break };
            self.running.push(next.clone());
            started.push(next);
        }
        self.peak = self.peak.max(self.running.len() as u32);
        started
    }
}
