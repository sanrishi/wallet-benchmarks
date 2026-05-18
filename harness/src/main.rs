mod config;
mod driver;
mod metrics;

use std::fs::read_to_string;
use std::path::Path;

use config::Config;

fn main() {
    let config_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config.toml");
    let config_raw = read_to_string(&config_path).unwrap_or_else(|error| {
        panic!("failed to read config from {}: {error}", config_path.display())
    });
    let config: Config = toml::from_str(&config_raw).unwrap_or_else(|error| {
        panic!(
            "failed to parse config from {}: {error}",
            config_path.display()
        )
    });

    println!("{config:?}");
}
