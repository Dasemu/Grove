//! Branch name to directory name mapping (section 5 of the plan).

/// Map a branch name to a visible, filesystem-safe directory name that is not in `taken`.
/// `/` becomes `-`; collisions get a short deterministic suffix derived from the branch.
pub fn dir_name(branch: &str, taken: &[&str]) -> String {
    let mut base: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    base = base.trim_start_matches('.').to_string();
    if base.is_empty() {
        base = "branch".to_string();
    }
    if !taken.contains(&base.as_str()) {
        return base;
    }
    let suffixed = format!("{base}-{:06x}", fnv1a(branch) & 0xff_ffff);
    if !taken.contains(&suffixed.as_str()) {
        return suffixed;
    }
    (2..)
        .map(|n| format!("{suffixed}-{n}"))
        .find(|c| !taken.contains(&c.as_str()))
        .expect("unbounded range")
}

fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_slashes() {
        assert_eq!(dir_name("feat/login", &[]), "feat-login");
        assert_eq!(dir_name("main", &[]), "main");
    }

    #[test]
    fn collisions_get_a_deterministic_suffix() {
        let a = dir_name("feat/x", &["feat-x"]);
        assert_ne!(a, "feat-x");
        assert!(a.starts_with("feat-x-"));
        assert_eq!(a, dir_name("feat/x", &["feat-x"]));
        // Two branches mapping to the same base end up different.
        let first = dir_name("feat/x", &[]);
        let second = dir_name("feat-x", &[first.as_str()]);
        assert_ne!(first, second);
    }

    #[test]
    fn reserved_names_are_avoided() {
        assert_ne!(dir_name(".bare", &[".bare"]), ".bare");
        assert!(!dir_name("..", &[]).starts_with('.'));
        assert!(!dir_name("", &[]).is_empty());
    }

    /// Fuzz-style invariants with a tiny LCG (no extra dependency).
    #[test]
    fn invariants_hold_for_random_names() {
        let alphabet: Vec<char> = "ab/._- é\\:*?\"<>|0Z".chars().collect();
        let mut seed = 0x1234_5678_u64;
        let mut taken: Vec<String> = vec![".bare".into(), ".gitsync-stage".into()];
        for _ in 0..2000 {
            let len = {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 60) as usize
            };
            let name: String = (0..len)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    alphabet[(seed >> 33) as usize % alphabet.len()]
                })
                .collect();
            let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
            let d = dir_name(&name, &refs);
            assert!(!d.is_empty());
            assert!(!d.contains('/'));
            assert!(!d.starts_with('.'));
            assert!(!taken.contains(&d), "{name:?} -> {d:?} already taken");
            taken.push(d);
        }
    }
}
