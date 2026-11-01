//! Builds txiki.js (third_party/txiki.js) with CMake as a static library, compiles the C glue in csrc/ and tells cargo how to link it all.
//!
//! The result is statically self-contained apart from libc, libm, libdl, libpthread and librt: libuv, QuickJS, mbedTLS, libwebsockets, ada and
//! miniz are linked as archives, and so is libstdc++ (ada is C++) when the static archive can be found.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Files that prove a (nested) submodule has been checked out.
const REQUIRED: &[&str] = &[
    "CMakeLists.txt",
    "src/vm.c",
    "deps/libuv/CMakeLists.txt",
    "deps/quickjs/quickjs.h",
    "deps/mbedtls/CMakeLists.txt",
    "deps/mbedtls/framework/scripts/all-core.sh",
    "deps/libwebsockets/CMakeLists.txt",
];

const FETCH_COMMAND: &str = "git submodule update --init --depth 1 third_party/txiki.js \
    && git -C third_party/txiki.js submodule update --init --depth 1 deps/libuv deps/quickjs deps/mbedtls deps/libwebsockets \
    && git -C third_party/txiki.js/deps/mbedtls submodule update --init --depth 1 framework";

/// (directory below the CMake build tree, library name) in link order: every archive comes after the ones that use it.
const ARCHIVES: &[(&str, &str)] = &[
    ("", "tjs_core"),
    ("deps/quickjs", "qjs"),
    ("deps/libuv", "uv"),
    ("deps/ada", "ada"),
    ("deps/miniz", "miniz"),
    ("deps/libwebsockets/lib", "websockets"),
    ("deps/mbedtls/library", "mbedtls"),
    ("deps/mbedtls/library", "mbedx509"),
    ("deps/mbedtls/library", "mbedcrypto"),
    ("deps/mbedtls/3rdparty/everest", "everest"),
    ("deps/mbedtls/3rdparty/p256-m", "p256m"),
];

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !matches!(target_os.as_str(), "linux" | "macos") {
        panic!("tjs-sys supports Linux and macOS only (target_os = {target_os})");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-env-changed=TJS_SYS_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=TJS_SYS_DYNAMIC_STDCXX");

    let source = match env::var_os("TJS_SYS_SOURCE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => manifest.join("../third_party/txiki.js"),
    };
    let missing: Vec<&str> = REQUIRED.iter().copied().filter(|file| !source.join(file).exists()).collect();
    if !missing.is_empty() {
        panic!(
            "the txiki.js sources are incomplete at {} (missing: {}).\nRun this in the repository root:\n\n    {}\n",
            source.display(),
            missing.join(", "),
            FETCH_COMMAND
        );
    }
    let source = source.canonicalize().expect("canonicalize the txiki.js source dir");
    println!("cargo:rerun-if-changed={}", source.join("CMakeLists.txt").display());
    println!("cargo:rerun-if-changed={}", source.join("src").display());

    let out = cmake::Config::new(&source)
        .profile("Release")
        .define("BUILD_WITH_MIMALLOC", "OFF")
        .define("BUILD_WITH_WASM", "OFF")
        .define("BUILD_WITH_SQLITE", "OFF")
        .define("BUILD_WITH_FFI", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        // Only the static library: no CLI, no test fixtures.
        .build_target("tjs")
        .build();
    let build = out.join("build");

    compile_glue(&manifest, &source, &build);

    for (dir, name) in ARCHIVES {
        let dir = build.join(dir);
        if !dir.join(format!("lib{name}.a")).exists() {
            panic!(
                "expected {} to exist after building txiki.js",
                dir.join(format!("lib{name}.a")).display()
            );
        }
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static={name}");
    }

    link_system_libs(&target_os);
}

fn compile_glue(manifest: &Path, source: &Path, build: &Path) {
    let mut glue = cc::Build::new();
    glue.file(manifest.join("csrc/fakellm_tjs.c"))
        .include(manifest.join("csrc"))
        .include(source.join("src"))
        .include(source.join("deps/quickjs"))
        .include(source.join("deps/libuv/include"))
        .include(source.join("deps/libwebsockets/include"))
        .include(build.join("deps/libwebsockets/include"))
        .include(source.join("deps/mbedtls/include"))
        // private.h embeds an mbedtls_x509_crt: the layout depends on the same user config txiki.js is built with.
        .define(
            "MBEDTLS_USER_CONFIG_FILE",
            Some(format!("\"{}\"", source.join("src/mbedtls_config.h").display()).as_str()),
        )
        // Part of CMAKE_BUILD_TYPE=Release; some structs change shape without it.
        .define("NDEBUG", None)
        .std("gnu11")
        .warnings(true)
        .extra_warnings(false)
        .pic(true);
    glue.compile("fakellm_tjs");
}

fn link_system_libs(target_os: &str) {
    match target_os {
        "linux" => {
            link_cxx_runtime("stdc++");
            for lib in ["m", "dl", "pthread", "rt"] {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
        "macos" => {
            link_cxx_runtime("c++");
        }
        _ => unreachable!(),
    }
}

/// ada is C++. Link libstdc++ statically when the archive is around so the binary does not depend on the C++ runtime of the machine it runs on.
fn link_cxx_runtime(name: &str) {
    if name == "stdc++" && env::var_os("TJS_SYS_DYNAMIC_STDCXX").is_none() {
        let compiler = cc::Build::new().cpp(true).get_compiler();
        let found = Command::new(compiler.path())
            .arg("-print-file-name=libstdc++.a")
            .output()
            .ok()
            .and_then(|output| {
                let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
                (path.is_absolute() && path.exists()).then_some(path)
            });
        if let Some(path) = found {
            println!("cargo:rustc-link-search=native={}", path.parent().unwrap().display());
            println!("cargo:rustc-link-lib=static=stdc++");
            return;
        }
    }
    println!("cargo:rustc-link-lib={name}");
}
