use super::*;

#[test]
fn decoded_limit_is_charged_before_any_sink_receives_excess() {
    for (encoding, first, last) in [
        (Encoding::Base64, b"YW".as_slice(), b"Jj".as_slice()),
        (
            Encoding::QuotedPrintable,
            b"=61=62=".as_slice(),
            b"63".as_slice(),
        ),
    ] {
        for limit in 0..=3 {
            let mut decoder = Decoder::new(encoding, limit);
            let mut output = [0; 8];
            let result = decoder.step(first, &mut output, false).and_then(|step| {
                assert_eq!(step.status, StepStatus::NeedInput);
                assert_eq!(step.consumed, first.len());
                let last = decoder.step(last, &mut output[step.produced..], true)?;
                assert_eq!(last.status, StepStatus::Finished);
                Ok(step.produced + last.produced)
            });
            if limit < 3 {
                assert_eq!(result.unwrap_err(), DecodeError::DecodedLimit { limit });
                assert!(decoder.stats.decoded_bytes <= limit);
                assert!(decoder.stats.produced_bytes <= limit);
            } else {
                assert_eq!(&output[..result.unwrap()], b"abc");
            }
        }
    }
    assert_eq!(
        Decoder::new(Encoding::Base64, usize::MAX).limit,
        MAX_DECODED_BYTES
    );
}
