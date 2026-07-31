//! The short, human-typeable device-pairing code.
//!
//! Not a [`kallip_common::authtoken::MintedToken`] (those are 32-byte prefixed
//! bearers, not typeable). The pairing code is 8 chars from the Crockford base32
//! alphabet — `2^32^8 = 2^40` entropy, formatted `XXXX-XXXX` for display and QR
//! encoding. Only its SHA-256 hash is stored; the plaintext is shown once.

use kallip_common::authtoken::TokenHash;

/// The Crockford base32 encoding alphabet (digits + letters, excluding only
/// `I`, `L`, `O`, `U` to avoid confusion). Exactly 32 symbols — `0` and `1` ARE
/// valid. Pinned: the brute-force budget (2^40) depends on this exact size.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

// A random byte is 0..=256; mapping it with `% ALPHABET.len()` is unbiased only
// because 256 is an exact multiple of the alphabet size. Pin that invariant so
// a future alphabet change cannot silently introduce modulo bias.
const _: () = assert!(256 % ALPHABET.len() as u32 == 0);

/// Code length in symbols. `32^8 = 2^40` entropy.
const CODE_LEN: usize = 8;

/// Generate a fresh pairing code: 8 random Crockford symbols, formatted as
/// `XXXX-XXXX`. The dashes are display-only and stripped before hashing.
pub fn generate() -> String {
    let mut buf = [0u8; CODE_LEN];
    getrandom::fill(&mut buf).expect("getrandom pairing code entropy");
    // Unbiased: see the `const _` assert above (256 is a multiple of 32).
    let symbols: String = buf
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &symbols[..4], &symbols[4..])
}

/// Canonicalize a user/QR-submitted code before hashing: uppercase, then apply
/// Crockford input canonicalization (`O`→`0`, `I`/`L`→`1`) and strip dashes /
/// whitespace. Makes the typed `XXXX-XXXX` and the QR-encoded `XXXXXXXX` (and
/// common mis-reads like `O` for `0`) hash identically.
pub fn canonicalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

/// The hash stored as the pairing code's primary key.
pub fn hash_of(canonical_code: &str) -> TokenHash {
    TokenHash::of(canonical_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_is_exactly_32_symbols() {
        assert_eq!(ALPHABET.len(), 32, "entropy budget 2^40 depends on 32");
        // No excluded symbols present.
        for forbidden in b"ILOU" {
            assert!(
                !ALPHABET.contains(forbidden),
                "alphabet must exclude {}",
                *forbidden as char
            );
        }
        // All valid Crockford symbols are distinct.
        let mut sorted: Vec<u8> = ALPHABET.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 32, "alphabet must have no duplicates");
    }

    #[test]
    fn generated_codes_draw_from_the_alphabet_and_have_the_dashed_shape() {
        for _ in 0..256 {
            let code = generate();
            assert_eq!(code.len(), 9, "XXXX-XXXX is 9 chars");
            assert_eq!(code.chars().nth(4), Some('-'));
            let symbols: String = code.chars().filter(|c| *c != '-').collect();
            assert_eq!(symbols.len(), 8);
            for b in symbols.bytes() {
                assert!(ALPHABET.contains(&b), "{b} not in alphabet");
            }
        }
    }

    #[test]
    fn dashed_and_undashed_forms_hash_identically() {
        let dashed = "ABCD-EFGH";
        let undashed = "ABCDEFGH";
        assert_eq!(
            hash_of(&canonicalize(dashed)),
            hash_of(&canonicalize(undashed)),
        );
    }

    #[test]
    fn canonicalize_accepts_common_misreads_and_case() {
        // The QR / user may substitute O for 0, I/L for 1, and any case.
        assert_eq!(canonicalize("abcd-efgh"), "ABCDEFGH");
        assert_eq!(canonicalize("OBCD-EFGH"), "0BCDEFGH");
        assert_eq!(canonicalize("IBCD-EFGH"), "1BCDEFGH");
        assert_eq!(canonicalize("LBCD-EFGH"), "1BCDEFGH");
        assert_eq!(canonicalize(" ab cd "), "ABCD");
    }

    #[test]
    fn generated_codes_have_full_entropy_space() {
        // Generating many codes should produce many distinct values (sanity
        // check that the RNG is feeding all 8 positions, not a constant).
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1024 {
            seen.insert(generate());
        }
        assert!(
            seen.len() > 900,
            "expected near-unique codes, got {}",
            seen.len()
        );
    }
}
