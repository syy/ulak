//! commands.jsonl — the audit trail. "What did ulak actually run?"
//! must never be unanswerable: every ssh/rsync argv is appended, with
//! its exit code, to a per-workspace JSONL file under the user's state
//! dir (NOT the project — audit lines must never sync to the server).
//!
//! Secrets hygiene: values of KEY=VALUE assignments inside recorded
//! command strings are redacted — argv shape is auditable, env values
//! are not part of the record. This is the LAST gate, and deliberately
//! the widest: `passthrough::redact_argv` knows which flags a given
//! command treats as secret, but only commands that route through the
//! catalog reach it, and everything ever written reaches this.

use std::io::Write;
use std::sync::OnceLock;

static CONTEXT: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// Bind the audit log to a workspace (by path-hash). Called once after
/// project discovery; recording is a no-op before that.
pub fn set_context(path_hash: &str) {
    let _ = CONTEXT.set(trail(path_hash));
}

/// Same state root as the ledger, the lock and the footprint cache — it
/// used to hard-code ~/.local/state and ignore XDG_STATE_HOME.
fn trail(path_hash: &str) -> Option<std::path::PathBuf> {
    crate::invocation::state_dir().map(|state| state.join(path_hash).join("commands.jsonl"))
}

/// Append one entry. Failures are swallowed — the audit trail must
/// never break the operation it audits.
pub fn record(kind: &str, argv: &[String], exit: Option<i32>) {
    let Some(Some(path)) = CONTEXT.get() else {
        return;
    };
    append(path, kind, argv, exit);
}

/// Record into a NAMED trail without touching the process-wide context.
///
/// The agent installs itself during an ordinary command — the first one
/// typed in a terminal — and `CONTEXT` is a `OnceLock` bound to that
/// command's workspace moments later. Setting it early would silently
/// misfile the whole command's ssh and rsync argv under the service, and
/// not setting it at all would leave "ulak wrote a login item"
/// unrecorded. Neither is acceptable, so this writes straight to the
/// trail it names.
pub fn record_command_in(
    trail_name: &str,
    kind: &str,
    cmd: &std::process::Command,
    exit: Option<i32>,
) {
    let Some(path) = trail(trail_name) else {
        return;
    };
    append(&path, kind, &argv_of(cmd), exit);
}

fn append(path: &std::path::Path, kind: &str, argv: &[String], exit: Option<i32>) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = serde_json::json!({
        "ts_unix": ts,
        "kind": kind,
        "argv": argv.iter().map(|a| redact(a)).collect::<Vec<_>>(),
        "exit": exit,
    });
    // The trail names hosts and paths — keep it private.
    let dir = path.parent().unwrap();
    let _ = crate::invocation::private_dir(dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{entry}");
    }
}

/// Record a std::process::Command (program + args) after it ran.
pub fn record_command(kind: &str, cmd: &std::process::Command, exit: Option<i32>) {
    record(kind, &argv_of(cmd), exit);
}

fn argv_of(cmd: &std::process::Command) -> Vec<String> {
    let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
    argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    argv
}

/// Flags whose value is a user-supplied `KEY=VALUE` pair.
///
/// The judgement this list encodes: the key stays, the value goes.
/// `--env` is not inherently secret and blanking every env var would
/// gut the trail for the question it is read for — "what was that
/// container actually handed?" — while keeping the value is how
/// `--env=DB_PASSWORD=hunter2` ended up in a plaintext file that
/// outlives the container by years. `DB_PASSWORD=***` answers the first
/// question and not the second, so that is the line.
///
/// `--label` and `--annotation` are here for consistency rather than
/// suspicion. An env-shaped label already lost its value to the rule
/// below; a dotted one (`--label com.acme.tier=prod`) kept it purely
/// because of the key's charset, and a redactor whose reach depends on
/// whether the user's key happens to contain a dot is not a policy.
const KV_FLAGS: &[&str] = &[
    "-e",
    "--env",
    "-l",
    "--label",
    "--build-arg",
    "--annotation",
];

/// Redact VALUE in every `KEY=VALUE` word (env assignments may carry
/// profile lists today and someone's secret tomorrow). Leading shell
/// quotes are stripped for the key check — sh_quote glues a `'` onto
/// quoted assignments and must not defeat redaction.
///
/// pflag gives the same env var five spellings and docker honours every
/// one of them; only the ones with a space in them used to be redacted
/// here, so `--env=DB_PASSWORD=hunter2` was written out verbatim.
fn redact(arg: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut pair_follows = false;
    for word in arg.split(' ') {
        let vouched = std::mem::take(&mut pair_follows);
        let stripped = word.trim_start_matches(['\'', '"']);
        let prefix = &word[..word.len() - stripped.len()];
        if KV_FLAGS.contains(&stripped) {
            pair_follows = true;
            out.push(word.to_string());
            continue;
        }
        let body = match stripped.split_once('=') {
            // `--env=KEY=VALUE`: the flag vouches for the rest of its own
            // word, so the key is whatever docker accepts there.
            Some((flag, pair)) if KV_FLAGS.contains(&flag) => {
                format!("{flag}={}", hide_value(pair, true))
            }
            // A short flag swallows the rest of its own word, and a
            // CLUSTER of them does too: `-eK=V` and `-iteK=V` both reach
            // the daemon with K set (measured, docker 29.4).
            // `attached_short` recognises the first shape and cannot see
            // the second without a real pflag parser, so neither is
            // parsed here — everything up to the first `=` is kept, which
            // leaves the flag AND the key readable. Being broad costs
            // nothing legible: `buildx build -o type=local` already
            // reaches the trail as `type=***`, so all that changes is
            // that writing a flag attached no longer keeps the value
            // writing it spaced has always dropped.
            _ if stripped.starts_with('-') && !stripped.starts_with("--") => {
                hide_value(stripped, true)
            }
            // A pair the previous word introduced (`--label com.acme.k=v`).
            // Guarded on not looking like a flag, so a boolean short
            // option that happens to precede one cannot drag it in.
            _ => hide_value(stripped, vouched && !stripped.starts_with('-')),
        };
        out.push(format!("{prefix}{body}"));
    }
    out.join(" ")
}

/// Drop the VALUE of a `KEY=VALUE` word, keeping the key.
///
/// `vouched` means a flag has already said this word is a pair, so the
/// key may be dotted, hyphenated, anything. Without that word of honour
/// the key has to LOOK like an environment variable, and that is what
/// keeps the rest of the trail legible: `--filter=dangling` is the
/// question being asked, not a payload, and a reader needs to see it.
fn hide_value(word: &str, vouched: bool) -> String {
    match word.split_once('=') {
        Some((key, _))
            if !key.is_empty()
                && (vouched || key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')) =>
        {
            format!("{key}=***")
        }
        _ => word.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact text one `docker …` argv leaves on disk. A value only
    /// counts as safe if it survives NEITHER gate: `Remote::spell`
    /// redacts and sh-quotes on the way into the remote command string,
    /// and `append` redacts that string again on the way to the file.
    fn trail_line(args: &[&str], secret_flags: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let spelled = crate::passthrough::redact_argv(&args, secret_flags)
            .iter()
            .map(|a| crate::ssh::sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        redact(&spelled)
    }

    /// Every spelling that leaks `s3cret`, named. Collected rather than
    /// asserted one at a time: the interesting failure is WHICH of pflag's
    /// spellings the redactor forgot, and stopping at the first one hides
    /// the rest.
    fn leaks(cases: &[&[&str]], secret_flags: &[&str]) -> Vec<String> {
        cases
            .iter()
            .map(|args| (args, trail_line(args, secret_flags)))
            .filter(|(_, line)| line.contains("s3cret"))
            .map(|(args, line)| format!("{args:?} → {line}"))
            .collect()
    }

    /// pflag accepts four spellings of the same env var and docker 29.4
    /// honours all of them (measured — `-ieFOO=bar` sets FOO too, because
    /// a short flag swallows the rest of its word even inside a cluster).
    /// The trail used to redact only the ones with a space in them, so a
    /// user who typed `--env=` or `-e` attached got their secret written
    /// to a plaintext file for the life of the disk.
    #[test]
    fn an_env_value_never_reaches_the_trail_in_any_spelling() {
        let cases: &[&[&str]] = &[
            &["run", "-e", "DB_PASSWORD=s3cret", "web"],
            &["run", "--env", "DB_PASSWORD=s3cret", "web"],
            &["run", "--env=DB_PASSWORD=s3cret", "web"],
            &["run", "-eDB_PASSWORD=s3cret", "web"],
            &["run", "-ieDB_PASSWORD=s3cret", "web"],
            &["run", "-ite", "DB_PASSWORD=s3cret", "web"],
        ];
        assert_eq!(leaks(cases, &[]), Vec::<String>::new());
        // The key is the half worth keeping — a trail of `-e ***` answers
        // nothing about which variable the container was handed.
        for args in cases {
            let line = trail_line(args, &[]);
            assert!(
                line.contains("DB_PASSWORD"),
                "{args:?} lost the key: {line}"
            );
        }
    }

    /// The same payload shape under the other flags that carry one. An
    /// env-shaped key already lost its value here; a dotted one kept it,
    /// purely because of the key's charset.
    #[test]
    fn the_other_key_value_flags_lose_their_value_too() {
        let cases: &[&[&str]] = &[
            &["build", "--build-arg=NPM_TOKEN=s3cret", "."],
            &["build", "--build-arg", "NPM_TOKEN=s3cret", "."],
            &["build", "--label=com.acme.tier=s3cret", "."],
            &["build", "--label", "com.acme.tier=s3cret", "."],
            &[
                "build",
                "--annotation=org.opencontainers.image.url=s3cret",
                ".",
            ],
            &["run", "-lcom.acme.tier=s3cret", "web"],
        ];
        assert_eq!(leaks(cases, &[]), Vec::<String>::new());
    }

    /// The catalog's own secret flags, for the same three spellings —
    /// `redact_argv` is supposed to cover all of them, and this is the
    /// test that says so out loud.
    #[test]
    fn a_catalog_secret_flag_is_covered_in_every_spelling() {
        let joined: &[&[&str]] = &[
            &["swarm", "join", "--token=SWMTKN-s3cret", "h:2377"],
            &["swarm", "join", "--token", "SWMTKN-s3cret", "h:2377"],
        ];
        assert_eq!(leaks(joined, &["--token"]), Vec::<String>::new());
        // `login -p` is refused outright rather than redacted, but the
        // redactor still has to hold if that refusal ever moves.
        let password: &[&[&str]] = &[
            &["login", "-p", "s3cret", "reg.io"],
            &["login", "-ps3cret", "reg.io"],
            &["login", "--password=s3cret", "reg.io"],
        ];
        assert_eq!(leaks(password, &["-p", "--password"]), Vec::<String>::new());
    }

    /// What must stay readable. A trail that redacts the query as well as
    /// the payload answers nothing, and these are the words a reader
    /// actually needs: which filter, which port, which image.
    #[test]
    fn the_shape_of_a_command_survives_redaction() {
        let line = trail_line(
            &["run", "--filter=dangling", "-p8080:80", "-v/data:/data"],
            &[],
        );
        assert!(
            line.contains("--filter=dangling"),
            "the filter is the question being asked, not a payload: {line}"
        );
        assert!(
            line.contains("-p8080:80"),
            "a published port is not a secret: {line}"
        );
        assert!(
            line.contains("-v/data:/data"),
            "a bind mount is not a secret: {line}"
        );
    }

    #[test]
    fn env_values_are_redacted() {
        assert_eq!(
            redact("cd dir && COMPOSE_PROFILES='dev' docker compose up"),
            "cd dir && COMPOSE_PROFILES=*** docker compose up"
        );
        assert_eq!(redact("plain words only"), "plain words only");
        // '=' inside non-env words is left alone
        assert_eq!(redact("--filter=P /data"), "--filter=P /data");
        // sh_quote's leading apostrophe must not defeat redaction
        assert_eq!(
            redact("run -e 'DB_PASSWORD=s3cret' web"),
            "run -e 'DB_PASSWORD=*** web"
        );
    }
}
