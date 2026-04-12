fn main() {
    cc::Build::new()
        .file("mbelib/mbelib.c")
        .file("mbelib/ecc.c")
        .file("mbelib/imbe7200x4400.c")
        .file("mbelib/imbe7100x4400.c")
        .file("mbelib/ambe3600x2400.c")
        .file("mbelib/ambe3600x2450.c")
        .include("mbelib")
        // MSVC needs _USE_MATH_DEFINES for M_PI / M_E
        .define("_USE_MATH_DEFINES", None)
        .warnings(false)
        .compile("mbelib");
}
