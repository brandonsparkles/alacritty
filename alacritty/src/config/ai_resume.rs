//! AI CLI session-resume command policy. macOS only.
//!
//! Alacritty owns the mechanics of detecting per-tab AI CLI sessions, but
//! the permissive flags are local policy and should be configurable without
//! rebuilding the terminal.
//!
//! Sample `~/.config/alacritty/alacritty.toml`:
//!
//! ```toml
//! [ai_resume.claude]
//! flags = ["--your-claude-flag"]
//!
//! [ai_resume.codex]
//! flags = ["--your-codex-flag"]
//!
//! [ai_resume.copilot]
//! flags = ["--your-copilot-flag"]
//! ```

use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;

#[derive(ConfigDeserialize, Serialize, Default, Clone, PartialEq, Debug)]
pub struct AiResumeConfig {
    pub claude: ClaudeResumeConfig,
    pub codex: CodexResumeConfig,
    pub copilot: CopilotResumeConfig,
}

#[derive(ConfigDeserialize, Serialize, Default, Clone, PartialEq, Eq, Debug)]
pub struct ClaudeResumeConfig {
    pub flags: Vec<String>,
}

#[derive(ConfigDeserialize, Serialize, Default, Clone, PartialEq, Eq, Debug)]
pub struct CodexResumeConfig {
    pub flags: Vec<String>,
}

#[derive(ConfigDeserialize, Serialize, Default, Clone, PartialEq, Eq, Debug)]
pub struct CopilotResumeConfig {
    pub flags: Vec<String>,
}
