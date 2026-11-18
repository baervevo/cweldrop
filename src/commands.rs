//! The command registry and fuzzy matching behind the `:` hint panel.

/// One `:command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Argument synopsis; `<x>` is required, `[x]` optional.
    pub args: &'static str,
    pub desc: &'static str,
}

impl CommandSpec {
    /// Whether running it needs an argument the user hasn't given yet.
    pub fn needs_arg(&self) -> bool {
        self.args.starts_with('<')
    }

    pub fn matches_word(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "connect",
        aliases: &["c"],
        args: "<ip:port>",
        desc: "Dial a peer by address",
    },
    CommandSpec {
        name: "disconnect",
        aliases: &["d"],
        args: "",
        desc: "Disconnect from the selected peer",
    },
    CommandSpec {
        name: "send",
        aliases: &["f"],
        args: "<path>",
        desc: "Offer a file to the selected peer",
    },
    CommandSpec {
        name: "accept",
        aliases: &["a"],
        args: "[n|name|all]",
        desc: "Accept an incoming file offer",
    },
    CommandSpec {
        name: "reject",
        aliases: &[],
        args: "[n|name|all]",
        desc: "Decline an incoming file offer",
    },
    CommandSpec {
        name: "files",
        aliases: &[],
        args: "",
        desc: "Pick from the files in this chat",
    },
    CommandSpec {
        name: "view",
        aliases: &["v"],
        args: "[n|name]",
        desc: "Open an image or text file from this chat",
    },
    CommandSpec {
        name: "open",
        aliases: &["o"],
        args: "[n]",
        desc: "Open the newest (or n-th newest) link in the browser",
    },
    CommandSpec {
        name: "verify",
        aliases: &[],
        args: "[confirm|forget]",
        desc: "Show the safety code / mark this contact verified",
    },
    CommandSpec {
        name: "settings",
        aliases: &["set"],
        args: "",
        desc: "File policy, size cap, download folder",
    },
    CommandSpec {
        name: "nick",
        aliases: &[],
        args: "<name>",
        desc: "Change your display name",
    },
    CommandSpec {
        name: "help",
        aliases: &["h"],
        args: "",
        desc: "List all commands",
    },
    CommandSpec {
        name: "quit",
        aliases: &["q"],
        args: "",
        desc: "Exit cweldrop",
    },
];

/// The command whose name or alias is exactly `word`.
pub fn lookup(word: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.matches_word(word))
}

/// Fuzzy subsequence match of `query` in `text` (case-insensitive). Returns
/// a score (higher is better) and the char indices matched, or `None`.
/// Bonuses for a match at the start, at word starts, and for runs.
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let mut idx = Vec::with_capacity(q.len());
    let mut score = 0;
    let mut ti = 0;
    for (qi, &qc) in q.iter().enumerate() {
        let next = (ti..t.len()).find(|&i| t[i] == qc)?;
        let consecutive = idx.last().is_some_and(|&l| l + 1 == next);
        // Keep a run going; otherwise prefer this char at a word start.
        let at = if consecutive {
            next
        } else {
            (next..t.len())
                .find(|&i| {
                    t[i] == qc
                        && (i == 0 || !t[i - 1].is_alphanumeric())
                        && is_subsequence(&q[qi + 1..], &t[i + 1..])
                })
                .unwrap_or(next)
        };
        score += 1;
        if at == 0 {
            score += 8;
        } else if !t[at - 1].is_alphanumeric() {
            score += 5;
        }
        if idx.last().is_some_and(|&l| l + 1 == at) {
            score += 4;
        }
        score -= (at - ti).min(5) as i32 / 2;
        idx.push(at);
        ti = at + 1;
    }
    Some((score, idx))
}

fn is_subsequence(needle: &[char], hay: &[char]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|c| it.any(|h| h == c))
}

/// A suggestion: the command, its score, and which chars of `name` matched
/// (empty when it matched through an alias or the description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub spec: &'static CommandSpec,
    pub score: i32,
    pub name_hits: Vec<usize>,
}

/// Commands matching `query`, best first. Empty query lists everything in
/// registry order.
pub fn suggest(query: &str) -> Vec<Suggestion> {
    let query = query.trim();
    let mut out: Vec<Suggestion> = COMMANDS
        .iter()
        .filter_map(|spec| {
            let by_name = fuzzy(query, spec.name).map(|(s, h)| (s + 20, h));
            let by_alias = spec
                .aliases
                .iter()
                .filter(|a| a.eq_ignore_ascii_case(query))
                .map(|_| (40, Vec::new()))
                .next();
            let by_desc = fuzzy(query, spec.desc).map(|(s, _)| (s, Vec::new()));
            [by_alias, by_name, by_desc]
                .into_iter()
                .flatten()
                .max_by_key(|(s, _)| *s)
                .map(|(score, name_hits)| Suggestion {
                    spec,
                    score,
                    name_hits,
                })
        })
        .collect();
    if !query.is_empty() {
        // Stable: equal scores keep registry order.
        out.sort_by_key(|s| std::cmp::Reverse(s.score));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(q: &str) -> Vec<&'static str> {
        suggest(q).into_iter().map(|s| s.spec.name).collect()
    }

    #[test]
    fn empty_query_lists_everything() {
        assert_eq!(names("").len(), COMMANDS.len());
    }

    #[test]
    fn prefix_and_fuzzy_name_matches_rank_first() {
        assert_eq!(names("se")[0], "send");
        assert_eq!(names("stg")[0], "settings");
        assert_eq!(names("dis")[0], "disconnect");
        assert_eq!(names("vrf")[0], "verify");
        assert_eq!(names("acc")[0], "accept");
    }

    #[test]
    fn aliases_and_descriptions_match() {
        assert_eq!(names("q")[0], "quit");
        assert_eq!(names("v")[0], "view");
        assert!(names("browser").contains(&"open"));
        assert!(names("image").contains(&"view"));
        assert!(names("zzzz").is_empty());
    }

    #[test]
    fn fuzzy_scoring() {
        let (_, hits) = fuzzy("stg", "settings").unwrap();
        assert_eq!(hits, vec![0, 2, 6]);
        assert!(fuzzy("xyz", "settings").is_none());
        let (prefix, _) = fuzzy("se", "send").unwrap();
        let (middle, _) = fuzzy("se", "close").unwrap();
        assert!(prefix > middle);
        assert_eq!(fuzzy("", "x"), Some((0, vec![])));
        // Jumping to a word start must not lose a match that exists earlier.
        assert_eq!(fuzzy("ac", "xac ab").unwrap().1, vec![1, 2]);
        assert_eq!(fuzzy("ab", "xa ab").unwrap().1, vec![3, 4]);
    }

    #[test]
    fn lookup_and_args() {
        assert_eq!(lookup("c").unwrap().name, "connect");
        assert!(lookup("connect").unwrap().needs_arg());
        assert!(!lookup("files").unwrap().needs_arg());
        assert!(!lookup("view").unwrap().needs_arg());
        assert!(lookup("bogus").is_none());
    }

    /// Every registry entry is handled by `App::run_command`.
    #[test]
    fn registry_matches_dispatcher() {
        let mut app = crate::app::App::new("me".into(), 7421);
        for c in COMMANDS {
            if c.name == "quit" {
                continue;
            }
            app.status_msg.clear();
            app.run_command(c.name);
            assert!(
                !app.status_msg.starts_with("unknown command"),
                "{} not handled",
                c.name
            );
        }
    }
}
