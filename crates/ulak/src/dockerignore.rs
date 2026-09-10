//! `.dockerignore` — docker's dialect, deliberately not git's.
//!
//! A build context is not a tree ulak invented; it is the set of files
//! docker itself would tar up, and `.dockerignore` is where the user has
//! already written down which of them matter. Honouring it is how a
//! workspace stops carrying hundreds of megabytes that no `COPY` ever
//! reads.
//!
//! Why a hand-written matcher when the `ignore` crate is right there:
//! the two dialects diverge, silently, exactly where it costs the most.
//!
//!   * docker anchors EVERY pattern at the context root. `*` means
//!     "every top-level entry"; in `.gitignore` a bare `*` means
//!     "everything, at every depth".
//!   * docker can re-include a file inside an excluded directory
//!     (`*` then `!sub/keep`); git famously cannot.
//!   * `*` and `?` never cross `/`, and `**` does.
//!
//! A `.dockerignore` of this shape opens with `*` and a list of
//! `!<dir>` lines. Read with git's rules, that is "exclude
//! everything" — the workspace would have arrived empty and every build
//! would have failed.
//!
//! The reference is moby's `patternmatcher`: `dockerignore.ReadAll` for
//! the file, `MatchesOrParentMatches` for the verdict, and the tar
//! walker's own rule for when an excluded directory may still be
//! descended into. One deliberate divergence, in the safe direction:
//! moby compiles patterns into a Go regexp and lets `+ ( ) |` through as
//! REGEXP metacharacters. ulak follows the semantics docker
//! documents — `filepath.Match` — and treats them as literals, which can
//! only ever match fewer paths, i.e. send more.
//!
//! **What this file may narrow, and what it may not touch.** A
//! `.dockerignore` speaks for a BUILD. A path the stack needs for any
//! other reason — a bind mount, a config, a secret, an env_file, a
//! compose file — overrides it and travels. That is not a special case
//! invented here: a local bind mount does not consult `.dockerignore`
//! either, so filtering one would make the user live something they do
//! not live locally. Measured on a large monorepo: applying the ignore
//! file blindly broke most of the stack's bind mounts to save under a
//! megabyte.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One build context, as the compose model reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContext {
    /// The context root on THIS machine.
    pub root: PathBuf,
    /// The Dockerfile this build uses, when it lives here too. Docker
    /// keeps it in the tar whatever the ignore file says, and reads
    /// `<dockerfile>.dockerignore` beside it in preference to the one at
    /// the context root.
    #[serde(default)]
    pub dockerfile: Option<PathBuf>,
}

impl BuildContext {
    /// The ignore file that governs this context, if there is one.
    /// Docker's precedence: the Dockerfile's own file wins.
    pub fn ignore_file(&self) -> Option<PathBuf> {
        if let Some(df) = &self.dockerfile {
            let beside = sibling_ignore(df);
            if beside.is_file() {
                return Some(beside);
            }
        }
        let at_root = self.root.join(".dockerignore");
        at_root.is_file().then_some(at_root)
    }
}

/// `…/Dockerfile` → `…/Dockerfile.dockerignore`
pub fn sibling_ignore(dockerfile: &Path) -> PathBuf {
    let mut s = dockerfile.as_os_str().to_os_string();
    s.push(".dockerignore");
    PathBuf::from(s)
}

/// What the build contexts have to say about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No build context governs it, or one of them wants it, or it is in
    /// the footprint for a reason a `.dockerignore` has no say over.
    Keep,
    /// Every governing context drops it. `descend` is docker's own tar
    /// rule: a dropped DIRECTORY is still walked when a `!` pattern names
    /// something inside it (or when the footprint pins a path in there),
    /// because the file underneath may yet be wanted.
    Drop { descend: bool },
}

/// The build-context filter for one footprint.
///
/// Built from PATHS, never from footprint entries: `drop_contained`
/// collapses many build contexts and bind mounts into a single
/// `Why::Build` entry at the anchor, so by the time `entries` is
/// final the reason a path travels can no longer be read off it. The two
/// path sets are harvested before that collapse and survive it.
#[derive(Debug, Default)]
pub struct BuildFilter {
    contexts: Vec<Governed>,
    /// Paths the stack needs for a reason other than a build.
    pinned: Vec<PathBuf>,
}

#[derive(Debug)]
struct Governed {
    root: PathBuf,
    patterns: Patterns,
}

impl BuildFilter {
    /// Read every context's ignore file from disk. Deliberately not
    /// cached with the footprint: editing `.dockerignore` must change the
    /// next sync, and a file read is cheaper than a staleness check that
    /// can be wrong.
    pub fn new(contexts: &[BuildContext], pinned: &[PathBuf]) -> BuildFilter {
        let mut governed: Vec<Governed> = Vec::new();
        let mut pinned: Vec<PathBuf> = pinned.to_vec();
        for ctx in contexts {
            let file = ctx.ignore_file();
            // Docker always keeps these two in the tar, no matter what
            // the patterns say — and the server's build reads the ignore
            // file itself, so it has to be up there. Without it the
            // server would build from a context narrowed one way and
            // filtered another.
            if let Some(f) = &file {
                pinned.push(f.clone());
            }
            if let Some(df) = &ctx.dockerfile {
                pinned.push(df.clone());
            }
            let Some(file) = file else { continue };
            // Several services building from one root is the normal
            // shape; reading and matching that file once per service is
            // not.
            if governed
                .iter()
                .any(|g| g.root == ctx.root && g.patterns.source == file)
            {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let patterns = Patterns::parse(&text, file);
            if patterns.is_empty() {
                continue;
            }
            governed.push(Governed {
                root: ctx.root.clone(),
                patterns,
            });
        }
        pinned.sort();
        pinned.dedup();
        BuildFilter {
            contexts: governed,
            pinned,
        }
    }

    /// The verdict on one absolute local path.
    ///
    /// Contexts can nest, and then they can disagree — one build's
    /// `.dockerignore` drops a file the other's keeps. The looser one
    /// wins: the file has to be in the tar of the build that reads it,
    /// and the cost of being wrong the other way is a broken build on the
    /// server versus a few extra bytes on the wire.
    pub fn verdict(&self, abs: &Path, is_dir: bool) -> Verdict {
        if self.contexts.is_empty() {
            return Verdict::Keep;
        }
        if self.pinned.iter().any(|p| abs.starts_with(p)) {
            return Verdict::Keep;
        }
        let mut governed = false;
        let mut reachable = false;
        for ctx in &self.contexts {
            let Ok(rel) = abs.strip_prefix(&ctx.root) else {
                continue;
            };
            let rel = rel.to_string_lossy();
            // The context root itself is never a candidate: docker tars
            // its contents, it does not ask whether the root survives.
            if rel.is_empty() {
                return Verdict::Keep;
            }
            governed = true;
            if !ctx.patterns.matches(&rel) {
                return Verdict::Keep;
            }
            if is_dir && ctx.patterns.may_reach_into(&rel) {
                reachable = true;
            }
        }
        if !governed {
            return Verdict::Keep;
        }
        // A dropped directory that still holds a path the footprint pins
        // (the repos compose mounts config out of, in the measured
        // project) has to be walked, or the mount arrives empty.
        let holds_pinned = is_dir && self.pinned.iter().any(|p| p.starts_with(abs));
        Verdict::Drop {
            descend: is_dir && (reachable || holds_pinned),
        }
    }
}

// ─── the pattern file ───────────────────────────────────────────────

/// One `.dockerignore`, parsed.
#[derive(Debug)]
pub struct Patterns {
    rules: Vec<Rule>,
    has_negation: bool,
    /// Where it was read from — the key that lets two builds sharing a
    /// context share one parse.
    source: PathBuf,
}

#[derive(Debug)]
struct Rule {
    negated: bool,
    /// The cleaned pattern text, `!` stripped. Docker's own prune rule
    /// compares this LITERALLY, so it is kept alongside the tokens.
    text: String,
    tokens: Vec<Tok>,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Lit(char),
    /// `?` — one character, never `/`
    AnyOne,
    /// `*` — any run of characters, never crossing `/`
    Star,
    /// `**` at the end of a pattern: anything at all, separators too.
    Tail,
    /// `**` anywhere else (the following `/` is eaten): zero or more
    /// whole path segments.
    Segments,
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Debug, PartialEq)]
enum ClassItem {
    One(char),
    Range(char, char),
}

impl Patterns {
    pub fn parse(text: &str, source: PathBuf) -> Patterns {
        let mut rules = Vec::new();
        let mut has_negation = false;
        for raw in text.lines() {
            // Docker tests the comment marker on the RAW line, before
            // trimming — "  # x" is a pattern, not a comment.
            let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
            if raw.starts_with('#') {
                continue;
            }
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let (negated, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            if body.is_empty() {
                // Docker rejects a bare "!" and fails the build. Nothing
                // useful to do with it here; leaving it out only ever
                // sends more.
                continue;
            }
            let text = clean(body);
            let text = if text.len() > 1 {
                text.trim_start_matches('/').to_string()
            } else {
                text
            };
            if text == "." {
                continue;
            }
            has_negation |= negated;
            rules.push(Rule {
                negated,
                tokens: tokenize(&text),
                text,
            });
        }
        Patterns {
            rules,
            has_negation,
            source,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// moby's `MatchesOrParentMatches`, rule for rule.
    ///
    /// A path is excluded when it matches, and ALSO when any of its
    /// parent directories matches — that is how a lone `*` reaches
    /// `a/b/c.txt` even though `*` itself never crosses a `/`. Patterns
    /// are read in order and the last one that has a say wins, but a
    /// pattern only gets a say when it could change the answer: a `!`
    /// line is skipped while the path is still included, a plain line
    /// while it is already excluded.
    pub fn matches(&self, rel: &str) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let parents = parent_prefixes(rel);
        let mut matched = false;
        for rule in &self.rules {
            if rule.negated != matched {
                continue;
            }
            let hit = rule.matches_path(rel) || parents.iter().any(|p| rule.matches_path(p));
            if hit {
                matched = !rule.negated;
            }
        }
        matched
    }

    /// Docker's tar walker prunes an excluded directory unless some `!`
    /// pattern's LITERAL text points inside it. This is deliberately the
    /// same lossy test docker uses (`!**/keep` does NOT rescue a file in
    /// a pruned directory) — matching docker exactly is what makes the
    /// server build from the same context the user's machine would, and
    /// being generous here would mean walking the whole monorepo to find
    /// files docker was never going to read.
    pub fn may_reach_into(&self, rel_dir: &str) -> bool {
        if !self.has_negation {
            return false;
        }
        let dir_slash = format!("{rel_dir}/");
        self.rules
            .iter()
            .filter(|r| r.negated)
            .any(|r| format!("{}/", r.text).starts_with(&dir_slash))
    }
}

impl Rule {
    fn matches_path(&self, path: &str) -> bool {
        let p: Vec<char> = path.chars().collect();
        let width = p.len() + 1;
        let mut seen = vec![false; (self.tokens.len() + 1) * width];
        walk_tokens(&self.tokens, 0, &p, 0, &mut seen, width)
    }
}

/// Anchored glob match, with a visited table so a pattern full of stars
/// cannot turn into exponential backtracking on a long path. Re-entering
/// a (token, position) pair that is already on the stack or already
/// explored can only fail — if it could succeed, the first visit would
/// have returned already.
fn walk_tokens(
    toks: &[Tok],
    ti: usize,
    p: &[char],
    pi: usize,
    seen: &mut Vec<bool>,
    width: usize,
) -> bool {
    let key = ti * width + pi;
    if seen[key] {
        return false;
    }
    seen[key] = true;
    let Some(tok) = toks.get(ti) else {
        return pi == p.len();
    };
    match tok {
        Tok::Lit(c) => p.get(pi) == Some(c) && walk_tokens(toks, ti + 1, p, pi + 1, seen, width),
        Tok::AnyOne => {
            p.get(pi).is_some_and(|c| *c != '/')
                && walk_tokens(toks, ti + 1, p, pi + 1, seen, width)
        }
        Tok::Class { negated, items } => {
            p.get(pi)
                .is_some_and(|c| *c != '/' && (items.iter().any(|i| i.contains(*c)) != *negated))
                && walk_tokens(toks, ti + 1, p, pi + 1, seen, width)
        }
        Tok::Star => {
            let mut j = pi;
            loop {
                if walk_tokens(toks, ti + 1, p, j, seen, width) {
                    return true;
                }
                match p.get(j) {
                    Some('/') | None => return false,
                    Some(_) => j += 1,
                }
            }
        }
        Tok::Tail => {
            let mut j = pi;
            loop {
                if walk_tokens(toks, ti + 1, p, j, seen, width) {
                    return true;
                }
                if j >= p.len() {
                    return false;
                }
                j += 1;
            }
        }
        Tok::Segments => {
            if walk_tokens(toks, ti + 1, p, pi, seen, width) {
                return true;
            }
            for (j, c) in p.iter().enumerate().skip(pi) {
                if *c == '/' && walk_tokens(toks, ti + 1, p, j + 1, seen, width) {
                    return true;
                }
            }
            false
        }
    }
}

impl ClassItem {
    fn contains(&self, c: char) -> bool {
        match self {
            ClassItem::One(x) => *x == c,
            ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        }
    }
}

fn tokenize(pattern: &str) -> Vec<Tok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                // "Treat **/ as **" — docker eats the separator.
                if chars.get(i) == Some(&'/') {
                    i += 1;
                }
                toks.push(if i >= chars.len() {
                    Tok::Tail
                } else {
                    Tok::Segments
                });
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '?' => {
                toks.push(Tok::AnyOne);
                i += 1;
            }
            '\\' if i + 1 < chars.len() => {
                toks.push(Tok::Lit(chars[i + 1]));
                i += 2;
            }
            '[' => match parse_class(&chars, &mut i) {
                Some(tok) => toks.push(tok),
                // Unterminated: docker's own syntax check rejects the
                // whole file and the build stops. A literal `[` is the
                // reading that sends more, so it is the one taken.
                None => {
                    toks.push(Tok::Lit('['));
                    i += 1;
                }
            },
            c => {
                toks.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    toks
}

fn parse_class(chars: &[char], i: &mut usize) -> Option<Tok> {
    let mut j = *i + 1;
    let negated = chars.get(j) == Some(&'^');
    if negated {
        j += 1;
    }
    let mut items = Vec::new();
    while let Some(&c) = chars.get(j) {
        if c == ']' {
            *i = j + 1;
            return Some(Tok::Class { negated, items });
        }
        let lo = if c == '\\' && j + 1 < chars.len() {
            j += 1;
            chars[j]
        } else {
            c
        };
        if chars.get(j + 1) == Some(&'-') && chars.get(j + 2).is_some_and(|c| *c != ']') {
            let mut k = j + 2;
            let hi = if chars[k] == '\\' && k + 1 < chars.len() {
                k += 1;
                chars[k]
            } else {
                chars[k]
            };
            items.push(ClassItem::Range(lo, hi));
            j = k + 1;
        } else {
            items.push(ClassItem::One(lo));
            j += 1;
        }
    }
    None
}

/// Go's `filepath.Clean`, on the slash-separated paths a `.dockerignore`
/// is made of. Docker runs every pattern through it, which is why
/// `./build/`, `build` and `build//` are one and the same rule.
fn clean(p: &str) -> String {
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&"..") | None if !rooted => out.push(".."),
                Some(&"..") | None => {}
                Some(_) => {
                    out.pop();
                }
            },
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// "a/b/c" → ["a", "a/b"]. The path itself is not in here; the caller
/// tests that separately, exactly as moby does.
fn parent_prefixes(rel: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut cut = 0;
    while let Some(next) = rel[cut..].find('/') {
        cut += next;
        if cut > 0 {
            out.push(&rel[..cut]);
        }
        cut += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(lines: &str) -> Patterns {
        Patterns::parse(lines, PathBuf::from("/x/.dockerignore"))
    }

    /// The shape a large monorepo uses, and the reason the gitignore
    /// matcher could not be reused: `*` here means "every top-level
    /// entry", and a `!` line takes one of them back. Reading this file
    /// with git's rules empties the workspace.
    #[test]
    fn star_then_negation_is_a_whitelist_not_an_empty_tree() {
        let p = pats("*\n!stack\n!vendor-mock-stubs\n");
        assert!(p.matches("junk.zip"));
        assert!(p.matches("app-core-be-vendor"));
        // A path deep inside is dropped by its PARENT matching `*`,
        // never by `*` itself — `*` does not cross a separator.
        assert!(p.matches("app-core-be-vendor/src/App.java"));
        assert!(!p.matches("stack"));
        assert!(!p.matches("stack/compose.yaml"));
        assert!(!p.matches("vendor-mock-stubs/stub.json"));
    }

    #[test]
    fn a_negation_reaches_inside_an_excluded_directory() {
        // git cannot do this; docker can, and files depend on it.
        let p = pats("*\n!sub/keep.txt\n");
        assert!(p.matches("sub"));
        assert!(!p.matches("sub/keep.txt"));
        assert!(p.matches("sub/other.txt"));
        // …so the walker may not prune `sub`, and docker's own test for
        // that is the literal prefix of the negation.
        assert!(p.may_reach_into("sub"));
        assert!(!p.may_reach_into("other"));
    }

    /// Docker's prune rule is lossy and ulak copies it on purpose: a
    /// `**` negation does NOT rescue a file inside a pruned directory.
    /// Diverging would mean walking a whole monorepo to collect files
    /// docker was never going to put in the tar.
    #[test]
    fn a_double_star_negation_does_not_unprune_a_directory() {
        let p = pats("*\n!**/keep.txt\n");
        assert!(!p.may_reach_into("sub"));
    }

    #[test]
    fn stars_respect_separators_and_double_stars_do_not() {
        let p = pats("a/*.log\n");
        assert!(p.matches("a/x.log"));
        assert!(!p.matches("a/b/x.log"), "* must not cross a slash");

        let p = pats("**/target\n");
        assert!(p.matches("target"));
        assert!(p.matches("a/target"));
        assert!(p.matches("a/b/target"));
        assert!(!p.matches("a/targets"));

        let p = pats("logs/**\n");
        assert!(p.matches("logs/a"));
        assert!(p.matches("logs/a/b"));
        assert!(!p.matches("logs"), "`logs/**` is the contents, not the dir");

        let p = pats("a/**/b\n");
        assert!(p.matches("a/b"), "** may stand for no segment at all");
        assert!(p.matches("a/x/b"));
        assert!(p.matches("a/x/y/b"));
    }

    #[test]
    fn question_marks_and_character_classes() {
        let p = pats("tmp?\n");
        assert!(p.matches("tmp1"));
        assert!(!p.matches("tmp"));
        assert!(!p.matches("tmp/x"), "? must not match a separator");

        let p = pats("*.[oa]\n");
        assert!(p.matches("lib.o"));
        assert!(p.matches("lib.a"));
        assert!(!p.matches("lib.c"));

        let p = pats("v[0-9]\n");
        assert!(p.matches("v7"));
        assert!(!p.matches("va"));

        let p = pats("[^x]y\n");
        assert!(p.matches("ay"));
        assert!(!p.matches("xy"));
    }

    #[test]
    fn a_dot_is_a_dot_and_a_backslash_escapes() {
        // moby escapes `.` into the regexp, so it never means "any char".
        let p = pats("a.txt\n");
        assert!(p.matches("a.txt"));
        assert!(!p.matches("axtxt"));

        let p = pats("we\\*ird\n");
        assert!(p.matches("we*ird"));
        assert!(!p.matches("weXird"));
    }

    /// Deliberate divergence from moby, documented at the top of this
    /// file: moby leaks regexp metacharacters, ulak reads the
    /// semantics docker documents. Erring this way matches FEWER paths,
    /// which means sending more — the chosen direction.
    #[test]
    fn regexp_metacharacters_are_literals_not_quantifiers() {
        let p = pats("a+b\n");
        assert!(p.matches("a+b"));
        assert!(!p.matches("aab"));
    }

    #[test]
    fn comments_blank_lines_and_path_spelling() {
        let p = pats("# a comment\n\n  \n/build/\n./dist\nsrc//gen\n");
        assert!(p.matches("build"));
        assert!(p.matches("build/out.js"));
        assert!(p.matches("dist"));
        assert!(p.matches("src/gen"));
        // A `#` only starts a comment in the FIRST column, as docker
        // reads it before trimming.
        let p = pats("  #notacomment\n");
        assert!(p.matches("#notacomment"));
    }

    #[test]
    fn the_last_pattern_with_a_say_wins() {
        // Excluded, taken back, excluded again.
        let p = pats("docs\n!docs/api\ndocs/api/internal\n");
        assert!(p.matches("docs/readme.md"));
        assert!(!p.matches("docs/api/v1.md"));
        assert!(p.matches("docs/api/internal/secret.md"));
    }

    #[test]
    fn cleaning_matches_go() {
        assert_eq!(clean("./a/b/"), "a/b");
        assert_eq!(clean("a//b"), "a/b");
        assert_eq!(clean("a/../b"), "b");
        assert_eq!(clean("/a"), "/a");
        assert_eq!(clean(""), ".");
        assert_eq!(clean("."), ".");
        assert_eq!(parent_prefixes("a/b/c"), vec!["a", "a/b"]);
        assert!(parent_prefixes("a").is_empty());
    }

    #[test]
    fn a_pattern_full_of_stars_still_answers_quickly() {
        // A visited table is what keeps this from backtracking
        // exponentially — without it this assertion never returns.
        let p = pats("*a*a*a*a*a*a*a*a*a*a*b\n");
        assert!(!p.matches(&"a".repeat(64)));
    }

    // ─── the filter as a whole ──────────────────────────────────────

    fn ctx(root: &str) -> BuildContext {
        BuildContext {
            root: PathBuf::from(root),
            dockerfile: None,
        }
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn a_path_under_no_build_context_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "ctx/.dockerignore", "*\n");
        let f = BuildFilter::new(&[ctx(&root.join("ctx").to_string_lossy())], &[]);
        assert_eq!(f.verdict(&root.join("elsewhere/x"), false), Verdict::Keep);
        assert!(matches!(
            f.verdict(&root.join("ctx/x"), false),
            Verdict::Drop { .. }
        ));
    }

    /// The broken mounts, in one assertion. Some repos are excluded by
    /// the ignore file and no Dockerfile ever COPYs them — but compose
    /// mounts config files out of each, and a local bind mount does
    /// not consult `.dockerignore`. Pinned paths override, and the
    /// directory above them stays walkable.
    #[test]
    fn a_mounted_path_overrides_the_ignore_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".dockerignore", "*\n!stack\n");
        write(root, "app-core-be/listeners.yml", "x");
        write(root, "app-core-be/src/App.java", "x");
        let mount = root.join("app-core-be/listeners.yml");
        let f = BuildFilter::new(
            &[ctx(&root.to_string_lossy())],
            std::slice::from_ref(&mount),
        );

        assert_eq!(f.verdict(&mount, false), Verdict::Keep);
        assert!(
            matches!(
                f.verdict(&root.join("app-core-be/src/App.java"), false),
                Verdict::Drop { .. }
            ),
            "the rest of that repo has no reason to travel"
        );
        // The directory is dropped but must still be entered, or the
        // mount arrives empty.
        assert_eq!(
            f.verdict(&root.join("app-core-be"), true),
            Verdict::Drop { descend: true }
        );
        assert_eq!(
            f.verdict(&root.join("build-junk"), true),
            Verdict::Drop { descend: false }
        );
    }

    #[test]
    fn the_ignore_file_and_the_dockerfile_always_travel() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // `*` would swallow both, and then the server would build from a
        // context nobody narrowed.
        write(root, ".dockerignore", "*\n");
        write(root, "Dockerfile", "FROM scratch\n");
        let f = BuildFilter::new(
            &[BuildContext {
                root: root.to_path_buf(),
                dockerfile: Some(root.join("Dockerfile")),
            }],
            &[],
        );
        assert_eq!(f.verdict(&root.join(".dockerignore"), false), Verdict::Keep);
        assert_eq!(f.verdict(&root.join("Dockerfile"), false), Verdict::Keep);
    }

    #[test]
    fn a_dockerfile_ignore_file_wins_over_the_context_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".dockerignore", "*\n");
        write(root, "Dockerfile.web", "FROM scratch\n");
        write(root, "Dockerfile.web.dockerignore", "only-this\n");
        let c = BuildContext {
            root: root.to_path_buf(),
            dockerfile: Some(root.join("Dockerfile.web")),
        };
        assert_eq!(
            c.ignore_file(),
            Some(root.join("Dockerfile.web.dockerignore"))
        );
        let f = BuildFilter::new(&[c], &[]);
        assert!(matches!(
            f.verdict(&root.join("only-this"), false),
            Verdict::Drop { .. }
        ));
        assert_eq!(f.verdict(&root.join("anything-else"), false), Verdict::Keep);
    }

    #[test]
    fn when_nested_contexts_disagree_the_looser_one_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".dockerignore", "svc\n");
        write(root, "svc/.dockerignore", "junk\n");
        let f = BuildFilter::new(
            &[
                ctx(&root.to_string_lossy()),
                ctx(&root.join("svc").to_string_lossy()),
            ],
            &[],
        );
        // The outer build drops all of `svc`; the inner one reads
        // `svc/app.js` and would break without it.
        assert_eq!(f.verdict(&root.join("svc/app.js"), false), Verdict::Keep);
        // Both agree on this one.
        assert!(matches!(
            f.verdict(&root.join("svc/junk"), false),
            Verdict::Drop { .. }
        ));
    }

    #[test]
    fn a_context_without_an_ignore_file_narrows_nothing() {
        // Docker sends the whole context when there is no ignore file,
        // and so does ulak. A build context at the repo root with no
        // `.dockerignore` is not a bug to fix — it is what the user asked
        // docker for.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let f = BuildFilter::new(&[ctx(&root.to_string_lossy())], &[]);
        assert_eq!(f.verdict(&root.join("anything"), false), Verdict::Keep);
        assert_eq!(f.verdict(&root.join("deep/thing"), true), Verdict::Keep);
    }
}
