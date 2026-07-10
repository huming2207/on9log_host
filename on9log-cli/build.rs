use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("web").join("dist");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded_web.rs");

    println!("cargo:rerun-if-changed={}", dist_dir.display());

    let mut assets = Vec::new();
    if !dist_dir.is_dir() {
        fail_missing_dist(&dist_dir, "directory does not exist");
    }
    collect_assets(&dist_dir, &dist_dir, &mut assets);
    assets.sort_by(|a, b| a.0.cmp(&b.0));
    if assets.is_empty() {
        fail_missing_dist(&dist_dir, "directory contains no files");
    }
    if !assets.iter().any(|(path, _)| path == "index.html") {
        fail_missing_dist(&dist_dir, "index.html is missing");
    }

    let mut out = String::from("const WEB_ASSETS: &[EmbeddedAsset] = &[\n");
    for (path, file) in assets {
        println!("cargo:rerun-if-changed={}", file.display());
        let mime = mime_for_path(&path);
        out.push_str(&format!(
            "    EmbeddedAsset {{ path: {:?}, mime: {:?}, bytes: include_bytes!(r#\"{}\"#) }},\n",
            path,
            mime,
            file.display()
        ));
    }
    out.push_str("];\n");

    let mut file = fs::File::create(out_path).unwrap();
    file.write_all(out.as_bytes()).unwrap();
}

fn fail_missing_dist(dist_dir: &Path, reason: &str) -> ! {
    panic!(
        "on9log web UI bundle is required but {reason}: {}\n\
         Run `cd on9log-cli/web && bun run build` before building `on9log_cli`.",
        dist_dir.display()
    );
}

fn collect_assets(root: &Path, dir: &Path, assets: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
            collect_assets(root, &path, assets);
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap();
            let web_path = rel
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            assets.push((web_path, path));
        }
    }
}

fn mime_for_path(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
