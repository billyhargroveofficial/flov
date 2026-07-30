use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub whisper: WhisperConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub openrouter: OpenRouterConfig,
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub hotkey: HotkeyConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HotkeyConfig {
    /// Plus-separated key combo, e.g. "Ctrl+Win", "Ctrl+Alt+Space".
    #[serde(default = "default_hotkey_combo")]
    pub combo: String,
}
impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            combo: default_hotkey_combo(),
        }
    }
}
fn default_hotkey_combo() -> String {
    // Mac users press Cmd+Option (the "Ctrl+Win" calque maps to
    // Ctrl+Cmd on macOS, which collides with system shortcuts
    // like Ctrl+Cmd+Q = lock screen and Ctrl+Cmd+Space = emoji picker).
    #[cfg(target_os = "macos")]
    {
        "Cmd+Alt".to_string()
    }
    // A right-side modifier is a convenient push-to-talk key and does not
    // collide with Hyprland's usual Super-based compositor shortcuts.
    #[cfg(target_os = "linux")]
    {
        "RCtrl".to_string()
    }
    #[cfg(target_os = "windows")]
    {
        "Ctrl+Win".to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "Ctrl+Win".to_string()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// Starts the local OpenAI-compatible HTTP API.
    #[serde(default = "default_server_enabled")]
    pub enabled: bool,
    /// Socket address only (hostnames are intentionally not accepted).
    #[serde(default = "default_server_bind")]
    pub bind: String,
    /// Optional bearer token. It is mandatory when binding beyond loopback.
    #[serde(default)]
    pub api_key: String,
    /// Maximum encoded request body size.
    #[serde(default = "default_server_max_body_mb")]
    pub max_body_mb: u64,
    /// Maximum decoded audio duration.
    #[serde(default = "default_server_max_audio_seconds")]
    pub max_audio_seconds: u64,
    /// Apply the configured OpenRouter cleanup unless a request overrides it.
    #[serde(default)]
    pub postprocess: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: default_server_enabled(),
            bind: default_server_bind(),
            api_key: String::new(),
            max_body_mb: default_server_max_body_mb(),
            max_audio_seconds: default_server_max_audio_seconds(),
            postprocess: false,
        }
    }
}

fn default_server_enabled() -> bool {
    true
}

fn default_server_bind() -> String {
    "127.0.0.1:17432".to_string()
}

fn default_server_max_body_mb() -> u64 {
    25
}

fn default_server_max_audio_seconds() -> u64 {
    10 * 60
}

#[derive(Debug, Deserialize, Clone)]
pub struct BackendConfig {
    /// "auto" picks the highest-priority available sidecar; otherwise must
    /// match a sidecar binary name suffix (cuda, vulkan, metal, cpu).
    #[serde(default = "default_backend_choice")]
    pub choice: String,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            choice: default_backend_choice(),
        }
    }
}

fn default_backend_choice() -> String {
    "auto".to_string()
}

#[derive(Debug, Deserialize)]
pub struct WhisperConfig {
    #[serde(default = "default_model_path")]
    pub model_path: PathBuf,
    #[serde(default = "default_language")]
    pub language: String,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self {
            model_path: default_model_path(),
            language: default_language(),
        }
    }
}

fn default_language() -> String {
    "ru".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AudioConfig {
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    /// Preferred input device name as reported by cpal. `None` (or empty
    /// string in toml) → use the system default. Mismatched names fall
    /// back to default with a warning.
    #[serde(default)]
    pub device: Option<String>,
}

// Manual Default — the previous `derive(Default)` returned
// `sample_rate: 0`, which only matters when there's no flov.toml on
// disk (fresh installer). With sample_rate=0 the recording_loop did
// `samples.len() as f64 / 0`, producing +∞ which serde_json then
// serialized as `null` for stats.json. Defaulting to 16 kHz here
// matches `default_sample_rate()` and fixes the null seconds bug.
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            device: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenRouterConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_openrouter_model")]
    pub model: String,
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: default_openrouter_model(),
            system_prompt: default_system_prompt(),
        }
    }
}

fn default_openrouter_model() -> String {
    "openai/gpt-4o-mini".to_string()
}

fn default_system_prompt() -> String {
    "Ты — редактор голосовых транскрипций. Ты получаешь сырой текст, распознанный из речи через Whisper. Твоя задача — превратить его в чистый, грамотный текст, готовый к отправке в чат или другую языковую модель.\n\nПравила обработки:\n\n1. Исправь ошибки распознавания речи, восстанови правильные слова по контексту.\n\n2. Замени всю обсценную и ненормативную лексику на нейтральные эквиваленты, точно передающие эмоцию и смысл. Не смягчай и не теряй интенсивность высказывания — только убери мат.\n\n3. Сохрани все английские слова и термины как есть. Автор — программист и математик, в его речи регулярно встречаются англоязычные термины: названия технологий, функций, библиотек, математических концепций. Не транслитерируй и не переводи их.\n\n4. Расставь знаки препинания: запятые, точки, тире, двоеточия. Следуй правилам русской пунктуации.\n\n5. Раздели текст на логические абзацы по смыслу. Каждая отдельная мысль или тезис — новый абзац.\n\n6. Сохрани точный смысл и цель высказывания. Автор формулирует задачи, вопросы и инструкции для других языковых моделей. Критически важно не потерять, не исказить и не додумать его намерение. Не добавляй ничего от себя.\n\n7. Выведи только готовый текст. Без комментариев, пояснений, приветствий и маркдаун-разметки.".to_string()
}

fn default_model_path() -> PathBuf {
    // Same filename as before so users carrying a manually-downloaded
    // model only need to drop it into the user data dir
    // (see paths::user_data_dir).
    crate::paths::user_data_dir()
        .map(|d| d.join("ggml-large-v3-turbo.bin"))
        .unwrap_or_else(|_| PathBuf::from("ggml-large-v3-turbo.bin"))
}

fn default_sample_rate() -> u32 {
    16000
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = crate::paths::config_path().context("Failed to compute config path")?;
        let data_dir = crate::paths::user_data_dir().context("Failed to compute data dir")?;

        if !config_path.exists() {
            return Ok(Config {
                whisper: WhisperConfig::default(),
                audio: AudioConfig::default(),
                openrouter: OpenRouterConfig::default(),
                backend: BackendConfig::default(),
                hotkey: HotkeyConfig::default(),
                server: ServerConfig::default(),
            });
        }

        let config_str = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config from {:?}", config_path))?;

        let mut config: Config = toml::from_str(&config_str).context("Failed to parse config")?;

        // Resolve relative model_path against the data dir (where the
        // Models window also drops downloads).
        if config.whisper.model_path.is_relative() {
            config.whisper.model_path = data_dir.join(&config.whisper.model_path);
        }

        Ok(config)
    }

    /// Path to flov.toml. Used by writers; load() also resolves to this
    /// same path.
    pub fn path() -> Result<PathBuf> {
        crate::paths::config_path()
    }

    /// Surgically updates `[backend].choice` in flov.toml using toml_edit so
    /// user-authored comments and section ordering survive. Creates the file
    /// (and the [backend] section) if missing.
    pub fn write_backend_choice(choice: &str) -> Result<()> {
        write_field(&["backend", "choice"], choice)
    }

    /// Same surgical update for `[whisper].model_path`. Stores the path as
    /// given (typically absolute, sometimes relative-to-exe).
    pub fn write_model_path(model_path: &std::path::Path) -> Result<()> {
        write_field(
            &["whisper", "model_path"],
            model_path.to_string_lossy().as_ref(),
        )
    }

    /// Updates `[openrouter].<field>` in flov.toml.
    pub fn write_openrouter_field(field: &str, value: &str) -> Result<()> {
        write_field(&["openrouter", field], value)
    }

    /// Updates `[hotkey].combo` in flov.toml.
    pub fn write_hotkey_combo(combo: &str) -> Result<()> {
        write_field(&["hotkey", "combo"], combo)
    }

    /// Updates `[audio].device` in flov.toml. Empty string means
    /// "system default".
    pub fn write_audio_device(device: &str) -> Result<()> {
        write_field(&["audio", "device"], device)
    }
}

/// Walk `[section][key]` in flov.toml, set the leaf string value, and write
/// back. Creates the file and any missing sections.
fn write_field(path_keys: &[&str], value: &str) -> Result<()> {
    let path = Config::path()?;
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("read {:?}", path))?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut =
        existing.parse().context("flov.toml is not valid TOML")?;

    let (last, parents) = path_keys.split_last().expect("path_keys must be non-empty");

    let mut node = doc.as_item_mut();
    for k in parents {
        if !matches!(node, toml_edit::Item::Table(_) | toml_edit::Item::None) {
            anyhow::bail!("flov.toml has a non-table at [{}]", k);
        }
        if node.is_none() {
            *node = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let table = node.as_table_mut().expect("checked above");
        if !table.contains_key(k) {
            table.insert(k, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        node = &mut table[*k];
    }
    let table = node
        .as_table_mut()
        .with_context(|| format!("flov.toml [{}] is not a table", parents.join(".")))?;
    table[*last] = toml_edit::value(value);

    std::fs::write(&path, doc.to_string()).with_context(|| format!("write {:?}", path))?;
    Ok(())
}
