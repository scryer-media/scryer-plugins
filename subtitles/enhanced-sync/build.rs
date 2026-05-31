use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FFMPEG_VENDOR_ARCHIVE_FILE: &str = "source.tar.zst";
const FFMPEG_VENDOR_METADATA_FILE: &str = "SCRYER_VENDOR_METADATA";
const FFMPEG_BUILD_CONFIG_VERSION: &str = "targeted-flac-transcode-v4";
const LIBFVAD_VENDOR_ARCHIVE_FILE: &str = "source.tar.zst";
const LIBFVAD_VENDOR_METADATA_FILE: &str = "SCRYER_VENDOR_METADATA";
const LIBFVAD_BUILD_CONFIG_VERSION: &str = "webrtc-vad-v1";
const LIBFVAD_C_SOURCES: &[&str] = &[
    "src/fvad.c",
    "src/signal_processing/division_operations.c",
    "src/signal_processing/energy.c",
    "src/signal_processing/get_scaling_square.c",
    "src/signal_processing/resample_48khz.c",
    "src/signal_processing/resample_by_2_internal.c",
    "src/signal_processing/resample_fractional.c",
    "src/signal_processing/spl_inl.c",
    "src/vad/vad_core.c",
    "src/vad/vad_filterbank.c",
    "src/vad/vad_gmm.c",
    "src/vad/vad_sp.c",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WasmRequiredFeature {
    Simd128,
    RelaxedSimd,
}

#[derive(Clone, Debug)]
struct WasmFeatureSet {
    required_features: Vec<WasmRequiredFeature>,
}

impl WasmFeatureSet {
    fn from_env() -> Self {
        let raw = env::var("SCRYER_WASM_REQUIRED_FEATURES").unwrap_or_default();
        if raw.trim().is_empty() {
            return Self {
                required_features: Vec::new(),
            };
        }

        let mut required_features = raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| match value {
                "simd128" => WasmRequiredFeature::Simd128,
                "relaxed-simd" => WasmRequiredFeature::RelaxedSimd,
                other => panic!("unsupported SCRYER_WASM_REQUIRED_FEATURES entry `{other}`"),
            })
            .collect::<Vec<_>>();
        required_features.sort_by_key(|feature| match feature {
            WasmRequiredFeature::Simd128 => 0_u8,
            WasmRequiredFeature::RelaxedSimd => 1_u8,
        });
        required_features.dedup();

        let feature_set = Self { required_features };
        if feature_set.has_feature(WasmRequiredFeature::RelaxedSimd)
            && !feature_set.has_feature(WasmRequiredFeature::Simd128)
        {
            panic!("relaxed-simd requires simd128 in SCRYER_WASM_REQUIRED_FEATURES");
        }
        feature_set
    }

    fn has_feature(&self, feature: WasmRequiredFeature) -> bool {
        self.required_features.contains(&feature)
    }

    fn is_baseline(&self) -> bool {
        self.required_features.is_empty()
    }

    fn c_opt_level_flag(&self) -> &'static str {
        if self.is_baseline() { "-Oz" } else { "-O3" }
    }

    fn clang_target_feature_flags(&self) -> Vec<&'static str> {
        let mut flags = Vec::new();
        if self.has_feature(WasmRequiredFeature::Simd128) {
            flags.push("-msimd128");
        }
        if self.has_feature(WasmRequiredFeature::RelaxedSimd) {
            flags.push("-mrelaxed-simd");
        }
        flags
    }

    fn build_stamp_fragment(&self) -> String {
        if self.is_baseline() {
            "baseline".to_string()
        } else {
            self.required_features
                .iter()
                .map(|feature| match feature {
                    WasmRequiredFeature::Simd128 => "simd128",
                    WasmRequiredFeature::RelaxedSimd => "relaxed-simd",
                })
                .collect::<Vec<_>>()
                .join(",")
        }
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor/ffmpeg");
    let source_archive = vendor_dir.join(FFMPEG_VENDOR_ARCHIVE_FILE);
    let source_dir = out_dir.join("ffmpeg-source");
    let build_dir = out_dir.join("ffmpeg-build");
    let libfvad_vendor_dir = manifest_dir.join("vendor/libfvad");
    let libfvad_source_archive = libfvad_vendor_dir.join(LIBFVAD_VENDOR_ARCHIVE_FILE);
    let libfvad_source_dir = out_dir.join("libfvad-source");
    let libfvad_build_dir = out_dir.join("libfvad-build");
    let target = env::var("TARGET").unwrap();
    let is_wasi = target == "wasm32-wasip1";
    let feature_set = WasmFeatureSet::from_env();
    let vendor_metadata = read_ffmpeg_vendor_metadata(&vendor_dir);
    let libfvad_vendor_metadata = read_libfvad_vendor_metadata(&libfvad_vendor_dir);

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/ffmpeg_bridge.c");
    println!("cargo:rerun-if-changed=vendor/ffmpeg");
    println!("cargo:rerun-if-changed=vendor/libfvad");
    println!("cargo:rerun-if-env-changed=FFMPEG_WASI_SYSROOT");
    println!("cargo:rerun-if-env-changed=WASI_SYSROOT");
    println!("cargo:rerun-if-env-changed=CLANG");
    println!("cargo:rerun-if-env-changed=LLVM_AR");
    println!("cargo:rerun-if-env-changed=LLVM_NM");
    println!("cargo:rerun-if-env-changed=LLVM_RANLIB");
    println!("cargo:rerun-if-env-changed=LLVM_STRIP");
    println!("cargo:rerun-if-env-changed=SCRYER_WASM_REQUIRED_FEATURES");

    build_ffmpeg(
        &source_archive,
        &source_dir,
        &build_dir,
        is_wasi,
        &feature_set,
        &vendor_metadata.revision,
    );
    build_libfvad(
        &libfvad_source_archive,
        &libfvad_source_dir,
        &libfvad_build_dir,
        is_wasi,
        &feature_set,
        &libfvad_vendor_metadata.revision,
    );
    build_bridge(
        &manifest_dir,
        &source_dir,
        &build_dir,
        is_wasi,
        &feature_set,
    );

    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("libavcodec").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("libavformat").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("libswresample").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("libavutil").display()
    );
    println!("cargo:rustc-link-lib=static=avformat");
    println!("cargo:rustc-link-lib=static=avcodec");
    println!("cargo:rustc-link-lib=static=swresample");
    println!("cargo:rustc-link-lib=static=avutil");
}

fn build_ffmpeg(
    source_archive: &Path,
    source_dir: &Path,
    build_dir: &Path,
    is_wasi: bool,
    feature_set: &WasmFeatureSet,
    revision: &str,
) {
    let avcodec = build_dir.join("libavcodec/libavcodec.a");
    let avformat = build_dir.join("libavformat/libavformat.a");
    let swresample = build_dir.join("libswresample/libswresample.a");
    let avutil = build_dir.join("libavutil/libavutil.a");
    let config_stamp = build_dir.join(".scryer-ffmpeg-config");
    let build_stamp = format!(
        "{FFMPEG_BUILD_CONFIG_VERSION}\nrevision={revision}\nfeatures={}\n",
        feature_set.build_stamp_fragment()
    );
    if avcodec.exists()
        && avformat.exists()
        && swresample.exists()
        && avutil.exists()
        && source_dir.join("configure").exists()
        && fs::read_to_string(&config_stamp).is_ok_and(|stamp| stamp == build_stamp)
    {
        return;
    }

    if build_dir.exists() {
        fs::remove_dir_all(build_dir).unwrap();
    }
    if source_dir.exists() {
        fs::remove_dir_all(source_dir).unwrap();
    }
    fs::create_dir_all(build_dir).unwrap();
    fs::create_dir_all(source_dir).unwrap();
    extract_ffmpeg_source_archive(source_archive, source_dir);

    let mut configure = Command::new(source_dir.join("configure"));
    configure.current_dir(build_dir);
    configure.args([
        "--disable-everything",
        "--disable-version-tracking",
        "--disable-programs",
        "--disable-doc",
        "--disable-avdevice",
        "--enable-avformat",
        "--disable-avfilter",
        "--disable-swscale",
        "--enable-swresample",
        "--disable-network",
        "--disable-runtime-cpudetect",
        "--disable-pthreads",
        "--disable-zlib",
        "--disable-bzlib",
        "--disable-lzma",
        "--disable-securetransport",
        "--disable-audiotoolbox",
        "--disable-videotoolbox",
        "--disable-iconv",
        "--disable-asm",
        "--disable-x86asm",
        "--enable-avcodec",
        "--enable-avformat",
        "--enable-swresample",
        "--enable-avutil",
        "--enable-decoder=ac3,eac3,dca,truehd,mlp",
        "--enable-encoder=flac",
        "--enable-parser=ac3,dca,mlp",
        "--enable-demuxer=ac3,eac3,dts,dtshd,matroska,mov,mpegts,truehd",
        "--enable-muxer=flac",
        "--enable-protocol=file",
        "--enable-static",
        "--disable-shared",
    ]);

    if is_wasi {
        let sysroot = wasi_sysroot();
        let clang = clang_path();
        let mut extra_cflags = vec![
            "--target=wasm32-wasip1".to_string(),
            format!("--sysroot={}", sysroot.display()),
            feature_set.c_opt_level_flag().to_string(),
            "-fvisibility=hidden".to_string(),
            "-D_GNU_SOURCE".to_string(),
        ];
        extra_cflags.extend(
            feature_set
                .clang_target_feature_flags()
                .into_iter()
                .map(str::to_string),
        );
        let mut extra_ldflags = vec![
            "--target=wasm32-wasip1".to_string(),
            format!("--sysroot={}", sysroot.display()),
            "-fuse-ld=lld".to_string(),
            "-nostdlib".to_string(),
            "-Wl,--no-entry".to_string(),
        ];
        extra_ldflags.extend(
            feature_set
                .clang_target_feature_flags()
                .into_iter()
                .map(str::to_string),
        );
        configure.args([
            "--enable-cross-compile",
            "--target-os=none",
            "--arch=wasm32",
            &format!("--cc={}", clang.display()),
            &format!("--ar={}", llvm_tool("LLVM_AR", "llvm-ar").display()),
            &format!(
                "--ranlib={}",
                llvm_tool("LLVM_RANLIB", "llvm-ranlib").display()
            ),
            &format!("--nm={}", llvm_tool("LLVM_NM", "llvm-nm").display()),
            &format!(
                "--strip={}",
                llvm_tool("LLVM_STRIP", "llvm-strip").display()
            ),
            &format!("--extra-cflags={}", extra_cflags.join(" ")),
            &format!("--extra-ldflags={}", extra_ldflags.join(" ")),
        ]);
    }

    configure.env("revision", revision);
    run(&mut configure, "configure vendored FFmpeg");

    if is_wasi {
        patch_wasi_config(build_dir);
    }

    let mut make = Command::new(env::var_os("MAKE").unwrap_or_else(|| "make".into()));
    make.current_dir(build_dir)
        .env("revision", revision)
        .arg(format!(
            "-j{}",
            env::var("NUM_JOBS").unwrap_or_else(|_| "1".to_string())
        ))
        .args([
            "libavcodec/libavcodec.a",
            "libavformat/libavformat.a",
            "libswresample/libswresample.a",
            "libavutil/libavutil.a",
        ]);
    run(&mut make, "build vendored FFmpeg");
    fs::write(config_stamp, build_stamp).unwrap();
}

fn build_libfvad(
    source_archive: &Path,
    source_dir: &Path,
    build_dir: &Path,
    is_wasi: bool,
    feature_set: &WasmFeatureSet,
    revision: &str,
) {
    let config_stamp = build_dir.join(".scryer-libfvad-config");
    let build_stamp = format!(
        "{LIBFVAD_BUILD_CONFIG_VERSION}\nrevision={revision}\nfeatures={}\n",
        feature_set.build_stamp_fragment()
    );
    let stamp_matches = fs::read_to_string(&config_stamp).is_ok_and(|stamp| stamp == build_stamp);
    if !source_dir.join("include/fvad.h").exists() || !stamp_matches {
        if build_dir.exists() {
            fs::remove_dir_all(build_dir).unwrap();
        }
        if source_dir.exists() {
            fs::remove_dir_all(source_dir).unwrap();
        }
        fs::create_dir_all(build_dir).unwrap();
        fs::create_dir_all(source_dir).unwrap();
        extract_libfvad_source_archive(source_archive, source_dir);
        fs::write(&config_stamp, &build_stamp).unwrap();
    }

    let mut build = cc::Build::new();
    for relative in LIBFVAD_C_SOURCES {
        build.file(source_dir.join(relative));
    }
    build
        .include(source_dir.join("include"))
        .include(source_dir.join("src"))
        .out_dir(build_dir)
        .warnings(false)
        .flag_if_supported("-std=c11")
        .flag_if_supported("-fvisibility=hidden");

    if is_wasi {
        let sysroot = wasi_sysroot();
        build
            .compiler(clang_path())
            .flag("--target=wasm32-wasip1")
            .flag(format!("--sysroot={}", sysroot.display()))
            .flag(feature_set.c_opt_level_flag())
            .flag("-D_GNU_SOURCE");
        for flag in feature_set.clang_target_feature_flags() {
            build.flag(flag);
        }
    }

    build.compile("scryer_webrtc_vad");
}

fn extract_ffmpeg_source_archive(source_archive: &Path, source_dir: &Path) {
    let archive = fs::File::open(source_archive).unwrap_or_else(|error| {
        panic!(
            "failed to open {}: {error}. Run `cargo xtask ffmpeg revendor --commit <ffmpeg-commit>` to refresh the vendored FFmpeg archive.",
            source_archive.display()
        )
    });
    let decoder = zstd::stream::Decoder::new(archive).unwrap_or_else(|error| {
        panic!("failed to decompress {}: {error}", source_archive.display())
    });
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(source_dir).unwrap_or_else(|error| {
        panic!(
            "failed to unpack {} into {}: {error}",
            source_archive.display(),
            source_dir.display()
        )
    });
}

fn extract_libfvad_source_archive(source_archive: &Path, source_dir: &Path) {
    let archive = fs::File::open(source_archive).unwrap_or_else(|error| {
        panic!(
            "failed to open {}: {error}. Run `cargo xtask vad revendor --commit <libfvad-commit>` to refresh the vendored libfvad archive.",
            source_archive.display()
        )
    });
    let decoder = zstd::stream::Decoder::new(archive).unwrap_or_else(|error| {
        panic!("failed to decompress {}: {error}", source_archive.display())
    });
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(source_dir).unwrap_or_else(|error| {
        panic!(
            "failed to unpack {} into {}: {error}",
            source_archive.display(),
            source_dir.display()
        )
    });
}

fn build_bridge(
    manifest_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
    is_wasi: bool,
    feature_set: &WasmFeatureSet,
) {
    let mut build = cc::Build::new();
    build
        .file(manifest_dir.join("src/ffmpeg_bridge.c"))
        .include(build_dir)
        .include(source_dir)
        .warnings(false)
        .flag_if_supported("-std=c11")
        .flag_if_supported("-fvisibility=hidden");

    if is_wasi {
        let sysroot = wasi_sysroot();
        build
            .compiler(clang_path())
            .flag("--target=wasm32-wasip1")
            .flag(format!("--sysroot={}", sysroot.display()))
            .flag(feature_set.c_opt_level_flag())
            .flag("-D_GNU_SOURCE");
        for flag in feature_set.clang_target_feature_flags() {
            build.flag(flag);
        }
    }

    build.compile("scryer_ffmpeg_bridge");
}

fn patch_wasi_config(build_dir: &Path) {
    for relative in ["config.h", "ffbuild/config.mak"] {
        let path = build_dir.join(relative);
        let mut contents = fs::read_to_string(&path).unwrap();
        for feature in ["GETHRTIME", "MKSTEMP", "MMAP", "SYSCTL", "TEMPNAM"] {
            contents = contents.replace(
                &format!("#define HAVE_{feature} 1"),
                &format!("#define HAVE_{feature} 0"),
            );
            contents = contents.replace(
                &format!("\nHAVE_{feature}=yes"),
                &format!("\n!HAVE_{feature}=yes"),
            );
        }
        fs::write(path, contents).unwrap();
    }
}

fn wasi_sysroot() -> PathBuf {
    env_path("FFMPEG_WASI_SYSROOT")
        .or_else(|| env_path("WASI_SYSROOT"))
        .or_else(|| candidate("/opt/homebrew/opt/wasi-libc/share/wasi-sysroot"))
        .or_else(|| candidate("/usr/local/opt/wasi-libc/share/wasi-sysroot"))
        .or_else(|| candidate("/usr/share/wasi-sysroot"))
        .unwrap_or_else(|| {
            panic!(
                "wasi-libc sysroot not found; set FFMPEG_WASI_SYSROOT or WASI_SYSROOT to the wasi-sysroot directory"
            )
        })
}

fn clang_path() -> PathBuf {
    env_path("CLANG")
        .or_else(|| candidate("/opt/homebrew/opt/llvm/bin/clang"))
        .or_else(|| candidate("/usr/local/opt/llvm/bin/clang"))
        .unwrap_or_else(|| PathBuf::from("clang"))
}

fn llvm_tool(env_name: &str, tool: &str) -> PathBuf {
    env_path(env_name)
        .or_else(|| {
            let clang = clang_path();
            clang
                .parent()
                .map(|parent| parent.join(tool))
                .filter(|path| path.exists())
        })
        .unwrap_or_else(|| PathBuf::from(tool))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn candidate(path: impl AsRef<Path>) -> Option<PathBuf> {
    let path = path.as_ref().to_path_buf();
    path.exists().then_some(path)
}

struct FfmpegVendorMetadata {
    revision: String,
}

struct LibfvadVendorMetadata {
    revision: String,
}

fn read_ffmpeg_vendor_metadata(source_dir: &Path) -> FfmpegVendorMetadata {
    let path = source_dir.join(FFMPEG_VENDOR_METADATA_FILE);
    let contents = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "failed to read {}: {error}. Run `cargo xtask ffmpeg revendor --commit <ffmpeg-commit>` to refresh the vendored FFmpeg metadata.",
            path.display()
        )
    });
    let mut revision = None;
    let mut commit = None;
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or_else(|| {
            panic!(
                "invalid FFmpeg vendor metadata line `{line}` in {}",
                path.display()
            )
        });
        let value = value.trim();
        match key.trim() {
            "revision" => revision = Some(value.to_string()),
            "commit" => commit = Some(value.to_string()),
            _ => {}
        }
    }

    let revision = revision
        .or_else(|| commit.map(|commit| format!("git-{commit}")))
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| panic!("missing `revision=` or `commit=` in {}", path.display()));

    FfmpegVendorMetadata { revision }
}

fn read_libfvad_vendor_metadata(source_dir: &Path) -> LibfvadVendorMetadata {
    let path = source_dir.join(LIBFVAD_VENDOR_METADATA_FILE);
    let contents = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "failed to read {}: {error}. Run `cargo xtask vad revendor --commit <libfvad-commit>` to refresh the vendored libfvad metadata.",
            path.display()
        )
    });
    let mut revision = None;
    let mut commit = None;
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or_else(|| {
            panic!(
                "invalid libfvad vendor metadata line `{line}` in {}",
                path.display()
            )
        });
        let value = value.trim();
        match key.trim() {
            "revision" => revision = Some(value.to_string()),
            "commit" => commit = Some(value.to_string()),
            _ => {}
        }
    }

    let revision = revision
        .or_else(|| commit.map(|commit| format!("git-{commit}")))
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| panic!("missing `revision=` or `commit=` in {}", path.display()));

    LibfvadVendorMetadata { revision }
}

fn run(command: &mut Command, label: &str) {
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let status = command.status().unwrap_or_else(|error| {
        panic!("failed to {label}: {error}");
    });
    if !status.success() {
        panic!("{label} failed with {status}");
    }
}
