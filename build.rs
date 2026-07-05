fn main() {
    embed_resource::compile("res/floaty.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed resources");
    println!("cargo:rerun-if-changed=res/floaty.rc");
    println!("cargo:rerun-if-changed=res/floaty.manifest");
    println!("cargo:rerun-if-changed=res/floaty.ico");
}
