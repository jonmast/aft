use crate::compress::generic::{ceil_char_boundary, floor_char_boundary};

pub const FINAL_OUTPUT_CAP_BYTES: usize = 16 * 1024;
pub const FINAL_OUTPUT_HEAD_BYTES: usize = 6 * 1024;
pub const FINAL_OUTPUT_TAIL_BYTES: usize = 10 * 1024;

pub const RUNNING_OUTPUT_PREVIEW_BYTES: usize = 8 * 1024;
pub const COMPLETION_OUTPUT_PREVIEW_BYTES: usize = 4 * 1024;
pub const COMPLETION_OUTPUT_HEAD_BYTES: usize = 2 * 1024;
pub const COMPLETION_OUTPUT_TAIL_BYTES: usize = 2 * 1024;

pub const RAW_PASSTHROUGH_CAP_BYTES: usize = 50 * 1024;
pub const RAW_PASSTHROUGH_HEAD_BYTES: usize = 20 * 1024;
pub const RAW_PASSTHROUGH_TAIL_BYTES: usize = 30 * 1024;

pub const STRUCTURED_OUTPUT_CAP_BYTES: usize = 50 * 1024;

pub const COMPRESS_INPUT_CAP_BYTES: usize = 10 * 1024 * 1024;
pub const COMPRESS_INPUT_HEAD_BYTES: usize = 4 * 1024 * 1024;
pub const COMPRESS_INPUT_TAIL_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedText {
    pub text: String,
    pub truncated: bool,
}

pub fn cap_final_output(input: &str) -> CappedText {
    cap_head_tail(
        input,
        FINAL_OUTPUT_CAP_BYTES,
        FINAL_OUTPUT_HEAD_BYTES,
        FINAL_OUTPUT_TAIL_BYTES,
    )
}

pub fn cap_final_output_with_marker(input: &str, marker: &str) -> CappedText {
    cap_head_tail_with_marker(
        input,
        FINAL_OUTPUT_CAP_BYTES,
        FINAL_OUTPUT_HEAD_BYTES,
        FINAL_OUTPUT_TAIL_BYTES,
        marker,
    )
}

pub fn cap_completion_output(input: &str) -> CappedText {
    cap_head_tail(
        input,
        COMPLETION_OUTPUT_PREVIEW_BYTES,
        COMPLETION_OUTPUT_HEAD_BYTES,
        COMPLETION_OUTPUT_TAIL_BYTES,
    )
}

pub fn cap_completion_output_with_marker(input: &str, marker: &str) -> CappedText {
    cap_head_tail_with_marker(
        input,
        COMPLETION_OUTPUT_PREVIEW_BYTES,
        COMPLETION_OUTPUT_HEAD_BYTES,
        COMPLETION_OUTPUT_TAIL_BYTES,
        marker,
    )
}

pub fn cap_head_tail(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
) -> CappedText {
    if input.len() <= threshold_bytes {
        return CappedText {
            text: input.to_string(),
            truncated: false,
        };
    }

    let head_end = floor_char_boundary(input, keep_head.min(input.len()));
    let mut tail_start = ceil_char_boundary(input, input.len().saturating_sub(keep_tail));

    if head_end >= tail_start {
        return CappedText {
            text: input.to_string(),
            truncated: false,
        };
    }

    let marker_prefix_len = if head_end == 0 || input[..head_end].ends_with('\n') {
        0
    } else {
        1
    };
    loop {
        let truncated_bytes = tail_start - head_end;
        let marker_len = marker_prefix_len
            + "...<truncated ".len()
            + truncated_bytes.to_string().len()
            + " bytes>...\n".len();
        let max_tail = threshold_bytes.saturating_sub(head_end + marker_len);
        let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(max_tail));
        if adjusted_tail_start <= tail_start {
            break;
        }
        tail_start = adjusted_tail_start;
        if head_end >= tail_start {
            return CappedText {
                text: input.to_string(),
                truncated: false,
            };
        }
    }

    let truncated_bytes = tail_start - head_end;
    let mut output = String::with_capacity(threshold_bytes.min(input.len()));
    output.push_str(&input[..head_end]);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("...<truncated ");
    output.push_str(&truncated_bytes.to_string());
    output.push_str(" bytes>...\n");
    output.push_str(&input[tail_start..]);

    CappedText {
        text: output,
        truncated: true,
    }
}

pub fn cap_head_tail_with_marker(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
    marker: &str,
) -> CappedText {
    if input.len() <= threshold_bytes {
        return CappedText {
            text: input.to_string(),
            truncated: false,
        };
    }
    if marker.is_empty() {
        return cap_head_tail(input, threshold_bytes, keep_head, keep_tail);
    }

    let mut head_budget = keep_head.min(input.len());
    let mut tail_budget = keep_tail.min(input.len());

    for _ in 0..8 {
        let head_end = floor_char_boundary(input, head_budget);
        let tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
        if head_end >= tail_start {
            return CappedText {
                text: input.to_string(),
                truncated: false,
            };
        }

        let marker_prefix_len = if head_end == 0 || input[..head_end].ends_with('\n') {
            0
        } else {
            1
        };
        let marker_len = marker_prefix_len + marker.len() + 1;
        let available = threshold_bytes.saturating_sub(marker_len);
        let next_head = keep_head.min(available).min(input.len());
        let next_tail = keep_tail
            .min(available.saturating_sub(next_head))
            .min(input.len().saturating_sub(next_head));

        if next_head == head_budget && next_tail == tail_budget {
            break;
        }
        head_budget = next_head;
        tail_budget = next_tail;
    }

    let head_end = floor_char_boundary(input, head_budget);
    let tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    if head_end >= tail_start {
        return CappedText {
            text: input.to_string(),
            truncated: false,
        };
    }

    let mut output = String::with_capacity(threshold_bytes.min(input.len()).max(marker.len() + 1));
    output.push_str(&input[..head_end]);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(marker);
    output.push('\n');
    output.push_str(&input[tail_start..]);

    CappedText {
        text: output,
        truncated: true,
    }
}

pub fn json_output_pointer(total_bytes: u64, path: &str) -> String {
    let kb = total_bytes.div_ceil(1024);
    format!("[JSON output {kb} KB; full output at {path}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_cap_respects_utf8_boundaries() {
        let input = format!("{}{}", "🦀".repeat(4_000), "tail");
        let capped = cap_head_tail(&input, 128, 64, 64);
        assert!(capped.truncated);
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(capped.text.contains("...<truncated "));
        assert!(capped.text.len() <= 128);
        assert!(capped.text.ends_with("tail"));
    }
}
