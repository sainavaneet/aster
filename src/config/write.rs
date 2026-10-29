#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    StatusIndicators(super::StatusIndicatorStyle),
    Sound(bool),
    Polaris(bool),
    ToastDelivery(super::ToastDelivery),
    /// `ui.style`, and `ui.glyphs` when the choice also sets it.
    UiStyle(super::UiStyleConfig, Option<super::UiGlyphsConfig>),
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::StatusIndicators(_) => "status indicators",
            Self::Polaris(_) => "Polaris startup",
            Self::Sound(_) => "sound setting",
            Self::ToastDelivery(_) => "toast setting",
            Self::UiStyle(..) => "ui style",
        }
    }

    pub(crate) fn apply(self, content: &str) -> String {
        match self {
            Self::Theme(name) => {
                let content =
                    super::upsert_section_value(content, "theme", "name", &format!("\"{name}\""));
                super::upsert_section_bool(&content, "theme", "auto_switch", false)
            }
            Self::StatusIndicators(style) => super::upsert_section_value(
                content,
                "ui",
                "status_indicators",
                &format!("\"{}\"", style.as_str()),
            ),
            Self::Polaris(enabled) => {
                super::upsert_section_bool(content, "polaris", "enabled", enabled)
            }
            Self::Sound(enabled) => {
                super::upsert_section_bool(content, "ui.sound", "enabled", enabled)
            }
            Self::UiStyle(style, glyphs) => {
                let value = match style {
                    super::UiStyleConfig::Modern => "\"modern\"",
                    super::UiStyleConfig::Classic => "\"classic\"",
                };
                let content = super::upsert_section_value(content, "ui", "style", value);
                match glyphs {
                    Some(super::UiGlyphsConfig::Unicode) => {
                        super::upsert_section_value(&content, "ui", "glyphs", "\"unicode\"")
                    }
                    Some(super::UiGlyphsConfig::Ascii) => {
                        super::upsert_section_value(&content, "ui", "glyphs", "\"ascii\"")
                    }
                    None => content,
                }
            }
            Self::ToastDelivery(delivery) => {
                let value = match delivery {
                    super::ToastDelivery::Off => "\"off\"",
                    super::ToastDelivery::Aster => "\"aster\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
        }
    }
}

pub(crate) fn update_file_at(
    path: &std::path::Path,
    description: &str,
    update: impl FnOnce(&str) -> Result<String, String>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create config directory: {error}"))?;
    }
    let content = match super::io::read_optional_config(path) {
        Ok(Some(content)) => content,
        Ok(None) => String::new(),
        Err(error) => {
            return Err(format!(
                "failed to read config before saving {description}: {error}"
            ));
        }
    };
    let updated = update(&content)?;
    std::fs::write(path, updated).map_err(|error| format!("failed to save {description}: {error}"))
}

pub(crate) fn write_edit(edit: ConfigEdit<'_>) -> Result<(), String> {
    update_file_at(&super::config_path(), edit.description(), |content| {
        Ok(edit.apply(content))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_style_edit_writes_style_and_optionally_glyphs() {
        use crate::config::{UiGlyphsConfig, UiStyleConfig};
        let source = "[ui]\nsidebar_width = 30\n";
        let modern =
            ConfigEdit::UiStyle(UiStyleConfig::Modern, Some(UiGlyphsConfig::Ascii)).apply(source);
        let config: crate::config::Config = toml::from_str(&modern).unwrap();
        assert_eq!(config.ui.style, UiStyleConfig::Modern);
        assert_eq!(config.ui.glyphs, UiGlyphsConfig::Ascii);
        assert_eq!(config.ui.sidebar_width, 30);

        let classic = ConfigEdit::UiStyle(UiStyleConfig::Classic, None).apply(&modern);
        let config: crate::config::Config = toml::from_str(&classic).unwrap();
        assert_eq!(config.ui.style, UiStyleConfig::Classic);
        assert_eq!(config.ui.glyphs, UiGlyphsConfig::Ascii, "glyphs untouched");
    }

    #[test]
    fn polaris_toggle_preserves_provider_model_and_other_settings() {
        let source = "[polaris]\nenabled = true\nprovider = \"claude\"\nmodel = \"fable\"\n\n[theme]\nname = \"nord\"\n";
        let disabled = ConfigEdit::Polaris(false).apply(source);
        let config: crate::config::Config = toml::from_str(&disabled).unwrap();
        assert!(!config.polaris.enabled);
        assert_eq!(config.polaris.provider, "claude");
        assert_eq!(config.polaris.model, "fable");
        assert!(disabled.contains("name = \"nord\""));
        let enabled = ConfigEdit::Polaris(true).apply(&disabled);
        assert!(
            toml::from_str::<crate::config::Config>(&enabled)
                .unwrap()
                .polaris
                .enabled
        );
    }

    #[test]
    fn update_file_at_does_not_move_a_leading_bom_into_the_file() {
        let dir = std::env::temp_dir().join(format!("aster-config-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        )
        .unwrap();

        update_file_at(&path, "onboarding setting", |content| {
            Ok(crate::config::upsert_top_level_bool(
                content,
                "onboarding",
                false,
            ))
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);

        assert!(
            !written.contains('\u{feff}'),
            "unexpected BOM in {written:?}"
        );
        assert!(
            toml::from_str::<toml::Value>(&written).is_ok(),
            "written config is not valid TOML: {written:?}"
        );
    }
}
