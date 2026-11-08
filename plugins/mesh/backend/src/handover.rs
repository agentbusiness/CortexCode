//! Who serves a tag on this computer. Every app says its own sharing priority on its card
//! (`cc_protocol::contracts::mesh::Card::sharing_priority`), so no app knows another by name: when
//! an app of a higher priority than this one runs on this computer and its card lists a model of
//! a tag, that app serves the tag here. This app hands the tag over:
//!
//! - its own model of that tag is no option for its claims, and the other app's model takes the
//!   place of its own (tier 0);
//! - it stops lending that model: it does not run it, so it is not its to share. The model moves
//!   from the card's `models` to its `uses`, which only says what this app needs. The app that
//!   runs the model lends it;
//! - its own model stops, once it has finished what it is running ([`Mesh::hand_over`]).
//!
//! When the other app goes or stops serving the tag, this app serves it again itself. A model with
//! several tags is handed over when another app serves every one of them.

use crate::service::{Mesh, lock};
use cc_protocol::contracts::mesh::{Card, ModelCard, UsedModel};

/// The priority an app said on its card (none: it comes last).
pub fn sharing_priority(card: &Card) -> u32 {
    card.sharing_priority.unwrap_or(0)
}

/// The app of `card` serves before this app.
pub fn serves_before_me(card: &Card) -> bool {
    sharing_priority(card) > crate::SHARING_PRIORITY
}

/// One model of an app that serves before this one on this computer.
#[derive(Clone)]
pub(crate) struct Server {
    priority: u32,
    pub(crate) node: String,
    pub(crate) model: ModelCard,
}

fn serves_all(above: &[Server], tags: &[String]) -> bool {
    !tags.is_empty() && tags.iter().all(|tag| above.iter().any(|server| server.model.tags.contains(tag)))
}

impl Mesh {
    /// The models of the connected apps on this computer that serve before this one.
    pub(crate) fn servers_above(&self) -> Vec<Server> {
        let cards = lock(&self.cards).clone();
        let conns = self.connections();
        let mut above = Vec::new();
        for member in self.members().into_values().filter(|member| member.local && conns.contains_key(&member.node)) {
            let Some(card) = cards.get(&member.node).filter(|card| serves_before_me(card)) else { continue };
            let priority = sharing_priority(card);
            above.extend(card.models.iter().map(|model| Server { priority, node: member.node.clone(), model: model.clone() }));
        }
        above
    }

    /// A model with these tags is handed over: apps before this one serve every one of them here.
    pub fn is_handed(&self, tags: &[String]) -> bool {
        serves_all(&self.servers_above(), tags)
    }

    /// The model that serves `used` here now: of the app of the highest priority that serves one of
    /// its tags, the same model when it has it, else one of the same family.
    pub(crate) fn server_for(&self, used: &UsedModel) -> Option<Server> {
        self.servers_above()
            .into_iter()
            .filter(|server| used.tags.iter().any(|tag| server.model.tags.contains(tag)))
            .min_by_key(|server| (std::cmp::Reverse(server.priority), server.model.model_id != used.model_id, server.model.family != used.family))
    }

    /// The generic rule on this computer's card: a model this app does not run because another
    /// app runs it is not lent, it is only listed as used.
    pub fn apply_handover(&self, card: &mut Card) {
        let above = self.servers_above();
        let mut handed = lock(&self.handed);
        card.models.retain(|model| {
            if !serves_all(&above, &model.tags) {
                return true;
            }
            handed.insert(model.plugin.clone(), UsedModel { model_id: model.model_id.clone(), family: model.family.clone(), tags: model.tags.clone(), loaded: model.loaded });
            false
        });
        let before = handed.len();
        handed.retain(|_, used| serves_all(&above, &used.tags));
        let served_again = handed.len() < before;
        card.uses = handed.values().cloned().collect();
        drop(handed);
        if served_again {
            self.forget_card();
        }
    }
}
