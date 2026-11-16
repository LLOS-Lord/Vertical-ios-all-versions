//! Every built-in address of an outside service vanilla doesn't contact, in one place
//! (docs/layers.md#built-in-addresses). `tools/check_addresses.py` keeps new ones from going
//! anywhere else.
//!
//! This is a Horizon-layer crate: the engine and the platform never depend on it. Platform code
//! that talks to a service gets the address passed in from a Horizon crate.

/// Horizon's official survival server (SMP), offered once at the top of the server list by the
/// game and the launcher (`launcher/links.json` may name another). Its SRV record leads to the
/// port.
pub const OFFICIAL_SERVER: &str = "play.playhorizon.dev";

/// The official server's earlier addresses: the game and the launcher move a server list's
/// entries for them to [`OFFICIAL_SERVER`], once. A move appends the old address here (and to
/// `official_server_previous` in `launcher/links.json`).
pub const OFFICIAL_SERVER_PREVIOUS: &[&str] = &["horizon.bunchy7s.dev"];

/// Afri-Network, a partner server the game added once below the official server (#9299) until
/// the partnership ended: kept so the game can take back out the entry it added.
pub const WITHDRAWN_PARTNER_SERVER: &str = "afri-network.co.za";

/// [`OFFICIAL_SERVER`] with the port its SRV record gives, for askers that don't look SRV up
/// (the telemetry server's status checks).
pub const OFFICIAL_SERVER_STATUS: &str = "play.playhorizon.dev:24454";

/// Horizon's website, where the launcher is downloaded: the game started without its prepared
/// files sends the player there.
pub const WEBSITE: &str = "https://playhorizon.dev";

/// The cosmetics service: cosmetics, emotes, "Show me as a Horizon player"
/// (`HORIZON_COSMETICS_API` overrides it).
pub const COSMETICS_API: &str = "https://cosmetics.playhorizon.dev";

/// The telemetry ingest server (`crates/horizon-telemetry-server`): opt-in telemetry, the
/// "I'm playing" ping, benchmark uploads (`HORIZON_TELEMETRY_URL` overrides it).
pub const TELEMETRY: &str = "https://telemetry.playhorizon.dev";

/// The performance viewer page a saved capture opens in (`HORIZON_PERF_VIEWER` overrides it).
pub const PERF_VIEWER: &str = "https://playhorizon.dev/perf";

/// The capture upload API behind Share (`HORIZON_PERF_CAPTURES` overrides it).
pub const PERF_CAPTURES: &str = "https://playhorizon.dev/api/perf/captures";

/// Public STUN servers for Play with Friends' NAT discovery: two independent operators, so one
/// outage doesn't break discovery (`HORIZON_STUN_SERVERS` overrides them).
pub const STUN_SERVERS: &[&str] = &["stun.cloudflare.com:3478", "stun.l.google.com:19302"];

/// Play with Friends' troubleshooting page, shown to hosts nobody can reach.
pub const FRIENDS_HELP: &str =
    "https://github.com/horizon-rust/horizon/blob/main/docs/play-with-friends.md#troubleshooting";

/// Remote JSON catalogs listing prepared Horizon resource bundles across game versions for mobile
/// launchers (`HORIZON_VERSION_CATALOG_URL` overrides the primary catalog).
pub const MOBILE_VERSION_CATALOGS: &[&str] = &[
    "https://raw.githubusercontent.com/LLOS-Lord/Vertical-ios/main/packaging/ios/versions-catalog.json",
    "https://horizon-rust.github.io/horizon-mods/versions-catalog.json",
];

/// Default prepared game-files mirror archive for mobile launchers (`HORIZON_GAME_FILES_URL`
/// overrides it).
pub const MOBILE_PREPARED_26_3_ZIP: &str = "https://cdn.gayfeet.lol/horizon/26.3.zip";

/// Built-in prepared Horizon bundle mirrors (`(id, label, version, url, notes)`), passed into the
/// engine by `horizon-official` so mobile launchers can browse mirrors or add custom server URLs.
pub const MOBILE_PREPARED_MIRRORS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "vertical-cdn-26.3",
        "Vertical Community Mirror (26.3 Full Prepared)",
        "26.3",
        "https://cdn.gayfeet.lol/horizon/26.3.zip",
        "Full PC horizon-prepare bundle (simulation tables + Mojang 26.3 assets). Unlocks full offline simulation for all Mojang versions.",
    ),
    (
        "github-release-26.3",
        "Vertical-iOS GitHub Releases (26.3 Prepared)",
        "26.3",
        "https://github.com/LLOS-Lord/Vertical-ios/releases/latest/download/horizon-resources-26.3.zip",
        "GitHub Release mirror for prepared 26.3 game files (~589 MB).",
    ),
];

