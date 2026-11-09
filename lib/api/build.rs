#[cfg(feature = "v8")]
fn build_v8() {
    use bindgen::callbacks::ParseCallbacks;
    use std::{
        env, fs,
        path::{Path, PathBuf},
        sync::{LazyLock, Mutex},
    };

    const WEE8_RELEASE_VERSION: &str = "11.9.7";
    const NEXT_LINUX_CACHE: &str = "next-13.6.233.17-r42556478120-56d46612";
    const NEXT_LINUX_ASSET: &str =
        "https://api.github.com/repos/wasmerio/wasmer-next/releases/assets/726693826";
    const NEXT_LINUX_ARCHIVE_SHA: &str =
        "56d466127af223f0fc4ca453d6326bcd8db8eeba50482326def81c9674949a41";
    const NEXT_LINUX_LIBRARY_SHA: &str =
        "8624c72e07e7c4c08f12fa7274ff57ddc3f11ddc3b3a74c45bc246a088d3496c";

    fn assert_sha256(mut input: impl std::io::Read, expected: &str, kind: &str) {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = input
                .read(&mut buffer)
                .unwrap_or_else(|_| panic!("cannot read accepted wee8 {kind} for verification"));
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        assert_eq!(
            format!("{:x}", digest.finalize()),
            expected,
            "accepted wee8 {kind} checksum mismatch; remove the affected owned cache or use WASMER_V8_ARTIFACT_DIR"
        );
    }

    let (asset_name, platform_name) = match (
        env::var("CARGO_CFG_TARGET_OS").unwrap().as_str(),
        env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str(),
        env::var("CARGO_CFG_TARGET_ENV")
            .unwrap_or_default()
            .as_str(),
    ) {
        ("macos", "aarch64", _) => ("v8-darwin-aarch64.tar.xz", "darwin-aarch64"),
        ("linux", "x86_64", "gnu") => ("v8-linux-amd64.tar.xz", "linux-amd64"),
        ("linux", "x86_64", "musl") => ("v8-linux-musl.tar.xz", "linux-musl"),
        ("windows", "x86_64", _) => ("v8-windows-amd64.tar.xz", "windows-amd64"),
        ("android", "aarch64", _) => ("v8-android-arm64.tar.xz", "android-arm64"),
        (os, arch, _) => panic!("target os + arch combination not supported: {os}, {arch}"),
    };

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = PathBuf::from(&out_dir);
    let crate_root = env::var("CARGO_MANIFEST_DIR").unwrap();
    let v8_header_path = PathBuf::from(&crate_root).join("third-party").join("wee8");
    let cache_dir = stable_target_dir(&out_path)
        .join("wee8-artifacts")
        .join(if platform_name == "linux-amd64" {
            NEXT_LINUX_CACHE
        } else {
            WEE8_RELEASE_VERSION
        })
        .join(platform_name);
    let archive_path = cache_dir.join(asset_name);
    let library_name = if cfg!(target_os = "windows") {
        "v8.lib"
    } else {
        "libv8.a"
    };
    println!("cargo:rerun-if-env-changed=WASMER_V8_ARTIFACT_DIR");
    let configured = env::var_os("WASMER_V8_ARTIFACT_DIR").map(|value| {
        let root = PathBuf::from(value);
        assert!(
            root.is_absolute(),
            "WASMER_V8_ARTIFACT_DIR must be an absolute artifact directory"
        );
        let root = root.canonicalize().unwrap_or_else(|error| {
            panic!(
                "cannot open WASMER_V8_ARTIFACT_DIR {}: {error}",
                root.display()
            )
        });
        let library = root.join("lib").join(library_name);
        assert!(
            library.is_file(),
            "WASMER_V8_ARTIFACT_DIR must contain lib/{library_name}: {}",
            root.display()
        );
        println!("cargo:rerun-if-changed={}", library.display());
        root.join("lib")
    });
    let explicit_artifact = configured.is_some();
    let pinned_linux = platform_name == "linux-amd64" && !explicit_artifact;
    let isolated_prefix = explicit_artifact || pinned_linux;
    let v8_lib_dir = configured.unwrap_or_else(|| cache_dir.join("lib"));
    let v8_lib_path = v8_lib_dir.join(library_name);

    // A configured input is authoritative, including failures after validation.
    // Never turn a bad/missing candidate into a download of the old provider.
    if !explicit_artifact && !v8_lib_path.exists() {
        fs::create_dir_all(&cache_dir).unwrap_or_else(|err| {
            panic!(
                "failed to create v8 cache dir {}: {err}",
                cache_dir.display()
            )
        });

        if !archive_path.exists() {
            use std::io::Write;

            let tar_data = if pinned_linux {
                // Credentials are used only for this private request. Do not emit
                // rerun-if-env-changed for them: Cargo persists watched values.
                let token = env::var("GH_TOKEN")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| {
                        env::var("GITHUB_TOKEN")
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                    })
                    .unwrap_or_else(|| panic!(
                        "accepted Linux wee8 download requires GH_TOKEN or GITHUB_TOKEN with Contents: read access to private wasmerio/wasmer-next; alternatively set WASMER_V8_ARTIFACT_DIR to a verified unpacked artifact"
                    ));
                let mut authorization =
                    ureq::http::HeaderValue::from_str(&format!("Bearer {}", token.trim()))
                        .unwrap_or_else(|_| {
                            panic!("invalid GitHub read-token header for private wee8")
                        });
                authorization.set_sensitive(true);
                let bytes = ureq::get(NEXT_LINUX_ASSET)
                    .header("Accept", "application/octet-stream")
                    .header("Authorization", authorization)
                    .header("User-Agent", "wasmer-v8-build")
                    .config()
                    .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
                    .timeout_global(Some(std::time::Duration::from_secs(180)))
                    .build()
                    .call()
                    .unwrap_or_else(|_| panic!(
                        "cannot download accepted private Linux wee8 asset; check token Contents: read access or use WASMER_V8_ARTIFACT_DIR"
                    ))
                    .body_mut()
                    .with_config()
                    .limit(200 * 1024 * 1024)
                    .read_to_vec()
                    .unwrap_or_else(|_| panic!("cannot read accepted private Linux wee8 asset"));
                assert_sha256(bytes.as_slice(), NEXT_LINUX_ARCHIVE_SHA, "archive");
                bytes
            } else {
                let url = format!(
                    "https://github.com/wasmerio/wee8-custom-builds/releases/download/{WEE8_RELEASE_VERSION}/{asset_name}"
                );
                ureq::get(&url)
                    .call()
                    .expect("failed to download v8")
                    .body_mut()
                    .with_config()
                    // Windows prebuilts are substantially larger.
                    .limit(200 * 1024 * 1024)
                    .read_to_vec()
                    .expect("failed to download v8 lib")
            };

            fs::write(&archive_path, tar_data).unwrap_or_else(|err| {
                panic!(
                    "failed to write v8 archive cache {}: {err}",
                    archive_path.display()
                )
            });
        }

        let tar_data = fs::read(&archive_path).unwrap_or_else(|err| {
            panic!(
                "failed to read v8 archive cache {}: {err}",
                archive_path.display()
            )
        });
        if pinned_linux {
            assert_sha256(
                tar_data.as_slice(),
                NEXT_LINUX_ARCHIVE_SHA,
                "cached archive",
            );
        }
        let unpack_tmp = tempfile::TempDir::new_in(&cache_dir).unwrap_or_else(|err| {
            panic!(
                "failed to create temporary wee8 unpack directory in {}: {err}",
                cache_dir.display()
            )
        });
        let tar = xz::read::XzDecoder::new(tar_data.as_slice());
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&cache_dir).unwrap_or_else(|err| {
            panic!(
                "failed to unpack v8 archive cache {} into {}: {err}",
                archive_path.display(),
                cache_dir.display()
            )
        });
    }

    if pinned_linux {
        let library = fs::File::open(&v8_lib_path)
            .unwrap_or_else(|_| panic!("cannot open accepted Linux wee8 library cache"));
        assert_sha256(library, NEXT_LINUX_LIBRARY_SHA, "library");
    }
    if !isolated_prefix {
        println!("cargo:rustc-link-search=native={}", v8_lib_dir.display());
    }

    // Rename the wasm-c-api symbols from V8 so they do not collide with
    // Wasmer's own wasm-c-api exports when this crate is linked into tests.
    static WEE8_RENAMED: LazyLock<Mutex<Vec<(String, String)>>> =
        LazyLock::new(|| Mutex::new(Vec::new()));

    #[derive(Debug)]
    struct Wee8Renamer {}
    impl ParseCallbacks for Wee8Renamer {
        /// This function will run for every extern variable and function. The returned value determines
        /// the link name in the bindings.
        fn generated_link_name_override(
            &self,
            item_info: bindgen::callbacks::ItemInfo<'_>,
        ) -> Option<String> {
            if item_info.name.starts_with("wasm") {
                let new_name = format!("wee8_{}", item_info.name);
                WEE8_RENAMED
                    .lock()
                    .expect("cannot lock WEE8_RENAMED")
                    .push((item_info.name.to_string(), new_name.clone()));
                Some(new_name)
            } else {
                None
            }
        }
    }

    let header_path = v8_header_path.join("wasm.h");
    let mut args = vec![];
    if cfg!(target_os = "macos") {
        args.push("-I/usr/local/include");
        args.push("-I/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include/c++/v1");
        args.push("-I/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include");
        args.push("-I/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/include");
        args.push("-I/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/System/Library/Frameworks");
    }
    let bindings = bindgen::Builder::default()
        .header(header_path.display().to_string())
        .clang_args(args)
        .derive_default(true)
        .derive_debug(true)
        .parse_callbacks(Box::new(Wee8Renamer {}))
        .generate()
        .expect("Unable to generate bindings for `v8`!");

    bindings
        .write_to_file(out_path.join("v8_bindings.rs"))
        .expect("Couldn't write bindings");

    let objcopy_names = ["llvm-objcopy", "objcopy", "gobjcopy"];

    let mut objcopy = None;
    for n in objcopy_names {
        if which::which(n).is_ok() {
            objcopy = Some(n);
            break;
        }
    }

    if objcopy.is_none() {
        panic!(
            "No program akin to `objcopy` found\nI searched for these programs in your path: {}",
            objcopy_names.join(", ")
        );
    }

    let objcopy = objcopy.unwrap();

    let syms: Vec<String> = WEE8_RENAMED
        .lock()
        .expect("cannot lock WEE8_RENAMED")
        .iter()
        .map(|(old, new)| {
            // A bit hacky: we need a way to figure out if we're going to target a Mach-O
            // library or an ELF one to take care of the "_" in front of symbols.
            if cfg!(any(target_os = "macos", target_os = "ios")) {
                format!("--redefine-sym=_{old}={new}")
            } else {
                format!("--redefine-sym={old}={new}")
            }
        })
        .collect();
    let prefix_dir = if isolated_prefix {
        let directory = out_path.join("wee8-prefixed");
        fs::create_dir_all(&directory).expect("failed to create owned wee8 prefix directory");
        println!("cargo:rustc-link-search=native={}", directory.display());
        directory
    } else {
        v8_lib_dir.clone()
    };
    let prefixed_v8_lib_path = prefix_dir.join("libv8prefixed.a");
    let output = std::process::Command::new(objcopy)
        .args(syms)
        .arg(v8_lib_path.display().to_string())
        .arg(prefixed_v8_lib_path.display().to_string())
        .output()
        .expect("objcopy command failed");

    if !output.status.success() {
        panic!(
            "{objcopy} failed with error code {}: {}",
            output.status,
            String::from_utf8(output.stderr).unwrap()
        );
    }

    // Use the paired public C++ destroyer for the actual Store owner. Keep
    // this archive before v8prefixed so Store::destroy resolves there; retain
    // the existing target C++ standard-library selection above.
    for file in [
        "owned-store.cc",
        "wasm.hh",
        "wasm.h",
        "LICENSE",
        "SOURCE.md",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            v8_header_path.join(file).display()
        );
    }
    cc::Build::new()
        .cpp(true)
        .cpp_link_stdlib(None)
        .std("c++11")
        .define("LIBWASM_STATIC", None)
        .include(&v8_header_path)
        .file(v8_header_path.join("owned-store.cc"))
        .compile("wasmer_v8_store_owner");

    println!("cargo:rustc-link-lib=static=v8prefixed");

    // Static link dependencies must follow the archive that references them.
    if cfg!(any(target_os = "linux",)) {
        println!("cargo:rustc-link-lib=stdc++");
    } else if cfg!(target_os = "windows") {
        println!("cargo:rustc-link-lib=winmm");
        println!("cargo:rustc-link-lib=dbghelp");
        println!("cargo:rustc-link-lib=shlwapi");
    } else {
        println!("cargo:rustc-link-lib=c++");
    }

    fn stable_target_dir(out_path: &Path) -> PathBuf {
        let mut dir = out_path;
        for _ in 0..4 {
            dir = dir
                .parent()
                .expect("OUT_DIR is not inside Cargo's target build output");
        }
        dir.to_path_buf()
    }
}

#[allow(unused)]
fn main() {
    #[cfg(feature = "v8")]
    build_v8();
}
