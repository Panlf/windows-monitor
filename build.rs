fn main() {
    // 将 winmon.rc 作为资源嵌入 exe：RT_ICON(1) 应用图标 + RT_MANIFEST 应用清单
    let _ = embed_resource::compile("winmon.rc", embed_resource::NONE);
    println!("cargo:rerun-if-changed=winmon.rc");
    println!("cargo:rerun-if-changed=winmon.ico");
    println!("cargo:rerun-if-changed=winmon.manifest");
}
