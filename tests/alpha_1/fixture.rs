//! Deterministic Alpha 1 repository fixture and its versioned oracles.
//!
//! This module deliberately uses only `std`: the acceptance and benchmark
//! binaries can include it without introducing a second fixture model or a
//! runtime dependency. Paths and bytes produced here are part of the Alpha 1
//! oracle and must change only together with `spec-v1.json` and the checked-in
//! manifests.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

pub const CONTRACT_VERSION: u32 = 1;
pub const FIXTURE_VERSION: u32 = 1;
pub const FIXTURE_SEED: u64 = 0x5a45_435f_414c_5048;
pub const EXPECTED_ENTRY_COUNT: usize = 10_000;
pub const EXPECTED_REGULAR_FILE_COUNT: usize = 9_900;
pub const EXPECTED_DIRECTORY_COUNT: usize = 96;
pub const EXPECTED_SYMLINK_COUNT: usize = 4;
pub const EXPECTED_UTF8_TEXT_BYTES: u64 = 104_857_600;
pub const LARGE_LOGICAL_LINES: usize = 100_000;
pub const LARGE_LF_COUNT: usize = 99_999;
pub const BENCH_SEARCH_HITS: usize = 1_000;
pub const SEARCH_RESULT_LIMIT: usize = 100;
pub const BENCH_QUICK_OPEN_QUERIES: usize = 100;
pub const WORKFLOW_RUNS: u8 = 20;

pub const FIXED_WORKSPACE: &str = "/tmp/zec-alpha-1-v1";
pub const FIXED_ROOT: &str = "/tmp/zec-alpha-1-v1/repo";
pub const FIXED_ROOT_ALIAS: &str = "/tmp/zec-alpha-1-v1/repo-alias";
pub const FIXED_OUTSIDE_CONTROL: &str = "/tmp/zec-alpha-1-v1/outside-control.txt";

pub const READY_PATH: &str = "README.md";
pub const READY_SENTINEL: &str = "ALPHA1_READY_SENTINEL";
pub const EDIT_A_PATH: &str = "src/日本 語.rs";
pub const EDIT_B_PATH: &str = "src/crlf-edit.rs";
pub const EDIT_C_PATH: &str = "src/no-final-newline.txt";
pub const EDIT_D_PATH: &str = "scratch/新規 メモ.txt";
pub const OUTSIDE_CONTROL_NAME: &str = "outside-control.txt";
pub const ROOT_ALIAS_SUFFIX: &str = "-alias";

pub const SPEC_RELATIVE_PATH: &str = "tests/alpha_1/spec-v1.json";
pub const BEFORE_MANIFEST_RELATIVE_PATH: &str = "tests/alpha_1/expected-before-manifest-v1.jsonl";
pub const AFTER_MANIFEST_RELATIVE_PATH: &str =
    "tests/alpha_1/expected-after-workflow-01-manifest-v1.jsonl";
pub const POC_TEST_IDS_RELATIVE_PATH: &str = "tests/alpha_1/poc-test-ids-v1.txt";

const GENERATOR_SOURCE: &[u8] = include_bytes!("fixture.rs");
const CHECKED_SPEC: &[u8] = include_bytes!("spec-v1.json");
const CHECKED_BEFORE_MANIFEST: &[u8] = include_bytes!("expected-before-manifest-v1.jsonl");
const CHECKED_AFTER_MANIFEST: &[u8] =
    include_bytes!("expected-after-workflow-01-manifest-v1.jsonl");
const CHECKED_POC_TEST_IDS: &str = include_str!("poc-test-ids-v1.txt");

const README_CONTENT: &str = concat!(
    "# Alpha 1 deterministic repository\n\n",
    "ALPHA1_READY_SENTINEL\n",
    "This repository is generated from seed 0x5a45435f414c5048.\n",
);
const EDIT_A_BEFORE: &[u8] =
    b"\xef\xbb\xbf// Alpha 1 UTF-8 BOM fixture\npub const TOKEN: &str = \"ALPHA1_EDIT_A_OLD\";\n";
const EDIT_B_BEFORE: &[u8] =
    b"// Alpha 1 CRLF fixture\r\npub const TOKEN: &str = \"ALPHA1_FIND_B_OLD\";\r\n";
const EDIT_C_BEFORE: &[u8] = b"Alpha 1 file without final newline :: ALPHA1_EDIT_C_OLD";
const CONTROL_CONTENT: &str = "ALPHA1_EXCLUDED_SENTINEL in-scope control\n";
const STALE_A_CONTENT: &str = "ALPHA1_STALE_A old query result\n";
const STALE_B_CONTENT: &str = "ALPHA1_STALE_B current query result\n";
const EXCLUDED_CONTENT: &str = "ALPHA1_EXCLUDED_SENTINEL excluded by repository rules\n";
const DOT_GITIGNORE_CONTENT: &str = "/ignored/\n/target/\n";
const SPACE_PATH_CONTENT: &str = "space path fixture\n";
const UNICODE_PATH_CONTENT: &str = "Unicode path fixture: 日本語 e\u{301}\n";

#[derive(Clone, Debug, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File(Content),
    Symlink(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Content {
    Static(&'static [u8], bool),
    BenchSearch(u16),
    LargeLines,
    Deterministic { size: usize, salt: u64 },
    EditA(u8),
    EditB(u8),
    EditC(u8),
    EditD(u8),
}

impl Content {
    fn is_utf8_text(&self) -> bool {
        match self {
            Self::Static(_, is_text) => *is_text,
            _ => true,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Static(bytes, _) => bytes.len(),
            Self::BenchSearch(index) => bench_search_content(*index).len(),
            Self::LargeLines => large_lines_content().len(),
            Self::Deterministic { size, .. } => *size,
            Self::EditA(run) => edit_a_after(*run).len(),
            Self::EditB(run) => edit_b_after(*run).len(),
            Self::EditC(run) => edit_c_after(*run).len(),
            Self::EditD(run) => edit_d_after(*run).len(),
        }
    }

    fn bytes(&self, path: &str) -> Vec<u8> {
        match self {
            Self::Static(bytes, _) => bytes.to_vec(),
            Self::BenchSearch(index) => bench_search_content(*index).into_bytes(),
            Self::LargeLines => large_lines_content(),
            Self::Deterministic { size, salt } => deterministic_text(path, *size, *salt),
            Self::EditA(run) => edit_a_after(*run),
            Self::EditB(run) => edit_b_after(*run),
            Self::EditC(run) => edit_c_after(*run),
            Self::EditD(run) => edit_d_after(*run),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlannedEntry {
    path: String,
    kind: EntryKind,
}

impl PlannedEntry {
    fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::Directory,
        }
    }

    fn file(path: impl Into<String>, content: Content) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::File(content),
        }
    }

    fn symlink(path: impl Into<String>, target: &'static str) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::Symlink(target),
        }
    }
}

fn static_text(bytes: &'static [u8]) -> Content {
    Content::Static(bytes, true)
}

fn before_entries() -> Vec<PlannedEntry> {
    let mut entries = Vec::with_capacity(EXPECTED_ENTRY_COUNT);

    for path in [
        "aliases", "bench", "docs", "ignored", "links", "scratch", "src", "target", "tests", "tree",
    ] {
        entries.push(PlannedEntry::directory(path));
    }
    for index in 0..86 {
        entries.push(PlannedEntry::directory(format!("tree/d{index:03}")));
    }

    entries.extend([
        PlannedEntry::file(".gitignore", static_text(DOT_GITIGNORE_CONTENT.as_bytes())),
        PlannedEntry::file(READY_PATH, static_text(README_CONTENT.as_bytes())),
        PlannedEntry::file(EDIT_A_PATH, static_text(EDIT_A_BEFORE)),
        PlannedEntry::file(EDIT_B_PATH, static_text(EDIT_B_BEFORE)),
        PlannedEntry::file(EDIT_C_PATH, static_text(EDIT_C_BEFORE)),
        PlannedEntry::file("src/control.txt", static_text(CONTROL_CONTENT.as_bytes())),
        PlannedEntry::file("src/stale-a.txt", static_text(STALE_A_CONTENT.as_bytes())),
        PlannedEntry::file("src/stale-b.txt", static_text(STALE_B_CONTENT.as_bytes())),
        PlannedEntry::file(
            "docs/read me.md",
            static_text(SPACE_PATH_CONTENT.as_bytes()),
        ),
        PlannedEntry::file(
            "docs/組合せ-é.txt",
            static_text(UNICODE_PATH_CONTENT.as_bytes()),
        ),
        PlannedEntry::file(
            "ignored/excluded.txt",
            static_text(EXCLUDED_CONTENT.as_bytes()),
        ),
        PlannedEntry::file(
            "target/excluded.txt",
            static_text(EXCLUDED_CONTENT.as_bytes()),
        ),
        PlannedEntry::file("bench/large-100000-lines.txt", Content::LargeLines),
        PlannedEntry::file(
            "bench/save-5mib.txt",
            Content::Deterministic {
                size: 5 * 1024 * 1024,
                salt: 0x5341_5645,
            },
        ),
        PlannedEntry::file(
            "tests/binary-with-nul.dat",
            Content::Static(b"\0ALPHA1_EXCLUDED_SENTINEL\xffbinary fixture\0", false),
        ),
    ]);

    for index in 0..BENCH_SEARCH_HITS {
        entries.push(PlannedEntry::file(
            format!("bench/search-{index:04}.txt"),
            Content::BenchSearch(index as u16),
        ));
    }

    let fixed_regular_count = entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File(_)))
        .count();
    let filler_count = EXPECTED_REGULAR_FILE_COUNT - fixed_regular_count;
    let fixed_text_bytes = entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::File(content) if content.is_utf8_text() => Some(content.len() as u64),
            _ => None,
        })
        .sum::<u64>();
    let remaining = EXPECTED_UTF8_TEXT_BYTES
        .checked_sub(fixed_text_bytes)
        .expect("fixed Alpha 1 fixture exceeds text payload budget");
    let base_size = remaining / filler_count as u64;
    let extra_files = (remaining % filler_count as u64) as usize;

    for index in 0..filler_count {
        let directory = index % 86;
        let size = base_size as usize + usize::from(index < extra_files);
        entries.push(PlannedEntry::file(
            format!("tree/d{directory:03}/file-{index:04}.txt"),
            Content::Deterministic {
                size,
                salt: index as u64,
            },
        ));
    }

    entries.extend([
        PlannedEntry::symlink("aliases/日本 語.rs", "../src/日本 語.rs"),
        PlannedEntry::symlink("links/control-alias.txt", "../src/control.txt"),
        PlannedEntry::symlink("links/dangling", "../missing.txt"),
        PlannedEntry::symlink("links/root-loop", "root-loop"),
    ]);
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    entries
}

fn after_entries(run: u8) -> Result<Vec<PlannedEntry>, String> {
    validate_run(run)?;
    let mut entries = before_entries();
    for entry in &mut entries {
        if entry.path == EDIT_A_PATH {
            entry.kind = EntryKind::File(Content::EditA(run));
        } else if entry.path == EDIT_B_PATH {
            entry.kind = EntryKind::File(Content::EditB(run));
        } else if entry.path == EDIT_C_PATH {
            entry.kind = EntryKind::File(Content::EditC(run));
        }
    }
    entries.push(PlannedEntry::file(EDIT_D_PATH, Content::EditD(run)));
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    Ok(entries)
}

fn validate_run(run: u8) -> Result<(), String> {
    if (1..=WORKFLOW_RUNS).contains(&run) {
        Ok(())
    } else {
        Err(format!(
            "workflow run must be 1..={WORKFLOW_RUNS}, got {run}"
        ))
    }
}

fn bench_search_content(index: u16) -> String {
    format!("row {index:04}: ALPHA1_BENCH_SEARCH result {index:04}\n")
}

fn large_lines_content() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(LARGE_LOGICAL_LINES * 22);
    for index in 0..LARGE_LOGICAL_LINES {
        write!(&mut bytes, "large-line-{index:06} alpha1").expect("write to Vec cannot fail");
        if index + 1 != LARGE_LOGICAL_LINES {
            bytes.push(b'\n');
        }
    }
    bytes
}

fn edit_a_after(run: u8) -> Vec<u8> {
    EDIT_A_BEFORE
        .windows(b"ALPHA1_EDIT_A_OLD".len())
        .position(|window| window == b"ALPHA1_EDIT_A_OLD")
        .map(|position| {
            let mut bytes = EDIT_A_BEFORE.to_vec();
            bytes.splice(
                position..position + b"ALPHA1_EDIT_A_OLD".len(),
                format!("ALPHA1_EDIT_A_{run:02} A3_{run:02}_0001").bytes(),
            );
            bytes
        })
        .expect("edit A token is fixed")
}

fn edit_b_after(run: u8) -> Vec<u8> {
    EDIT_B_BEFORE
        .windows(b"ALPHA1_FIND_B_OLD".len())
        .position(|window| window == b"ALPHA1_FIND_B_OLD")
        .map(|position| {
            let mut bytes = EDIT_B_BEFORE.to_vec();
            bytes.splice(
                position..position + b"ALPHA1_FIND_B_OLD".len(),
                format!("ALPHA1_EDIT_B_{run:02} A3_{run:02}_0002").bytes(),
            );
            bytes
        })
        .expect("edit B token is fixed")
}

fn edit_c_after(run: u8) -> Vec<u8> {
    let mut bytes = EDIT_C_BEFORE.to_vec();
    write!(&mut bytes, " :: ALPHA1_EDIT_C_{run:02} A3_{run:02}_0003")
        .expect("write to Vec cannot fail");
    bytes
}

fn edit_d_after(run: u8) -> Vec<u8> {
    format!("Alpha 1 scratch 日本語\nworkflow token ALPHA1_EDIT_D_{run:02} A3_{run:02}_0004\n")
        .into_bytes()
}

fn deterministic_text(path: &str, size: usize, salt: u64) -> Vec<u8> {
    let mut state = FIXTURE_SEED ^ salt ^ fnv1a64(path.as_bytes());
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789 ";
    let mut bytes = Vec::with_capacity(size);
    for index in 0..size {
        if index % 80 == 79 {
            bytes.push(b'\n');
        } else {
            state = splitmix64(state);
            bytes.push(alphabet[(state as usize) % alphabet.len()]);
        }
    }
    bytes
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[derive(Clone)]
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.total_len = self.total_len.wrapping_add(bytes.len() as u64);
        if self.buffer_len != 0 {
            let needed = 64 - self.buffer_len;
            let copied = needed.min(bytes.len());
            self.buffer[self.buffer_len..self.buffer_len + copied]
                .copy_from_slice(&bytes[..copied]);
            self.buffer_len += copied;
            bytes = &bytes[copied..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            } else {
                debug_assert!(bytes.is_empty());
                return;
            }
        }
        while bytes.len() >= 64 {
            let block: &[u8; 64] = bytes[..64].try_into().expect("64-byte SHA-256 block");
            self.compress(block);
            bytes = &bytes[64..];
        }
        self.buffer[..bytes.len()].copy_from_slice(bytes);
        self.buffer_len = bytes.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
        } else {
            self.buffer[self.buffer_len..56].fill(0);
        }
        self.buffer[56..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut digest = [0; 32];
        for (chunk, word) in digest.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a_2f98,
            0x7137_4491,
            0xb5c0_fbcf,
            0xe9b5_dba5,
            0x3956_c25b,
            0x59f1_11f1,
            0x923f_82a4,
            0xab1c_5ed5,
            0xd807_aa98,
            0x1283_5b01,
            0x2431_85be,
            0x550c_7dc3,
            0x72be_5d74,
            0x80de_b1fe,
            0x9bdc_06a7,
            0xc19b_f174,
            0xe49b_69c1,
            0xefbe_4786,
            0x0fc1_9dc6,
            0x240c_a1cc,
            0x2de9_2c6f,
            0x4a74_84aa,
            0x5cb0_a9dc,
            0x76f9_88da,
            0x983e_5152,
            0xa831_c66d,
            0xb003_27c8,
            0xbf59_7fc7,
            0xc6e0_0bf3,
            0xd5a7_9147,
            0x06ca_6351,
            0x1429_2967,
            0x27b7_0a85,
            0x2e1b_2138,
            0x4d2c_6dfc,
            0x5338_0d13,
            0x650a_7354,
            0x766a_0abb,
            0x81c2_c92e,
            0x9272_2c85,
            0xa2bf_e8a1,
            0xa81a_664b,
            0xc24b_8b70,
            0xc76c_51a3,
            0xd192_e819,
            0xd699_0624,
            0xf40e_3585,
            0x106a_a070,
            0x19a4_c116,
            0x1e37_6c08,
            0x2748_774c,
            0x34b0_bcb5,
            0x391c_0cb3,
            0x4ed8_aa4a,
            0x5b9c_ca4f,
            0x682e_6ff3,
            0x748f_82ee,
            0x78a5_636f,
            0x84c8_7814,
            0x8cc7_0208,
            0x90be_fffa,
            0xa450_6ceb,
            0xbef9_a3f7,
            0xc671_78f2,
        ];
        let mut schedule = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes(chunk.try_into().expect("4-byte SHA word"));
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finish())
}

fn sha256_reader(reader: &mut impl Read) -> Result<String, String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("read for SHA-256: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finish()))
}

fn hex_digest(digest: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("write to String cannot fail");
    }
    output
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(&mut output, "\\u{:04x}", character as u32)
                    .expect("write to String cannot fail");
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

fn manifest_bytes(entries: &[PlannedEntry]) -> Vec<u8> {
    let mut output = String::with_capacity(entries.len() * 190);
    for entry in entries {
        match &entry.kind {
            EntryKind::Directory => {
                writeln!(
                    &mut output,
                    "{{\"path\":{},\"kind\":\"directory\",\"mode\":\"0755\",\"size\":0,\"content_sha256\":null,\"symlink_target\":null}}",
                    json_string(&entry.path)
                )
                .expect("write to String cannot fail");
            }
            EntryKind::File(content) => {
                let bytes = content.bytes(&entry.path);
                writeln!(
                    &mut output,
                    "{{\"path\":{},\"kind\":\"file\",\"mode\":\"0644\",\"size\":{},\"content_sha256\":\"{}\",\"symlink_target\":null}}",
                    json_string(&entry.path),
                    bytes.len(),
                    sha256_hex(&bytes)
                )
                .expect("write to String cannot fail");
            }
            EntryKind::Symlink(target) => {
                writeln!(
                    &mut output,
                    "{{\"path\":{},\"kind\":\"symlink\",\"mode\":\"0777\",\"size\":{},\"content_sha256\":null,\"symlink_target\":{}}}",
                    json_string(&entry.path),
                    target.len(),
                    json_string(target)
                )
                .expect("write to String cannot fail");
            }
        }
    }
    output.into_bytes()
}

pub fn expected_before_manifest() -> Vec<u8> {
    manifest_bytes(&before_entries())
}

pub fn expected_after_manifest(run: u8) -> Result<Vec<u8>, String> {
    Ok(manifest_bytes(&after_entries(run)?))
}

pub fn required_case_ids() -> Vec<String> {
    let mut ids = vec![
        "A1_ROOT_IDENTITY".to_owned(),
        "A1_OUTSIDE_TRACE".to_owned(),
        "A1_PARTIAL_STARTUP".to_owned(),
        "A2_QUICK_OPEN".to_owned(),
        "A2_PROJECT_SEARCH".to_owned(),
        "A2_STALE_RESULT".to_owned(),
    ];
    for run in 1..=WORKFLOW_RUNS {
        ids.push(format!("A3_WORKFLOW_{run:02}"));
    }
    for scenario in ["OPEN", "SEARCH", "SAVE"] {
        for run in 1..=WORKFLOW_RUNS {
            ids.push(format!("A5_{scenario}_{run:02}"));
        }
    }
    for scenario in ["INT", "QUIT", "TERM", "HUP", "TSTP_CONT"] {
        for run in 1..=WORKFLOW_RUNS {
            ids.push(format!("A5_{scenario}_{run:02}"));
        }
    }
    ids
}

fn write_search_result(
    output: &mut String,
    indent: &str,
    path: &str,
    line: usize,
    column: usize,
    preview: &str,
    trailing_comma: bool,
) {
    writeln!(
        output,
        "{indent}{{\"path\":{},\"line\":{line},\"column\":{column},\"preview\":{}}}{}",
        json_string(path),
        json_string(preview),
        if trailing_comma { "," } else { "" }
    )
    .expect("write to String cannot fail");
}

pub fn spec_bytes() -> Vec<u8> {
    let mut output = String::with_capacity(64 * 1024);
    output.push_str("{\n");
    writeln!(output, "  \"contract_version\": {CONTRACT_VERSION},")
        .expect("write to String cannot fail");
    writeln!(output, "  \"fixture_version\": {FIXTURE_VERSION},")
        .expect("write to String cannot fail");
    writeln!(output, "  \"seed\": \"0x{FIXTURE_SEED:016x}\",")
        .expect("write to String cannot fail");
    output.push_str(
        "  \"environment\": {\n    \"runner\": \"github-hosted-ubuntu-24.04-x86_64\",\n    \"locale\": \"C.UTF-8\",\n    \"term\": \"xterm-256color\",\n    \"columns\": 120,\n    \"rows\": 40,\n    \"pty_parser\": \"vt100 0.16.2\"\n  },\n",
    );
    writeln!(
        output,
        "  \"fixture_counts\": {{\"entries\":{EXPECTED_ENTRY_COUNT},\"regular_files\":{EXPECTED_REGULAR_FILE_COUNT},\"directories\":{EXPECTED_DIRECTORY_COUNT},\"symlinks\":{EXPECTED_SYMLINK_COUNT}}},"
    )
    .expect("write to String cannot fail");
    writeln!(
        output,
        "  \"payload\": {{\"utf8_text_bytes\":{EXPECTED_UTF8_TEXT_BYTES},\"large_file_logical_lines\":{LARGE_LOGICAL_LINES},\"large_file_lf_count\":{LARGE_LF_COUNT},\"edited_regular_file_max_bytes\":10485760,\"display_line_max_bytes\":65536}},"
    )
    .expect("write to String cannot fail");
    output.push_str(
        "  \"manifest_format\": {\n    \"encoding\": \"UTF-8 JSONL with final LF\",\n    \"sort\": \"relative path UTF-8 bytes ascending\",\n    \"fields\": [\"path\",\"kind\",\"mode\",\"size\",\"content_sha256\",\"symlink_target\"],\n    \"directory_size\": 0,\n    \"excluded_subtrees\": [\".git\"]\n  },\n",
    );
    output.push_str("  \"fixed_paths\": {\n");
    writeln!(
        output,
        "    \"workspace\": {},",
        json_string(FIXED_WORKSPACE)
    )
    .expect("write to String cannot fail");
    writeln!(output, "    \"root\": {},", json_string(FIXED_ROOT))
        .expect("write to String cannot fail");
    writeln!(
        output,
        "    \"root_alias\": {},",
        json_string(FIXED_ROOT_ALIAS)
    )
    .expect("write to String cannot fail");
    writeln!(
        output,
        "    \"outside_control\": {}",
        json_string(FIXED_OUTSIDE_CONTROL)
    )
    .expect("write to String cannot fail");
    output.push_str("  },\n");
    output.push_str("  \"root_inputs\": [\n");
    output.push_str(
        "    {\"id\":\"cwd\",\"cwd\":\"/tmp/zec-alpha-1-v1/repo\",\"argument\":null},\n    {\"id\":\"directory\",\"cwd\":\"/tmp/zec-alpha-1-v1\",\"argument\":\"repo\"},\n    {\"id\":\"dot\",\"cwd\":\"/tmp/zec-alpha-1-v1/repo\",\"argument\":\".\"},\n    {\"id\":\"dotdot\",\"cwd\":\"/tmp/zec-alpha-1-v1/repo/src\",\"argument\":\"..\"},\n    {\"id\":\"absolute\",\"cwd\":\"/tmp/zec-alpha-1-v1\",\"argument\":\"/tmp/zec-alpha-1-v1/repo\"},\n    {\"id\":\"symlink_alias\",\"cwd\":\"/tmp/zec-alpha-1-v1\",\"argument\":\"/tmp/zec-alpha-1-v1/repo-alias\"}\n  ],\n",
    );
    writeln!(
        output,
        "  \"ready\": {{\"path\":{},\"root_label\":\"repo\",\"body_sentinel\":{},\"predicate\":{{\"alternate_screen\":true,\"columns\":120,\"rows\":40,\"visible_cursor_in_body\":true}}}},",
        json_string(READY_PATH),
        json_string(READY_SENTINEL)
    )
    .expect("write to String cannot fail");
    output.push_str(
        "  \"timing\": {\n    \"clock\": \"CLOCK_MONOTONIC\",\n    \"unit\": \"integer microseconds\",\n    \"startup_begins\": \"immediately before child spawn call\",\n    \"operation_begins\": \"immediately after final operation byte is flushed to PTY\",\n    \"frame_time\": \"read completion time of first matching VT generation\",\n    \"vt_generation_increment\": \"after every completed PTY read and parser update\",\n    \"screen_predicate_timeout_ms\": 15000,\n    \"child_cleanup_timeout_ms\": 5000,\n    \"pty_scenario_timeout_ms\": 120000\n  },\n",
    );
    output.push_str("  \"identity\": {\n");
    writeln!(
        output,
        "    \"canonical_file\": {},",
        json_string(EDIT_A_PATH)
    )
    .expect("write to String cannot fail");
    output.push_str(
        "    \"aliases\": [\"src/日本 語.rs\",\"./src/日本 語.rs\",\"src/../src/日本 語.rs\",\"aliases/日本 語.rs\",\"/tmp/zec-alpha-1-v1/repo/src/日本 語.rs\"],\n    \"expected_repository_root_count\": 1,\n    \"expected_worktree_root_count\": 1,\n    \"expected_buffer_id_count\": 1,\n    \"expected_tab_id_count\": 1\n  },\n",
    );
    output.push_str(
        "  \"outside_trace\": {\n    \"path\": \"/tmp/zec-alpha-1-v1/outside-control.txt\",\n    \"allowed_operations\": [\"open-self\",\"stat-self\",\"stat-ancestor-git\"],\n    \"outside_parent_read_dir_count\": 0,\n    \"outside_sibling_read_dir_count\": 0\n  },\n  \"partial_startup\": {\n    \"normal_path\": \"README.md\",\n    \"eloop_path\": \"links/root-loop\",\n    \"expected_error\": \"ELOOP\",\n    \"editable_token\": \"ALPHA1_PARTIAL_STARTUP_EDIT\",\n    \"expected_exit_code\": 0\n  },\n",
    );
    output.push_str("  \"exclusions\": {\n");
    output.push_str(
        "    \"sentinel\": \"ALPHA1_EXCLUDED_SENTINEL\",\n    \"excluded_paths\": [\".git/alpha1-excluded.txt\",\"ignored/excluded.txt\",\"target/excluded.txt\",\"tests/binary-with-nul.dat\",\"/tmp/zec-alpha-1-v1/outside-control.txt\"],\n    \"in_scope_control_path\": \"src/control.txt\",\n    \"quick_open_exclusion_queries\": [\n      {\"query\":\".git/alpha1-excluded.txt\",\"expected_results\":[]},\n      {\"query\":\"ignored/excluded.txt\",\"expected_results\":[]},\n      {\"query\":\"target/excluded.txt\",\"expected_results\":[]},\n      {\"query\":\"/tmp/zec-alpha-1-v1/outside-control.txt\",\"expected_results\":[]}\n    ],\n",
    );
    output.push_str("    \"project_search_expected_results\": [\n");
    write_search_result(
        &mut output,
        "      ",
        "src/control.txt",
        1,
        1,
        "ALPHA1_EXCLUDED_SENTINEL in-scope control",
        false,
    );
    output.push_str("    ]\n  },\n");
    output.push_str(
        "  \"quick_open\": {\n    \"shortcut_bytes_hex\": \"10\",\n    \"query\": \"日本 語.rs\",\n    \"expected_selected_path\": \"src/日本 語.rs\",\n    \"expected_open_path\": \"src/日本 語.rs\",\n    \"alias_reopen_path\": \"aliases/日本 語.rs\",\n    \"expected_tab_count_after_alias_reopen\": 1\n  },\n",
    );
    output.push_str(
        "  \"project_search\": {\n    \"shortcut_bytes_hex\": \"1b66\",\n    \"case_sensitive\": true,\n    \"unicode_normalization\": \"none\",\n    \"result_limit\": 100,\n    \"coordinates\": \"1-based logical line and BOM-excluded Unicode scalar column\",\n    \"queries\": [\n",
    );
    output.push_str(
        "      {\"id\":\"edit_b\",\"text\":\"ALPHA1_FIND_B_OLD\",\"expected_results\":[\n",
    );
    write_search_result(
        &mut output,
        "        ",
        EDIT_B_PATH,
        2,
        26,
        "pub const TOKEN: &str = \"ALPHA1_FIND_B_OLD\";",
        false,
    );
    output.push_str("      ]},\n");
    output.push_str(
        "      {\"id\":\"case_variant\",\"text\":\"alpha1_find_b_old\",\"expected_results\":[]},\n",
    );
    output.push_str(
        "      {\"id\":\"nfc_no_normalization\",\"text\":\"é\",\"expected_results\":[]},\n",
    );
    output.push_str("      {\"id\":\"nfd_scalar_column\",\"text\":\"é\",\"expected_results\":[\n");
    write_search_result(
        &mut output,
        "        ",
        "docs/組合せ-é.txt",
        1,
        27,
        "Unicode path fixture: 日本語 é",
        false,
    );
    output.push_str("      ]},\n");
    output.push_str("      {\"id\":\"bom_scalar_column\",\"text\":\"Alpha 1 UTF-8 BOM fixture\",\"expected_results\":[\n");
    write_search_result(
        &mut output,
        "        ",
        EDIT_A_PATH,
        1,
        4,
        "// Alpha 1 UTF-8 BOM fixture",
        false,
    );
    output.push_str("      ]},\n");
    output.push_str(
        "      {\"id\":\"excluded\",\"text\":\"ALPHA1_EXCLUDED_SENTINEL\",\"expected_results\":[\n",
    );
    write_search_result(
        &mut output,
        "        ",
        "src/control.txt",
        1,
        1,
        "ALPHA1_EXCLUDED_SENTINEL in-scope control",
        false,
    );
    output.push_str("      ]},\n");
    output
        .push_str("      {\"id\":\"stale_a\",\"text\":\"ALPHA1_STALE_A\",\"expected_results\":[\n");
    write_search_result(
        &mut output,
        "        ",
        "src/stale-a.txt",
        1,
        1,
        "ALPHA1_STALE_A old query result",
        false,
    );
    output.push_str("      ]},\n");
    output
        .push_str("      {\"id\":\"stale_b\",\"text\":\"ALPHA1_STALE_B\",\"expected_results\":[\n");
    write_search_result(
        &mut output,
        "        ",
        "src/stale-b.txt",
        1,
        1,
        "ALPHA1_STALE_B current query result",
        false,
    );
    output.push_str("      ]},\n");
    output.push_str("      {\"id\":\"benchmark\",\"text\":\"ALPHA1_BENCH_SEARCH\",\"expected_total_hits\":1000,\"expected_results\":[\n");
    for index in 0..SEARCH_RESULT_LIMIT {
        write_search_result(
            &mut output,
            "        ",
            &format!("bench/search-{index:04}.txt"),
            1,
            11,
            &format!("row {index:04}: ALPHA1_BENCH_SEARCH result {index:04}"),
            index + 1 != SEARCH_RESULT_LIMIT,
        );
    }
    output.push_str("      ]}\n    ]\n  },\n");
    output.push_str(
        "  \"stale_result_schedule\": {\n    \"query_a\": \"ALPHA1_STALE_A\",\n    \"query_b\": \"ALPHA1_STALE_B\",\n    \"completion_order\": [\"B\",\"A\"],\n    \"expected_publish_log\": [\"B\"],\n    \"expected_final_query\": \"ALPHA1_STALE_B\",\n    \"expected_final_path\": \"src/stale-b.txt\"\n  },\n  \"in_flight_actions\": {\n    \"replace_query_expected\": \"new-query-results-only\",\n    \"escape_expected\": \"prompt-absent-and-no-stale-publish\",\n    \"ctrl_q_expected\": \"child-exited-and-no-stale-publish\"\n  },\n",
    );
    output.push_str(
        "  \"workflow\": {\n    \"runs\": 20,\n    \"retry_count\": 0,\n    \"fresh_fixture_each_run\": true,\n    \"fresh_config_each_run\": true,\n    \"fresh_process_each_run\": true,\n    \"input_id_template\": \"A3_{RUN_2}_{SEQUENCE_4}\",\n    \"edit_a\": {\"path\":\"src/日本 語.rs\",\"encoding\":\"UTF-8 BOM\",\"replace\":\"ALPHA1_EDIT_A_OLD\",\"with_template\":\"ALPHA1_EDIT_A_{RUN_2} A3_{RUN_2}_0001\"},\n    \"edit_b\": {\"path\":\"src/crlf-edit.rs\",\"line_endings\":\"CRLF\",\"replace\":\"ALPHA1_FIND_B_OLD\",\"with_template\":\"ALPHA1_EDIT_B_{RUN_2} A3_{RUN_2}_0002\"},\n    \"edit_c\": {\"path\":\"src/no-final-newline.txt\",\"final_newline_before\":false,\"append\":\" :: ALPHA1_EDIT_C_{RUN_2} A3_{RUN_2}_0003\"},\n    \"edit_d\": {\"path\":\"scratch/新規 メモ.txt\",\"contents_template\":\"Alpha 1 scratch 日本語\\nworkflow token ALPHA1_EDIT_D_{RUN_2} A3_{RUN_2}_0004\\n\"},\n    \"reopen_paths\": [\"src/日本 語.rs\",\"src/crlf-edit.rs\",\"src/no-final-newline.txt\",\"scratch/新規 メモ.txt\"],\n    \"expected_dirty_conflict_deleted_markers\": 0,\n    \"expected_exit_code\": 0,\n    \"terminal_baseline_fields\": [\"tcgetattr\",\"alternate-screen\",\"mouse-tracking\",\"bracketed-paste\",\"cursor-visibility\",\"application-cursor-mode\",\"application-keypad-mode\"]\n  },\n",
    );
    output.push_str(
        "  \"benchmark\": {\n    \"report_schema_version\": 1,\n    \"startup\": {\"warmups\":2,\"samples\":20,\"p95_max_us\":3000000},\n    \"quick_open\": {\"warmups\":10,\"samples\":100,\"p95_max_us\":150000,\"max_us\":500000,\"queries\":[\n",
    );
    for index in 0..BENCH_QUICK_OPEN_QUERIES {
        let path = format!("bench/search-{index:04}.txt");
        writeln!(
            output,
            "      {{\"query\":{},\"expected_selected_path\":{}}}{}",
            json_string(&path),
            json_string(&path),
            if index + 1 == BENCH_QUICK_OPEN_QUERIES {
                ""
            } else {
                ","
            }
        )
        .expect("write to String cannot fail");
    }
    output.push_str(
        "    ]},\n    \"project_search\": {\"warmups\":2,\"samples\":10,\"p95_max_us\":5000000,\"expected_hits\":1000,\"visible_results\":100},\n    \"in_flight_search\": {\"attempts_each\":20,\"max_us\":250000},\n    \"editing\": {\"path\":\"bench/large-100000-lines.txt\",\"warmups\":10,\"samples\":500,\"p95_max_us\":100000,\"max_us\":500000,\"input_id_template\":\"EDIT_{SEQUENCE_4}\",\"payload_suffix\":\"LF\"},\n    \"save\": {\"path\":\"bench/save-5mib.txt\",\"size\":5242880,\"warmups\":2,\"samples\":10,\"max_us\":2000000},\n    \"vm_hwm_max_bytes\": 1073741824,\n    \"descendant_process_count\": 0,\n    \"nearest_rank_p95\": \"sorted_samples[ceil(0.95*N)-1]\",\n    \"input_invariants\": {\"sent_equals_applied_equals_expected\":true,\"dropped_count\":0,\"reordered\":false}\n  },\n",
    );
    output.push_str(
        "  \"failures\": {\n    \"open_eloop_path\": \"links/root-loop\",\n    \"save_enotdir_parent_path\": \"src/control.txt\",\n    \"save_enotdir_child_path\": \"src/control.txt/child.txt\",\n    \"controlled_search_error\": \"EIO\",\n    \"signals\": [\"SIGINT\",\"SIGQUIT\",\"SIGTERM\",\"SIGHUP\",\"SIGTSTP/SIGCONT\"],\n    \"attempts_each\": 20,\n    \"normal_signal_exit_code\": 0,\n    \"stop_timeout_ms\": 5000,\n    \"resume_ready_timeout_ms\": 15000,\n    \"child_reap_timeout_ms\": 5000,\n    \"reader_join_timeout_ms\": 5000,\n    \"expected_descendant_pids_after_exit\": 0,\n    \"expected_fd_count_delta_after_exit\": 0\n  },\n  \"reports\": {\n    \"acceptance_schema_version\": 1,\n    \"benchmark_schema_version\": 1,\n    \"acceptance_failed\": 0,\n    \"acceptance_required_case_count\": 186,\n    \"required_hashes\": [\"generator_source_sha256\",\"spec_sha256\",\"before_manifest_sha256\",\"after_manifest_sha256\"],\n    \"required_runner_fields\": [\"cpu_model\",\"cpu_core_count\",\"ram_bytes\",\"kernel\",\"runner_image_version\"],\n    \"verify_recomputes_statistics\": true\n  },\n",
    );
    output.push_str("  \"required_acceptance_case_ids\": [\n");
    let ids = required_case_ids();
    for (index, id) in ids.iter().enumerate() {
        writeln!(
            output,
            "    {}{}",
            json_string(id),
            if index + 1 == ids.len() { "" } else { "," }
        )
        .expect("write to String cannot fail");
    }
    output.push_str("  ],\n");
    output.push_str(
        "  \"oracles\": {\n    \"generator_source\": \"tests/alpha_1/fixture.rs\",\n    \"spec\": \"tests/alpha_1/spec-v1.json\",\n    \"before_manifest\": \"tests/alpha_1/expected-before-manifest-v1.jsonl\",\n    \"after_manifest\": \"tests/alpha_1/expected-after-workflow-01-manifest-v1.jsonl\",\n    \"after_manifest_workflow_run\": 1,\n    \"poc_test_ids\": \"tests/alpha_1/poc-test-ids-v1.txt\",\n    \"poc_test_count\": 94,\n    \"acceptance_case_count\": 186\n  }\n",
    );
    output.push_str("}\n");
    output.into_bytes()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedFixture {
    pub root: PathBuf,
    pub root_alias: PathBuf,
    pub outside_control: PathBuf,
    pub generator_source_sha256: String,
    pub spec_sha256: String,
    pub before_manifest_sha256: String,
}

impl GeneratedFixture {
    pub fn summary_json(&self) -> String {
        format!(
            "{{\"root\":{},\"root_alias\":{},\"outside_control\":{},\"generator_source_sha256\":\"{}\",\"spec_sha256\":\"{}\",\"before_manifest_sha256\":\"{}\"}}",
            json_string(&self.root.to_string_lossy()),
            json_string(&self.root_alias.to_string_lossy()),
            json_string(&self.outside_control.to_string_lossy()),
            self.generator_source_sha256,
            self.spec_sha256,
            self.before_manifest_sha256,
        )
    }
}

pub fn generator_source_sha256() -> String {
    sha256_hex(GENERATOR_SOURCE)
}

pub fn expected_workflow_file(path: &str, run: u8) -> Result<Vec<u8>, String> {
    validate_run(run)?;
    match path {
        EDIT_A_PATH => Ok(edit_a_after(run)),
        EDIT_B_PATH => Ok(edit_b_after(run)),
        EDIT_C_PATH => Ok(edit_c_after(run)),
        EDIT_D_PATH => Ok(edit_d_after(run)),
        _ => Err(format!("{path:?} is not an Alpha 1 workflow output path")),
    }
}

pub fn generate(root: &Path) -> Result<GeneratedFixture, String> {
    if root.exists() {
        return Err(format!("fixture root already exists: {}", root.display()));
    }
    let parent = root
        .parent()
        .ok_or_else(|| format!("fixture root has no parent: {}", root.display()))?;
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("fixture root name is not UTF-8: {}", root.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create fixture parent {}: {error}", parent.display()))?;
    let root_alias = parent.join(format!("{root_name}{ROOT_ALIAS_SUFFIX}"));
    let outside_control = parent.join(OUTSIDE_CONTROL_NAME);
    for companion in [&root_alias, &outside_control] {
        if fs::symlink_metadata(companion).is_ok() {
            return Err(format!(
                "fixture companion already exists: {}",
                companion.display()
            ));
        }
    }

    fs::create_dir(root)
        .map_err(|error| format!("create fixture root {}: {error}", root.display()))?;
    set_mode(root, 0o755)?;
    for entry in before_entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Directory))
    {
        let path = root.join(&entry.path);
        fs::create_dir(&path)
            .map_err(|error| format!("create fixture directory {}: {error}", path.display()))?;
        set_mode(&path, 0o755)?;
    }
    for entry in before_entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File(_)))
    {
        let EntryKind::File(content) = &entry.kind else {
            unreachable!();
        };
        let path = root.join(&entry.path);
        let mut file = File::create(&path)
            .map_err(|error| format!("create fixture file {}: {error}", path.display()))?;
        file.write_all(&content.bytes(&entry.path))
            .map_err(|error| format!("write fixture file {}: {error}", path.display()))?;
        drop(file);
        set_mode(&path, 0o644)?;
    }
    for entry in before_entries()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Symlink(_)))
    {
        let EntryKind::Symlink(target) = &entry.kind else {
            unreachable!();
        };
        let path = root.join(&entry.path);
        symlink(target, &path)
            .map_err(|error| format!("create fixture symlink {}: {error}", path.display()))?;
    }

    let git_dir = root.join(".git");
    fs::create_dir(&git_dir)
        .map_err(|error| format!("create excluded .git {}: {error}", git_dir.display()))?;
    set_mode(&git_dir, 0o755)?;
    let git_sentinel = git_dir.join("alpha1-excluded.txt");
    fs::write(&git_sentinel, EXCLUDED_CONTENT)
        .map_err(|error| format!("write excluded .git sentinel: {error}"))?;
    set_mode(&git_sentinel, 0o644)?;
    fs::write(
        &outside_control,
        "ALPHA1_EXCLUDED_SENTINEL root-outside control\n",
    )
    .map_err(|error| {
        format!(
            "write outside control {}: {error}",
            outside_control.display()
        )
    })?;
    set_mode(&outside_control, 0o644)?;
    symlink(root_name, &root_alias).map_err(|error| {
        format!(
            "create root alias {} -> {root_name}: {error}",
            root_alias.display()
        )
    })?;

    verify_fixture(root, None)?;
    let before = expected_before_manifest();
    Ok(GeneratedFixture {
        root: root.to_path_buf(),
        root_alias,
        outside_control,
        generator_source_sha256: generator_source_sha256(),
        spec_sha256: sha256_hex(&spec_bytes()),
        before_manifest_sha256: sha256_hex(&before),
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {:04o} {}: {error}", mode, path.display()))
}

#[derive(Debug)]
struct ActualRecord {
    path: String,
    kind: &'static str,
    mode: u32,
    size: u64,
    content_sha256: Option<String>,
    symlink_target: Option<String>,
}

pub fn manifest_for_root(root: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(root)
        .map_err(|error| format!("stat fixture root {}: {error}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "fixture root is not a directory: {}",
            root.display()
        ));
    }
    let mut records = Vec::with_capacity(EXPECTED_ENTRY_COUNT + 1);
    collect_actual_records(root, Path::new(""), &mut records)?;
    records.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let mut output = String::with_capacity(records.len() * 190);
    for record in records {
        let content_sha256 = record
            .content_sha256
            .as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".to_owned());
        let symlink_target = record
            .symlink_target
            .as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".to_owned());
        writeln!(
            output,
            "{{\"path\":{},\"kind\":{},\"mode\":\"{:04o}\",\"size\":{},\"content_sha256\":{},\"symlink_target\":{}}}",
            json_string(&record.path),
            json_string(record.kind),
            record.mode,
            record.size,
            content_sha256,
            symlink_target,
        )
        .expect("write to String cannot fail");
    }
    Ok(output.into_bytes())
}

fn collect_actual_records(
    root: &Path,
    relative: &Path,
    records: &mut Vec<ActualRecord>,
) -> Result<(), String> {
    let directory = root.join(relative);
    let read_dir = fs::read_dir(&directory)
        .map_err(|error| format!("read fixture directory {}: {error}", directory.display()))?;
    for result in read_dir {
        let entry =
            result.map_err(|error| format!("read entry in {}: {error}", directory.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("non-UTF-8 entry in {}", directory.display()))?;
        let child_relative = relative.join(&name);
        if child_relative == Path::new(".git") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("lstat {}: {error}", path.display()))?;
        let relative_string = child_relative
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 relative path: {}", child_relative.display()))?
            .replace('\\', "/");
        let mode = metadata.mode() & 0o7777;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .map_err(|error| format!("read symlink {}: {error}", path.display()))?;
            let target = target
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 symlink target at {}", path.display()))?
                .to_owned();
            records.push(ActualRecord {
                path: relative_string,
                kind: "symlink",
                mode,
                size: metadata.len(),
                content_sha256: None,
                symlink_target: Some(target),
            });
        } else if metadata.is_dir() {
            records.push(ActualRecord {
                path: relative_string,
                kind: "directory",
                mode,
                size: 0,
                content_sha256: None,
                symlink_target: None,
            });
            collect_actual_records(root, &child_relative, records)?;
        } else if metadata.is_file() {
            let file = File::open(&path)
                .map_err(|error| format!("open fixture file {}: {error}", path.display()))?;
            let mut reader = BufReader::new(file);
            records.push(ActualRecord {
                path: relative_string,
                kind: "file",
                mode,
                size: metadata.len(),
                content_sha256: Some(sha256_reader(&mut reader)?),
                symlink_target: None,
            });
        } else {
            return Err(format!(
                "unsupported fixture entry type: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub fn verify_fixture(root: &Path, after_run: Option<u8>) -> Result<(), String> {
    let actual = manifest_for_root(root)?;
    let expected = match after_run {
        Some(run) => expected_after_manifest(run)?,
        None => expected_before_manifest(),
    };
    compare_bytes("fixture manifest", &actual, &expected)
}

fn compare_bytes(label: &str, actual: &[u8], expected: &[u8]) -> Result<(), String> {
    if actual == expected {
        return Ok(());
    }
    let first_difference = actual
        .iter()
        .zip(expected)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| actual.len().min(expected.len()));
    Err(format!(
        "{label} mismatch at byte {first_difference}: actual length {}, expected length {}, actual sha256 {}, expected sha256 {}",
        actual.len(),
        expected.len(),
        sha256_hex(actual),
        sha256_hex(expected)
    ))
}

pub fn write_oracles(repo: &Path) -> Result<(), String> {
    let outputs = [
        (SPEC_RELATIVE_PATH, spec_bytes()),
        (BEFORE_MANIFEST_RELATIVE_PATH, expected_before_manifest()),
        (AFTER_MANIFEST_RELATIVE_PATH, expected_after_manifest(1)?),
    ];
    for (relative, bytes) in outputs {
        let path = repo.join(relative);
        fs::write(&path, bytes)
            .map_err(|error| format!("write oracle {}: {error}", path.display()))?;
    }
    Ok(())
}

pub fn verify_oracles(repo: &Path) -> Result<(), String> {
    for (relative, expected) in [
        (SPEC_RELATIVE_PATH, spec_bytes()),
        (BEFORE_MANIFEST_RELATIVE_PATH, expected_before_manifest()),
        (AFTER_MANIFEST_RELATIVE_PATH, expected_after_manifest(1)?),
    ] {
        let path = repo.join(relative);
        let actual =
            fs::read(&path).map_err(|error| format!("read oracle {}: {error}", path.display()))?;
        compare_bytes(relative, &actual, &expected)?;
    }
    verify_poc_test_ids(&repo.join(POC_TEST_IDS_RELATIVE_PATH))
}

fn verify_poc_test_ids(path: &Path) -> Result<(), String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("read PoC test IDs {}: {error}", path.display()))?;
    if !contents.ends_with('\n') {
        return Err(format!("PoC test IDs must end with LF: {}", path.display()));
    }
    let ids = contents.lines().collect::<Vec<_>>();
    if ids.len() != 94 {
        return Err(format!("expected 94 PoC test IDs, found {}", ids.len()));
    }
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != ids.len() {
        return Err("PoC test IDs contain duplicates".to_owned());
    }
    if ids.iter().any(|id| id.is_empty() || id.ends_with(": test")) {
        return Err("PoC test IDs must be nonempty names without cargo list suffixes".to_owned());
    }
    Ok(())
}

pub fn oracle_hashes_json(repo: &Path) -> Result<String, String> {
    verify_oracles(repo)?;
    let spec = fs::read(repo.join(SPEC_RELATIVE_PATH))
        .map_err(|error| format!("read checked spec: {error}"))?;
    let before = fs::read(repo.join(BEFORE_MANIFEST_RELATIVE_PATH))
        .map_err(|error| format!("read checked before manifest: {error}"))?;
    let after = fs::read(repo.join(AFTER_MANIFEST_RELATIVE_PATH))
        .map_err(|error| format!("read checked after manifest: {error}"))?;
    Ok(format!(
        "{{\"generator_source_sha256\":\"{}\",\"spec_sha256\":\"{}\",\"before_manifest_sha256\":\"{}\",\"after_manifest_sha256\":\"{}\"}}",
        generator_source_sha256(),
        sha256_hex(&spec),
        sha256_hex(&before),
        sha256_hex(&after),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_standard_vectors_and_chunking() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let bytes = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&bytes),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        let mut hasher = Sha256::new();
        for chunk in bytes.chunks(17) {
            hasher.update(chunk);
        }
        assert_eq!(
            hex_digest(hasher.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn plan_has_exact_counts_payload_and_special_files() {
        let entries = before_entries();
        assert_eq!(entries.len(), EXPECTED_ENTRY_COUNT);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::Directory))
                .count(),
            EXPECTED_DIRECTORY_COUNT
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::File(_)))
                .count(),
            EXPECTED_REGULAR_FILE_COUNT
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::Symlink(_)))
                .count(),
            EXPECTED_SYMLINK_COUNT
        );
        assert!(entries.windows(2).all(|pair| pair[0].path < pair[1].path));
        let unique_paths = entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(unique_paths.len(), entries.len());

        let text_bytes = entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::File(content) if content.is_utf8_text() => Some(content.len() as u64),
                _ => None,
            })
            .sum::<u64>();
        assert_eq!(text_bytes, EXPECTED_UTF8_TEXT_BYTES);

        let content = |path: &str| {
            let entry = entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("missing {path}"));
            let EntryKind::File(content) = &entry.kind else {
                panic!("{path} is not a file");
            };
            content.bytes(path)
        };
        assert!(content(EDIT_A_PATH).starts_with(&[0xef, 0xbb, 0xbf]));
        let edit_b = content(EDIT_B_PATH);
        assert_eq!(edit_b.iter().filter(|byte| **byte == b'\n').count(), 2);
        assert_eq!(edit_b.windows(2).filter(|pair| *pair == b"\r\n").count(), 2);
        assert!(!content(EDIT_C_PATH).ends_with(b"\n"));
        assert!(content("tests/binary-with-nul.dat").contains(&0));
        assert!(
            String::from_utf8(content(READY_PATH))
                .expect("README is UTF-8")
                .contains(READY_SENTINEL)
        );

        let large = content("bench/large-100000-lines.txt");
        assert_eq!(
            large.iter().filter(|byte| **byte == b'\n').count(),
            LARGE_LF_COUNT
        );
        assert_eq!(
            large.split(|byte| *byte == b'\n').count(),
            LARGE_LOGICAL_LINES
        );
        assert!(!large.ends_with(b"\n"));
    }

    #[test]
    fn workflow_preserves_required_byte_properties() {
        for run in 1..=WORKFLOW_RUNS {
            let edit_a = expected_workflow_file(EDIT_A_PATH, run).expect("edit A");
            assert!(edit_a.starts_with(&[0xef, 0xbb, 0xbf]));
            assert!(
                edit_a
                    .windows(16)
                    .any(|window| window == format!("ALPHA1_EDIT_A_{run:02}").as_bytes())
            );
            let edit_b = expected_workflow_file(EDIT_B_PATH, run).expect("edit B");
            assert_eq!(edit_b.windows(2).filter(|pair| *pair == b"\r\n").count(), 2);
            assert_eq!(edit_b.iter().filter(|byte| **byte == b'\n').count(), 2);
            let edit_c = expected_workflow_file(EDIT_C_PATH, run).expect("edit C");
            assert!(!edit_c.ends_with(b"\n"));
            let edit_d = expected_workflow_file(EDIT_D_PATH, run).expect("edit D");
            assert!(
                std::str::from_utf8(&edit_d)
                    .expect("edit D UTF-8")
                    .contains("日本語")
            );
            for (index, bytes) in [&edit_a, &edit_b, &edit_c, &edit_d].into_iter().enumerate() {
                let input_id = format!("A3_{run:02}_{:04}", index + 1);
                assert!(
                    bytes
                        .windows(input_id.len())
                        .any(|window| window == input_id.as_bytes())
                );
            }
        }
    }

    #[test]
    fn required_case_ids_are_exactly_186_unique_ids() {
        let ids = required_case_ids();
        assert_eq!(ids.len(), 186);
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
        assert_eq!(ids.first().map(String::as_str), Some("A1_ROOT_IDENTITY"));
        assert_eq!(ids.last().map(String::as_str), Some("A5_TSTP_CONT_20"));
    }

    #[test]
    fn checked_in_oracles_equal_generator_output() {
        assert_eq!(CHECKED_SPEC, spec_bytes());
        assert_eq!(CHECKED_BEFORE_MANIFEST, expected_before_manifest());
        assert_eq!(
            CHECKED_AFTER_MANIFEST,
            expected_after_manifest(1).expect("workflow run 1")
        );
        assert_eq!(CHECKED_POC_TEST_IDS.lines().count(), 94);
    }
}
