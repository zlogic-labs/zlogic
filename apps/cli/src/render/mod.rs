//! Rendering layer. `view` is a pure function of `AppState`;
//! `history` wraps settled content before `insert_before`.

pub mod history;
pub mod replay;
pub mod timeline;
pub mod view;
