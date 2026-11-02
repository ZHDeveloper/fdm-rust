fn main() {
    tauri_build::build();
    // xwin 提供的 MSVC CRT 静态库（libcmt.lib 等）内部仍引用微软构建机上的
    // PDB 路径（D:\a\_work\...\libcmt.amd64.pdb），本机不存在，链接时会刷一堆
    // LNK4099 “Cannot use debug info”。纯噪音、无副作用，屏蔽即可。
    // 只对最终链接阶段生效，不影响其他 crate 的编译指纹。
    //
    // 注意：/ignore:4099 是 MSVC link.exe 的专有参数。构建脚本按宿主机编译，
    // 只能通过 CARGO_CFG_TARGET_ENV 判断目标平台；macOS 的 clang/ld 不认识该
    // 参数，若一并输出会把 /ignore:4099 当作输入文件而链接失败。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/ignore:4099");
    }
}
