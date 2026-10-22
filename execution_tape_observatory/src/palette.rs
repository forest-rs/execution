// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use ratatui::style::Color;

// Use xterm-256 colors so native terminals need not implement 24-bit RGB.

pub(super) const BG: Color = Color::Indexed(233);
pub(super) const FG: Color = Color::Indexed(255);
pub(super) const MUTED: Color = Color::Indexed(246);
pub(super) const GREEN: Color = Color::Indexed(85);
pub(super) const BLUE: Color = Color::Indexed(75);
pub(super) const GOLD: Color = Color::Indexed(221);
pub(super) const PURPLE: Color = Color::Indexed(177);
pub(super) const CYAN: Color = Color::Indexed(87);
pub(super) const ROSE: Color = Color::Indexed(211);
pub(super) const LANES: [Color; 4] = [PURPLE, GOLD, CYAN, ROSE];
