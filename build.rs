fn main() {
    // When statically linking FFmpeg on Windows (e.g. via vcpkg), the avcodec
    // library's Media Foundation encoder references COM interfaces that live in
    // Windows SDK system libraries. Link them explicitly so the linker can
    // resolve IID_ICodecAPI, IID_IMFMediaEventGenerator, IID_IMFTransform, etc.
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=mfuuid");
        println!("cargo:rustc-link-lib=strmiids");
        println!("cargo:rustc-link-lib=mfplat");
        println!("cargo:rustc-link-lib=ole32");
        println!("cargo:rustc-link-lib=user32");
    }
}
