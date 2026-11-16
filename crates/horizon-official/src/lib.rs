//! The official game: the platform's game (`horizon-game`) with Horizon's own in-game parts
//! plugged in through the platform's hooks (docs/layers.md, "How a Horizon crate plugs in").
//! The release builds ship its executable under the game's name, `horizon-client`
//! (`game-build.yml`); horizon-client keeps its own plain executable.
//!
//! Each Horizon crate that runs in the game gives a `&'static` value implementing
//! `horizon_platform::hooks::Hooks`, and its settings store and pages if it has any, and
//! [`register`] adds them. Nothing here depends on an engine crate.
#![forbid(unsafe_code)]

use horizon_platform::hooks::{self, Hooks};
use horizon_platform::porter::{self, PortOutcome, Porter, PreparedMirror};
use horizon_platform::{pages, settings};

struct OfficialPorter;

impl Porter for OfficialPorter {
    fn default_prepared_url(&self) -> Option<&str> {
        Some(horizon_addresses::MOBILE_PREPARED_26_3_ZIP)
    }

    fn simulation_base_url(&self) -> Option<&str> {
        Some(horizon_addresses::MOBILE_SIMULATION_BASE_26_3_ZIP)
    }

    fn catalog_urls(&self) -> Vec<String> {
        horizon_addresses::MOBILE_VERSION_CATALOGS
            .iter()
            .map(|u| (*u).to_owned())
            .collect()
    }

    fn prepared_mirrors(&self) -> Vec<PreparedMirror> {
        horizon_addresses::MOBILE_PREPARED_MIRRORS
            .iter()
            .map(|(id, label, version, url, notes)| PreparedMirror {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
                version: (*version).to_owned(),
                url: (*url).to_owned(),
                notes: (*notes).to_owned(),
                sha256: None,
                bytes: None,
            })
            .collect()
    }

    fn search_modrinth(
        &self,
        query: &str,
        minecraft_version: Option<&str>,
        loader: Option<&str>,
        sort: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<String, String> {
        horizon_port_pack::store::search_projects(
            query,
            minecraft_version,
            loader,
            sort,
            limit,
            offset,
        )
    }

    fn modrinth_versions(
        &self,
        project_id: &str,
        minecraft_version: Option<&str>,
        loader: Option<&str>,
    ) -> Result<String, String> {
        horizon_port_pack::store::project_versions(project_id, minecraft_version, loader)
    }

    fn install_modrinth(
        &self,
        project_id: &str,
        version_id: Option<&str>,
        minecraft_version: Option<&str>,
        mods_dir: &std::path::Path,
        work_dir: &std::path::Path,
    ) -> Result<PortOutcome, String> {
        let out = horizon_port_pack::store::install_from_modrinth(
            project_id,
            version_id,
            minecraft_version,
            mods_dir,
            work_dir,
        )?;
        Ok(to_outcome(out))
    }

    fn port_jar(
        &self,
        jar_path: &std::path::Path,
        mods_dir: Option<&std::path::Path>,
        work_dir: &std::path::Path,
    ) -> Result<PortOutcome, String> {
        let out = horizon_port_pack::store::port_local_jar(jar_path, mods_dir, work_dir)?;
        Ok(to_outcome(out))
    }
}

fn to_outcome(out: horizon_port_pack::store::InstalledPort) -> PortOutcome {
    PortOutcome {
        mod_id: out.metadata.mod_id,
        name: out.metadata.name,
        version: out.metadata.version,
        loader: out.metadata.loader,
        verdict: out.metadata.verdict,
        summary: out.metadata.summary,
        ports: out.metadata.ports,
        skipped: out.metadata.skipped,
        namespaces: out.metadata.namespaces,
        installed_dir: out.installed_dir,
        project_id: out.metadata.project_id,
        version_id: out.metadata.version_id,
        icon_url: out.metadata.icon_url,
    }
}

static OFFICIAL_PORTER: OfficialPorter = OfficialPorter;

/// Every Horizon part in the game, in the order its hooks run.
const PARTS: [&dyn Hooks; 5] = [
    // The anonymous "I'm playing" ping.
    &horizon_heartbeat::HOOKS,
    // Play with Friends: `friend:` addresses join through its tunnel.
    &horizon_friends::game::HOOKS,
    // The official server in the player's server list.
    &horizon_official_servers::HOOKS,
    // "Playing Horizon" on the player's Discord profile.
    &horizon_discord::HOOKS,
    // Open and Share for saved performance captures.
    &horizon_perf_share::HOOKS,
];
/// Their settings, in the player's settings files.
const STORES: [&dyn settings::Store; 1] = [&horizon_discord::SETTING];
/// Their rows and pages in the options screens.
const PAGES: [&dyn pages::Page; 1] = [&horizon_discord::SECTION];

/// Registers Horizon's in-game parts with the game. The executable calls it before it runs the
/// game, and so does the launcher's generated executable for native mods.
pub fn register() {
    if let Err(e) = porter::register(&OFFICIAL_PORTER) {
        log::warn!("Horizon: mod/version porter did not register: {e}");
    }
    let late = PARTS.into_iter().map(hooks::register);
    let late = late.chain(STORES.into_iter().map(settings::register));
    for result in late.chain(PAGES.into_iter().map(pages::register)) {
        if let Err(e) = result {
            // The executable registered after starting the game: a bug, not a player's problem.
            log::warn!("Horizon: an in-game part did not register: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_platform::porter::Porter;

    #[test]
    fn official_porter_exposes_prepared_mirrors_and_catalogs() {
        let mirrors = OFFICIAL_PORTER.prepared_mirrors();
        assert!(!mirrors.is_empty());
        assert!(mirrors.iter().any(|m| m.version == "26.3" && m.url.starts_with("https://")));
        assert!(!OFFICIAL_PORTER.catalog_urls().is_empty());
        assert!(OFFICIAL_PORTER.default_prepared_url().is_some());
    }
}
