// Static assertion tests — compile-time grep guards.
//
// These are integration tests that programmatically scan source code
// to enforce architectural constraints:
//   - No retry/backoff/throttle in scenario execution paths
//   - No dispatch serialization (Semaphore) in concurrent scenarios
//   - No UTXO pre-partitioning in volume scenarios
//
// Matching PR6's testing methodology to demonstrate the same level of
// rigour in upholding the benchmark's "no interference" contract.

/// Remove `#[cfg(test)] mod tests { ... }` blocks (balanced-brace aware).
fn strip_test_modules(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut depth: i32 = 0;
    let mut in_test_block = false;

    while i < source.len() {
        if !in_test_block {
            if source[i..].starts_with("#[cfg(test)]") {
                if let Some(brace_offset) = source[i..].find('{') {
                    in_test_block = true;
                    depth = 1;
                    i += brace_offset + 1;
                    continue;
                }
            }
            out.push(source[i..].chars().next().unwrap());
            i += source[i..].chars().next().unwrap().len_utf8();
        } else {
            let c = source[i..].chars().next().unwrap();
            let c_len = c.len_utf8();
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        in_test_block = false;
                    }
                }
                _ => {}
            }
            i += c_len;
        }
    }
    out
}

/// Remove single-line (`//`) and multi-line (`/* ... */`) comments.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            }
            continue;
        }
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Check if `source` contains any of the given patterns and return
/// `(line_number, matched_text)` tuples.
fn find_forbidden(source: &str, patterns: &[&str]) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for pattern in patterns {
        let re = regex::Regex::new(pattern).unwrap();
        for m in re.find_iter(source) {
            let line_num = source[..m.start()].matches('\n').count() + 1;
            hits.push((line_num, m.as_str().to_string()));
        }
    }
    hits
}

/// Run guards against a specific source file.
fn check_file(path: &str, patterns: &[&str], label: &str) {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let cleaned = strip_comments(&strip_test_modules(&raw));
    let hits = find_forbidden(&cleaned, patterns);

    assert!(
        hits.is_empty(),
        "\n--- {label} ---\nFile: {path}\nFound forbidden patterns in production code:\n{}",
        hits.iter()
            .map(|(line, text)| format!("  line {line}: `{text}`"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

const SCENARIO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/scenarios.rs");

// ── Guard 1: No retry / backoff / throttle in scenario code ─────────

#[test]
fn ac01_no_retry_in_scenarios() {
    check_file(
        SCENARIO_PATH,
        &[
            r"\bretry\w*\s*\(",
            r"\bbackoff\b",
            r"\bthrottle\b",
            r"\brate_limit\b",
        ],
        "AC-01: No retry/backoff/throttle in scenario code",
    );
}

#[test]
fn ac02_no_serialization_semaphore_in_scenarios() {
    check_file(
        SCENARIO_PATH,
        &[r"\bSemaphore\b"],
        "AC-02: No dispatch serialisation via Semaphore in scenarios",
    );
}

// ── Guard 2: S1 does not pre-partition UTXOs ────────────────────────

#[test]
fn ac03_s1_no_utxo_pre_partitioning() {
    check_file(
        SCENARIO_PATH,
        &[
            r"\bchunk\s*\(",
            r"\bpartition\s*\(",
            r"\bsplit_at\s*\(",
        ],
        "AC-03: S1 volume does not pre-partition UTXOs",
    );
}

// ── Guard 3: S4 uses concurrent dispatch (join_all / JoinSet) ───────

#[test]
fn ac04_s4_uses_concurrent_dispatch() {
    let raw =
        std::fs::read_to_string(SCENARIO_PATH).unwrap_or_else(|e| panic!("read {SCENARIO_PATH}: {e}"));
    let cleaned = strip_comments(&strip_test_modules(&raw));

    let has_join_all = cleaned.contains("join_all");
    let has_join_set = cleaned.contains("JoinSet");
    assert!(
        has_join_all || has_join_set,
        "AC-04: S4 must use join_all or JoinSet for concurrent dispatch (found neither)"
    );

    let hits = find_forbidden(
        &cleaned,
        &[
            r"Mutex<dyn\s+WalletDriver",
            r"Mutex<Box<dyn\s+WalletDriver",
        ],
    );
    assert!(
        hits.is_empty(),
        "AC-04: S4 must not serialise dispatch via Mutex<dyn WalletDriver>\n{}",
        hits.iter()
            .map(|(l, t)| format!("  line {l}: `{t}`"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
