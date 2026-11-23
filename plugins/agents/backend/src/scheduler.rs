//! The agent queue: who runs and who waits. The number allowed in flight (`limit`) is set by the
//! control loop ([`crate::control`]); this queue never exceeds it: an agent starts only when fewer
//! than `limit` are running here. An agent whose model calls a joined computer serves (`remote`)
//! takes no place in that count: the limit is this computer's model's. Nor does an agent a frontier
//! CLI works for (`frontier`): those have their own fixed limit (`frontier_limit`).

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
    /// Running agents whose model calls run on a joined computer.
    pub remote: Vec<AgentKey>,
    /// Agents a frontier CLI works for (queued or running): they take no place in `limit` (this
    /// computer's model is not theirs) and run up to `frontier_limit` at once.
    pub frontier: Vec<AgentKey>,
    /// Frontier agents allowed at once (data: `[scheduler] frontier_parallel`).
    pub frontier_limit: u32,
    /// Highest number of agents ever running at once (for verification and reporting).
    pub peak: u32,
    /// The app's work is paused: nothing starts until Resume.
    pub paused: bool,
    /// Agents whose run stopped for the pause: when their run ends they go back to the front of
    /// the queue (not out of it), to start again on Resume.
    pub held: Vec<AgentKey>,
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
        self.remote.retain(|remote| remote != key);
        // Stopped for the pause: it waits at the front of the queue, in its lane.
        if self.held.contains(key) {
            self.held.retain(|held| held != key);
            self.queued.push_front(key.clone());
        } else {
            self.frontier.retain(|frontier| frontier != key);
        }
        self.fill()
    }

    /// The agent's run stopped for the pause: when it ends, the agent waits in the queue.
    pub fn hold(&mut self, key: &AgentKey) {
        if !self.held.contains(key) {
            self.held.push(key.clone());
        }
    }

    /// Pause (nothing starts) or resume. Returns the agents to start now.
    pub fn set_paused(&mut self, paused: bool) -> Vec<AgentKey> {
        self.paused = paused;
        self.fill()
    }

    /// Whether a frontier CLI works for this agent (set before it is queued).
    pub fn mark_frontier(&mut self, key: &AgentKey, frontier: bool) {
        self.frontier.retain(|known| known != key);
        if frontier {
            self.frontier.push(key.clone());
        }
    }

    /// A new frontier limit. Returns the agents to start now.
    pub fn set_frontier_limit(&mut self, limit: u32) -> Vec<AgentKey> {
        self.frontier_limit = limit.max(1);
        self.fill()
    }

    /// Frontier agents running now.
    pub fn frontier_running(&self) -> usize {
        self.running.iter().filter(|key| self.frontier.contains(key)).count()
    }

    /// An agent runs on a joined computer's model: started now if it was queued, and out of this
    /// computer's count either way. Returns the agents to start now (its place here is free).
    pub fn start_remote(&mut self, key: &AgentKey) -> Vec<AgentKey> {
        self.queued.retain(|queued| queued != key);
        if !self.running.contains(key) {
            self.running.push(key.clone());
        }
        if !self.remote.contains(key) {
            self.remote.push(key.clone());
        }
        self.peak = self.peak.max(self.running.len() as u32);
        self.fill()
    }

    /// Agents running on this computer's model.
    pub fn local_running(&self) -> usize {
        self.running.iter().filter(|key| !self.remote.contains(key) && !self.frontier.contains(key)).count()
    }

    /// Remove a queued agent that has not started.
    /// The agent's run is going now (here or on a member's model).
    pub fn is_running(&self, key: &AgentKey) -> bool {
        self.running.contains(key)
    }

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

    /// Start queued agents, in order, while their lane has room: this computer's model (`limit`)
    /// or the frontier CLIs (`frontier_limit`). An agent whose lane is full keeps its place.
    fn fill(&mut self) -> Vec<AgentKey> {
        if self.paused {
            return Vec::new();
        }
        let limit = self.limit.max(1) as usize;
        let frontier_limit = self.frontier_limit.max(1) as usize;
        let mut started = Vec::new();
        let mut waiting = VecDeque::new();
        while let Some(next) = self.queued.pop_front() {
            let room = if self.frontier.contains(&next) { self.frontier_running() < frontier_limit } else { self.local_running() < limit };
            if room {
                self.running.push(next.clone());
                started.push(next);
            } else {
                waiting.push_back(next);
            }
        }
        self.queued = waiting;
        self.peak = self.peak.max(self.running.len() as u32);
        started
    }
}
