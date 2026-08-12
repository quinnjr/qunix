//! Pacman-compatible version ordering, implemented from the documented
//! behaviour in vercmp(8) and PKGBUILD(5): `[epoch:]pkgver[-pkgrel]`, epoch
//! dominating, segments compared numerically when numeric and lexically when
//! alphabetic, digits outranking letters, and a *trailing attached* alpha
//! run ageing a version (`1.0a < 1.0`) while any separated trailer advances
//! it (`1.0.a > 1.0`). Corner cases the man page does not pin down (e.g.
//! repeated separators) are this implementation's own choices, fixed by the
//! tests below — qpkg only ever compares an upstream version against a built
//! one, so agreement with the documented ordering is the requirement.

use std::cmp::Ordering;

pub fn vercmp(a: &str, b: &str) -> Ordering {
    let (ae, av, ar) = split(a);
    let (be, bv, br) = split(b);
    cmp_numeric(ae.as_bytes(), be.as_bytes())
        .then_with(|| cmp_segments(av, bv))
        .then_with(|| match (ar, br) {
            // A missing pkgrel matches any pkgrel, the same way dependency
            // resolution treats an unqualified version.
            (Some(x), Some(y)) => cmp_segments(x, y),
            _ => Ordering::Equal,
        })
}

/// `[epoch:]pkgver[-pkgrel]` → (epoch, pkgver, pkgrel). The epoch prefix only
/// counts when it is wholly numeric; pkgver cannot contain `-`, so the *last*
/// dash starts the pkgrel.
fn split(v: &str) -> (&str, &str, Option<&str>) {
    let (epoch, rest) = match v.find(':') {
        Some(i) if i > 0 && v[..i].bytes().all(|b| b.is_ascii_digit()) => (&v[..i], &v[i + 1..]),
        _ => ("0", v),
    };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], Some(&rest[i + 1..])),
        None => (epoch, rest, None),
    }
}

fn cmp_segments(a: &str, b: &str) -> Ordering {
    let mut x = a.as_bytes();
    let mut y = b.as_bytes();
    loop {
        // Exhaustion is checked *before* separators are skipped: the survival
        // of a separator is what distinguishes `1.0.a` (separated trailer,
        // newer than `1.0`) from `1.0a` (attached alpha, older than `1.0`) at
        // the showdown below.
        if x.is_empty() || y.is_empty() {
            break;
        }
        while let [c, rest @ ..] = x {
            if c.is_ascii_alphanumeric() {
                break;
            }
            x = rest;
        }
        while let [c, rest @ ..] = y {
            if c.is_ascii_alphanumeric() {
                break;
            }
            y = rest;
        }
        let (Some(&cx), Some(&cy)) = (x.first(), y.first()) else {
            break;
        };
        let ord = if cx.is_ascii_digit() {
            if !cy.is_ascii_digit() {
                // A numeric segment outranks an alphabetic one: 1.0.1 > 1.0.a.
                return Ordering::Greater;
            }
            let (xs, xr) = take_run(x, u8::is_ascii_digit);
            let (ys, yr) = take_run(y, u8::is_ascii_digit);
            (x, y) = (xr, yr);
            cmp_numeric(xs, ys)
        } else {
            if cy.is_ascii_digit() {
                return Ordering::Less;
            }
            let (xs, xr) = take_run(x, u8::is_ascii_alphabetic);
            let (ys, yr) = take_run(y, u8::is_ascii_alphabetic);
            (x, y) = (xr, yr);
            xs.cmp(ys)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // One side is exhausted. A leftover that *starts alphabetic* is a
    // pre-release marker and ages its version (1.0rc < 1.0); anything else
    // left over — a separator, a number — advances it (1.0.1 > 1.0, and
    // 1.0.a > 1.0 because the dot is what remains at this point).
    match (x.first(), y.first()) {
        (None, None) => Ordering::Equal,
        (Some(c), None) => {
            if c.is_ascii_alphabetic() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (None, Some(c)) => {
            if c.is_ascii_alphabetic() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        // The loop only exits with at least one side empty.
        (Some(_), Some(_)) => unreachable!(),
    }
}

fn take_run(s: &[u8], pred: impl Fn(&u8) -> bool) -> (&[u8], &[u8]) {
    let n = s.iter().take_while(|c| pred(c)).count();
    s.split_at(n)
}

/// Digit-string comparison: leading zeros are insignificant, then a longer
/// run is a bigger number, then lexical order decides.
fn cmp_numeric(a: &[u8], b: &[u8]) -> Ordering {
    let a = trim_zeros(a);
    let b = trim_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn trim_zeros(mut s: &[u8]) -> &[u8] {
    while let [b'0', rest @ ..] = s {
        s = rest;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts `a < b` and, because ordering must be antisymmetric to be an
    /// ordering at all, `b > a`.
    fn lt(a: &str, b: &str) {
        assert_eq!(vercmp(a, b), Ordering::Less, "{a} should sort before {b}");
        assert_eq!(vercmp(b, a), Ordering::Greater, "{b} should sort after {a}");
    }

    fn eq(a: &str, b: &str) {
        assert_eq!(vercmp(a, b), Ordering::Equal, "{a} should equal {b}");
        assert_eq!(vercmp(b, a), Ordering::Equal, "{b} should equal {a}");
    }

    #[test]
    fn the_documented_suffix_chain_in_full() {
        // Verbatim from vercmp(8). Every adjacent pair, both directions.
        let chain = ["1.0a", "1.0b", "1.0beta", "1.0p", "1.0pre", "1.0rc", "1.0", "1.0.a", "1.0.1"];
        for w in chain.windows(2) {
            lt(w[0], w[1]);
        }
    }

    #[test]
    fn the_documented_numeric_chain_in_full() {
        let chain = ["1", "1.0", "1.1", "1.1.1", "1.2", "2.0", "3.0.0"];
        for w in chain.windows(2) {
            lt(w[0], w[1]);
        }
    }

    #[test]
    fn numeric_segments_compare_as_integers() {
        lt("9", "10");
        lt("1.9", "1.10");
        eq("1.01", "1.1");
        eq("010", "10");
    }

    #[test]
    fn epoch_dominates_everything() {
        lt("999.9", "1:0.1");
        lt("1:99", "2:1.0");
        eq("0:1.0", "1.0");
        // Negative: a non-numeric prefix before a colon is not an epoch, so
        // "abc:1" is just a version whose first segment is alphabetic — and a
        // numeric first segment outranks it.
        eq("abc:1", "abc:1");
        lt("abc:1", "1.0");
    }

    #[test]
    fn pkgrel_breaks_ties_and_its_absence_matches_any() {
        lt("1.0-1", "1.0-2");
        lt("1.0-2", "1.0-10");
        eq("1.0", "1.0-5");
        eq("1.0-3", "1.0-3");
        // Negative: pkgrel only compares when pkgver already tied.
        lt("1.0-9", "1.1-1");
    }

    #[test]
    fn equality_is_reflexive() {
        eq("1.0", "1.0");
        eq("1:2.3-4", "1:2.3-4");
    }
}
