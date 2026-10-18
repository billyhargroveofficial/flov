// Whisper model catalog + filesystem layout.
//
// Models live under `<exe_dir>/models/whisper/<filename>`. The catalog is
// hardcoded for now (Phase 2: discover variants from HuggingFace). Each
// entry knows where to download the .bin from on HuggingFace and how big
// the file is so the UI can show size + estimated bytes-to-go without
// HEAD-pinging the server.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub family: &'static str,
    pub label: &'static str,
    pub filename: &'static str,
    pub size_bytes: u64,
    pub url: String,
    pub languages: &'static str,
    pub notes: &'static str,
    pub local_path: PathBuf,
    pub downloaded: bool,
    pub active: bool,
}

pub struct CatalogEntry {
    pub id: &'static str,
    pub family: &'static str,
    pub label: &'static str,
    pub filename: &'static str,
    pub size_bytes: u64,
    pub languages: &'static str,
    pub notes: &'static str,
}

// Sizes are taken from HuggingFace's `ggerganov/whisper.cpp` release manifest
// (rounded to bytes). They're used only for UI/UX (progress bars, "do you
// have free disk?"); the actual download writes whatever bytes arrive.
//
// Curated to the models that actually carry their weight: smaller variants
// for fast/CPU runs, large-v3-turbo (f16 or q8_0) as the daily driver. Older
// `large` revisions and their quantized builds were removed because turbo
// strictly dominates them on a modern GPU and the picker stayed cluttered.
const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "tiny",
        family: "whisper",
        label: "Tiny",
        filename: "ggml-tiny.bin",
        size_bytes: 75_000_000,
        languages: "multi",
        notes: "Smallest, fastest. Quality only OK for clean English.",
    },
    CatalogEntry {
        id: "base",
        family: "whisper",
        label: "Base",
        filename: "ggml-base.bin",
        size_bytes: 142_000_000,
        languages: "multi",
        notes: "Better than tiny, still very fast.",
    },
    CatalogEntry {
        id: "small",
        family: "whisper",
        label: "Small",
        filename: "ggml-small.bin",
        size_bytes: 466_000_000,
        languages: "multi",
        notes: "Reasonable quality / speed tradeoff.",
    },
    CatalogEntry {
        id: "medium",
        family: "whisper",
        label: "Medium",
        filename: "ggml-medium.bin",
        size_bytes: 1_530_000_000,
        languages: "multi",
        notes: "Good quality, modest GPU.",
    },
    CatalogEntry {
        id: "large-v3-turbo",
        family: "whisper",
        label: "Large v3 Turbo",
        filename: "ggml-large-v3-turbo.bin",
        size_bytes: 1_620_000_000,
        languages: "multi",
        notes: "Quality close to v3, ~6× faster decoder. Default.",
    },
    // Measured on RTX 4050 Laptop / CUDA against the f16 turbo on 45 Russian
    // speech clips (2-28 s, three LibriVox readers): 4 of 816 words differ,
    // in both directions; inference ~17% faster, model load ~150-200 ms
    // faster, which is the wait on phrases shorter than the load.
    CatalogEntry {
        id: "large-v3-turbo-q8_0",
        family: "whisper",
        label: "Large v3 Turbo Q8",
        filename: "ggml-large-v3-turbo-q8_0.bin",
        size_bytes: 874_188_075,
        languages: "multi",
        notes: "8-bit Turbo: same quality on tests, faster load and inference.",
    },
];

const HF_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

fn url_for(filename: &str) -> String {
    format!("{}/{}", HF_BASE, filename)
}

/// Where downloaded models live — `<user_data_dir>/models/<family>/`.
/// See `crate::paths::user_data_dir` for the platform-specific layout
/// (this used to be next to the exe on every OS but that breaks on
/// macOS where the .app bundle is read-only after code-signing).
pub fn models_dir(family: &str) -> Result<PathBuf> {
    crate::paths::models_dir(family)
}

pub fn local_path(entry: &CatalogEntry) -> Result<PathBuf> {
    Ok(models_dir(entry.family)?.join(entry.filename))
}

/// Returns the catalog with `downloaded` and `active` flags filled in.
/// `active_path` is the absolute path of the model currently selected for
/// transcription (may be `None` if config points elsewhere).
pub fn list(active_path: Option<&std::path::Path>) -> Vec<ModelInfo> {
    CATALOG
        .iter()
        .map(|e| {
            let local = local_path(e).unwrap_or_default();
            let downloaded = local.exists();
            let active = match active_path {
                Some(p) => p == local.as_path(),
                None => false,
            };
            ModelInfo {
                id: e.id.to_string(),
                family: e.family,
                label: e.label,
                filename: e.filename,
                size_bytes: e.size_bytes,
                url: url_for(e.filename),
                languages: e.languages,
                notes: e.notes,
                local_path: local,
                downloaded,
                active,
            }
        })
        .collect()
}

pub fn find(id: &str) -> Result<&'static CatalogEntry> {
    CATALOG
        .iter()
        .find(|e| e.id == id)
        .with_context(|| format!("unknown model id: {}", id))
}

pub fn entry_local_path(id: &str) -> Result<PathBuf> {
    let e = find(id)?;
    local_path(e)
}

pub fn entry_url(id: &str) -> Result<String> {
    let e = find(id)?;
    Ok(url_for(e.filename))
}

pub fn entry_size(id: &str) -> Result<u64> {
    Ok(find(id)?.size_bytes)
}

pub fn delete_file(id: &str) -> Result<()> {
    let path = entry_local_path(id)?;
    if !path.exists() {
        bail!("model {} not downloaded", id);
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {:?}", path))?;
    Ok(())
}
