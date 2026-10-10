//! Build script wdk-rs : configure les flags linker/WDK selon la config
//! déclarée dans `[package.metadata.wdk.driver-model]`.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
