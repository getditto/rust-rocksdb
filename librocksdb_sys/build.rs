// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate bindgen;
extern crate cc;
extern crate cmake;

use cc::Build;
use cmake::Config;
use std::path::{Path, PathBuf};
use std::{env, str};

// On these platforms jemalloc-sys will use a prefixed jemalloc which cannot be linked together
// with RocksDB.
// See https://github.com/gnzlbg/jemallocator/blob/bfc89192971e026e6423d9ee5aaa02bc56585c58/jemalloc-sys/build.rs#L45
const NO_JEMALLOC_TARGETS: &[&str] = &["android", "dragonfly", "musl", "darwin"];

// Generate the bindings to rocksdb C-API.
// Try to disable the generation of platform-related bindings.
fn bindgen_rocksdb(file_path: &Path) {
    let bindings = bindgen::Builder::default()
        .header("crocksdb/crocksdb/c.h")
        .ctypes_prefix("libc")
        .generate()
        .expect("unable to generate rocksdb bindings");

    bindings
        .write_to_file(file_path)
        .expect("unable to write rocksdb bindings");
}

// Determine if need to update bindings. Supported platforms do not
// need to be updated by default unless the UPDATE_BIND is specified.
// Other platforms use bindgen to generate the bindings every time.
fn config_binding_path() {
    let file_path: PathBuf;

    let target = env::var("TARGET").unwrap_or_else(|_| "".to_owned());
    match target.as_str() {
        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu" => {
            file_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("bindings")
                .join(format!("{}-bindings.rs", target));
            if env::var("UPDATE_BIND")
                .map(|s| s.as_str() == "1")
                .unwrap_or(false)
            {
                bindgen_rocksdb(&file_path);
            }
        }
        _ => {
            file_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("rocksdb-bindings.rs");
            bindgen_rocksdb(&file_path);
        }
    };
    println!(
        "cargo:rustc-env=BINDING_PATH={}",
        file_path.to_str().unwrap()
    );
}

fn main() {
    println!("cargo:rerun-if-env-changed=UPDATE_BIND");

    let mut build = build_rocksdb();

    build.cpp(true).file("crocksdb/c.cc");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() != "windows" {
        build.flag("-std=c++17");
        build.flag("-fno-rtti");
    }
    link_cpp(&mut build);
    build.warnings(false).compile("libcrocksdb.a");
}

fn link_cpp(build: &mut Build) {
    let tool = build.get_compiler();
    let stdlib = if tool.is_like_gnu() {
        "libstdc++.a"
    } else if tool.is_like_clang() {
        "libc++.a"
    } else {
        // Don't link to c++ statically on windows.
        return;
    };
    let output = tool
        .to_command()
        .arg("--print-file-name")
        .arg(stdlib)
        .output()
        .unwrap();
    if !output.status.success() || output.stdout.is_empty() {
        // fallback to dynamically
        return;
    }
    let path = match str::from_utf8(&output.stdout) {
        Ok(path) => PathBuf::from(path),
        Err(_) => return,
    };
    if !path.is_absolute() {
        return;
    }
    // remove lib prefix and .a postfix.
    let libname = &stdlib[3..stdlib.len() - 2];
    // optional static linking
    if cfg!(feature = "static_libcpp") {
        println!("cargo:rustc-link-lib=static={}", &libname);
    } else {
        println!("cargo:rustc-link-lib=dylib={}", &libname);
    }
    println!(
        "cargo:rustc-link-search=native={}",
        path.parent().unwrap().display()
    );
    build.cpp_link_stdlib(None);
}

/// Fix cacheline_aligned_alloc to never return nullptr (DB-1237).
///
/// The upstream tikv/rocksdb (8.10.tikv branch) silently returns nullptr when
/// posix_memalign fails. StatisticsData::operator new[] always routes through
/// this function, so a failed allocation leaves CoreLocalArray::data_ null.
/// Any subsequent access to per-core statistics then dereferences null + offset
/// and crashes with SIGSEGV. (See DB-1237.)
///
/// Fix: fall back to malloc when posix_memalign fails. Cache-line alignment is
/// a performance optimization, not a correctness requirement. If malloc also
/// fails we abort() with a clear message rather than returning nullptr silently.
///
/// We also forcibly delete any cached port_posix.cc.o from the cmake build dir
/// so that cmake must recompile the patched source even when build caches from
/// ditto-action-prepare are present.
fn patch_cacheline_alloc(rocksdb_dir: &Path, out_dir: &Path) {
    // --- 1. Patch the source ---
    let port_posix = rocksdb_dir.join("port").join("port_posix.cc");
    let content = std::fs::read_to_string(&port_posix)
        .expect("failed to read port/port_posix.cc");

    // Diagnostic: always print path and patch state so CI logs are unambiguous.
    let already_patched = content.contains("fall back to plain malloc");
    println!(
        "cargo:warning=DB-1237: port_posix={:?} len={} already_patched={}",
        port_posix,
        content.len(),
        already_patched,
    );

    // The buggy two-liner: posix_memalign returns an error code; the code stores
    // it in errno and then returns nullptr if errno is non-zero.
    let old = "  errno = posix_memalign(&m, CACHE_LINE_SIZE, size);\n  return errno ? nullptr : m;";

    // Replacement: fall back to malloc on posix_memalign failure so we never
    // return nullptr.  Throwing std::bad_alloc is avoided here because this
    // function is called through operator new[] which is reachable from the
    // crocksdb C API; C++ exceptions propagating through extern "C" and then
    // into Rust cause undefined behaviour.
    let new_code = concat!(
        "  if (posix_memalign(&m, CACHE_LINE_SIZE, size) != 0) {\n",
        "    // posix_memalign failed; fall back to plain malloc.\n",
        "    // Cache-line alignment is a performance hint, not a correctness\n",
        "    // requirement.  If malloc also fails we abort with a clear message\n",
        "    // rather than silently returning nullptr (DB-1237).\n",
        "    m = malloc(size);\n",
        "    if (m == nullptr) {\n",
        "      fprintf(stderr,\n",
        "              \"cacheline_aligned_alloc: OOM for %zu bytes\\n\", size);\n",
        "      abort();\n",
        "    }\n",
        "  }\n",
        "  return m;",
    );

    let patched_content = if content.contains(old) {
        // Normal path: apply the patch.
        println!(
            "cargo:warning=DB-1237: applying patch to port_posix.cc \
             (cacheline_aligned_alloc: malloc fallback on posix_memalign failure)"
        );
        content.replace(old, new_code)
    } else if already_patched {
        // The file already has our patch text — but we still re-write it to
        // guarantee a fresh mtime, which tells cmake's dependency tracker that
        // port_posix.cc is newer than any cached port_posix.cc.o and must be
        // recompiled.  Without this, a self-hosted runner with a persisted
        // build directory would silently reuse the stale (possibly unpatched)
        // object file even though the source is correct.
        println!(
            "cargo:warning=DB-1237: port_posix.cc already patched; \
             re-writing to refresh mtime and force cmake recompile"
        );
        content
    } else {
        // Neither the original nor our patch is present — unexpected file version.
        println!(
            "cargo:warning=DB-1237: WARNING: could not locate patch target in \
             port/port_posix.cc (len={}); patch not applied",
            content.len()
        );
        return;
    };

    std::fs::write(&port_posix, &patched_content)
        .expect("failed to write port/port_posix.cc");
    println!("cargo:warning=DB-1237: wrote {:?}", port_posix);

    // --- 2. Invalidate the cmake build cache so cmake must recompile ---
    //
    // ditto-action-prepare restores a build cache that typically includes both
    // librocksdb.a and the cmake internal state files (CMakeFiles/). cmake uses
    // those state files to decide nothing needs rebuilding — it never re-checks
    // source file timestamps against the archive. Simply patching port_posix.cc
    // is therefore not enough; cmake skips recompilation entirely.
    //
    // Fix: delete librocksdb.a AND port_posix.cc.o from the cmake build dir.
    //   • Deleting librocksdb.a tells cmake the archive must be rebuilt.
    //   • Deleting port_posix.cc.o tells cmake that object must be recompiled.
    // If the cache also has other .o files for the ~400 remaining RocksDB
    // sources, cmake can re-archive quickly from those existing objects plus the
    // freshly-compiled patched port_posix.cc.o. If the cache only has
    // librocksdb.a (no .o files), cmake falls back to a full rebuild — slower
    // but correct.
    let cmake_build = out_dir.join("build");
    if cmake_build.is_dir() {
        // Delete librocksdb.a to force cmake to re-archive.
        for candidate in [
            cmake_build.join("librocksdb.a"),
            cmake_build.join("lib").join("librocksdb.a"),
        ] {
            if candidate.exists() {
                if std::fs::remove_file(&candidate).is_ok() {
                    println!("cargo:warning=DB-1237: removed {candidate:?} to force cmake re-archive");
                }
            }
        }
        // Delete port_posix.cc.o so cmake recompiles it from the patched source.
        delete_matching(&cmake_build, "port_posix.cc.o");
    }
}

/// Recursively delete all files named `target_name` under `dir`.
fn delete_matching(dir: &Path, target_name: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            delete_matching(&path, target_name);
        } else if path.file_name().map(|n| n == target_name).unwrap_or(false) {
            if std::fs::remove_file(&path).is_ok() {
                println!("cargo:warning=DB-1237: removed cached {path:?}");
            }
        }
    }
}

fn build_rocksdb() -> Build {
    let target = env::var("TARGET").expect("TARGET was not set");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let cur_dir = env::current_dir().unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    patch_cacheline_alloc(&cur_dir.join("rocksdb"), &out_dir);
    let mut cfg = Config::new("rocksdb");
    if cfg!(feature = "encryption") {
        cfg.register_dep("OPENSSL").define("WITH_OPENSSL", "ON");
    }
    if cfg!(feature = "jemalloc") && NO_JEMALLOC_TARGETS.iter().all(|i| !target.contains(i)) {
        cfg.register_dep("JEMALLOC").define("WITH_JEMALLOC", "ON");
        println!("cargo:rustc-link-lib=static=jemalloc");
    }
    if cfg!(feature = "portable") {
        cfg.define("PORTABLE", "ON");
    }
    if cfg!(feature = "sse") {
        cfg.define("FORCE_SSE42", "ON");
    }
    // RocksDB cmake script expect libz.a being under ${DEP_Z_ROOT}/lib, but libz-sys crate put it
    // under ${DEP_Z_ROOT}/build. Append the path to CMAKE_PREFIX_PATH to get around it.
    env::set_var("CMAKE_PREFIX_PATH", {
        let zlib_path = format!("{}/build", env::var("DEP_Z_ROOT").unwrap());
        if let Ok(prefix_path) = env::var("CMAKE_PREFIX_PATH") {
            format!("{};{}", prefix_path, zlib_path)
        } else {
            zlib_path
        }
    });
    // Propagate the Cargo profile.debug==false setting to the RocksDB build.
    // RocksDB debug information can be significiant in size (e.g on macOS librocksdb.a is ~800MiB
    // with default default debuginfo (`-g`), or ~200MiB with debuginfo disabled), but cmake-rs
    // doesn't communicate the Cargo setting by default so we do it explicitly here.
    if env::var("DEBUG").unwrap_or("false".to_owned()) == "false" {
        cfg.define("CMAKE_CXX_FLAGS_DEBUG", "-g0");
    }

    let dst = cfg
        .define("WITH_GFLAGS", "OFF")
        .register_dep("Z")
        .define("WITH_ZLIB", "ON")
        .register_dep("BZIP2")
        .define("WITH_BZ2", "ON")
        .register_dep("LZ4")
        .define("WITH_LZ4", "ON")
        .register_dep("ZSTD")
        .define("WITH_ZSTD", "ON")
        .register_dep("SNAPPY")
        .define("WITH_SNAPPY", "ON")
        .define("WITH_TESTS", "OFF")
        .define("WITH_TOOLS", "OFF")
        .build_target("rocksdb")
        .very_verbose(true)
        .build();
    let build_dir = format!("{}/build", dst.display());
    let mut build = Build::new();
    if target_os == "windows" {
        let profile = match &*env::var("PROFILE").unwrap_or_else(|_| "debug".to_owned()) {
            "bench" | "release" => "Release",
            _ => "Debug",
        };
        println!("cargo:rustc-link-search=native={}/{}", build_dir, profile);
        build.define("OS_WIN", None);
    } else {
        println!("cargo:rustc-link-search=native={}", build_dir);
        build.define("ROCKSDB_PLATFORM_POSIX", None);
    }
    if target_os == "macos" {
        build.define("OS_MACOSX", None);
    } else if target_os == "freebsd" {
        build.define("OS_FREEBSD", None);
    }

    config_binding_path();

    build.include(cur_dir.join("rocksdb").join("include"));
    build.include(cur_dir.join("rocksdb"));
    build.include(cur_dir.join("libtitan_sys").join("titan").join("include"));
    build.include(cur_dir.join("libtitan_sys").join("titan"));

    // Adding rocksdb specific compile macros.
    // TODO: should make sure crocksdb compile options is the same as rocksdb and titan.
    build.define("ROCKSDB_SUPPORT_THREAD_LOCAL", None);
    if cfg!(feature = "encryption") {
        build.define("OPENSSL", None);
    }

    println!("cargo:rustc-link-lib=static=rocksdb");
    println!("cargo:rustc-link-lib=static=titan");
    println!("cargo:rustc-link-lib=static=z");
    println!("cargo:rustc-link-lib=static=bz2");
    println!("cargo:rustc-link-lib=static=lz4");
    println!("cargo:rustc-link-lib=static=zstd");
    println!("cargo:rustc-link-lib=static=snappy");

    println!(
        "cargo:rerun-if-changed={}",
        cur_dir.join("crocksdb").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        cur_dir.join("rocksdb").display()
    );
    build
}
