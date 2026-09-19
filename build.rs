use std::path::PathBuf;

fn main() {
    // SDK 根目录查找顺序：
    //   1) 环境变量 SAP_SDK_DIR（推荐用于 Docker、CI、自定义安装路径）
    //   2) 环境变量 SAP_SDK_HOST_PATH（兼容 docker-compose.yml 里的旧名）
    //   3) ./nwrfcsdk（默认，仓库内子目录）
    let sdk_dir = std::env::var("SAP_SDK_DIR")
        .or_else(|_| std::env::var("SAP_SDK_HOST_PATH"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./nwrfcsdk"));

    // 根据目标平台三元组选择对应的 lib 子目录。
    // 目录约定：<sdk_dir>/lib/<os>-<arch>/
    //   - windows-x86_64   sapnwrfc.dll + sapnwrfc.lib + ICU dlls
    //   - linux-x86_64     libsapnwrfc.so (+ libsapucum.so)
    //   - linux-aarch64    同上 ARM64
    //   - darwin-x86_64    libsapnwrfc.dylib
    //   - darwin-aarch64    同上 Apple Silicon
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    // CARGO_CFG_TARGET_OS 对 macOS 返回 "macos"，但项目目录约定用 "darwin"
    let os_dir = if target_os == "macos" { "darwin" } else { &target_os };
    let target_dir = format!("{}-{}", os_dir, arch);

    let lib_dir = sdk_dir.join("lib").join(&target_dir);

    if !lib_dir.exists() {
        panic!(
            "未找到目标平台 [{target_dir}] 的 SAP NWRFC SDK 库目录：{}\n\
             请创建该目录并放入对应平台的 SDK 文件（见 nwrfcsdk/README.md）。\n\
             也可通过环境变量 SAP_SDK_DIR 指向已安装的 SDK 根目录。",
            lib_dir.display()
        );
    }

    // 检查目录内确实存在库文件（区分于「只有占位说明文件的空目录」）
    // .dll/.so/.dylib：真实 SDK 或 Unix stub 占位库
    // .lib：Windows 导入库（真实 SDK 含 sapnwrfc.lib + sapnwrfc.dll；
    //       CI 用 lib.exe 从 .def 生成的 stub sapnwrfc.lib 满足链接期符号解析）
    let has_lib = std::fs::read_dir(&lib_dir)
        .map(|entries| {
            entries.filter_map(Result::ok).any(|e| {
                let name = e.file_name().to_string_lossy().to_lowercase();
                name.ends_with(".dll")
                    || name.ends_with(".so")
                    || name.ends_with(".dylib")
                    || name.ends_with(".lib")
            })
        })
        .unwrap_or(false);

    if !has_lib {
        panic!(
            "目标平台 [{target_dir}] 的库目录中未找到任何 SDK 库文件 (.dll/.so/.dylib)：{}\n\
             目录可能只含占位说明。请从 SAP 下载对应平台的 NWRFC SDK 并把\n\
             sapnwrfc 的动态库放入该目录（详见 nwrfcsdk/README.md）。\n\
             也可通过环境变量 SAP_SDK_DIR 指向已安装的 SDK 根目录。",
            lib_dir.display()
        );
    }

    // 配置库文件搜索路径
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // 链接 SAP RFC 动态库（Windows: sapnwrfc.dll, Linux: libsapnwrfc.so）
    println!("cargo:rustc-link-lib=dylib=sapnwrfc");

    // CI stub 构建兼容：当 libsapnwrfc.so/.dylib 是空占位库（只含 stub 符号，
    // 没有 RfcOpenConnection 等真实符号）时，链接器会因 undefined symbol 报错。
    // 允许未定义符号在运行时解析（用户挂载真实 SDK 后即可正常调用）。
    // 真实 SDK 存在时符号会被正常解析，此选项无副作用。
    if target_os == "linux" {
        println!("cargo:rustc-link-arg=-Wl,--unresolved-symbols=ignore-all");
    } else if target_os == "macos" {
        println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
    }

    // 嵌入运行时库搜索路径（rpath），让可执行文件自动找到 SDK 动态库，
    // 用户无需手动设置 LD_LIBRARY_PATH / DYLD_LIBRARY_PATH。
    // 两条 rpath 覆盖两种场景，链接器依次尝试：
    //   @loader_path/nwrfcsdk/lib/<os>-<arch>            发布包（二进制与 nwrfcsdk/ 同级）
    //   @loader_path/../../nwrfcsdk/lib/<os>-<arch>      开发期（二进制在 target/release/）
    // Windows 不需要：DLL 搜索默认查 exe 同目录及其子目录。
    if target_os == "linux" || target_os == "macos" {
        let rpaths = [
            format!("@loader_path/nwrfcsdk/lib/{target_dir}"),
            format!("@loader_path/../../nwrfcsdk/lib/{target_dir}"),
        ];
        for rpath in &rpaths {
            if target_os == "linux" {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
            } else {
                // macOS 用 -rpath（与 Linux 同），注意带空格的写法
                println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
            }
        }
    }

    emit_git_commit();

    println!("cargo:warning=使用 SAP NWRFC SDK: {}", target_dir);
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SAP_SDK_DIR");
    println!("cargo:rerun-if-env-changed=SAP_SDK_HOST_PATH");
}

/// 注入 git 提交号（`/api/version` 自描述用），运行期经
/// `option_env!("SAP_GIT_COMMIT")` 读取。优先级：
///   1) 环境变量 SAP_BUILD_COMMIT（CI / 无 git 环境显式指定）
///   2) `git rev-parse --short HEAD`（工作树脏时附 `-dirty`）
///   3) 均不可用 → "unknown"（如从源码包构建），不 panic
///
/// 只监听 .git/HEAD 变化：同分支连续提交后 commit 号可能滞后到下次
/// 全量构建才刷新——发布构建（CI 全新 clone）不受影响，属可接受偏差。
fn emit_git_commit() {
    let commit = std::env::var("SAP_BUILD_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let short = std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(short) = short {
                // 工作树有未提交改动 → 标记 dirty（排查"连的是哪个构建"的关键信息）
                let dirty = std::process::Command::new("git")
                    .args(["status", "--porcelain"])
                    .output()
                    .map(|o| !o.stdout.is_empty())
                    .unwrap_or(false);
                Some(if dirty {
                    format!("{short}-dirty")
                } else {
                    short
                })
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SAP_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
