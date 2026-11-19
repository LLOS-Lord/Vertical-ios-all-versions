//! End-to-end integration tests for fail-closed multi-version installation, simulation-base
//! acquisition, partial Mojang refusal, active root switching, symlink aliases and `Documents/`
//! `.hmod` auto-import (`import::scan`).
//!
//! A 26.3 prepared tree under a foreign profile is *not* a playable foreign version. Foreign
//! versions need a real per-version Rust pack; these tests guard that boundary on the launcher's
//! public API. Synthetic fixtures prove contracts, not that their bytes are real Minecraft data.

use horizon_client::{import, launcher_mods, versions};
use horizon_protocol::adapter::set_preferred_server_version;
use serde_json::json;
use sha1::{Digest as _, Sha1};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

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

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn write_prepared_tree(root: &Path, mc_version: &str) {
    fs::create_dir_all(root.join("assets/minecraft/items")).unwrap();
    fs::create_dir_all(root.join("assets/minecraft/font")).unwrap();
    fs::create_dir_all(root.join("assets/minecraft/models/block")).unwrap();
    fs::create_dir_all(root.join("assets/minecraft/blockstates")).unwrap();
    fs::create_dir_all(root.join("assets/minecraft/textures/gui/sprites")).unwrap();
    fs::create_dir_all(root.join("assets/minecraft/lang")).unwrap();
    fs::create_dir_all(root.join("data/minecraft")).unwrap();
    fs::create_dir_all(root.join("worldgen/blocks")).unwrap();
    fs::create_dir_all(root.join("biome-parameters")).unwrap();
    let manifest = json!({
        "schema": 7,
        "data_version": horizon_world::DATA_VERSION,
        "minecraft": mc_version,
    });
    fs::write(
        root.join("horizon-resources.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    for name in versions::REQUIRED_SIMULATION_FILES {
        fs::write(root.join(name), b"{}").unwrap();
    }
    for rel in [
        "worldgen/blocks/block-properties.nbt",
        "worldgen/blocks/cached-collision.nbt",
        "worldgen/blocks/cached-support.nbt",
        "assets/minecraft/font/default.json",
    ] {
        fs::write(root.join(rel), b"{}").unwrap();
    }
    fs::write(
        root.join("assets/pack.mcmeta"),
        br#"{"pack":{"pack_format":55}}"#,
    )
    .unwrap();
    fs::write(root.join("assets/minecraft/sounds.json"), b"{}").unwrap();
    fs::write(root.join("assets/minecraft/lang/en_us.json"), b"{}").unwrap();
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
fn end_to_end_fake_version_is_refused_and_native_handoff_plus_mod_scan_work() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_preferred_server_version(None);
    let home = temp_home("all-versions");

    // A pinned 26.3 root is playable. A legacy fake 1.20.4 root has 26.3 facts plus a
    // 1.20.4 profile; it must be refused rather than activating 26.3 under a foreign label.
    let v26_3 = home.join("versions/26.3/resources");
    let v1_20_4 = home.join("versions/1.20.4/resources");
    write_prepared_tree(&v26_3, "26.3");
    write_prepared_tree(&v1_20_4, "26.3");
    fs::write(
        v1_20_4.join("version-profile.json"),
        r#"{"id":"1.20.4","release_type":"release","source":"mojang-official","full_simulation":true,"files":4,"bytes":128,"installed_epoch":10}"#,
    )
    .unwrap();

    let err = versions::validate_playable_root(&v1_20_4).unwrap_err();
    assert!(
        err.contains("fake version") || err.contains("version pack"),
        "{err}"
    );
    assert!(versions::select_version(&home, "1.20.4").is_err());
    assert_eq!(versions::active_version_id(&home), "26.3");
    assert_eq!(versions::version_id_for_root(&v26_3), "26.3");
    assert_eq!(versions::version_id_for_root(&v1_20_4), "1.20.4");

    let native = import::resources_root(&v26_3, Some(&home)).expect("native root");
    assert_eq!(
        fs::canonicalize(native).unwrap(),
        fs::canonicalize(&v26_3).unwrap()
    );
    // Simulate iOS `/var` -> `/private/var` symlink disparity between `home` and `sibling`.
    #[cfg(unix)]
    {
        let alias_parent = temp_home("ios-symlink-alias");
        let symlink_home = alias_parent.join("var_documents");
        std::os::unix::fs::symlink(&home, &symlink_home).unwrap();
        let canonical_v26_3 = fs::canonicalize(&v26_3).unwrap();
        let resolved = import::resources_root(&canonical_v26_3, Some(&symlink_home))
            .expect("symlinked home still finds the pinned root");
        assert_eq!(
            fs::canonicalize(resolved).unwrap(),
            fs::canonicalize(&v26_3).unwrap()
        );
        let _ = fs::remove_dir_all(&alias_parent);
    }
    let external = temp_home("external-resources");
    write_prepared_tree(&external, "26.3");
    assert_eq!(
        import::resources_root(&external, Some(&home)),
        Some(external.clone())
    );
    let _ = fs::remove_dir_all(&external);

    // A `.hmod` dropped into `Documents/` still installs and enables.
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

    versions::select_version(&home, "26.3").expect("select native 26.3");
    assert_eq!(versions::active_version_id(&home), "26.3");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn partial_mojang_install_stays_partial_and_sim_base_cannot_fake_1_20_4() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_preferred_server_version(None);
    let home = temp_home("partial-repair");

    // A build-4.1 iPhone state: Mojang 1.20.4 assets unpacked without simulation base.
    let jar_path = home.join("minecraft/versions/1.20.4/1.20.4.jar");
    fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
    {
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("assets/minecraft/textures/block/cobblestone.png", opts)
            .unwrap();
        zip.write_all(b"COBBLESTONE_1_20_4").unwrap();
        zip.finish().unwrap();
    }
    let sound_bytes = b"OGG_MENU_1_20_4";
    let sound_sha1 = hex_lower(&Sha1::digest(sound_bytes));
    let objects_dir = home.join("minecraft/assets/objects");
    let obj_path = objects_dir.join(&sound_sha1[..2]).join(&sound_sha1);
    fs::create_dir_all(obj_path.parent().unwrap()).unwrap();
    fs::write(&obj_path, sound_bytes).unwrap();
    let asset_index = json!({
        "objects": {
            "minecraft/sounds/ui/button/click.ogg": {
                "hash": sound_sha1,
                "size": sound_bytes.len()
            }
        }
    });
    fs::create_dir_all(home.join("minecraft/assets/indexes")).unwrap();
    fs::write(
        home.join("minecraft/assets/indexes/12.json"),
        serde_json::to_vec(&asset_index).unwrap(),
    )
    .unwrap();
    fs::write(
        home.join("minecraft/versions/1.20.4/1.20.4.json"),
        br#"{"id":"1.20.4","type":"release","assetIndex":{"id":"12"}}"#,
    )
    .unwrap();

    let partial = versions::prepare_mojang_on_device(
        &home,
        "1.20.4",
        "release",
        &jar_path,
        &asset_index,
        &objects_dir,
    )
    .expect("partial prepare");
    assert!(!partial.full_simulation);
    let ver_root = home.join("versions/1.20.4/resources");
    assert!(versions::validate_playable_root(&ver_root).is_err());
    assert!(versions::has_any_partial_mojang(&home));
    assert_eq!(versions::active_ready_root(&home), None);

    // Serve a compact 26.3 simulation base over local HTTP. Repair may install it, but must NOT
    // promote 1.20.4 to playable by hard-linking those 26.3 tables.
    let sim_src = home.join("sim-base-build");
    write_prepared_tree(&sim_src, "26.3");
    let sim_zip = home.join("sim-base-26.3.zip");
    import::pack(&sim_src, &sim_zip, |_, _| {}).unwrap();
    let sim_bytes = fs::read(&sim_zip).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut req = [0u8; 2048];
            let _ = stream.read(&mut req);
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nETag: \"sim1\"\r\nConnection: close\r\n\r\n",
                sim_bytes.len()
            );
            let _ = stream.write_all(hdr.as_bytes());
            let _ = stream.write_all(&sim_bytes);
        }
    });

    let sim_url = format!("http://127.0.0.1:{port}/sim-base-26.3.zip");
    versions::start_repair_versions(home.clone(), Some("1.20.4".into()), Some(sim_url))
        .expect("start repair");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = versions::job_status();
        if !st.active {
            assert!(
                !st.done,
                "repair must fail closed without a real 1.20.4 version pack"
            );
            let err = st.error.expect("repair error");
            assert!(
                err.contains("version pack") || err.contains("not playable"),
                "{err}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "repair job timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
    server.join().unwrap();

    let sim_root = home.join("versions/.sim-base-26.3/resources");
    versions::validate_playable_root(&sim_root).expect("26.3 base itself is playable");
    assert!(versions::validate_playable_root(&ver_root).is_err());
    assert!(versions::has_any_partial_mojang(&home));
    assert_eq!(versions::active_ready_root(&home), None);
    assert_eq!(versions::active_version_id(&home), "26.3");
    assert_eq!(
        fs::read(ver_root.join("assets/minecraft/textures/block/cobblestone.png")).unwrap(),
        b"COBBLESTONE_1_20_4"
    );
    assert_eq!(
        fs::read(ver_root.join("assets/minecraft/sounds/ui/button/click.ogg")).unwrap(),
        sound_bytes
    );
    // 26.3 simulation tables and its single-file cache must never materialise in the foreign root.
    assert!(!ver_root.join("model-states.json").is_file());
    assert!(!ver_root.join("creative-inventory.json").is_file());
    assert!(!ver_root.join("assets/minecraft/items/stone.json").is_file());
    assert!(
        !ver_root
            .join(horizon_core::prepared_files::FILE_NAME)
            .is_file()
    );
    assert!(versions::select_version(&home, "1.20.4").is_err());

    let _ = fs::remove_dir_all(&home);
}
