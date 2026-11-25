// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Exact-size JSON and fragmentation test utilities for boundary qualification.

use serde_json::json;

pub(crate) const GUARDED_CAP: usize = 1_048_576; // 1 MiB.
pub(crate) const UNGUARDED_CAP: usize = 33_554_432; // 32 MiB.
pub(crate) const CHUNK_BYTES: usize = 16_384; // 16 KiB.
pub(crate) const SEEDS: [&str; 3] = ["ascii", "café ☕", "quote\"slash\\newline\n"];

#[derive(Clone, Copy, Debug)]
pub(crate) enum Phase {
    Input,
    Output,
}

impl Phase {
    pub(crate) fn document(self, content: &str) -> serde_json::Value {
        match self {
            Self::Input => json!({"model":"test","messages":[{"role":"user","content":content}],"stream":false}),
            Self::Output => json!({"choices":[{"message":{"role":"assistant","content":content}}]}),
        }
    }

    pub(crate) fn selected(self) -> (bool, bool) {
        match self {
            Self::Input => (true, false),
            Self::Output => (false, true),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Framing {
    Single,
    Chunks,
    Splits,
}

pub(crate) fn exact_document(phase: Phase, target: usize, seed: &str) -> (Vec<u8>, String) {
    let overhead = serde_json::to_vec(&phase.document("")).unwrap().len();
    let seed_bytes = serde_json::to_vec(&phase.document(seed)).unwrap().len() - overhead;
    let available = target.checked_sub(overhead).expect("target holds the JSON envelope");
    let mut content = seed.repeat(available / seed_bytes);
    content.push_str(&"a".repeat(available % seed_bytes));
    let bytes = serde_json::to_vec(&phase.document(&content)).unwrap();
    assert_eq!(bytes.len(), target, "serialized JSON must hit the byte boundary");
    (bytes, content)
}

pub(crate) fn fragments(bytes: &[u8], framing: Framing) -> Vec<&[u8]> {
    match framing {
        Framing::Single => vec![bytes],
        Framing::Chunks => bytes.chunks(CHUNK_BYTES).collect(),
        Framing::Splits => {
            let mut cuts = vec![0, bytes.len()];
            if let Some(index) = bytes.windows(3).position(|part| part == "☕".as_bytes()) {
                cuts.extend([index + 1, index + 2]);
            }
            for escape in [br#"\""#, br"\\", br"\n"] {
                if let Some(index) = bytes.windows(2).position(|part| part == escape) {
                    cuts.push(index + 1);
                }
            }
            cuts.sort_unstable();
            cuts.dedup();
            cuts.windows(2).map(|pair| &bytes[pair[0]..pair[1]]).collect()
        },
    }
}
