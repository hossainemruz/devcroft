mod exchange;
// Retain the legacy transport and its regression tests while preferences
// migrate to interactive sessions. New requests use the managed terminal.
#[allow(dead_code)]
mod runner;
mod tutorial;
mod view;

pub(crate) use view::{AssistantView, OpenReviewSource, ReviewAgentRequested};
