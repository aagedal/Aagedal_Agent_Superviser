//! Which quantizations we offer, and which one we pick by default.
//!
//! Below 4 bits per weight a model loses too much to be useful as a coding agent, so those
//! files (Q1, Q2, Q3, TQ1, IQ1–IQ3) are never offered. The default is Q4_K_M, or the closest
//! 4-bit variant a repo publishes.

use std::sync::LazyLock;

use regex::Regex;

/// The fewest bits per weight we offer.
pub const MIN_BITS: u32 = 4;

/// Default picks, best first: Q4_K_M, then Unsloth's dynamic 4-bit variants, then other
/// 4-bit formats. Labels are compared case-insensitively.
pub const PREFERRED: &[&str] = &[
    "Q4_K_M",
    "UD-Q4_K_M",
    "UD-Q4_K_XL",
    "Q4_K_S",
    "UD-Q4_K_S",
    "MXFP4_MOE",
    "IQ4_XS",
    "UD-IQ4_XS",
    "IQ4_NL",
    "UD-IQ4_NL",
    "Q4_0",
    "Q4_1",
];

// The quant label at the end of a GGUF file stem, e.g. `UD-Q4_K_XL` in
// `GLM-5.3-Flash-UD-Q4_K_XL-00001-of-00004`.
static LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[-_.])((?:UD-)?(?:I?Q\d(?:_[A-Z0-9]+)*|TQ\d_\d|MXFP4(?:_MOE)?|BF16|F16|F32))$").unwrap()
});
static SHARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{5}-of-\d{5}$").unwrap());
static FIRST_SHARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-0*1-of-\d{5}\.gguf$").unwrap());

/// What Unsloth names GGUFs in mainline llama.cpp's layout when the plain `.gguf` files in
/// the same repo need its own llama.cpp build (`Mainline_Support/*.gguf_file`).
const MAINLINE_SUFFIX: &str = ".gguf_file";

/// Whether `path` is a GGUF in mainline llama.cpp's layout, published beside files that need
/// another build; see [`MAINLINE_SUFFIX`].
pub fn is_mainline(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(MAINLINE_SUFFIX)
}

/// Where a repo file is saved: the same path, with `.gguf_file` renamed to `.gguf` so llama.cpp
/// finds the other shards.
pub fn local_path(path: &str) -> String {
    match path.len().checked_sub(MAINLINE_SUFFIX.len()) {
        Some(stem) if is_mainline(path) => format!("{}.gguf", &path[..stem]),
        _ => path.to_string(),
    }
}

/// The quant label of a GGUF file path, upper-cased (`Q4_K_M`, `UD-Q4_K_XL`, `BF16`).
pub fn label(path: &str) -> Option<String> {
    let path = local_path(path);
    let file = path.rsplit('/').next()?;
    let stem = file.strip_suffix(".gguf").or_else(|| file.strip_suffix(".GGUF"))?;
    let stem = SHARD.replace(stem, "");
    LABEL.captures(&stem).map(|c| c[1].to_ascii_uppercase())
}

/// Bits per weight a quant label stands for (`IQ4_XS` → 4, `MXFP4_MOE` → 4, `BF16` → 16).
pub fn bits(label: &str) -> Option<u32> {
    let label = label.to_ascii_uppercase();
    let label = label.strip_prefix("UD-").unwrap_or(&label);
    if let Some(rest) = label.strip_prefix("MXFP") {
        return rest.chars().next()?.to_digit(10);
    }
    if label.starts_with("BF") || label.starts_with('F') {
        return label.trim_start_matches(['B', 'F']).parse().ok();
    }
    let rest = label.trim_start_matches(['I', 'T']).strip_prefix('Q')?;
    rest.chars().next()?.to_digit(10)
}

/// Whether we offer a quant at all.
pub fn allowed(label: &str) -> bool {
    bits(label).is_some_and(|bits| bits >= MIN_BITS)
}

/// The default quant among `labels`: the first of [`PREFERRED`] that is present.
pub fn preferred<'a>(labels: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let labels: Vec<&str> = labels.into_iter().collect();
    PREFERRED.iter().find_map(|want| labels.iter().copied().find(|have| have.eq_ignore_ascii_case(want)))
}

/// Whether `path` is the file llama.cpp should be pointed at: the only file, or the first shard.
pub fn is_entry_file(path: &str) -> bool {
    let path = local_path(path);
    !SHARD.is_match(path.trim_end_matches(".gguf")) || FIRST_SHARD.is_match(&path)
}

/// `(index, total)` of a shard such as `m-Q4_K_M-00002-of-00003.gguf`.
pub fn shard(path: &str) -> Option<(u32, u32)> {
    let path = local_path(path);
    let stem = path.trim_end_matches(".gguf");
    let tail = SHARD.find(stem)?.as_str();
    let (index, total) = tail.trim_start_matches('-').split_once("-of-")?;
    Some((index.parse().ok()?, total.parse().ok()?))
}

/// The label for llama.cpp's `general.file_type` (`llama_ftype`), for files whose name
/// doesn't say (Ollama's blobs).
pub fn label_for_file_type(file_type: u64) -> Option<&'static str> {
    Some(match file_type {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4_MOE",
        _ => return None,
    })
}

/// Projector weights for vision models: not a model on their own.
pub fn is_auxiliary(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    file.starts_with("mmproj") || file.contains("imatrix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_from_file_names() {
        assert_eq!(label("Qwen3.5-9B-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(label("Qwen3.5-9B-UD-Q4_K_XL.gguf").as_deref(), Some("UD-Q4_K_XL"));
        assert_eq!(
            label("UD-Q4_K_XL/GLM-5.3-Flash-UD-Q4_K_XL-00001-of-00004.gguf").as_deref(),
            Some("UD-Q4_K_XL")
        );
        assert_eq!(label("Qwen3-Coder-Next-MXFP4_MOE.gguf").as_deref(), Some("MXFP4_MOE"));
        assert_eq!(label("gpt-oss-20b-F16.gguf").as_deref(), Some("F16"));
        assert_eq!(label("model-bf16.gguf").as_deref(), Some("BF16"));
        assert_eq!(label("Qwen3.5-4B-IQ4_XS.gguf").as_deref(), Some("IQ4_XS"));
        assert_eq!(label("x-UD-TQ1_0.gguf").as_deref(), Some("UD-TQ1_0"));
        assert_eq!(label("README.md"), None);
    }

    #[test]
    fn nothing_below_four_bits() {
        for low in ["Q2_K", "UD-Q2_K_XL", "Q3_K_M", "IQ3_XXS", "UD-IQ1_S", "TQ1_0", "IQ2_M"] {
            assert!(!allowed(low), "{low} must not be offered");
        }
        for ok in ["Q4_K_M", "UD-Q4_K_XL", "IQ4_XS", "MXFP4_MOE", "Q5_K_M", "Q8_0", "BF16"] {
            assert!(allowed(ok), "{ok} should be offered");
        }
    }

    #[test]
    fn prefers_q4_k_m_then_close_variants() {
        assert_eq!(preferred(["Q8_0", "Q4_K_M", "UD-Q4_K_XL"]), Some("Q4_K_M"));
        assert_eq!(preferred(["UD-IQ4_XS", "UD-Q4_K_XL", "UD-Q4_K_M"]), Some("UD-Q4_K_M"));
        assert_eq!(preferred(["UD-Q4_K_XL", "UD-IQ4_XS"]), Some("UD-Q4_K_XL"));
        assert_eq!(preferred(["Q2_K", "Q8_0"]), None);
    }

    #[test]
    fn file_types_follow_the_same_policy() {
        assert_eq!(label_for_file_type(15), Some("Q4_K_M"));
        assert!(allowed(label_for_file_type(38).unwrap()));
        for low in [10, 12, 19, 24, 36] {
            assert!(!allowed(label_for_file_type(low).unwrap()), "file type {low}");
        }
        assert_eq!(label_for_file_type(999), None);
    }

    #[test]
    fn shards() {
        assert_eq!(shard("Q4/m-Q4_K_M-00002-of-00003.gguf"), Some((2, 3)));
        assert_eq!(shard("m-Q4_K_M.gguf"), None);
    }

    #[test]
    fn entry_files() {
        assert!(is_entry_file("a-Q4_K_M.gguf"));
        assert!(is_entry_file("Q4_K_M/a-Q4_K_M-00001-of-00003.gguf"));
        assert!(!is_entry_file("Q4_K_M/a-Q4_K_M-00002-of-00003.gguf"));
        assert!(is_auxiliary("mmproj-F16.gguf"));
    }

    #[test]
    fn mainline_files_are_saved_as_gguf() {
        let path = "Mainline_Support/Inkling-2-Mini-UD-Q4_K_M.gguf_file";
        assert!(is_mainline(path));
        assert_eq!(local_path(path), "Mainline_Support/Inkling-2-Mini-UD-Q4_K_M.gguf");
        assert_eq!(label(path).as_deref(), Some("UD-Q4_K_M"));
        assert!(is_entry_file(path));
        assert!(!is_entry_file("Mainline_Support/a-Q4_K_M-00002-of-00003.gguf_file"));
        assert_eq!(local_path("a-Q4_K_M.gguf"), "a-Q4_K_M.gguf");
    }
}
