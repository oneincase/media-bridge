//! 构建脚本：找出 macOS helper 动态库并让主 crate 把它嵌进去。
//!
//! 为什么要有这一步：macOS 15.4 起 MediaRemote 只对「被授权」的进程返回数据，
//! 中间件必须借 `/usr/bin/perl` 的身份去读（原理见 `crates/media-bridge-mac-helper`）。
//! 那个 helper 是个 cdylib，需要在构建主二进制**之前**先建好，然后嵌进来 ——
//! 这样发布产物仍然是**单文件**，消费方（dsh-wallpaper-engine 等）不用改下载逻辑。
//!
//! 找的顺序：
//!   1. 环境变量 `MB_MAC_HELPER_DYLIB`（CI 与自定义构建显式指定）
//!   2. 目标目录里已构建的 `libmedia_bridge_mac_helper.dylib`（本地先 `cargo build -p
//!      media-bridge-mac-helper` 再构建主 crate）
//! 都找不到时不嵌：运行时那条路径会退化成「直连 MediaRemote」（macOS < 15.4 仍然可用），
//! 并在 `status`/`diagnose` 里说明。

fn main() {
    // 本 crate 自己声明的 cfg（供 check-cfg 认知，避免 unexpected_cfgs 警告）
    println!("cargo:rustc-check-cfg=cfg(mac_helper_embedded)");
    println!("cargo:rerun-if-env-changed=MB_MAC_HELPER_DYLIB");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    if let Some(path) = find_helper() {
        println!("cargo:rustc-env=MB_MAC_HELPER_DYLIB={}", path.display());
        println!("cargo:rustc-cfg=mac_helper_embedded");
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn find_helper() -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    if let Ok(p) = std::env::var("MB_MAC_HELPER_DYLIB") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // OUT_DIR = target/<profile>/build/media-bridge-<hash>/out → 上溯三层到 target/<profile>
    let out = PathBuf::from(std::env::var("OUT_DIR").ok()?);
    let profile_dir = out.parent()?.parent()?.parent()?.to_path_buf();
    for name in ["libmedia_bridge_mac_helper.dylib"] {
        let candidate = profile_dir.join(name);
        if Path::new(&candidate).is_file() {
            return Some(candidate);
        }
    }
    // 交叉/自定义 profile 下也可能落在 deps/ 里
    let deps = profile_dir.join("deps").join("libmedia_bridge_mac_helper.dylib");
    deps.is_file().then_some(deps)
}
