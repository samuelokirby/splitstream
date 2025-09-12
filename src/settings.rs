use config::Config;
use serde::Deserialize;

/// Settings represents the application settings loaded from settings.toml
#[derive(Deserialize)]
pub struct Settings {
    pub compliance_mode_on_start: bool,
}

impl Settings {
    /// Load settings from settings.toml by using config's builder pattern and deserializing into the struct
    pub fn new() -> Self {
        let settings = Config::builder()
            .add_source(config::File::with_name("./settings.toml"))
            .build()
            .unwrap();

        // Deserialize the entire config into the struct
        settings.try_deserialize().unwrap()
    }
}
