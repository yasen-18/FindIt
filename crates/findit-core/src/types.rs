//! File type groups used by `type:` and the filter chips. Lists are sorted so
//! membership is a binary search.

pub const IMAGE: &[&str] = &[
    "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "ico", "jfif", "jpeg", "jpg", "jxl", "nef", "orf", "png", "psd",
    "raf", "raw", "rw2", "svg", "tga", "tif", "tiff", "webp",
];
pub const VIDEO: &[&str] =
    &["3gp", "asf", "avi", "f4v", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "ogv", "ts", "vob", "webm", "wmv"];
pub const AUDIO: &[&str] =
    &["aac", "aif", "aiff", "alac", "amr", "ape", "flac", "m4a", "m4b", "mid", "midi", "mp3", "mpa", "oga", "ogg", "opus", "wav", "wma"];
pub const DOC: &[&str] = &[
    "csv", "doc", "docm", "docx", "dot", "dotx", "epub", "md", "odp", "ods", "odt", "one", "pdf", "pps", "ppsx", "ppt", "pptm", "pptx",
    "rtf", "tex", "txt", "vsd", "vsdx", "xls", "xlsb", "xlsm", "xlsx", "xps",
];
pub const ARCHIVE: &[&str] = &[
    "7z", "arj", "bz2", "cab", "gz", "img", "iso", "lz", "lz4", "lzh", "lzma", "rar", "tar", "tbz2", "tgz", "txz", "vhd", "vhdx", "wim",
    "xz", "z", "zip", "zst",
];
pub const EXE: &[&str] = &["appx", "appxbundle", "bat", "cmd", "com", "exe", "lnk", "msi", "msix", "msixbundle", "ps1", "scr", "vbs"];
pub const CODE: &[&str] = &[
    "bash", "c", "cc", "cpp", "cs", "css", "go", "h", "hpp", "html", "java", "js", "json", "jsx", "kt", "lua", "m", "php", "pl", "py", "r",
    "rb", "rs", "scss", "sh", "sql", "swift", "toml", "ts", "tsx", "vue", "xml", "yaml", "yml",
];

/// Names accepted by `type:` and the list each maps to.
pub fn group(name: &str) -> Option<&'static [&'static str]> {
    Some(match name.to_ascii_lowercase().as_str() {
        "image" | "images" | "picture" | "pictures" | "photo" | "photos" => IMAGE,
        "video" | "videos" | "movie" | "movies" => VIDEO,
        "audio" | "music" | "sound" => AUDIO,
        "doc" | "docs" | "document" | "documents" => DOC,
        "archive" | "archives" | "compressed" => ARCHIVE,
        "exe" | "app" | "apps" | "program" | "programs" | "executable" => EXE,
        "code" | "source" => CODE,
        _ => return None,
    })
}

/// Extension (after the last dot) of a folded name, or `None`.
#[inline]
pub fn ext_of(folded_name: &[u8]) -> Option<&[u8]> {
    let dot = memchr::memrchr(b'.', folded_name)?;
    let ext = &folded_name[dot + 1..];
    (!ext.is_empty()).then_some(ext)
}

/// True when `ext` (folded) is in the sorted list.
#[inline]
pub fn in_list(ext: &[u8], list: &[&str]) -> bool {
    list.binary_search_by(|probe| probe.as_bytes().cmp(ext)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_sorted_and_unique() {
        for list in [IMAGE, VIDEO, AUDIO, DOC, ARCHIVE, EXE, CODE] {
            assert!(list.windows(2).all(|w| w[0] < w[1]), "{list:?}");
        }
    }

    #[test]
    fn membership() {
        assert!(in_list(b"jpg", IMAGE));
        assert!(!in_list(b"jpgx", IMAGE));
        assert_eq!(ext_of(b"report.final.pdf"), Some(&b"pdf"[..]));
        assert_eq!(ext_of(b"makefile"), None);
        assert_eq!(ext_of(b"trailing."), None);
        assert_eq!(group("Images"), Some(IMAGE));
        assert_eq!(group("nope"), None);
    }
}
