#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    /// Generator over `Perm` (any of the 32 possible 5-bit bitmaps).
    fn perm_gen() -> impl Generator<Perm> {
        gs::integers::<u8>()
            .min_value(0)
            .max_value(0b1_1111)
            .map(Perm::from_bits_truncate)
    }

    /// Generator over short ASCII identifiers - good enough for db / user
    /// names.  `fullmatch(true)` ensures the *entire* string is alphanumeric;
    /// the default `from_regex` only requires a substring match, which would
    /// happily include `\r`, `\n`, or whitespace and break the line-oriented
    /// user-store format.  This matches the identifier grammar enforced by
    /// the QL parser before any name reaches `User` / `UserStore`.
    fn name_gen() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9_]{0,15}").fullmatch(true)
    }

    /// Generator over a grants HashMap.
    fn grants_gen() -> impl Generator<HashMap<String, Perm>> {
        gs::vecs(name_gen().flat_map(|n| perm_gen().map(move |p| (n.clone(), p))))
            .max_size(6)
            .map(|pairs| pairs.into_iter().collect())
    }

    #[hegel::test]
    fn pbt_perm_display_parse_roundtrip(tc: TestCase) {
        let p = tc.draw(perm_gen());
        let s = p.to_string();
        assert_eq!(Perm::parse(&s).unwrap(), p);
    }

    #[hegel::test]
    fn pbt_perm_parse_letter_order_is_irrelevant(tc: TestCase) {
        // Any permutation of a subset of CRUDA parses to the same Perm.
        let p = tc.draw(perm_gen());
        let s = p.to_string();
        let mut chars: Vec<char> = s.chars().collect();
        if chars.len() <= 1 || chars == vec!['-'] {
            return;
        }
        // Permute by rotating - cheap deterministic shuffle from a draw.
        let shift = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let len = chars.len();
        chars.rotate_left(shift % len.max(1));
        let permuted: String = chars.into_iter().collect();
        assert_eq!(Perm::parse(&permuted).unwrap(), p);
    }

    #[hegel::test]
    fn pbt_perm_parse_case_insensitive(tc: TestCase) {
        let p = tc.draw(perm_gen());
        let upper = p.to_string();
        let lower = upper.to_lowercase();
        assert_eq!(Perm::parse(&upper), Perm::parse(&lower));
    }

    #[test]
    fn perm_parse_special_tokens() {
        assert_eq!(Perm::parse("*").unwrap(), Perm::ALL);
        assert_eq!(Perm::parse("-").unwrap(), Perm::NONE);
        assert_eq!(Perm::parse("").unwrap(), Perm::NONE);
    }

    #[hegel::test]
    fn pbt_perm_parse_rejects_unknown_letter(tc: TestCase) {
        let bad = tc.draw(
            gs::characters().filter(|c| !"CRUDAcruda \t".contains(*c) && *c != '*' && *c != '-'),
        );
        let mut s = String::from("CR");
        s.push(bad);
        assert!(Perm::parse(&s).is_err());
    }

    #[hegel::test]
    fn pbt_perm_bitops_are_commutative_and_associative(tc: TestCase) {
        let a = tc.draw(perm_gen());
        let b = tc.draw(perm_gen());
        let c = tc.draw(perm_gen());
        assert_eq!(a | b, b | a);
        assert_eq!(a & b, b & a);
        assert_eq!((a | b) | c, a | (b | c));
        assert_eq!((a & b) & c, a & (b & c));
    }

    #[hegel::test]
    fn pbt_perm_contains_iff_bits_are_subset(tc: TestCase) {
        let p = tc.draw(perm_gen());
        let q = tc.draw(perm_gen());
        let expected = (p.bits() & q.bits()) == q.bits();
        assert_eq!(p.contains(q), expected);
    }

    #[hegel::test]
    fn pbt_perm_insert_remove_inverse(tc: TestCase) {
        let mut p = tc.draw(perm_gen());
        let q = tc.draw(perm_gen());
        let original = p;
        p.insert(q);
        p.remove(q);
        // After insert+remove of the same bits, the originally-not-present bits
        // are now gone; the result is `original & !q`.
        assert_eq!(p, Perm::from_bits_truncate(original.bits() & !q.bits()));
    }

    #[hegel::test]
    fn pbt_user_verify_accepts_correct_password_only(tc: TestCase) {
        let name = tc.draw(name_gen());
        let pw = tc.draw(gs::text().min_size(1).max_size(32));
        let other = tc.draw(gs::text().min_size(1).max_size(32).filter({
            let pw = pw.clone();
            move |s| *s != pw
        }));
        let u = User::new(&name, &pw, HashMap::new());
        assert!(u.verify(&pw));
        assert!(!u.verify(&other));
    }

    #[hegel::test]
    fn pbt_user_effective_equals_direct_union_wildcard(tc: TestCase) {
        let grants = tc.draw(grants_gen());
        let db = tc.draw(name_gen());
        let u = User::new("u", "p", grants.clone()); // codeql[rust/hard-coded-cryptographic-value]

        let direct = grants.get(&db).copied().unwrap_or_default();
        let wildcard = grants.get("*").copied().unwrap_or_default();
        let expected = direct | wildcard;

        assert_eq!(u.effective(&db), expected);
    }

    #[hegel::test]
    fn pbt_user_is_global_admin_iff_admin_on_wildcard(tc: TestCase) {
        let grants = tc.draw(grants_gen());
        let u = User::new("u", "p", grants.clone()); // codeql[rust/hard-coded-cryptographic-value]
        let expected = grants
            .get("*")
            .map(|p| p.contains(Perm::A))
            .unwrap_or(false);
        assert_eq!(u.is_global_admin(), expected);
    }

    #[hegel::test]
    fn pbt_user_to_from_line_roundtrip(tc: TestCase) {
        let name = tc.draw(name_gen());
        let pw = tc.draw(gs::text().min_size(1).max_size(32));
        let grants = tc.draw(grants_gen());
        let u = User::new(&name, &pw, grants);

        let line = u.to_line();
        let parsed = User::from_line(&line).expect("from_line on its own to_line must succeed");
        assert_eq!(parsed.name, u.name);
        assert_eq!(parsed.grants, u.grants);
        assert!(parsed.verify(&pw));
    }

    #[hegel::test]
    fn pbt_store_add_then_get_returns_user(tc: TestCase) {
        let name = tc.draw(name_gen());
        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        assert!(s.get(&name).is_some());
        assert!(s.add(User::new(&name, "p", HashMap::new())).is_err()); // codeql[rust/hard-coded-cryptographic-value]
    }

    #[hegel::test]
    fn pbt_store_remove_then_get_returns_none(tc: TestCase) {
        let name = tc.draw(name_gen());
        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        s.remove(&name).unwrap();
        assert!(s.get(&name).is_none());
        assert!(s.remove(&name).is_err());
    }

    #[hegel::test]
    fn pbt_store_grant_then_revoke_is_identity_on_perms(tc: TestCase) {
        let name = tc.draw(name_gen());
        let db = tc.draw(name_gen());
        let to_add = tc.draw(perm_gen());

        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        let before = s.get(&name).unwrap().effective(&db);
        s.grant(&name, &db, to_add).unwrap();
        s.revoke(&name, &db, to_add).unwrap();
        assert_eq!(s.get(&name).unwrap().effective(&db), before);
    }

    #[hegel::test]
    fn pbt_store_grant_unions_into_existing_perms(tc: TestCase) {
        let name = tc.draw(name_gen());
        let db = tc.draw(name_gen());
        let p1 = tc.draw(perm_gen());
        let p2 = tc.draw(perm_gen());

        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        s.grant(&name, &db, p1).unwrap();
        let result = s.grant(&name, &db, p2).unwrap();
        assert_eq!(result, p1 | p2);
    }

    #[hegel::test]
    fn pbt_store_persists_round_trip(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
        let dir = tempdir().unwrap();
        let path = dir.path().join("u");

        let entries: Vec<(String, String, HashMap<String, Perm>)> = (0..n)
            .map(|i| (format!("user{i}"), format!("pw{i}"), tc.draw(grants_gen())))
            .collect();

        {
            let mut s = UserStore::open(&path).unwrap();
            for (name, pw, grants) in &entries {
                s.add(User::new(name, pw, grants.clone())).unwrap();
            }
        }
        let s2 = UserStore::open(&path).unwrap();
        for (name, pw, grants) in &entries {
            assert!(s2.verify(name, pw).is_some(), "verify after reload {name}");
            assert_eq!(&s2.get(name).unwrap().grants, grants);
        }
    }

    #[test]
    fn store_open_returns_empty_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope");
        let s = UserStore::open(&path).unwrap();
        assert!(s.names().is_empty());
    }

    #[test]
    fn store_open_rejects_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("u");
        fs::write(&path, "only_one_field\n").unwrap();
        assert!(UserStore::open(&path).is_err());
    }
}