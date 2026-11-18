//! 本机时间戳：日志文件名与输出文件名共用。

use chrono::Local;

/// `YYYYMMDD-HHMMSS`，用于日志文件名
pub fn timestamp_file() -> String {
    Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// `YYYYMMDDHHMMSS`，用于 `received_file_<时间戳>`
pub fn timestamp_compact() -> String {
    Local::now().format("%Y%m%d%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stamp_has_expected_shape() {
        let stamp = timestamp_file();
        assert_eq!(stamp.len(), 15, "stamp={stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-', "stamp={stamp}");
        assert!(stamp
            .chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn compact_stamp_is_all_digits() {
        let stamp = timestamp_compact();
        assert_eq!(stamp.len(), 14, "stamp={stamp}");
        assert!(stamp.chars().all(|c| c.is_ascii_digit()));
    }
}
