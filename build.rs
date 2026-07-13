fn main() {
    embed_resource::compile("EveRusthing.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed the EveRusthing application manifest");
}
