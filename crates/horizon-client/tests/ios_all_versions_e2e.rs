//! End-to-end integration test for multi-version installation, active root switching,
//! protocol adapter preference, and `Documents/` `.hmod` auto-import (`import::scan`).

use horizon_client::{import, launcher_mods, net, versions};
use horizon_protocol::adapter::{ServerVersion, preferred_server_version};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

fn temp_home(tag: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "horizon-ios-e2e-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_prepared_tree(root: &Path, mc_version: &str) {
    fs::create_dir_all(root.join("assets/minecraft/items")).unwrap();
    fs::create_dir_all(root.join("data/minecraft")).unwrap();
    fs::create_dir_all(root.join("worldgen")).unwrap();
    fs::write(
        root.join("horizon-resources.json"),
        format!(r#"{{"schema":7,"minecraft":{mc_version:?}}}"#),
    )
    .unwrap();
    fs::write(root.join("registries.json"), b"{}").unwrap();
    fs::write(root.join("facts.json"), b"{}").unwrap();
}

fn write_valid_hmod(path: &Path, id: &str, name: &str) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("horizon-mod.toml", opts).unwrap();
    let manifest = format!(
        "id = {id:?}\nname = {name:?}\nversion = \"1.2.0\"\ndescription = \"E2E content mod\"\napi = \"0.2\"\ncapabilities = [\"registries\"]\n"
    );
    zip.write_all(manifest.as_bytes()).unwrap();
    zip.start_file(format!("assets/{id}/models/item/orb.json"), opts)
        .unwrap();
    zip.write_all(br#"{"parent":"minecraft:item/generated"}"#)
        .unwrap();
    zip.finish().unwrap();
}

#[test]
fn end_to_end_multi_version_handoff_protocol_preference_and_documents_mod_scan() {
    let home = temp_home("all-versions");

    // 1. Install two versions under `Documents/versions/<id>/resources`: 26.3 and 1.20.4.
    let v26_3 = home.join("versions/26.3/resources");
    let v1_20_4 = home.join("versions/1.20.4/resources");
    write_prepared_tree(&v26_3, "26.3");
    write_prepared_tree(&v1_20_4, "26.3");
    fs::write(
        v1_20_4.join("version-profile.json"),
        r#"{"id":"1.20.4","release_type":"release","source":"mojang-official","full_simulation":true,"files":4,"bytes":128}"#,
    )
    .unwrap();

    // 2. Activate 1.20.4 and verify dynamic launch root + protocol preference (V1_20_3 / 765).
    versions::select_version(&home, "1.20.4").expect("select 1.20.4");
    assert_eq!(versions::active_version_id(&home), "1.20.4");
    // Even if `sibling` points to the previously loaded `v26_3` inside `home`, `resources_root`
    // must switch to the newly selected `v1_20_4`!
    assert_eq!(
        import::resources_root(&v26_3, Some(&home)),
        Some(v1_20_4.clone())
    );
    // An external desktop `--resources` path outside `home` still takes priority.
    let external = temp_home("external-resources");
    write_prepared_tree(&external, "26.3");
    assert_eq!(
        import::resources_root(&external, Some(&home)),
        Some(external.clone())
    );
    let _ = fs::remove_dir_all(&external);

    assert_eq!(preferred_server_version(), Some(ServerVersion::V1_20_3));
    assert!(net::older_servers_enabled());

    // 3. Drop a `.hmod` file into `Documents/` and verify `import::scan` installs & enables it!
    let dropped_hmod = home.join("sky_lanterns.hmod");
    write_valid_hmod(&dropped_hmod, "sky_lanterns", "Sky Lanterns");
    import::scan(&home);
    assert!(
        !dropped_hmod.exists(),
        "scan removes source .hmod after importing into horizon-mods/"
    );
    let mods = launcher_mods::list_installed(&home);
    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0].id, "sky_lanterns");
    assert!(mods[0].enabled);

    // 4. Switch back to 26.3 and verify preferred older server resets to None.
    versions::select_version(&home, "26.3").expect("select 26.3");
    assert_eq!(preferred_server_version(), None);

    let _ = fs::remove_dir_all(&home);
}
