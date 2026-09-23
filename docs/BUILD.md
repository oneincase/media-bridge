# 构建与分发：每个平台/架构都要单独构建吗？

**简短回答**：是的。Rust 编译成**本机机器码**，没有 JVM/解释器那一层，所以每个
「操作系统 × CPU 架构」组合都要单独构建一次。对这个项目还要多一层原因：
三平台的后端是 `cfg` 门控的（macOS 用 MediaRemote + CoreAudio，Windows 用 GSMTC + WASAPI，
Linux 用 MPRIS + PipeWire）——**同一份源码在不同平台上编译出的是不同的代码**，
不存在「一份二进制通用」的可能。

好消息是：**需要构建的组合没有想象中多**，而且 macOS 能把 x64/arm64 合成一个通用包。

## 1. 目标矩阵

| 平台 | target triple | 是否必须 | 说明 |
|---|---|---|---|
| macOS ARM | `aarch64-apple-darwin` | ✅ | Apple Silicon |
| macOS Intel | `x86_64-apple-darwin` | ✅ | 可与上一行**合成一个通用二进制**（见 §2） |
| Windows x64 | `x86_64-pc-windows-msvc` | ✅ | 覆盖绝大多数 Windows |
| Windows ARM | `aarch64-pc-windows-msvc` | ⭕ 可选 | **Windows 11 ARM64 能直接跑 x64 版本**（系统自带模拟），所以只发 x64 也能覆盖它；原生 ARM64 只是更快更省电 |
| Linux x64 (glibc) | `x86_64-unknown-linux-gnu` | ✅ | 主流桌面发行版 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | ⭕ 可选 | 树莓派 4/5、ARM 服务器 |
| Linux 静态 (musl) | `x86_64-unknown-linux-musl` | ⭕ 可选 | 静态链接，任何发行版（含 Alpine）都能跑；代价是不能用系统动态库 |

**覆盖全部用户的最小集合是 4 个产物**：macOS 通用包 + Windows x64 + Linux gnu x64 + Linux gnu arm64。
要「每个平台都原生」就是 6 个。

## 2. macOS：x64 + arm64 合成一个通用二进制

macOS 的 fat binary（universal 2）是平台自带的能力，一条 `lipo` 就能合。本仓库实测过：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create \
  target/aarch64-apple-darwin/release/media-bridge \
  target/x86_64-apple-darwin/release/media-bridge \
  -output media-bridge-darwin-universal

lipo -info media-bridge-darwin-universal
# → Architectures in the fat file: x86_64 arm64
arch -arm64  ./media-bridge-darwin-universal version   # → 平台：macos aarch64
arch -x86_64 ./media-bridge-darwin-universal version   # → 平台：macos x86_64
```

体积代价约 2 倍（本机实测：arm64-only 4.3MB、x64-only 4.9MB、通用包 9.2MB），
对一个中间件二进制完全可以接受。

**注意**：`lipo` 输入路径写错时**不会报错**，只会少一个切片 —— 合完必须用 `lipo -info` 核对
（我第一次就把 `target/release/…` 当成了 `target/aarch64-apple-darwin/release/…`）。

Windows 没有等价机制（x64/ARM64 是两套 PE），Linux 也没有（可以用 `objcopy` 之类硬塞多架构，
但发行版生态不认，别折腾）。

## 3. 能不能在一台机器上交叉编译出全部产物？

可以省掉一部分机器，但每一条都有前提：

| 从 ↓ 出 → | macOS | Windows | Linux |
|---|---|---|---|
| **macOS** | ✅ 原生 | ✅ msvc 用 [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin)（自动拉 MSVC 头文件与导入库）；gnu 用 mingw-w64 | ⚠️ 需要 Linux sysroot + 链接器（[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) 或 `cross`（Docker）） |
| **Linux** | ❌ 需要 Apple SDK，且许可不允许 | ✅ 同上（xwin / mingw） | ✅ 原生 |
| **Windows** | ❌ 同上 | ✅ 原生 | ✅ WSL 里当 Linux 用 |

结论：**最省事的做法是让 CI 的矩阵来构建**，而不是在一台机器上堆交叉工具链 ——
GitHub Actions 上每个系统都有现成 runner，构建完直接上传产物。

## 4. GitHub Actions 矩阵（可直接抄）

```yaml
name: release
on:
  push:
    tags: ['v*']
jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        include:
          - { runner: macos-14,        target: aarch64-apple-darwin,      name: darwin-arm64 }
          - { runner: macos-13,        target: x86_64-apple-darwin,       name: darwin-x64 }
          - { runner: windows-2022,    target: x86_64-pc-windows-msvc,    name: win32-x64 }
          - { runner: windows-11-arm,  target: aarch64-pc-windows-msvc,   name: win32-arm64 }
          - { runner: ubuntu-22.04,    target: x86_64-unknown-linux-gnu,  name: linux-x64 }
          - { runner: ubuntu-24.04-arm, target: aarch64-unknown-linux-gnu, name: linux-arm64 }
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: '${{ matrix.target }}' }
      - run: cargo build --release --target ${{ matrix.target }} --features embedded
      - uses: actions/upload-artifact@v4
        with:
          name: media-bridge-${{ matrix.name }}
          path: |
            target/${{ matrix.target }}/release/media-bridge
            target/${{ matrix.target }}/release/media-bridge.exe

  # macOS 两个切片合成通用包（另一个 job，依赖上面两个）
  universal:
    needs: build
    runs-on: macos-14
    steps:
      - uses: actions/download-artifact@v4
      - run: |
          lipo -create media-bridge-darwin-arm64/media-bridge \
                      media-bridge-darwin-x64/media-bridge \
                      -output media-bridge-darwin-universal
          lipo -info media-bridge-darwin-universal
      - uses: actions/upload-artifact@v4
        with: { name: media-bridge-darwin-universal, path: media-bridge-darwin-universal }
```

> **可选 vs 必需**：`build-macos` / `build-windows`(x64) / `build-linux` 是发布依赖链（必需，
> 挂了就不发）；`build-win32-arm64` 是可选 tail job（超时 25 分钟、`continue-on-error`，
> 卡住或失败都不挡发布，成功才追加附件）。`ubuntu-22.04-arm`（linux-arm64）实测稳定，
> 照旧放在必需里。
>
> runner 名字（`windows-11-arm`、`ubuntu-24.04-arm` 这类 ARM runner）在**公开仓库**里可用，
> 但 GitHub 会调整可用列表 —— 用之前对一下
> [官方 runner 文档](https://docs.github.com/actions/using-github-hosted-runners)。
> 私有仓库上 ARM runner 可能不可用，那就只能交叉编译（§3）或自建 runner。

## 4b. 本仓库已经配好的 CI（直接拿产物）

`.github/workflows/build.yml` 已经把上面这套矩阵落地了，**不需要你自己搭**：

| 触发 | 行为 |
|---|---|
| 打 `v*` 标签 | 跑三平台测试 → 构建全部产物 → **发 GitHub Release**（带 SHA256SUMS） |
| push 到 `main` | 跑测试 + 构建产物（产物在 Actions 页面的 artifacts 里，可保留 90 天） |
| Pull Request | 只跑三平台测试 |
| 手动 dispatch | 跑测试 + 构建产物 |

产物命名对齐 Node 的 `process.platform` / `process.arch`，插件侧可以直接按名字挑：

| 产物 | 内容 | 备注 |
|---|---|---|
| `media-bridge-darwin-universal` | Mach-O **通用二进制**（x86_64 + arm64 两个切片） | 13.1 MB；已在 CI 里 `lipo -info` 核对两个切片都在 |
| `media-bridge-win32-x64.exe` | PE32+ x86-64 | 7.7 MB；x64 与 ARM64 Windows 都能跑（后者靠系统模拟） |
| `media-bridge-win32-arm64.exe` | PE32+ Aarch64 | 6.4 MB；原生 ARM64。**可选产物**：`windows-11-arm` runner 实测会长时间卡在收尾步骤（2026-09-23：同一 job 25 分钟无进展，取消重跑后又卡同一个），所以它**不参与发布依赖链** —— 单独一个带 25 分钟上限的 job，成功后再由 `attach-win32-arm64` 补传到 Release（并顺手把哈希补进 SHA256SUMS）。没有它时 Windows ARM64 用 x64 产物即可 |
| `media-bridge-linux-x64` | ELF x86-64（动态） | 11.0 MB；需要 **glibc ≥ 2.34**（Ubuntu 22.04+ / Debian 12+ / RHEL 9+） |
| `media-bridge-linux-arm64` | ELF aarch64（动态） | 9.7 MB；同样 glibc ≥ 2.34 |
| `media-bridge-linux-x64-musl` | ELF x86-64 **static-pie** | 11.1 MB；**完全不依赖 glibc**，Alpine / 老发行版直接跑 |

取产物（两条路都行，推荐第一条）：

```bash
# 1) 从 Release 直链下载（最稳，也是脚本里最好用的形式）
curl -LO https://github.com/oneincase/media-bridge/releases/download/v0.1.1/media-bridge-darwin-universal
curl -LO https://github.com/oneincase/media-bridge/releases/download/v0.1.1/SHA256SUMS
shasum -a 256 -c SHA256SUMS

# 2) 从 CI 运行里取（产物保留 90 天）
gh run download <run-id> -R oneincase/media-bridge
```

> **`gh release download` 的一个坑**：它走的是 REST API 里 release 对象的 `assets` 字段，
> 而这个字段在部分新建 Release 上会返回空数组（实测踩到：网页版能正常列出 7 个附件、
> 直链也能下载，但 `gh release view --json assets` / `gh release download` 报「没有附件」）。
> 所以 **CI 的发布验收改成按直链校验**（逐个 curl 到 HTTP 200 + 体积检查），
> 而不是查那个字段 —— 拿它当验收标准会既误报、又误导排查。

CI 里每个产物构建完都会在**自己的原生 runner 上冒烟测试**（`version` + `diagnose`），
macOS 那个还会强制核对两个切片都在 —— 产物是「跑过一遍的」而不是「编译出来就发的」。

## 5. 对本项目两个消费方的实际影响

**WallpaperEM（Tauri 2 + Rust）：零额外工作。**
Tauri 的构建本来就是「一台机器构建一个平台」，把 media-bridge 作为 path 依赖加进 `src-tauri`：

```toml
# WallpaperEM/src-tauri/Cargo.toml
media-bridge = { path = "../../media-bridge/crates/media-bridge", features = ["embedded"] }
```

于是 macOS 构建机编译出 MediaRemote/CoreAudio 那一套、Windows 构建机编译出 GSMTC/WASAPI 那一套，
Tauri 打包的 `.app` / `.msi` / `.deb` 也各自按平台产出 —— **不需要为本项目单独准备产物**。

**dsh-wallpaper-engine（Node / Electron 插件）：需要预编译产物。**
宿主是 Node，没法在运行时「按平台编译」。两种做法：

1. **发预编译产物**（推荐，也是 Electron 生态的惯例，参考 `sharp` / `esbuild`）：
   按 `<平台>-<架构>` 命名，运行时按 `process.platform` + `process.arch` 选：

   ```js
   const name = `media-bridge-${process.platform}-${process.arch}${process.platform === 'win32' ? '.exe' : ''}`;
   const bin = path.join(__dirname, 'bin', name);
   ```
   macOS 只放一个 `media-bridge-darwin-universal` 也行（`process.arch` 判断可以省掉）。
2. 首次运行时用 `cargo` 现场编译 —— 要求用户机器上有 Rust 工具链，**不推荐**。

也就是说：**本项目确实需要按平台构建，但这份工作可以完全交给 CI 矩阵**；
消费方（Tauri / Node 插件）各自按自己的惯例拿对应产物即可。

### 5b. macOS：helper 要先建（权限通道）

macOS 15.4 起 MediaRemote 只对「被授权」的进程返回数据，中间件必须借 `/usr/bin/perl`
的身份去读（原理见 README 的「MediaRemote 权限通道」）。那个 helper 是个 cdylib，
由 `crates/media-bridge/build.rs` **嵌进主二进制**（发布产物仍是单文件）。所以：

```bash
cargo build --release -p media-bridge-mac-helper   # 先
cargo build --release                              # 后（build.rs 会找到并嵌入）
```

顺序反了**不会报错**，只是运行时退回「进程内直连」—— 在 macOS 15.4 以上读不到正在播放。
判据：`media-bridge diagnose` 会打印当前走的是哪条通道。CI 的 darwin 任务已把两步都排好
（两个 target 各建各自架构的 helper，通用二进制里每个切片嵌自己那一份）。

## 6. 为什么不能在别的平台上「顺带验证」

一个具体的例子：`friendly_app_name()`（把 `Spotify.exe` 这类 AUMID 变成可读名字）只存在于
`platform/windows.rs` 里，在 macOS/Linux 上**根本不会被编译**。它的一个顺序 bug
（先切点号段再去 `.exe`，会把 `Spotify.exe` 变成 `exe`）在 macOS 上跑一万次单元测试、
做多少遍交叉编译检查都不会暴露 —— 只有把测试拿到真 Windows 上跑才会现形。
同理，Windows 上 `diagnose` 的 panic（tokio 运行时里 `blocking_recv`）也只有真机才会崩。

所以本项目的验证策略是：**每个平台都在真机上跑一遍 `cargo test` 与功能自检**，
而不是「本机测试通过 + 交叉编译通过」就当验证过了。具体每条结论的证据见
[`PLATFORM.md`](PLATFORM.md)。
