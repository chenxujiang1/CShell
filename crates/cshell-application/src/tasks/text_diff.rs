//! Deterministic UTF-8 line alignment with bounded work. Line endings are significant.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
pub const MAX_TEXT_DIFF_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TEXT_DIFF_LINES: usize = 20_000;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextDiffKind {
    Equal,
    Delete,
    Insert,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextDiffRow {
    pub kind: TextDiffKind,
    pub left_line: Option<usize>,
    pub right_line: Option<usize>,
    pub left_offset: Option<u64>,
    pub right_offset: Option<u64>,
    pub text: String,
}
impl fmt::Debug for TextDiffRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextDiffRow")
            .field("kind", &self.kind)
            .field("body", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
pub fn align_text_output(left: &[u8], right: &[u8]) -> Result<Vec<TextDiffRow>, TaskError> {
    if left.len() > MAX_TEXT_DIFF_BYTES || right.len() > MAX_TEXT_DIFF_BYTES {
        return Err(TaskError::Limit);
    }
    let l = std::str::from_utf8(left).map_err(|_| TaskError::Invalid)?;
    let r = std::str::from_utf8(right).map_err(|_| TaskError::Invalid)?;
    fn lines(text: &str) -> Vec<(u64, &str)> {
        let mut offset = 0_u64;
        text.split_inclusive('\n')
            .map(|line| {
                let at = offset;
                offset += line.len() as u64;
                (at, line)
            })
            .collect::<Vec<_>>()
    }
    let l = lines(l);
    let r = lines(r);
    if l.len() > MAX_TEXT_DIFF_LINES || r.len() > MAX_TEXT_DIFF_LINES {
        return Err(TaskError::Limit);
    }
    if l.iter().chain(&r).any(|(_, line)| line.len() > 1024 * 1024) {
        return Err(TaskError::Limit);
    }
    let mut rows = Vec::new();
    fn row(
        kind: TextDiffKind,
        i: Option<usize>,
        j: Option<usize>,
        l: &[(u64, &str)],
        r: &[(u64, &str)],
    ) -> TextDiffRow {
        TextDiffRow {
            kind,
            left_line: i.map(|i| i + 1),
            right_line: j.map(|j| j + 1),
            left_offset: i.map(|i| l[i].0),
            right_offset: j.map(|j| r[j].0),
            text: i
                .map(|i| l[i].1)
                .unwrap_or_else(|| r[j.unwrap_or(0)].1)
                .to_owned(),
        }
    }
    // Unique common lines supply noncrossing anchors; bounded LCS handles small gaps.
    fn gap(
        a: std::ops::Range<usize>,
        b: std::ops::Range<usize>,
        l: &[(u64, &str)],
        r: &[(u64, &str)],
        out: &mut Vec<TextDiffRow>,
    ) {
        if (a.len() + 1).saturating_mul(b.len() + 1) <= 1_000_000 {
            let width = b.len() + 1;
            let mut dp = vec![0_u32; (a.len() + 1) * width];
            for i in (0..a.len()).rev() {
                for j in (0..b.len()).rev() {
                    dp[i * width + j] = if l[a.start + i].1 == r[b.start + j].1 {
                        1 + dp[(i + 1) * width + j + 1]
                    } else {
                        dp[(i + 1) * width + j].max(dp[i * width + j + 1])
                    };
                }
            }
            let (mut i, mut j) = (0, 0);
            while i < a.len() || j < b.len() {
                if i < a.len() && j < b.len() && l[a.start + i].1 == r[b.start + j].1 {
                    out.push(row(
                        TextDiffKind::Equal,
                        Some(a.start + i),
                        Some(b.start + j),
                        l,
                        r,
                    ));
                    i += 1;
                    j += 1;
                } else if i < a.len()
                    && (j == b.len() || dp[(i + 1) * width + j] >= dp[i * width + j + 1])
                {
                    out.push(row(TextDiffKind::Delete, Some(a.start + i), None, l, r));
                    i += 1;
                } else {
                    out.push(row(TextDiffKind::Insert, None, Some(b.start + j), l, r));
                    j += 1;
                }
            }
        } else {
            let (mut i, mut j) = (a.start, b.start);
            while i < a.end && j < b.end && l[i].1 == r[j].1 {
                out.push(row(TextDiffKind::Equal, Some(i), Some(j), l, r));
                i += 1;
                j += 1;
            }
            let mut tail = 0;
            while i + tail < a.end
                && j + tail < b.end
                && l[a.end - tail - 1].1 == r[b.end - tail - 1].1
            {
                tail += 1;
            }
            for k in i..a.end - tail {
                out.push(row(TextDiffKind::Delete, Some(k), None, l, r));
            }
            for k in j..b.end - tail {
                out.push(row(TextDiffKind::Insert, None, Some(k), l, r));
            }
            for k in 0..tail {
                out.push(row(
                    TextDiffKind::Equal,
                    Some(a.end - tail + k),
                    Some(b.end - tail + k),
                    l,
                    r,
                ));
            }
        }
    }
    let mut lm = BTreeMap::new();
    let mut ld = BTreeSet::new();
    let mut rm = BTreeMap::new();
    let mut rd = BTreeSet::new();
    for (i, (_, text)) in l.iter().enumerate() {
        if lm.insert(*text, i).is_some() {
            ld.insert(*text);
        }
    }
    for (i, (_, text)) in r.iter().enumerate() {
        if rm.insert(*text, i).is_some() {
            rd.insert(*text);
        }
    }
    let mut candidates = l
        .iter()
        .enumerate()
        .filter_map(|(i, (_, text))| {
            if ld.contains(text) || rd.contains(text) {
                None
            } else {
                rm.get(text).map(|j| (i, *j))
            }
        })
        .collect::<Vec<_>>();
    // Longest increasing subsequence of right positions, O(lines log lines).
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![None; candidates.len()];
    for k in 0..candidates.len() {
        let at = tails.partition_point(|index| candidates[*index].1 < candidates[k].1);
        if at > 0 {
            previous[k] = Some(tails[at - 1]);
        }
        if at == tails.len() {
            tails.push(k);
        } else {
            tails[at] = k;
        }
    }
    let mut anchors = Vec::new();
    let mut cursor = tails.last().copied();
    while let Some(k) = cursor {
        anchors.push(candidates[k]);
        cursor = previous[k];
    }
    anchors.reverse();
    candidates.clear();
    let (mut i, mut j) = (0, 0);
    for (a, b) in anchors {
        gap(i..a, j..b, &l, &r, &mut rows);
        rows.push(row(TextDiffKind::Equal, Some(a), Some(b), &l, &r));
        i = a + 1;
        j = b + 1;
    }
    gap(i..l.len(), j..r.len(), &l, &r, &mut rows);
    Ok(rows)
}
