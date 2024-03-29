use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub bg_color: (f32, f32, f32),

    pub xkb_layout: String,
    pub xkb_options: Option<String>,

    pub pointer: HashMap<String, PointerConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PointerConfig {
    pub tap_to_click: Option<bool>,
    pub natural_scroll: Option<bool>,
}

impl Config {
    pub fn new() -> Self {
        match config_path() {
            None => Self::default(),
            Some(path) => {
                let contents =
                    std::fs::read_to_string(path).expect("could not read the config file");
                toml_edit::de::from_str(&contents).expect("config error")
            }
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bg_color: (0.2, 0.1, 0.2),
            xkb_layout: String::new(),
            xkb_options: None,
            pointer: HashMap::new(),
        }
    }
}

fn config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(env::var_os("HOME")?).join(".config")))
}

fn config_path() -> Option<PathBuf> {
    let mut path = config_dir()?;
    path.push("ewc");
    path.push("config.toml");
    path.exists().then_some(path)
}
