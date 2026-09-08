// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Model catalog, hardware probing and downloads.

pub mod downloader;
pub mod hardware;
pub mod registry;

pub use hardware::Hardware;
pub use registry::{ModelInfo, Quant, Tier};
