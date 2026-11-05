//! Incremental body charset conversion after transfer decoding. Candidate
//! selection and the shared encoded-size reservation remain with the caller.
use encoding_rs::{CoderResult, Decoder, Encoding};

pub(super) const OUTPUT_CHUNK: usize = 64 * 1024;
const MAX_UTF8_BYTES: usize = 32 * 1024 * 1024;
const PROBE_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DecodeError {
    UnsupportedCharset,
    RawFallbackUtf7,
    Utf8Limit { limit: usize },
    UnexpectedInput,
    InvalidOutputBuffer,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StepStatus {
    NeedInput,
    NeedOutput,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Step {
    pub consumed: usize,
    pub produced: usize,
    pub status: StepStatus,
}

pub(super) struct CharsetDecoder {
    decoder: Option<Decoder>,
    utf8_bytes: usize,
    limit: usize,
    eof_started: bool,
    eof_input_exhausted: bool,
    closed: bool,
    pending: [u8; PROBE_BYTES],
    pending_len: usize,
    pending_cursor: usize,
    pending_status: StepStatus,
}

impl CharsetDecoder {
    /// `encoded_nonempty` describes the original MIME transfer-encoded body,
    /// before any Base64/QP whitespace or incomplete escape can disappear.
    /// Empty entities preserve the existing empty-body behavior for every label.
    /// UTF-7 retains its existing decoder through the caller's bounded raw path.
    pub fn new(
        label: &[u8],
        encoded_nonempty: bool,
        utf8_limit: usize,
    ) -> Result<Self, DecodeError> {
        let decoder = if encoded_nonempty {
            let charset = charset::Charset::for_label_no_replacement(label)
                .ok_or(DecodeError::UnsupportedCharset)?;
            if charset == charset::UTF_7 {
                return Err(DecodeError::RawFallbackUtf7);
            }
            // Charset normalizes GBK/gb2312 aliases to GB18030. Its canonical
            // name also preserves the mail reader's ASCII/windows-1252 mapping.
            let encoding = Encoding::for_label_no_replacement(charset.name().as_bytes())
                .ok_or(DecodeError::UnsupportedCharset)?;
            Some(encoding.new_decoder())
        } else {
            None
        };
        Ok(Self {
            decoder,
            utf8_bytes: 0,
            limit: utf8_limit.min(MAX_UTF8_BYTES),
            eof_started: false,
            eof_input_exhausted: false,
            closed: false,
            pending: [0; PROBE_BYTES],
            pending_len: 0,
            pending_cursor: 0,
            pending_status: StepStatus::NeedInput,
        })
    }

    /// Output must contain 4..=64 KiB; every successful prefix is complete UTF-8.
    /// NeedOutput retains the unconsumed input suffix even if the input was
    /// exhausted or the output prefix is shorter than the buffer. After eof=true,
    /// keep eof=true while draining until Finished. Call once with empty input
    /// and eof=true after the transfer decoder has finished its own final flush.
    /// Errors close the decoder; callers discard the incomplete body.
    pub fn step(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        eof: bool,
    ) -> Result<Step, DecodeError> {
        if self.closed {
            return Err(DecodeError::Closed);
        }
        let result = self.step_inner(input, output, eof);
        if result.is_err() {
            self.closed = true;
        }
        result
    }

    fn step_inner(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        eof: bool,
    ) -> Result<Step, DecodeError> {
        if (self.eof_started && !eof) || (self.eof_input_exhausted && !input.is_empty()) {
            return Err(DecodeError::Closed);
        }
        if !(4..=OUTPUT_CHUNK).contains(&output.len()) {
            return Err(DecodeError::InvalidOutputBuffer);
        }
        if self.pending_len != 0 {
            return Ok(self.drain_pending(output, 0));
        }
        let Some(decoder) = self.decoder.as_mut() else {
            if !input.is_empty() {
                return Err(DecodeError::UnexpectedInput);
            }
            self.closed = eof;
            return Ok(Step {
                consumed: 0,
                produced: 0,
                status: if eof {
                    StepStatus::Finished
                } else {
                    StepStatus::NeedInput
                },
            });
        };
        self.eof_started |= eof;
        let remaining = self.limit - self.utf8_bytes;
        let available = remaining.min(output.len());
        let pending_bound = decoder
            .max_utf8_buffer_length(0)
            .ok_or(DecodeError::InvalidOutputBuffer)?;
        // The pinned codec can replay two pending non-BOM bytes at EOF. Its
        // documented four-byte public minimum is insufficient for that path.
        // Use a fixed small buffer and the official state-dependent output
        // bound, including pending bytes, before asking it to decode a prefix.
        let private = available < PROBE_BYTES || available < pending_bound;
        let count = if private {
            let mut count = input.len().min(8);
            while decoder
                .max_utf8_buffer_length(count)
                .is_none_or(|needed| needed > PROBE_BYTES)
            {
                count = count
                    .checked_sub(1)
                    .ok_or(DecodeError::InvalidOutputBuffer)?;
            }
            if count == 0 && !input.is_empty() {
                return Err(DecodeError::InvalidOutputBuffer);
            }
            count
        } else {
            input.len()
        };
        let destination = if private {
            self.pending.as_mut_slice()
        } else {
            &mut output[..available]
        };
        let last = eof && count == input.len();
        let (result, consumed, produced, _) =
            decoder.decode_to_utf8(&input[..count], destination, last);
        self.utf8_bytes = self
            .utf8_bytes
            .checked_add(produced)
            .filter(|total| *total <= self.limit)
            .ok_or(DecodeError::Utf8Limit { limit: self.limit })?;
        self.eof_input_exhausted |= eof && consumed == input.len();
        let status = match result {
            CoderResult::InputEmpty if last => StepStatus::Finished,
            CoderResult::InputEmpty if consumed < input.len() => StepStatus::NeedOutput,
            CoderResult::InputEmpty => StepStatus::NeedInput,
            CoderResult::OutputFull => StepStatus::NeedOutput,
        };
        if private {
            self.pending_len = produced;
            self.pending_cursor = 0;
            self.pending_status = status;
            return Ok(self.drain_pending(output, consumed));
        }
        self.closed = status == StepStatus::Finished;
        Ok(Step {
            consumed,
            produced,
            status,
        })
    }

    fn drain_pending(&mut self, output: &mut [u8], consumed: usize) -> Step {
        let start = self.pending_cursor;
        let mut end = (start + output.len()).min(self.pending_len);
        while end < self.pending_len && self.pending[end] & 0xc0 == 0x80 {
            end -= 1;
        }
        let produced = end - start;
        output[..produced].copy_from_slice(&self.pending[start..end]);
        self.pending_cursor = end;
        let status = if end < self.pending_len {
            StepStatus::NeedOutput
        } else {
            self.pending_len = 0;
            self.pending_cursor = 0;
            self.pending_status
        };
        self.closed = status == StepStatus::Finished;
        Step {
            consumed,
            produced,
            status,
        }
    }
}

#[cfg(test)]
#[path = "charset_decode/tests.rs"]
mod tests;
