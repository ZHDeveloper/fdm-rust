fn main() {
    tauri_build::build();
    // xwin 提供的 MSVC CRT 静态库（libcmt.lib 等）内部仍引用微软构建机上的
    // PDB 路径（D:\a\_work\...\libcmt.amd64.pdb），本机不存在，链接时会刷一堆
    // LNK4099 “Cannot use debug info”。纯噪音、无副作用，屏蔽即可。
    // 只对最终链接阶段生效，不影响其他 crate 的编译指纹。
    println!("cargo:rustc-link-arg=/ignore:4099");
}
