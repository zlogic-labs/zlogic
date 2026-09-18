#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Reasoning(String),
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

#[derive(Debug, Default)]
pub struct ThinkTagSplitter {
    pending: String,
    inside: bool,
}

impl ThinkTagSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &str) -> Vec<Segment> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();

        loop {
            let (needle, other) = if self.inside {
                (CLOSE, OPEN)
            } else {
                (OPEN, CLOSE)
            };

            if let Some(pos) = self.pending.find(needle) {
                let head = self.pending[..pos].to_string();
                self.emit(&head, &mut out);
                self.pending = self.pending[pos + needle.len()..].to_string();
                self.inside = !self.inside;
                continue;
            }

            let keep = partial_tag_suffix_len(&self.pending, needle)
                .max(partial_tag_suffix_len(&self.pending, other));
            let flush_to = self.pending.len() - keep;
            if flush_to > 0 {
                let head = self.pending[..flush_to].to_string();
                self.emit(&head, &mut out);
                self.pending = self.pending[flush_to..].to_string();
            }
            break;
        }

        out
    }

    pub fn finish(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        let tail = std::mem::take(&mut self.pending);
        self.emit(&tail, &mut out);
        out
    }

    fn emit(&self, s: &str, out: &mut Vec<Segment>) {
        if s.is_empty() {
            return;
        }
        out.push(if self.inside {
            Segment::Reasoning(s.to_string())
        } else {
            Segment::Text(s.to_string())
        });
    }
}

fn partial_tag_suffix_len(s: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(s.len());
    for len in (1..=max).rev() {
        let start = s.len() - len;
        if !s.is_char_boundary(start) {
            continue;
        }
        if tag.as_bytes().starts_with(&s.as_bytes()[start..]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> Vec<Segment> {
        let mut s = ThinkTagSplitter::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.push(c));
        }
        out.extend(s.finish());
        out
    }

    fn run_all_splits(input: &str) -> Vec<Segment> {
        let whole = run(&[input]);
        for size in [1, 2, 3, 5, 8] {
            let chunks: Vec<String> = input
                .chars()
                .collect::<Vec<_>>()
                .chunks(size)
                .map(|c| c.iter().collect())
                .collect();
            let refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
            let got = merge(run(&refs));
            assert_eq!(
                got,
                merge(whole.clone()),
                "chunk size {size} produced a different result"
            );
        }
        merge(whole)
    }

    fn merge(segs: Vec<Segment>) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::new();
        for s in segs {
            match (out.last_mut(), &s) {
                (Some(Segment::Text(a)), Segment::Text(b)) => a.push_str(b),
                (Some(Segment::Reasoning(a)), Segment::Reasoning(b)) => a.push_str(b),
                _ => out.push(s),
            }
        }
        out
    }

    #[test]
    fn splits_a_single_block() {
        let got = run_all_splits("before<think>cot</think>after");
        assert_eq!(
            got,
            vec![
                Segment::Text("before".into()),
                Segment::Reasoning("cot".into()),
                Segment::Text("after".into()),
            ]
        );
    }

    #[test]
    fn close_tag_split_across_chunks() {
        let got = merge(run(&["<think>abc</thi", "nk>tail"]));
        assert_eq!(
            got,
            vec![
                Segment::Reasoning("abc".into()),
                Segment::Text("tail".into())
            ]
        );
    }

    #[test]
    fn never_flushes_a_partial_tag_as_visible_text() {
        let mut s = ThinkTagSplitter::new();
        let out = s.push("hello</thi");
        assert_eq!(out, vec![Segment::Text("hello".into())]);
    }

    #[test]
    fn multiple_blocks() {
        let got = run_all_splits("a<think>x</think>b<think>y</think>c");
        assert_eq!(
            got,
            vec![
                Segment::Text("a".into()),
                Segment::Reasoning("x".into()),
                Segment::Text("b".into()),
                Segment::Reasoning("y".into()),
                Segment::Text("c".into()),
            ]
        );
    }

    #[test]
    fn unclosed_think_stays_in_reasoning() {
        let got = run_all_splits("visible<think>dangling");
        assert_eq!(
            got,
            vec![
                Segment::Text("visible".into()),
                Segment::Reasoning("dangling".into())
            ]
        );
    }

    #[test]
    fn no_tags_at_all_is_pure_text() {
        let got = run_all_splits("just a normal reply < 5 and > 3");
        assert_eq!(
            got,
            vec![Segment::Text("just a normal reply < 5 and > 3".into())]
        );
    }

    #[test]
    fn multibyte_is_not_split_mid_char() {
        let got = run_all_splits("中文<think>思考中</think>结论");
        assert_eq!(
            got,
            vec![
                Segment::Text("中文".into()),
                Segment::Reasoning("思考中".into()),
                Segment::Text("结论".into()),
            ]
        );
    }
}
