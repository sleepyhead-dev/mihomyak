//! Node name patterns for filters and custom groups.
//!
//! Shell-style globs, case-insensitive: `*` any run of characters, `?` one
//! character. Globs cover real-world needs (`*🇳🇱*`, `*Germany*`, `NL-?`) without
//! pulling a regex engine into the binary. For proxies mihomo loads itself
//! (share-link providers) the same globs are translated to Go regexps.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    raw: String,
    chars: Vec<char>,
}

impl Pattern {
    pub fn new(raw: &str) -> Self {
        Self {
            raw: raw.to_owned(),
            chars: fold(raw).chars().collect(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn matches(&self, name: &str) -> bool {
        let text: Vec<char> = fold(name).chars().collect();
        glob_match(&self.chars, &text)
    }

    /// Equivalent RE2/Go regexp body (without anchors or flags).
    fn regex_body(&self) -> String {
        let mut out = String::new();
        for ch in self.raw.chars() {
            match ch {
                '*' => out.push_str(".*"),
                '?' => out.push('.'),
                c if "\\.+()[]{}|^$".contains(c) => {
                    out.push('\\');
                    out.push(c);
                }
                c => out.push(c),
            }
        }
        out
    }
}

/// Case folding good enough for node names (Cyrillic included).
fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// Iterative glob matcher with single-star backtracking (linear in practice).
fn glob_match(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some('?') => {
                p += 1;
                t += 1;
            }
            Some(c) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// A set of patterns; empty means "no constraint".
#[derive(Debug, Clone, Default)]
pub struct PatternSet(Vec<Pattern>);

impl PatternSet {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        Self(patterns.iter().map(|p| Pattern::new(p.as_ref())).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn patterns(&self) -> &[Pattern] {
        &self.0
    }

    pub fn any_match(&self, name: &str) -> bool {
        self.0.iter().any(|p| p.matches(name))
    }

    /// `(?i)^(?:a|b)$` for mihomo's `filter` / `exclude-filter`.
    pub fn to_regex(&self) -> String {
        let bodies: Vec<String> = self.0.iter().map(Pattern::regex_body).collect();
        format!("(?i)^(?:{})$", bodies.join("|"))
    }
}

/// Whether a node survives `[filter]`: in the whitelist (if any) and not blacklisted.
pub fn keep(name: &str, include: &PatternSet, exclude: &PatternSet) -> bool {
    (include.is_empty() || include.any_match(name)) && !exclude.any_match(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let p = Pattern::new("*🇳🇱*");
        assert!(p.matches("🇳🇱 Netherlands 1"));
        assert!(!p.matches("🇩🇪 Germany"));
        assert!(Pattern::new("nl-?").matches("NL-1"));
        assert!(!Pattern::new("nl-?").matches("NL-10"));
        assert!(Pattern::new("*германия*").matches("🇩🇪 Германия (Франкфурт)"));
        assert!(Pattern::new("a*b*c").matches("aXXbYYc"));
        assert!(!Pattern::new("a*b*c").matches("aXXbYY"));
        assert!(Pattern::new("*").matches(""));
        assert!(Pattern::new("exact").matches("EXACT"));
        assert!(!Pattern::new("exact").matches("exactly"));
    }

    #[test]
    fn filters() {
        let include = PatternSet::new(&["*NL*", "*DE*"]);
        let exclude = PatternSet::new(&["*test*"]);
        assert!(keep("🇳🇱 NL 1", &include, &exclude));
        assert!(!keep("🇳🇱 NL test", &include, &exclude));
        assert!(!keep("🇫🇮 FI", &include, &exclude));
        assert!(keep(
            "anything",
            &PatternSet::default(),
            &PatternSet::default()
        ));
    }

    #[test]
    fn regex_translation() {
        let set = PatternSet::new(&["*NL (1)*", "de-?"]);
        assert_eq!(set.to_regex(), r"(?i)^(?:.*NL \(1\).*|de-.)$");
    }
}
