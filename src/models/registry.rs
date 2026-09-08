// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Catalog of the models this program knows how to fetch.
//!
//! Data, not policy: nothing in the pipeline names a specific model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Roughly what class of machine an entry is meant for. Mainstream consumer
/// hardware — a card with 6 to 8 GB — is the floor designed against.
pub enum Tier {
    /// Fits a 6 GB card, or runs on CPU for someone patient.
    Lite,
    /// Comfortable on the 8 to 12 GB cards most people actually own.
    Standard,
    /// 12 GB and up.
    Max,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Lite => "Lite",
            Tier::Standard => "Standard",
            Tier::Max => "Max",
        }
    }
}

/// Weight precision, orthogonal to parameter count; together they decide both
/// memory and quality.
///
/// Low-bit quantization degrades a model unevenly: proper nouns and the
/// morphology of less-represented languages go first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quant {
    Q4KM,
    Q8_0,
}

impl Quant {
    pub fn label(self) -> &'static str {
        match self {
            Quant::Q4KM => "Q4_K_M",
            Quant::Q8_0 => "Q8_0",
        }
    }

    /// Roughly how many bits each weight costs, for the memory estimate.
    pub fn bits_per_weight(self) -> f64 {
        match self {
            Quant::Q4KM => 4.8,
            Quant::Q8_0 => 8.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub tier: Tier,
    /// Parameter count in billions — the other half of the size story.
    pub params_b: f32,
    pub quant: Quant,
    pub filename: String,
    pub url: String,
    /// Approximate download size, for progress and the free-space check.
    pub size_bytes: u64,
    /// Memory the weights occupy at inference time (MB), VRAM or RAM.
    pub memory_required_mb: u32,
    /// SHA-256 of the finished file; None skips verification.
    pub sha256: Option<String>,
}

/// Every model offered, cheapest first.
///
/// Sizes are each file's real `Content-Length`. `memory_required_mb` adds the
/// KV cache at an 8k context and ggml's compute buffers to the weights: what
/// the card has to hold, not what the download weighs.
pub fn catalog() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "qwen3-4b-q4".into(),
            name: "Qwen3 4B Q4_K_M".into(),
            tier: Tier::Lite,
            params_b: 4.0,
            quant: Quant::Q4KM,
            filename: "Qwen3-4B-Q4_K_M.gguf".into(),
            url: "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf"
                .into(),
            size_bytes: 2_497_280_256,
            memory_required_mb: 4_000,
            sha256: None,
        },
        ModelInfo {
            id: "qwen3-4b-q8".into(),
            name: "Qwen3 4B Q8_0".into(),
            tier: Tier::Lite,
            params_b: 4.0,
            quant: Quant::Q8_0,
            filename: "Qwen3-4B-Q8_0.gguf".into(),
            url: "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q8_0.gguf".into(),
            size_bytes: 4_280_000_000,
            memory_required_mb: 5_800,
            sha256: None,
        },
        ModelInfo {
            id: "qwen3-8b-q4".into(),
            name: "Qwen3 8B Q4_K_M".into(),
            tier: Tier::Standard,
            params_b: 8.0,
            quant: Quant::Q4KM,
            filename: "Qwen3-8B-Q4_K_M.gguf".into(),
            url: "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf"
                .into(),
            size_bytes: 5_027_000_000,
            memory_required_mb: 6_500,
            sha256: None,
        },
        ModelInfo {
            id: "qwen3-8b-q8".into(),
            name: "Qwen3 8B Q8_0".into(),
            tier: Tier::Standard,
            params_b: 8.0,
            quant: Quant::Q8_0,
            filename: "Qwen3-8B-Q8_0.gguf".into(),
            url: "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q8_0.gguf".into(),
            size_bytes: 8_805_000_000,
            memory_required_mb: 10_300,
            sha256: None,
        },
        ModelInfo {
            id: "qwen3-14b-q4".into(),
            name: "Qwen3 14B Q4_K_M".into(),
            tier: Tier::Max,
            params_b: 14.0,
            quant: Quant::Q4KM,
            filename: "Qwen3-14B-Q4_K_M.gguf".into(),
            url: "https://huggingface.co/Qwen/Qwen3-14B-GGUF/resolve/main/Qwen3-14B-Q4_K_M.gguf"
                .into(),
            size_bytes: 9_000_000_000,
            memory_required_mb: 11_500,
            sha256: None,
        },
    ]
}

/// The model installed when nothing is chosen and nothing can be detected:
/// the floor of the range, so a first run works on the weakest machine it
/// design for rather than failing on it.
pub fn default_model() -> ModelInfo {
    by_id("qwen3-4b-q4").expect("the default model must exist in the catalog")
}

pub fn by_id(id: &str) -> Option<ModelInfo> {
    catalog().into_iter().find(|m| m.id == id)
}

/// Best catalog entry that fits the given memory budget, in MB.
///
/// `vram_mb` of 0 means no GPU was detected. The floor then stands whatever
/// the system RAM is: CPU inference on a larger model is slow enough that
/// offering it would be a worse experience than not offering it.
pub fn recommend(vram_mb: u32, ram_mb: u32) -> ModelInfo {
    if vram_mb == 0 {
        return default_model();
    }

    let budget = vram_mb.min(ram_mb);

    // Among everything that fits, prefer parameters first and precision second.
    // That ordering is the conventional one and it stands until the
    // measurements say otherwise — the point of carrying both axes in the
    // catalog is that the question can be re-answered with evidence.
    catalog()
        .into_iter()
        .filter(|m| m.memory_required_mb <= budget)
        .max_by(|a, b| {
            a.params_b
                .partial_cmp(&b.params_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.quant.cmp(&b.quant))
        })
        .unwrap_or_else(default_model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_gpu_stays_at_the_floor() {
        assert_eq!(recommend(0, 64_000).id, "qwen3-4b-q4");
    }

    #[test]
    fn a_common_eight_gig_card_gets_eight_billion_parameters() {
        // 8B Q4 needs 6.5 GB all-in, which is what mainstream hardware has.
        assert_eq!(recommend(8_000, 32_000).id, "qwen3-8b-q4");
    }

    #[test]
    fn precision_breaks_the_tie_at_equal_parameters() {
        // 8B Q8 (10.3 GB) fits here and beats 8B Q4 on bits; 14B does not fit.
        assert_eq!(recommend(11_000, 32_000).id, "qwen3-8b-q8");
    }

    #[test]
    fn big_card_gets_the_most_parameters() {
        assert_eq!(recommend(24_000, 64_000).id, "qwen3-14b-q4");
    }

    #[test]
    fn system_ram_caps_the_choice() {
        // A big card in a machine with little RAM still cannot stage the weights.
        assert_eq!(recommend(24_000, 5_000).id, "qwen3-4b-q4");
    }

    #[test]
    fn the_two_axes_can_cost_the_same() {
        // The comparison worth measuring: near-identical memory, different trade.
        let q8 = by_id("qwen3-4b-q8").unwrap();
        let b8 = by_id("qwen3-8b-q4").unwrap();
        assert!(b8.memory_required_mb.abs_diff(q8.memory_required_mb) < 1_000);
        assert_ne!(q8.params_b, b8.params_b);
        assert_ne!(q8.quant, b8.quant);
    }

    #[test]
    fn default_is_in_the_catalog() {
        assert!(by_id(&default_model().id).is_some());
    }
}
