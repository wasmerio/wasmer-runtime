#[cfg(feature = "v8")]
fn build_v8() {
    use bindgen::callbacks::ParseCallbacks;
    use std::{
        env, fs,
        path::{Path, PathBuf},
        sync::{LazyLock, Mutex},
    };

    const WEE8_RELEASE_VERSION: &str = "15.9.111.2";
    const V8_ARTIFACT_DIR: &str = "WASMER_V8_ARTIFACT_DIR";

    // `cfg!(target_os)` is the host in a build script, so the target comes
    // from Cargo's environment.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    let (asset_name, platform_name) = match (
        target_os.as_str(),
        env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str(),
        env::var("CARGO_CFG_TARGET_ENV")
            .unwrap_or_default()
            .as_str(),
    ) {
        ("macos", "aarch64", _) => ("v8-darwin-aarch64.tar.xz", "darwin-aarch64"),
        ("linux", "x86_64", "gnu") => ("v8-linux-amd64.tar.xz", "linux-amd64"),
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
        .join(WEE8_RELEASE_VERSION)
        .join(platform_name);
    let archive_path = cache_dir.join(asset_name);
    let library_name = if target_os == "windows" {
        "v8.lib"
    } else {
        "libv8.a"
    };

    // A build without network access names an unpacked artifact, which is
    // then used as it is: nothing is downloaded, and nothing is written to it.
    println!("cargo:rerun-if-env-changed={V8_ARTIFACT_DIR}");
    let prebuilt_lib_dir = env::var_os(V8_ARTIFACT_DIR).map(|dir| {
        let dir = PathBuf::from(dir);
        assert!(
            dir.is_absolute(),
            "{V8_ARTIFACT_DIR} must be an absolute path: {}",
            dir.display()
        );
        let library = dir.join("lib").join(library_name);
        assert!(
            library.is_file(),
            "{V8_ARTIFACT_DIR} must contain lib/{library_name}: {}",
            dir.display()
        );
        println!("cargo:rerun-if-changed={}", library.display());
        dir.join("lib")
    });
    let is_prebuilt = prebuilt_lib_dir.is_some();
    let v8_lib_dir = prebuilt_lib_dir.unwrap_or_else(|| cache_dir.join("lib"));
    let v8_lib_path = v8_lib_dir.join(library_name);

    if !is_prebuilt && !v8_lib_path.exists() {
        fs::create_dir_all(&cache_dir).unwrap_or_else(|err| {
            panic!(
                "failed to create v8 cache dir {}: {err}",
                cache_dir.display()
            )
        });

        if !archive_path.exists() {
            let url = format!(
                "https://github.com/wasmerio/v8-custom-builds/releases/download/{WEE8_RELEASE_VERSION}/{asset_name}"
            );
            let tar_data = ureq::get(&url)
                .call()
                .expect("failed to download v8")
                .body_mut()
                .with_config()
                // Windows prebuilts are substantially larger.
                .limit(200 * 1024 * 1024)
                .read_to_vec()
                .expect("failed to download v8 lib");

            // Written beside its name and renamed to it, so that no build
            // finds half an archive there: not one stopped while writing it,
            // nor one running alongside.
            let partial = cache_dir.join(format!("{asset_name}.{}.partial", std::process::id()));
            fs::write(&partial, tar_data).unwrap_or_else(|err| {
                panic!(
                    "failed to write v8 archive cache {}: {err}",
                    partial.display()
                )
            });
            fs::rename(&partial, &archive_path).unwrap_or_else(|err| {
                panic!(
                    "failed to move v8 archive cache to {}: {err}",
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
        // The library being there is what says the cache is complete, so it
        // is unpacked beside the cache and its directory moved in at once.
        let unpack_dir = cache_dir.join(format!(".unpack.{}", std::process::id()));
        let _ = fs::remove_dir_all(&unpack_dir);
        let tar = xz::read::XzDecoder::new(tar_data.as_slice());
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&unpack_dir).unwrap_or_else(|err| {
            panic!(
                "failed to unpack v8 archive cache {} into {}: {err}",
                archive_path.display(),
                unpack_dir.display()
            )
        });
        // What an interrupted unpack left of the directory, before this
        // script unpacked atomically.
        if v8_lib_dir.exists() && !v8_lib_path.exists() {
            let _ = fs::remove_dir_all(&v8_lib_dir);
        }
        match fs::rename(unpack_dir.join("lib"), &v8_lib_dir) {
            Ok(()) => {}
            // A build running alongside moved its own copy in first.
            Err(_) if v8_lib_path.exists() => {}
            Err(err) => panic!(
                "failed to move the unpacked v8 library to {}: {err}",
                v8_lib_dir.display()
            ),
        }
        let _ = fs::remove_dir_all(&unpack_dir);
    }

    // The library with its symbols renamed goes next to the one it is made
    // from, except for a prebuilt artifact, which may be read-only.
    let prefixed_lib_dir = if is_prebuilt {
        out_path.clone()
    } else {
        v8_lib_dir.clone()
    };
    println!(
        "cargo:rustc-link-search=native={}",
        prefixed_lib_dir.display()
    );

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
    println!("cargo:rerun-if-changed={}", header_path.display());
    let mut args: Vec<String> = vec![];
    if target_os == "macos" {
        // The SDK that `xcrun` selects, which need not be the one inside
        // `/Applications/Xcode.app`: the Command Line Tools have their own.
        let sdk = std::process::Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|sdk| !sdk.is_empty())
            .unwrap_or_else(|| {
                "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk".to_string()
            });
        args.push("-I/usr/local/include".to_string());
        args.push(format!("-I{sdk}/usr/include/c++/v1"));
        args.push(format!("-I{sdk}/usr/include"));
        args.push("-I/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/include".to_string());
        args.push(format!("-I{sdk}/System/Library/Frameworks"));
    }
    let bindings = bindgen::Builder::default()
        .header(header_path.display().to_string())
        .clang_args(args)
        // These libc declarations come from transitive includes and are not part of the V8 API.
        .blocklist_function("memcpy|memmove|memset|memcmp|strlen|bcmp")
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
            // Mach-O symbols have a "_" in front; ELF and COFF x86_64 ones do not.
            if matches!(target_os.as_str(), "macos" | "ios") {
                format!("--redefine-sym=_{old}={new}")
            } else {
                format!("--redefine-sym={old}={new}")
            }
        })
        .collect();
    // The cache is shared by every build, so the renamed library is written
    // beside its name and renamed to it: a build linking it never reads one
    // that another build is writing.
    let prefixed_v8_lib_path = prefixed_lib_dir.join("libv8prefixed.a");
    let partial_v8_lib_path =
        prefixed_lib_dir.join(format!("libv8prefixed.a.{}.partial", std::process::id()));
    let output = std::process::Command::new(objcopy)
        .args(syms)
        .arg(v8_lib_path.display().to_string())
        .arg(partial_v8_lib_path.display().to_string())
        .output()
        .expect("objcopy command failed");

    if !output.status.success() {
        let _ = fs::remove_file(&partial_v8_lib_path);
        panic!(
            "{objcopy} failed with error code {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::rename(&partial_v8_lib_path, &prefixed_v8_lib_path).unwrap_or_else(|err| {
        panic!(
            "failed to move the renamed v8 library to {}: {err}",
            prefixed_v8_lib_path.display()
        )
    });

    println!("cargo:rustc-link-lib=static=v8prefixed");

    // Static link dependencies must follow the archive that references them.
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=stdc++");
    } else if target_os == "windows" {
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
