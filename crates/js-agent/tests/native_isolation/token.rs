//! The random token every left-behind thing carries, and the matcher that finds tokens in
//! text a view of the machine printed.

use std::fmt;

/// Eight random bytes, shown as sixteen lowercase hex digits. Every item run A leaves
/// behind has its own: in its name, its key or its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    bytes: [u8; 8],
    text: String,
}

impl Token {
    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        Self {
            bytes,
            text: String::new(),
        }
    }

    pub fn fresh() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).expect("the system has random bytes");
        Self::from_bytes(bytes)
    }

    /// The token written as `as_str` prints it: sixteen lowercase hex digits and nothing
    /// else.
    pub fn parse(text: &str) -> Option<Self> {
        let _ = text;
        None
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The key of this token's System V objects: the last four bytes, never 0 (which
    /// is `IPC_PRIVATE`, a key nobody can look up again).
    pub fn sysv_key(&self) -> i32 {
        0
    }

    /// The name of this token's POSIX objects: a slash, `prefix`, the token. macOS holds
    /// a POSIX name to 31 bytes, so the prefix is short.
    pub fn posix_name(&self, prefix: &str) -> String {
        let _ = prefix;
        String::new()
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// The tokens of `tokens` that occur in `text`, each once, in the order given.
pub fn find_tokens<'a>(text: &str, tokens: &'a [Token]) -> Vec<&'a Token> {
    let _ = (text, tokens);
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
    const B: [u8; 8] = [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];

    /// Asserts the token is its bytes as sixteen lowercase hex digits and that `parse`
    /// takes exactly that spelling back. It catches a token printed in upper case or
    /// short, which no view of the machine would match.
    #[test]
    fn a_token_is_sixteen_lowercase_hex_digits_and_parses_back() {
        let token = Token::from_bytes(A);
        assert_eq!(token.as_str(), "0123456789abcdef");
        assert_eq!(token.to_string(), "0123456789abcdef");
        assert_eq!(Token::parse("0123456789abcdef"), Some(token));
        for bad in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123456789ABCDEF",
            "0123456789abcdeg",
            " 123456789abcdef",
        ] {
            assert_eq!(Token::parse(bad), None, "{bad:?} was accepted");
        }
    }

    /// Asserts that two fresh tokens differ and that the System V key is the token's last
    /// four bytes, and never 0 whatever the bytes. It catches a key of 0, which is
    /// `IPC_PRIVATE`: the object would be made but run B could never look it up.
    #[test]
    fn the_system_v_key_comes_from_the_token_and_is_never_zero() {
        assert_ne!(Token::fresh(), Token::fresh());
        assert_eq!(Token::from_bytes(A).sysv_key(), 0x89ab_cdef_u32 as i32);
        assert_ne!(
            Token::from_bytes(B).sysv_key(),
            Token::from_bytes(A).sysv_key()
        );
        assert_ne!(Token::from_bytes([0; 8]).sysv_key(), 0);
        assert_ne!(Token::from_bytes([9, 9, 9, 9, 0, 0, 0, 0]).sysv_key(), 0);
    }

    /// Asserts that the POSIX name starts with a slash, holds the token and fits the 31
    /// bytes macOS allows. It catches a prefix long enough to make `shm_open` refuse the
    /// name, which would show as an object that was never left.
    #[test]
    fn a_posix_name_has_a_slash_the_token_and_fits_macos() {
        let token = Token::from_bytes(A);
        let name = token.posix_name("jsi");
        assert_eq!(name, "/jsi0123456789abcdef");
        assert!(name.len() <= 31);
    }

    /// Asserts that the matcher finds each token present once, in the order given, finds
    /// none for text that has only part of a token, and ignores tokens it was not given.
    /// It catches a matcher that is satisfied by a prefix, which would call a clean view
    /// dirty (or the reverse).
    #[test]
    fn the_matcher_finds_whole_tokens_only() {
        let a = Token::from_bytes(A);
        let b = Token::from_bytes(B);
        let both = [a.clone(), b.clone()];
        let text = "700 411 /bin/sh -c sleep 600 fedcba9876543210\nlabel com.x.0123456789abcdef";
        assert_eq!(find_tokens(text, &both), vec![&a, &b]);
        assert_eq!(find_tokens(text, &both[1..]), vec![&b]);
        assert!(find_tokens("0123456789abcde fedcba987654321", &both).is_empty());
        assert!(find_tokens("", &both).is_empty());
        assert!(find_tokens(text, &[]).is_empty());
        let twice = format!("{a} {a}");
        assert_eq!(find_tokens(&twice, &both), vec![&a]);
    }
}
