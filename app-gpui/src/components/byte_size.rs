//! Byte counts, for humans.

/// `812 B`, `4.2 KB`, `13.7 MB`, `1.1 GB` — one decimal above a kilobyte and
/// none below it, because "1.0 KB" is worse than "1024 B" at telling you how
/// big something is, and a fractional byte is nonsense.
///
/// Units of 1024 labelled KB/MB/GB: the SI pedantry (KiB) buys nothing here.
/// The only reader is a human deciding whether a preserved implementation is
/// worth keeping, and the answer never turns on 2.4%.
pub fn byte_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    const MB: f64 = KB * 1024.;
    const GB: f64 = MB * 1024.;
    let n = bytes as f64;
    if n < KB {
        format!("{bytes} B")
    } else if n < MB {
        format!("{:.1} KB", n / KB)
    } else if n < GB {
        format!("{:.1} MB", n / MB)
    } else {
        format!("{:.1} GB", n / GB)
    }
}

#[cfg(test)]
mod tests {
    use super::byte_size;

    #[test]
    fn sizes_read_the_way_a_file_listing_does() {
        assert_eq!(byte_size(0), "0 B");
        assert_eq!(byte_size(812), "812 B");
        // The boundary in both directions: bytes below, one decimal at.
        assert_eq!(byte_size(1023), "1023 B");
        assert_eq!(byte_size(1024), "1.0 KB");
        assert_eq!(byte_size(4300), "4.2 KB");
        assert_eq!(byte_size(14_365_491), "13.7 MB");
        assert_eq!(byte_size(1_181_116_006), "1.1 GB");
    }
}
