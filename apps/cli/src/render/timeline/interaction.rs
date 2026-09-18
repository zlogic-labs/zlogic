//! Permission, confirmation, input, and form interaction renderer boundary.

use crate::app::PendingInteraction;

pub fn kind(interaction: &PendingInteraction) -> &'static str {
    match interaction {
        PendingInteraction::Permission { .. } => "permission",
        PendingInteraction::Confirmation { .. } => "confirmation",
        PendingInteraction::Form { .. } => "form",
    }
}
