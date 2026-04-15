use sha2::{Digest, Sha256};

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hash() {
        let hash = sha256_hex(b"hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn empty_input() {
        let hash = sha256_hex(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn same_content_same_hash() {
        let a = sha256_hex(b"fn main() {}");
        let b = sha256_hex(b"fn main() {}");
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_different_hash() {
        let a = sha256_hex(b"hello");
        let b = sha256_hex(b"world");
        assert_ne!(a, b);
    }
}
