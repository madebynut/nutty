//! Platform defaults used before native font discovery.

pub fn default_latin() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(windows) {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

pub fn default_chinese() -> &'static str {
    if cfg!(target_os = "macos") {
        "PingFang SC"
    } else if cfg!(windows) {
        "Microsoft YaHei"
    } else {
        "Noto Sans CJK SC"
    }
}
