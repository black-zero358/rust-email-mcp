use super::*;
fn oracle(label: &str, input: &[u8]) -> Result<String, DecodeError> {
    if !input.is_empty() && charset::Charset::for_label_no_replacement(label.as_bytes()).is_none() {
        return Err(DecodeError::UnsupportedCharset);
    }
    let ctype = mailparse::parse_content_type(&format!("text/plain; charset={label}"));
    match mailparse::body::Body::new(input, &ctype, &Some("binary".into())) {
        mailparse::body::Body::Binary(body) => Ok(body.get_as_string().unwrap()),
        _ => unreachable!(),
    }
}
fn incremental(
    label: &str,
    input: &[u8],
    width: usize,
    output_width: usize,
) -> Result<String, DecodeError> {
    let result = incremental_mode(label, input, width, output_width, false, usize::MAX);
    assert_eq!(
        incremental_mode(label, input, width, output_width, true, usize::MAX),
        result,
        "empty final flush: label={label}, input={input:?}, width={width}, output={output_width}"
    );
    result
}

fn incremental_mode(
    label: &str,
    input: &[u8],
    width: usize,
    output_width: usize,
    final_empty: bool,
    utf8_limit: usize,
) -> Result<String, DecodeError> {
    if input.is_empty() {
        return Ok(String::new());
    }
    let mut decoder = CharsetDecoder::new(label.as_bytes(), true, utf8_limit)?;
    let mut output = vec![0; output_width];
    let mut result = Vec::new();
    let chunks = input
        .chunks(width)
        .enumerate()
        .map(|(n, chunk)| (chunk, !final_empty && (n + 1) * width >= input.len()));
    for (chunk, eof) in chunks.chain(final_empty.then_some((b"".as_slice(), true))) {
        let mut consumed = 0;
        loop {
            output.fill(0x7f);
            let step = match decoder.step(&chunk[consumed..], &mut output, eof) {
                Ok(step) => step,
                Err(error) => {
                    if matches!(error, DecodeError::Utf8Limit { .. }) {
                        assert!(output.iter().all(|byte| *byte == 0x7f));
                    }
                    return Err(error);
                }
            };
            consumed += step.consumed;
            assert_eq!(
                step.status == StepStatus::Finished,
                eof && consumed == chunk.len() && step.status != StepStatus::NeedOutput
            );
            assert!(std::str::from_utf8(&output[..step.produced]).is_ok());
            result.extend_from_slice(&output[..step.produced]);
            if matches!(step.status, StepStatus::Finished | StepStatus::NeedInput) {
                break;
            }
            assert!(step.consumed != 0 || step.produced != 0, "nonprogress");
        }
    }
    Ok(String::from_utf8(result).unwrap())
}
fn check(label: &str, input: &[u8]) {
    let expected = oracle(label, input);
    for width in [1, 2, 3, 4, 7, 65535, 65536, 65537] {
        for output_width in [4, 5, 7, 65536] {
            assert_eq!(
                incremental(label, input, width, output_width),
                expected,
                "label={label} input={input:?} width={width} output={output_width}"
            );
        }
    }
}
#[test]
fn multibyte_and_stateful_encodings_match_whole_body() {
    for (label, input) in [
        ("utf-8", "A中文€🦀Z".as_bytes()),
        ("utf-16le", b"A\0\x2d\x4e\x3d\xd8\x00\xdeZ\0".as_slice()),
        ("utf-16be", b"\0A\x4e\x2d\xd8\x3d\xde\x00\0Z".as_slice()),
        ("gb2312", b"\x81\x30\x81\x30\xd6\xd0\xce\xc4".as_slice()),
        ("shift_jis", b"A\x93\xfa\x96\x7bZ".as_slice()),
        ("iso-2022-jp", b"A\x1b$B\x46\x7c\x4b\x5c\x1b(BZ".as_slice()),
        ("euc-kr", b"A\xc7\xd1\xb1\xdbZ".as_slice()),
        ("big5", b"A\xa4\xa4\xa4\xe5Z".as_slice()),
        ("us-ascii", b"A\x80\xffZ".as_slice()),
        ("iso-8859-1", b"A\x80\xffZ".as_slice()),
    ] {
        check(label, input);
    }
}
#[test]
fn bom_overrides_even_mismatched_declared_encoding_at_every_split() {
    for label in [
        "utf-8",
        "utf-16le",
        "utf-16be",
        "windows-1252",
        "gbk",
        "iso-2022-jp",
    ] {
        for input in [
            b"\xef\xbb\xbfabc".as_slice(),
            b"\xff\xfeA\0B\0".as_slice(),
            b"\xfe\xff\0A\0B".as_slice(),
            b"\xef\xbb\xbf\xef\xbb\xbfZ".as_slice(),
        ] {
            check(label, input);
        }
    }
}
#[test]
fn malformed_sequences_and_every_truncated_tail_match_replacement_policy() {
    for label in [
        "utf-8",
        "utf-16le",
        "utf-16be",
        "shift_jis",
        "gb18030",
        "iso-2022-jp",
        "us-ascii",
    ] {
        for input in [
            b"\xe4\xb8\xad\xf0\x9f\xa6\x80".as_slice(),
            b"\xff\xc0\x80\xe2(\xa1".as_slice(),
            b"\x1b$B\x46\x7c\x1b(B".as_slice(),
            b"\xef\xbb\xbf\xff".as_slice(),
        ] {
            for end in 0..=input.len() {
                check(label, &input[..end]);
            }
        }
    }
}
#[test]
fn unknown_labels_and_replacement_labels_keep_nonempty_rejection() {
    for label in ["x-unknown", "iso-2022-kr", "hz-gb-2312", "replacement"] {
        assert_eq!(oracle(label, b"abc"), Err(DecodeError::UnsupportedCharset));
        assert_eq!(
            CharsetDecoder::new(label.as_bytes(), true, 20).err(),
            Some(DecodeError::UnsupportedCharset)
        );
        assert_eq!(oracle(label, b""), Ok(String::new()));
    }
    assert_eq!(
        CharsetDecoder::new(b"utf-7", true, 20).err(),
        Some(DecodeError::RawFallbackUtf7)
    );
    assert_eq!(oracle("us-ascii", b"\x80").unwrap(), "€");
}
#[test]
fn deterministic_random_bytes_match_whole_body() {
    let mut state = 0x123456789abcdef_u64;
    let mut random = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for n in 0..1000 {
        let length = (random() % 97) as usize;
        let input = (0..length).map(|_| random() as u8).collect::<Vec<_>>();
        let label = [
            "utf-8",
            "utf-16le",
            "utf-16be",
            "gbk",
            "shift_jis",
            "iso-2022-jp",
            "windows-1252",
        ][n % 7];
        assert_eq!(
            incremental(label, &input, 7, 7),
            oracle(label, &input),
            "{label} {input:?}"
        );
    }
}
#[test]
fn real_64k_boundary_holds_incomplete_multibyte_or_escape_state() {
    for (label, tail) in [
        ("utf-8", "中文🦀".as_bytes()),
        ("shift_jis", b"\x93\xfa\x96\x7b".as_slice()),
        ("iso-2022-jp", b"\x1b$B\x46\x7c\x1b(B".as_slice()),
    ] {
        for before in 65533..=65538 {
            let mut input = vec![b'A'; before];
            input.extend_from_slice(tail);
            input.push(b'Z');
            assert_eq!(
                incremental(label, &input, 65536, 65536),
                oracle(label, &input)
            );
        }
    }
}
#[test]
fn limits_are_measured_in_utf8_and_failure_is_terminal() {
    let mut decoder = CharsetDecoder::new(b"windows-1252", true, 2).unwrap();
    let mut output = [0; 4];
    assert_eq!(
        decoder.step(b"\x80", &mut output, true).err(),
        Some(DecodeError::Utf8Limit { limit: 2 })
    );
    assert_eq!(
        decoder.step(b"", &mut output, true).err(),
        Some(DecodeError::Closed)
    );
    let mut decoder = CharsetDecoder::new(b"windows-1252", true, 3).unwrap();
    let step = decoder.step(b"\x80", &mut output, true).unwrap();
    assert_eq!(&output[..step.produced], "€".as_bytes());
}

#[test]
fn original_encoded_empty_entities_accept_all_labels_and_validate_input() {
    for label in [
        b"x-unknown".as_slice(),
        b"replacement",
        b"utf-7",
        b"utf-8",
        b"",
    ] {
        let mut decoder = CharsetDecoder::new(label, false, 0).unwrap();
        let mut output = [0x7f; 4];
        assert_eq!(
            decoder.step(b"", &mut output, false).unwrap(),
            Step {
                consumed: 0,
                produced: 0,
                status: StepStatus::NeedInput
            }
        );
        assert_eq!(
            decoder.step(b"", &mut output, true).unwrap().status,
            StepStatus::Finished
        );
        assert_eq!(output, [0x7f; 4]);
        assert_eq!(
            decoder.step(b"", &mut output, true),
            Err(DecodeError::Closed)
        );
        let mut invalid = CharsetDecoder::new(label, false, 10).unwrap();
        assert_eq!(
            invalid.step(b"x", &mut output, false),
            Err(DecodeError::UnexpectedInput)
        );
        assert_eq!(
            invalid.step(b"", &mut output, true),
            Err(DecodeError::Closed)
        );
    }
    // A nonempty encoded QP '=' can decode to empty bytes. Label validation
    // still happens before transfer decoding, preserving the old MIME rule.
    assert_eq!(
        CharsetDecoder::new(b"x-unknown", true, 10).err(),
        Some(DecodeError::UnsupportedCharset)
    );
}

#[test]
fn tiny_remaining_quotas_never_write_excess_utf8_to_output() {
    for (label, input, expected) in [
        (
            b"windows-1252".as_slice(),
            b"\x80".as_slice(),
            "€".as_bytes(),
        ),
        (b"utf-8".as_slice(), "🦀".as_bytes(), "🦀".as_bytes()),
        (b"utf-8".as_slice(), b"\xe4".as_slice(), "�".as_bytes()),
    ] {
        for final_empty in [false, true] {
            let label = std::str::from_utf8(label).unwrap();
            for limit in 0..expected.len() {
                assert_eq!(
                    incremental_mode(label, input, 1, 7, final_empty, limit),
                    Err(DecodeError::Utf8Limit { limit })
                );
            }
            assert_eq!(
                incremental_mode(label, input, 1, 7, final_empty, expected.len())
                    .unwrap()
                    .as_bytes(),
                expected
            );
        }
    }
}

#[test]
fn exact_cap_with_pending_eof_flush_and_iso2022jp_outputfull_preserves_tail() {
    let mut decoder = CharsetDecoder::new(b"iso-2022-jp", true, 4).unwrap();
    let mut output = [0; 4];
    let first = decoder.step(b"\x1b$", &mut output, true).unwrap();
    assert_eq!(
        first,
        Step {
            consumed: 2,
            produced: 4,
            status: StepStatus::Finished
        }
    );
    assert_eq!(&output[..first.produced], "�$".as_bytes());
    let mut capped = CharsetDecoder::new(b"utf-8", true, 1).unwrap();
    assert_eq!(
        capped.step(b"a", &mut output, false).unwrap().status,
        StepStatus::NeedInput
    );
    assert_eq!(
        capped.step(b"", &mut output, true).unwrap().status,
        StepStatus::Finished
    );
}

#[test]
fn eof_contract_and_output_limits_close_on_misuse() {
    for width in [0, 1, 2, 3, OUTPUT_CHUNK + 1] {
        let mut decoder = CharsetDecoder::new(b"utf-8", true, 10).unwrap();
        assert_eq!(
            decoder.step(b"x", &mut vec![0; width], false),
            Err(DecodeError::InvalidOutputBuffer)
        );
        assert_eq!(
            decoder.step(b"", &mut [0; 4], true),
            Err(DecodeError::Closed)
        );
    }
    for (input, eof) in [(b"extra".as_slice(), true), (b"".as_slice(), false)] {
        let mut decoder = CharsetDecoder::new(b"iso-2022-jp", true, 10).unwrap();
        assert_eq!(
            decoder.step(b"abcdefgh", &mut [0; 4], true).unwrap().status,
            StepStatus::NeedOutput
        );
        assert_eq!(
            decoder.step(input, &mut [0; 4], eof),
            Err(DecodeError::Closed)
        );
    }
}

#[test]
fn split_bom_candidate_in_iso2022jp_empty_eof_uses_bounded_replay() {
    let mut decoder = CharsetDecoder::new(b"iso-2022-jp", true, 6).unwrap();
    let mut output = [0; 4];
    for byte in [0xef, 0xbb] {
        assert_eq!(
            decoder.step(&[byte], &mut output, false).unwrap(),
            Step {
                consumed: 1,
                produced: 0,
                status: StepStatus::NeedInput
            }
        );
    }
    let first = decoder.step(b"", &mut output, true).unwrap();
    assert_eq!(
        first,
        Step {
            consumed: 0,
            produced: 3,
            status: StepStatus::NeedOutput
        }
    );
    assert_eq!(&output[..first.produced], "�".as_bytes());
    assert_eq!(decoder.utf8_bytes, 6);
    let last = decoder.step(b"", &mut output, true).unwrap();
    assert_eq!(
        last,
        Step {
            consumed: 0,
            produced: 3,
            status: StepStatus::Finished
        }
    );
    assert_eq!(&output[..last.produced], "�".as_bytes());
    for limit in 0..6 {
        let mut capped = CharsetDecoder::new(b"iso-2022-jp", true, limit).unwrap();
        capped.step(b"\xef\xbb", &mut output, false).unwrap();
        output.fill(0x7f);
        assert_eq!(
            capped.step(b"", &mut output, true),
            Err(DecodeError::Utf8Limit { limit })
        );
        assert_eq!(output, [0x7f; 4]);
        assert_eq!(
            capped.step(b"", &mut output, true),
            Err(DecodeError::Closed)
        );
    }
}

#[test]
fn utf8_exact_32m_and_next_byte_use_the_fixed_global_cap() {
    let mut decoder = CharsetDecoder::new(b"utf-8", true, usize::MAX).unwrap();
    assert_eq!(decoder.limit, MAX_UTF8_BYTES);
    let input = [b'x'; OUTPUT_CHUNK];
    let mut output = [0; OUTPUT_CHUNK];
    for _ in 0..512 {
        let step = decoder.step(&input, &mut output, false).unwrap();
        assert_eq!(step.consumed, OUTPUT_CHUNK);
        assert_eq!(step.produced, OUTPUT_CHUNK);
    }
    assert_eq!(decoder.utf8_bytes, MAX_UTF8_BYTES);
    output.fill(0x7f);
    assert_eq!(
        decoder.step(b"x", &mut output, false),
        Err(DecodeError::Utf8Limit {
            limit: MAX_UTF8_BYTES
        })
    );
    assert!(output.iter().all(|byte| *byte == 0x7f));
}

#[test]
fn every_supported_encoding_preserves_split_bom_candidates_and_final_tails() {
    for encoding in [
        encoding_rs::BIG5,
        encoding_rs::EUC_JP,
        encoding_rs::EUC_KR,
        encoding_rs::GBK,
        encoding_rs::IBM866,
        encoding_rs::ISO_2022_JP,
        encoding_rs::ISO_8859_10,
        encoding_rs::ISO_8859_13,
        encoding_rs::ISO_8859_14,
        encoding_rs::ISO_8859_15,
        encoding_rs::ISO_8859_16,
        encoding_rs::ISO_8859_2,
        encoding_rs::ISO_8859_3,
        encoding_rs::ISO_8859_4,
        encoding_rs::ISO_8859_5,
        encoding_rs::ISO_8859_6,
        encoding_rs::ISO_8859_7,
        encoding_rs::ISO_8859_8,
        encoding_rs::ISO_8859_8_I,
        encoding_rs::KOI8_R,
        encoding_rs::KOI8_U,
        encoding_rs::SHIFT_JIS,
        encoding_rs::UTF_16BE,
        encoding_rs::UTF_16LE,
        encoding_rs::UTF_8,
        encoding_rs::GB18030,
        encoding_rs::MACINTOSH,
        encoding_rs::WINDOWS_1250,
        encoding_rs::WINDOWS_1251,
        encoding_rs::WINDOWS_1252,
        encoding_rs::WINDOWS_1253,
        encoding_rs::WINDOWS_1254,
        encoding_rs::WINDOWS_1255,
        encoding_rs::WINDOWS_1256,
        encoding_rs::WINDOWS_1257,
        encoding_rs::WINDOWS_1258,
        encoding_rs::WINDOWS_874,
        encoding_rs::X_MAC_CYRILLIC,
        encoding_rs::X_USER_DEFINED,
    ] {
        let label = encoding.name();
        for prefix in [
            b"".as_slice(),
            b"\xef",
            b"\xef\xbb",
            b"\xef\xbb\xbf",
            b"\xff",
            b"\xfe",
            b"\xff\xfe",
            b"\xfe\xff",
        ] {
            for tail in [
                b"".as_slice(),
                b"A",
                b"\0",
                b"\xff",
                b"\x1b$",
                "中".as_bytes(),
                b"\x3d\xd8\x00\xde",
            ] {
                let bytes = [prefix, tail].concat();
                check(label, &bytes);
            }
        }
        let mut decoder = CharsetDecoder::new(label.as_bytes(), true, MAX_UTF8_BYTES).unwrap();
        let mut output = [0; 4];
        for byte in [0xef, 0xbb] {
            let step = decoder.step(&[byte], &mut output, false).unwrap();
            assert_eq!(step.consumed, 1);
            while decoder.pending_len != 0 {
                decoder.step(b"", &mut output, false).unwrap();
            }
            let codec = decoder.decoder.as_ref().unwrap();
            assert!(
                codec.max_utf8_buffer_length(8).unwrap() <= PROBE_BYTES,
                "{label}"
            );
        }
    }
}
