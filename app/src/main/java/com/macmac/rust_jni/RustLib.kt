package com.macmac.rust_jni

object RustLib {
    external fun prove(domain: String, uri: String)

    init {
        System.loadLibrary("rust_native")
    }
}
