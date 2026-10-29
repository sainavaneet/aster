//! The modern style of the client shell. `model` is built when the snapshot or
//! configuration changes and may allocate; `draw` runs every frame and only
//! writes borrowed text into the buffer (enforced by
//! `scripts/test_ui_hot_path_architecture.py`).

pub(super) mod draw;
pub(super) mod model;
