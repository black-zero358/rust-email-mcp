use super::*;

fn oracle(kind: Encoding, input: &[u8]) -> Result<Vec<u8>, ()> {
    let ctype = mailparse::parse_content_type("application/octet-stream");
    let encoding = Some(
        match kind {
            Encoding::Base64 => "base64",
            Encoding::QuotedPrintable => "quoted-printable",
            Encoding::Identity => return Ok(input.to_vec()),
        }
        .into(),
    );
    match mailparse::body::Body::new(input, &ctype, &encoding) {
        mailparse::body::Body::Base64(body) | mailparse::body::Body::QuotedPrintable(body) => {
            body.get_decoded().map_err(|_| ())
        }
        _ => unreachable!(),
    }
}

fn pump(
    decoder: &mut Decoder,
    input: &[u8],
    eof: bool,
    width: usize,
) -> Result<Vec<u8>, DecodeError> {
    let mut buffer = vec![0; width];
    let mut offset = 0;
    let mut result = Vec::new();
    loop {
        let step = decoder.step(&input[offset..], &mut buffer, eof)?;
        assert!(step.consumed <= input.len() - offset);
        assert!(step.produced <= width);
        assert!(
            step.consumed != 0
                || step.produced != 0
                || step.status == StepStatus::Finished
                || (!eof && step.status == StepStatus::NeedInput)
        );
        offset += step.consumed;
        result.extend_from_slice(&buffer[..step.produced]);
        match step.status {
            StepStatus::NeedOutput => {}
            StepStatus::NeedInput => {
                assert!(!eof);
                assert_eq!(offset, input.len());
                return Ok(result);
            }
            StepStatus::Finished => {
                assert!(eof);
                assert_eq!(offset, input.len());
                return Ok(result);
            }
        }
    }
}

fn decode(
    kind: Encoding,
    input: &[u8],
    splits: &[usize],
    limit: usize,
) -> (Result<Vec<u8>, DecodeError>, Stats) {
    let mut decoder = Decoder::new(kind, limit);
    let mut result = Vec::new();
    let mut start = 0;
    for &end in splits.iter().chain(std::iter::once(&input.len())) {
        match pump(&mut decoder, &input[start..end], false, 7) {
            Ok(bytes) => result.extend(bytes),
            Err(error) => return (Err(error), decoder.stats),
        }
        start = end;
    }
    let final_result = pump(&mut decoder, b"", true, 7).map(|bytes| {
        result.extend(bytes);
        result
    });
    (final_result, decoder.stats)
}

fn check(kind: Encoding, input: &[u8], splits: &[usize]) {
    let expected = oracle(kind, input);
    let (actual, stats) = decode(kind, input, splits, MAX_DECODED_BYTES);
    assert_eq!(
        actual.as_ref().map_err(|_| ()),
        expected.as_ref().map_err(|_| ()),
        "{kind:?}: {input:?}, splits={splits:?}"
    );
    if let Ok(bytes) = actual {
        assert_eq!(stats.decoded_bytes, bytes.len());
        assert_eq!(stats.produced_bytes, bytes.len());
        assert_eq!(stats.encoded_bytes, input.len());
        assert!(stats.peak_pending_capacity <= MAX_DECODED_BYTES);
    }
}

fn b64(input: &[u8]) -> Vec<u8> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::new();
    for run in input.chunks(3) {
        let a = run[0];
        let b = run.get(1).copied().unwrap_or(0);
        let c = run.get(2).copied().unwrap_or(0);
        output.extend([
            TABLE[(a >> 2) as usize],
            TABLE[(((a & 3) << 4) | (b >> 4)) as usize],
            if run.len() > 1 {
                TABLE[(((b & 15) << 2) | (c >> 6)) as usize]
            } else {
                b'='
            },
            if run.len() > 2 {
                TABLE[(c & 63) as usize]
            } else {
                b'='
            },
        ]);
    }
    output
}
#[test]
fn base64_examples_every_split_and_noncanonical_groups() {
    for input in [
        b"".as_slice(),
        b"YQ==",
        b"YWI=",
        b"YWJj",
        b"YQ==Yg==",
        b"YQ==AAAA",
        b"Zh==",
        b"Zm9=",
        b"Z\r\nh \t=\x0c=",
        b"=",
        b"YQ",
        b"YQ=",
        b"YQ===",
        b"====",
        b"A===",
        b"Y=Q=",
        b"YQ==!",
        b"YW\x0bJj",
        b"YW\xffJj",
        b"YWJj\0",
        b"____",
        b"YW-J",
    ] {
        for cut in 0..=input.len() {
            check(Encoding::Base64, input, &[cut]);
        }
        let cuts = (0..=input.len()).collect::<Vec<_>>();
        check(Encoding::Base64, input, &cuts);
    }
}
#[test]
fn quoted_printable_examples_every_split() {
    for input in [
        b"".as_slice(),
        b"=",
        b"=A",
        b"=AG",
        b"==",
        b"===",
        b"====",
        b"=3d",
        b"=aF",
        b"a=\r\nb",
        b"a=\nb",
        b"a\rb",
        b"a\r",
        b"a \t\r\nb \t",
        b"a\n",
        b"a\n\n",
        b"\n\n",
        b"=\n",
        b"= \t\r\n",
        b"a \xff b",
        b"= =A",
        b"=3 \r\n",
        b"a=\rX",
        b"a=3\rX",
        b"\t \0\xff",
        b"=4\xff1",
        b"=0D=0A",
    ] {
        for cut in 0..=input.len() {
            check(Encoding::QuotedPrintable, input, &[cut]);
        }
        let cuts = (0..=input.len()).collect::<Vec<_>>();
        check(Encoding::QuotedPrintable, input, &cuts);
    }
}
#[test]
fn every_ascii_and_non_ascii_byte_at_escape_and_padding_boundaries() {
    for byte in 0..=255u8 {
        for kind in [Encoding::Base64, Encoding::QuotedPrintable] {
            for pattern in [
                vec![byte],
                vec![b'=', byte],
                vec![b'=', b'4', byte],
                vec![b'a', b' ', byte, b' ', b'\r', b'\n', b'b'],
                vec![b'Y', b'Q', byte, b'='],
            ] {
                for cut in 0..=pattern.len() {
                    check(kind, &pattern, &[cut]);
                }
            }
        }
    }
}
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
#[test]
fn random_bytes_and_random_chunks_match_the_raw_codecs() {
    let mut seed = 0x123456789abcdefu64;
    for n in 0..5000 {
        let len = (random(&mut seed) % 512) as usize;
        let bytes = (0..len)
            .map(|_| random(&mut seed) as u8)
            .collect::<Vec<_>>();
        let kind = if n % 2 == 0 {
            Encoding::Base64
        } else {
            Encoding::QuotedPrintable
        };
        let input = if kind == Encoding::Base64 && n % 4 == 0 {
            b64(&bytes)
        } else {
            bytes
        };
        let mut cuts = vec![0];
        while *cuts.last().unwrap() < input.len() {
            cuts.push(
                (cuts.last().unwrap() + (random(&mut seed) % 67) as usize + 1).min(input.len()),
            );
        }
        check(kind, &input, &cuts);
    }
}
#[test]
fn large_base64_and_qp_cross_the_64k_input_boundary() {
    let plain = (0..65536 + 13).map(|n| (n % 251) as u8).collect::<Vec<_>>();
    let encoded = b64(&plain);
    for cut in 65536 - 5..=65536 + 5 {
        check(Encoding::Base64, &encoded, &[cut]);
    }
    for width in [1, 2, 3, 4, 7, 65535, 65536, 65537] {
        let cuts = (width..encoded.len()).step_by(width).collect::<Vec<_>>();
        check(Encoding::Base64, &encoded, &cuts);
    }
    let mut encoded = vec![b'x'; 65536 - 2];
    encoded.extend(b"=4\r\n1 =20\r\n=0D=0Aend \t\r\n");
    for cut in 65536 - 6..=65536 + 12 {
        check(Encoding::QuotedPrintable, &encoded, &[cut]);
    }
}
#[test]
fn every_truncated_tail_and_random_chunking_match_success_or_failure() {
    let b = b64(b"raw synthetic attachment\xff\x00 bytes");
    let q = b"raw=20synthetic=0D=0Aattachment=FF=00=\r\n bytes=3";
    for (kind, input) in [
        (Encoding::Base64, b.as_slice()),
        (Encoding::QuotedPrintable, q.as_slice()),
    ] {
        for end in 0..=input.len() {
            for width in [1, 2, 7, 64] {
                let cuts = (width..end).step_by(width).collect::<Vec<_>>();
                check(kind, &input[..end], &cuts);
            }
        }
    }
}
#[test]
fn decoded_limit_is_charged_before_any_sink_receives_excess() {
    for (kind, input) in [
        (Encoding::Base64, b64(b"abc")),
        (Encoding::QuotedPrintable, b"=61=62=63".to_vec()),
    ] {
        for limit in 0..=3 {
            let (result, stats) = decode(kind, &input, &[1, 2], limit);
            if limit < 3 {
                assert_eq!(result.unwrap_err(), DecodeError::DecodedLimit { limit });
                assert!(stats.decoded_bytes <= limit && stats.produced_bytes <= limit);
            } else {
                assert_eq!(result.unwrap(), b"abc");
            }
        }
    }
    let mut decoder = Decoder::new(Encoding::Base64, usize::MAX);
    assert_eq!(decoder.limit, MAX_DECODED_BYTES);
    pump(&mut decoder, b"", true, OUTPUT_CHUNK).unwrap();
}
#[test]
fn pending_whitespace_over_quota_is_discarded_only_when_it_is_trailing() {
    let input = [b"a".as_slice(), &[b' '; 1000], b"\r\n"].concat();
    let (result, stats) = decode(Encoding::QuotedPrintable, &input, &[7, 100], 3);
    assert_eq!(result.unwrap(), b"a\r\n");
    assert!(stats.peak_pending_capacity <= 3);
    let input = [b"a".as_slice(), &[b' '; 1000]].concat();
    let (result, stats) = decode(Encoding::QuotedPrintable, &input, &[100], 1);
    assert_eq!(result.unwrap(), b"a");
    assert!(stats.peak_pending_capacity <= 1);
    let input = [b"a".as_slice(), &[b' '; 1000], b"b"].concat();
    let (result, stats) = decode(Encoding::QuotedPrintable, &input, &[10], 3);
    assert_eq!(result.unwrap_err(), DecodeError::DecodedLimit { limit: 3 });
    assert!(stats.decoded_bytes <= 3 && stats.peak_pending_capacity <= 3);
}
#[test]
fn internal_whitespace_preserves_its_exact_order_across_chunks() {
    let mut input = b"head".to_vec();
    for n in 0..100000 {
        input.push(b" \t\r"[n % 3]);
    }
    input.extend(b"tail");
    check(Encoding::QuotedPrintable, &input, &[65536, 99999]);
}

#[test]
fn tiny_output_buffers_resume_groups_escapes_pending_and_eof_flush() {
    for (kind, input) in [
        (Encoding::Identity, b"raw\x00\xffbytes".as_slice()),
        (Encoding::Base64, b"YQ==YWI=YWJj".as_slice()),
        (
            Encoding::QuotedPrintable,
            b"=G!head \t\r \t tail=4\nend=Q".as_slice(),
        ),
    ] {
        for width in [1, 2, 3, 4, 7, OUTPUT_CHUNK] {
            let mut decoder = Decoder::new(kind, MAX_DECODED_BYTES);
            let actual = pump(&mut decoder, input, true, width).unwrap();
            assert_eq!(actual, oracle(kind, input).unwrap());
            assert_eq!(
                decoder.step(b"", &mut [0; 1], true),
                Err(DecodeError::Closed)
            );
        }
    }
}

#[test]
fn qp_large_pending_run_drains_in_bounded_steps_before_triggering_input() {
    let mut decoder = Decoder::new(Encoding::QuotedPrintable, MAX_DECODED_BYTES);
    assert!(
        pump(
            &mut decoder,
            &vec![b' '; OUTPUT_CHUNK * 3 + 11],
            false,
            OUTPUT_CHUNK
        )
        .unwrap()
        .is_empty()
    );
    let mut output = [0; OUTPUT_CHUNK];
    let first = decoder.step(b"x", &mut output, true).unwrap();
    assert_eq!(
        first,
        Step {
            consumed: 0,
            produced: OUTPUT_CHUNK,
            status: StepStatus::NeedOutput
        }
    );
    assert!(output.iter().all(|byte| *byte == b' '));
    let rest = pump(&mut decoder, b"x", true, OUTPUT_CHUNK).unwrap();
    assert_eq!(
        rest,
        [vec![b' '; OUTPUT_CHUNK * 2 + 11], vec![b'x']].concat()
    );
}

#[test]
fn errors_and_successful_finish_are_terminal() {
    let mut finishing = Decoder::new(Encoding::QuotedPrintable, 10);
    assert_eq!(
        finishing.step(b"=Q", &mut [0; 1], true).unwrap().status,
        StepStatus::NeedOutput
    );
    assert_eq!(
        finishing.step(b"extra", &mut [0; 1], true),
        Err(DecodeError::Closed)
    );
    for (kind, input, eof, expected) in [
        (
            Encoding::Base64,
            b"YQ".as_slice(),
            true,
            DecodeError::InvalidBase64,
        ),
        (
            Encoding::Base64,
            b"!".as_slice(),
            false,
            DecodeError::InvalidBase64,
        ),
        (
            Encoding::Identity,
            b"xx".as_slice(),
            true,
            DecodeError::DecodedLimit { limit: 1 },
        ),
    ] {
        let mut decoder = Decoder::new(kind, 1);
        assert_eq!(decoder.step(input, &mut [0; 7], eof), Err(expected));
        assert_eq!(
            decoder.step(b"", &mut [0; 7], true),
            Err(DecodeError::Closed)
        );
    }
    for width in [0, OUTPUT_CHUNK + 1] {
        let mut decoder = Decoder::new(Encoding::Identity, 10);
        assert_eq!(
            decoder.step(b"", &mut vec![0; width], false),
            Err(DecodeError::InvalidOutputBuffer)
        );
        assert_eq!(
            decoder.step(b"", &mut [0; 1], true),
            Err(DecodeError::Closed)
        );
    }
}

#[test]
fn encoded_counter_overflow_fails_before_output() {
    let mut decoder = Decoder::new(Encoding::Identity, 10);
    decoder.stats.encoded_bytes = usize::MAX;
    let mut output = [0; 7];
    assert_eq!(
        decoder.step(b"x", &mut output, false),
        Err(DecodeError::InputCountOverflow)
    );
    assert_eq!(output, [0; 7]);
    assert_eq!(decoder.stats.decoded_bytes, 0);
}

#[test]
fn identity_preserves_every_byte_across_input_and_output_boundaries() {
    let bytes = (0..OUTPUT_CHUNK * 2 + 17)
        .map(|n| (n % 256) as u8)
        .collect::<Vec<_>>();
    for width in [1, 2, 7, OUTPUT_CHUNK - 1, OUTPUT_CHUNK] {
        let mut decoder = Decoder::new(Encoding::Identity, MAX_DECODED_BYTES);
        let mut actual = Vec::new();
        for chunk in bytes.chunks(OUTPUT_CHUNK - 3) {
            actual.extend(pump(&mut decoder, chunk, false, width).unwrap());
        }
        actual.extend(pump(&mut decoder, b"", true, width).unwrap());
        assert_eq!(actual, bytes);
    }
}

#[test]
fn exact_32m_and_plus_one_are_checked_before_output() {
    for kind in [
        Encoding::Identity,
        Encoding::Base64,
        Encoding::QuotedPrintable,
    ] {
        let mut decoder = Decoder::new(kind, usize::MAX);
        let block = if kind == Encoding::Base64 {
            vec![b'A'; OUTPUT_CHUNK]
        } else {
            vec![b'x'; OUTPUT_CHUNK]
        };
        let mut total = 0;
        let iterations = if kind == Encoding::Base64 { 682 } else { 512 };
        for _ in 0..iterations {
            total += pump(&mut decoder, &block, false, OUTPUT_CHUNK)
                .unwrap()
                .len();
        }
        if kind == Encoding::Base64 {
            total += pump(&mut decoder, &vec![b'A'; 43688], false, OUTPUT_CHUNK)
                .unwrap()
                .len();
            total += pump(&mut decoder, b"AAA=", false, OUTPUT_CHUNK)
                .unwrap()
                .len();
        }
        assert_eq!(total, MAX_DECODED_BYTES);
        assert_eq!(decoder.stats.decoded_bytes, MAX_DECODED_BYTES);
        let excess = if kind == Encoding::Base64 {
            b"YQ==".as_slice()
        } else {
            b"x".as_slice()
        };
        let mut output = [0; 7];
        assert_eq!(
            decoder.step(excess, &mut output, false),
            Err(DecodeError::DecodedLimit {
                limit: MAX_DECODED_BYTES
            })
        );
        assert_eq!(output, [0; 7]);
        assert_eq!(decoder.stats.decoded_bytes, MAX_DECODED_BYTES);
    }
}

#[test]
fn pending_capacity_stops_at_32m_and_trailing_overflow_is_trimmed() {
    let mut decoder = Decoder::new(Encoding::QuotedPrintable, MAX_DECODED_BYTES);
    let block = [b' '; OUTPUT_CHUNK];
    for _ in 0..513 {
        assert!(
            pump(&mut decoder, &block, false, OUTPUT_CHUNK)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(decoder.stats.peak_pending_capacity, MAX_DECODED_BYTES);
    let State::QuotedPrintable(state) = &decoder.state else {
        unreachable!()
    };
    assert!(state.overflow);
    assert_eq!(state.pending.capacity(), MAX_DECODED_BYTES);
    assert!(pump(&mut decoder, b"", true, 1).unwrap().is_empty());
    assert_eq!(decoder.stats.decoded_bytes, 0);
}

#[test]
fn exact_32m_can_finish_and_pending_newline_normalizes_after_overflow() {
    let mut decoder = Decoder::new(Encoding::Identity, MAX_DECODED_BYTES);
    for _ in 0..512 {
        pump(&mut decoder, &[b'x'; OUTPUT_CHUNK], false, OUTPUT_CHUNK).unwrap();
    }
    assert!(
        pump(&mut decoder, b"", true, OUTPUT_CHUNK)
            .unwrap()
            .is_empty()
    );
    let mut qp = Decoder::new(Encoding::QuotedPrintable, 2);
    assert_eq!(pump(&mut qp, &[b' '; 1000], false, 1).unwrap(), b"");
    assert_eq!(pump(&mut qp, b"\n", true, 1).unwrap(), b"\r\n");
}
