//! Platform registry for mobile/desktop prepared-version catalogs and pure-Rust Java/Modrinth mod
//! porting (`horizon-port-pack`), wired by `horizon-official` before `horizon_client::run()`.
//!
//! Keeping this trait in `horizon-hooks` (re-exported as `horizon_platform::porter`) lets the
//! engine (`horizon-client`) discover remote prepared mirrors, search Modrinth, and port Fabric /
//! NeoForge / Forge / Quilt `.jar` mods without depending on `horizon-addresses` or `platform`
//! crates directly.

use crate::{Registry, TooLate};
use std::path::Path;

/// One downloadable prepared Horizon game-files mirror entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedMirror {
    pub id: String,
    pub label: String,
    pub version: String,
    pub url: String,
    pub notes: String,
    pub sha256: Option<String>,
    pub bytes: Option<u64>,
}

/// Summary of checking or porting a Java `.jar` or Modrinth mod into `horizon-mods/<mod_id>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortOutcome {
    pub mod_id: String,
    pub name: String,
    pub version: String,
    pub loader: String,
    /// `"works"`, `"partly"`, or `"no"`.
    pub verdict: String,
    pub summary: String,
    pub ports: Vec<String>,
    pub skipped: Vec<String>,
    pub namespaces: Vec<String>,
    pub installed_dir: Option<std::path::PathBuf>,
    pub project_id: Option<String>,
    pub version_id: Option<String>,
    pub icon_url: Option<String>,
}

/// Pure-Rust mod store & catalog bridge registered by a Horizon-layer crate (`horizon-official`).
pub trait Porter: Sync {
    /// Default prepared archive URL when the player has not typed a custom one.
    fn default_prepared_url(&self) -> Option<&str> {
        None
    }

    /// Remote JSON catalog URLs listing prepared Horizon bundles across versions.
    fn catalog_urls(&self) -> Vec<String> {
        Vec::new()
    }

    /// Built-in prepared bundle mirrors.
    fn prepared_mirrors(&self) -> Vec<PreparedMirror> {
        Vec::new()
    }

    /// Search Modrinth (`project_type:mod`) and return a JSON string describing hits.
    fn search_modrinth(
        &self,
        _query: &str,
        _minecraft_version: Option<&str>,
        _loader: Option<&str>,
        _sort: Option<&str>,
        _limit: usize,
        _offset: usize,
    ) -> Result<String, String> {
        Err("Modrinth bridge is not registered in this build".into())
    }

    /// List compatible versions of a Modrinth project as a JSON string.
    fn modrinth_versions(
        &self,
        _project_id: &str,
        _minecraft_version: Option<&str>,
        _loader: Option<&str>,
    ) -> Result<String, String> {
        Err("Modrinth bridge is not registered in this build".into())
    }

    /// Download a Modrinth mod `.jar`, verify its SHA-1, port its content into `mods_dir/<mod_id>`,
    /// and return what ported.
    fn install_modrinth(
        &self,
        _project_id: &str,
        _version_id: Option<&str>,
        _minecraft_version: Option<&str>,
        _mods_dir: &Path,
        _work_dir: &Path,
    ) -> Result<PortOutcome, String> {
        Err("Modrinth installer is not registered in this build".into())
    }

    /// Inspect or port a local Fabric / NeoForge / Forge / Quilt `.jar` file. When `mods_dir` is
    /// `Some`, installs the carried folder mod into `mods_dir/<mod_id>`.
    fn port_jar(
        &self,
        _jar_path: &Path,
        _mods_dir: Option<&Path>,
        _work_dir: &Path,
    ) -> Result<PortOutcome, String> {
        Err("Java mod porter is not registered in this build".into())
    }
}

static PORTERS: Registry<dyn Porter> = Registry::new();

/// Register a [`Porter`] implementation before the game starts.
pub fn register(porter: &'static dyn Porter) -> Result<(), TooLate> {
    PORTERS.register(porter)
}

pub(crate) fn all() -> &'static [&'static dyn Porter] {
    PORTERS.all()
}

/// Engine-side dispatch helpers (`horizon-client`).
pub mod engine {
    use super::*;

    pub fn active() -> Option<&'static dyn Porter> {
        all().first().copied()
    }

    pub fn default_prepared_url() -> Option<String> {
        all()
            .iter()
            .find_map(|p| p.default_prepared_url().map(str::to_owned))
    }

    pub fn catalog_urls() -> Vec<String> {
        let mut out = Vec::new();
        for p in all() {
            for url in p.catalog_urls() {
                if !out.contains(&url) {
                    out.push(url);
                }
            }
        }
        out
    }

    pub fn prepared_mirrors() -> Vec<PreparedMirror> {
        let mut out = Vec::new();
        for p in all() {
            out.extend(p.prepared_mirrors());
        }
        out
    }
}
