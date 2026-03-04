// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only

pub mod dashboard;

pub use dashboard::{
    new_shared_state, restore_terminal, setup_terminal, tick_tui, SharedState, Term, WallEntry,
};
