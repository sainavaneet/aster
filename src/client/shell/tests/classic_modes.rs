use super::*;

// Captured using the actual mode-bar implementation from d870c4a8, before
// the redesign. The original classic-phase1 fixture remains unedited.
fn matrix() -> String {
    let mut out = String::new();
    for theme in ["catppuccin", "catppuccin-latte"] {
        for motion in ["full", "reduced", "off"] {
            for glyphs in ["unicode", "ascii"] {
                let config: Config = toml::from_str(&format!("[theme]\nname='{theme}'\n[ui]\nstyle='classic'\nanimations='{motion}'\nglyphs='{glyphs}'\n")).unwrap();
                let config = ClientShellConfig::from_config(&config);
                for width in [50, 64, 80, 120, 200] {
                    for scene in [
                        "prefix",
                        "navigate",
                        "resize",
                        "copy",
                        "copy-matches",
                        "copy-selected",
                        "copy-search",
                        "error",
                    ] {
                        for update in [false, true] {
                            let mut copy = super::modern_notifications::copy_mode_fixture();
                            let mode = match scene {
                                "prefix" => ClientShellMode::Prefix,
                                "navigate" => ClientShellMode::Navigate,
                                "resize" => ClientShellMode::Resize,
                                "error" => ClientShellMode::Terminal,
                                _ => ClientShellMode::Copy,
                            };
                            if scene == "copy-matches" {
                                copy.search_query = "needle".into();
                                copy.search_current_global = Some(2);
                                copy.search_total = 14;
                            }
                            if scene == "copy-selected" {
                                copy.selection =
                                    Some(ClientCopySelection::Linewise { anchor_row: 0 });
                            }
                            if scene == "copy-search" {
                                copy.search_prompt = Some(ClientCopySearchPrompt {
                                    direction: crate::api::schema::PaneCopySearchDirection::Forward,
                                    query: TextEditor::from("needle"),
                                });
                            }
                            let error = (scene == "error").then_some("offline endpoint");
                            let area = Rect::new(0, 0, width, 1);
                            let mut buffer = Buffer::empty(area);
                            super::super::render::render_mode_bar(
                                &mut buffer,
                                area,
                                mode,
                                Some(&copy),
                                error,
                                update,
                                &config.keybinds,
                                &config.palette,
                                None,
                            );
                            out.push_str(&format!(
                                "=== {theme}/{motion}/{glyphs}/{width}/{scene}/{update}\n"
                            ));
                            super::modern_notifications::capture(&mut out, &buffer);
                        }
                    }
                }
            }
        }
    }
    out
}

#[test]
fn classic_modes_match_pre_redesign() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/client/shell/tests/fixtures/classic-modes-phase0.txt"
    );
    assert_eq!(matrix(), std::fs::read_to_string(path).unwrap());
}
