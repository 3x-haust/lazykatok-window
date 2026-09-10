use anyhow::{Context, Result};
#[cfg(not(windows))]
use lazykatok::adapters::MacosAdapter;
use lazykatok::adapters::{FixtureAdapter, KakaocliAdapter, SourceAdapter};
use std::path::{Path, PathBuf};

pub(super) fn adapter_for_source(
    source: &str,
    path: Option<PathBuf>,
    #[cfg_attr(windows, allow(unused_variables))] data_dir: &Path,
) -> Result<Box<dyn SourceAdapter>> {
    match source {
        "fixture" => {
            let fixture_path = path.context("fixture source requires a JSONL path")?;
            Ok(Box::new(FixtureAdapter::new(fixture_path)))
        }
        "kakaocli" => Ok(Box::new(KakaocliAdapter)),
        #[cfg(windows)]
        "windows" | "kakao" => Ok(Box::new(lazykatok::adapters::WindowsAdapter::default())),
        #[cfg(not(windows))]
        "windows" => {
            anyhow::bail!("The Windows source must run on Windows with KakaoTalk installed")
        }
        #[cfg(not(windows))]
        "macos" | "kakao" => {
            let home = lazykatok::kakao::default_home().context("resolve home directory")?;
            Ok(Box::new(MacosAdapter::new(home, data_dir.to_path_buf())))
        }
        #[cfg(windows)]
        "macos" => {
            anyhow::bail!("The macOS source is unavailable on Windows; use --source windows")
        }
        other => anyhow::bail!("unsupported source adapter: {other}"),
    }
}
