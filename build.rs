// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    pack_espeak_data();
    tauri_build::build();
}

/// Pack espeak-ng's phoneme data into one file for the binary to embed.
///
/// espeak-rs-sys builds the data into its own `OUT_DIR` and compiles that
/// absolute path in as espeak-ng's fallback location, which resolves to nothing
/// on any other machine. The crate exports no metadata naming the directory, so
/// it is located by walking the build directory Cargo gave this script. It is a
/// build-dependency as well as a normal one, because a normal dependency is not
/// built when a build script runs.
fn pack_espeak_data() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let source = find_espeak_data(&out_dir)
        .expect("espeak-ng-data was not found under the build directory");

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect(&source, &source, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut packed = Vec::new();
    packed.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (name, path) in &files {
        let body = fs::read(path).expect("read espeak data file");
        packed.extend_from_slice(&(name.len() as u32).to_le_bytes());
        packed.extend_from_slice(name.as_bytes());
        packed.extend_from_slice(&(body.len() as u32).to_le_bytes());
        packed.extend_from_slice(&body);
    }

    let target = out_dir.join("espeak-ng-data.pack");
    let mut file = fs::File::create(&target).expect("create the espeak data pack");
    file.write_all(&packed).expect("write the espeak data pack");

    println!("cargo:rerun-if-changed={}", source.display());
}

/// The `espeak-ng-data` directory espeak-rs-sys installed for this profile.
///
/// `OUT_DIR` is `<target>/<profile>/build/<crate>-<hash>/out`, so its
/// grandparent holds every build script's output for the same profile.
fn find_espeak_data(out_dir: &Path) -> Option<PathBuf> {
    let builds = out_dir.parent()?.parent()?;
    let mut found: Option<PathBuf> = None;
    for entry in fs::read_dir(builds).ok()? {
        let entry = entry.ok()?;
        if !entry.file_name().to_string_lossy().starts_with("espeak-rs-sys-") {
            continue;
        }
        let data = entry.path().join("out/share/espeak-ng-data");
        // A build directory holds one entry per crate version and one more per
        // rebuild; the newest is the one this build just linked against.
        if data.join("phontab").is_file() {
            let stamp = data.metadata().and_then(|m| m.modified()).ok();
            let keep = match &found {
                None => true,
                Some(old) => old.metadata().and_then(|m| m.modified()).ok() < stamp,
            };
            if keep {
                found = Some(data);
            }
        }
    }
    found
}

fn collect(root: &Path, dir: &Path, into: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, into);
        } else if let Ok(name) = path.strip_prefix(root) {
            into.push((name.to_string_lossy().replace('\\', "/"), path.clone()));
        }
    }
}
